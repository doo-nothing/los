//! The DLD's tape engine — one channel of clean, clickless looping delay.
//!
//! The manual's metaphor, implemented literally: a ring of "tape" moving
//! at a constant sample rate under a write head and a read head. Delay
//! mode keeps the read head a fixed distance behind the write head;
//! Infinite Hold freezes writing and cycles the read head around a loop.
//! Every discontinuous head move — time change, reverse, hold toggle,
//! window scroll, clear — crossfades between the old and new read
//! trajectories over [`XFADE`] samples. If a head move ever clicks,
//! that's a bug in this file.
//!
//! Deliberate v1 deviation from hardware: Reverse flips only the *read*
//! trajectory (the hardware swaps both heads, so new audio recorded
//! during reverse comes out forwards; here it comes out reversed).
//! Documented in docs/plans/dld.md.

/// Crossfade length for head moves, in samples (~5 ms at 48 k).
pub const XFADE: usize = 256;
/// Loop-wrap crossfade in hold mode (~1.3 ms) — cleaner than hardware.
const WRAP_FADE: usize = 64;
/// Per-sample smoothing time for gain-ish params (~5 ms at 48 k).
const SMOOTH: f32 = 1.0 / 240.0;

/// One read trajectory: a position moving ±1 per sample around `len`.
#[derive(Debug, Clone, Copy)]
struct Head {
    pos: f64,
    fwd: bool,
}

impl Head {
    fn step(&mut self, len: usize) {
        if self.fwd {
            self.pos += 1.0;
            if self.pos >= len as f64 {
                self.pos -= len as f64;
            }
        } else {
            self.pos -= 1.0;
            if self.pos < 0.0 {
                self.pos += len as f64;
            }
        }
    }
}

/// Linear-interpolated read at a fractional position.
fn tap(buf: &[f32], pos: f64) -> f32 {
    let len = buf.len();
    let i = pos.floor() as usize % len;
    let j = (i + 1) % len;
    let frac = (pos - pos.floor()) as f32;
    buf[i] * (1.0 - frac) + buf[j] * frac
}

/// The loop's gentle limiter: keeps the 110% bloom musical instead of
/// explosive. tanh-shaped, unity below ~0.5.
fn soft_clip(x: f32) -> f32 {
    if x.abs() < 0.5 {
        x
    } else {
        x.signum() * (0.5 + (x.abs() - 0.5).tanh() * 0.5)
    }
}

/// Per-block control snapshot for one channel.
#[derive(Debug, Clone, Copy)]
pub struct ChannelParams {
    /// Read-head distance behind the write head, in samples (delay mode),
    /// or the loop length (hold mode).
    pub delay_samples: f32,
    /// 0.0–1.1 — feedback into the write path.
    pub feedback: f32,
    /// 0.0–1.0 — record level (Delay Feed).
    pub feed: f32,
    /// 0.0–1.0 — dry/wet on the output.
    pub mix: f32,
    /// Infinite Hold: stop writing, loop `delay_samples` of memory.
    pub hold: bool,
    /// Reverse the read trajectory.
    pub reverse: bool,
    /// 0.0–1.0 — hold-mode window scroll: start point offset as a
    /// fraction of one loop length.
    pub window: f32,
}

pub struct Channel {
    buf: Vec<f32>,
    write: usize,
    /// Live read head.
    head: Head,
    /// Fading-out former read head (after a discontinuous move).
    old_head: Option<Head>,
    xfade_left: usize,
    xfade_total: usize,
    /// The trajectory we are currently honoring (to detect changes).
    cur_delay: f32,
    cur_rev: bool,
    cur_hold: bool,
    cur_window: f32,
    /// Hold-mode loop geometry (absolute buffer positions).
    loop_start: f64,
    loop_len: f64,
    /// Smoothed gains (primed to the first block's params so nothing
    /// fades in from zero at boot).
    primed: bool,
    s_feedback: f32,
    s_feed: f32,
    s_mix: f32,
    /// Phase 0..1 through the current delay/loop period, for the loop
    /// clock output.
    pub loop_phase: f32,
}

impl Channel {
    pub fn new(max_samples: usize) -> Self {
        Self {
            buf: vec![0.0; max_samples.max(XFADE * 4)],
            write: 0,
            head: Head { pos: 0.0, fwd: true },
            old_head: None,
            xfade_left: 0,
            xfade_total: XFADE,
            cur_delay: 0.0,
            cur_rev: false,
            cur_hold: false,
            cur_window: 0.0,
            loop_start: 0.0,
            loop_len: 1.0,
            primed: false,
            s_feedback: 0.0,
            s_feed: 0.0,
            s_mix: 0.0,
            loop_phase: 0.0,
        }
    }

    /// Begin a crossfade from the current head to a new trajectory.
    fn move_head(&mut self, new: Head) {
        self.old_head = Some(self.head);
        self.head = new;
        self.xfade_left = XFADE;
        self.xfade_total = XFADE;
    }

    fn delay_head(&mut self, delay: f32, rev: bool) -> Head {
        let len = self.buf.len() as f64;
        let mut pos = self.write as f64 - delay as f64;
        while pos < 0.0 {
            pos += len;
        }
        Head { pos, fwd: !rev }
    }

    /// Fade-out, zero, fade-in. The fades happen via the normal
    /// crossfade engine: we declare the buffer silent and move the head.
    pub fn clear(&mut self) {
        self.buf.iter_mut().for_each(|v| *v = 0.0);
        let h = self.delay_head(self.cur_delay, self.cur_rev);
        self.move_head(h);
    }

    /// React to control changes, scheduling crossfaded head moves.
    fn retarget(&mut self, p: &ChannelParams) {
        let delay = p.delay_samples.clamp(XFADE as f32, (self.buf.len() - XFADE) as f32);
        if p.hold && !self.cur_hold {
            // Entering hold: the loop is the last `delay` worth of tape,
            // positioned so the head is INSIDE it regardless of
            // direction (a reversed head plays end->start; anchoring a
            // reversed head at the start put it outside the loop and
            // double-fired the wrap fade).
            self.loop_len = delay as f64;
            let len = self.buf.len() as f64;
            self.loop_start = if self.head.fwd {
                self.head.pos
            } else {
                (self.head.pos - self.loop_len + 1.0).rem_euclid(len)
            };
            self.cur_window = p.window;
        }
        if p.hold {
            // Window scroll and loop-length changes move the start.
            if (p.window - self.cur_window).abs() > 1e-3 {
                let len = self.buf.len() as f64;
                let delta = (p.window - self.cur_window) as f64 * self.loop_len;
                self.loop_start = (self.loop_start + delta).rem_euclid(len);
                self.cur_window = p.window;
                let pos = if p.reverse {
                    (self.loop_start + self.loop_len - 1.0).rem_euclid(len)
                } else {
                    self.loop_start
                };
                self.move_head(Head { pos, fwd: !p.reverse });
            }
            if (delay - self.cur_delay).abs() > 1.0 {
                self.loop_len = delay as f64;
            }
        } else if (delay - self.cur_delay).abs() > 1.0
            || p.reverse != self.cur_rev
            || self.cur_hold
        {
            let h = self.delay_head(delay, p.reverse);
            self.move_head(h);
        }
        self.cur_delay = delay;
        self.cur_rev = p.reverse;
        self.cur_hold = p.hold;
    }

    /// Process one block: `input` mono in, returns into `out` (mono).
    pub fn process(&mut self, input: &[f32], out: &mut [f32], p: &ChannelParams) {
        if !self.primed {
            self.primed = true;
            self.s_feedback = p.feedback;
            self.s_feed = p.feed;
            self.s_mix = p.mix;
            self.cur_delay = p.delay_samples;
            self.cur_rev = p.reverse;
            self.head = self.delay_head(p.delay_samples, p.reverse);
        }
        self.retarget(p);
        let len = self.buf.len();
        let flen = len as f64;
        for (n, (&x, o)) in input.iter().zip(out.iter_mut()).enumerate() {
            let _ = n;
            self.s_feedback += (p.feedback - self.s_feedback) * SMOOTH;
            self.s_feed += (p.feed - self.s_feed) * SMOOTH;
            self.s_mix += (p.mix - self.s_mix) * SMOOTH;

            // hold-mode loop wrap (with its own little crossfade)
            if p.hold {
                let rel = (self.head.pos - self.loop_start).rem_euclid(flen);
                if rel >= self.loop_len {
                    let h = if self.head.fwd {
                        Head { pos: self.loop_start, fwd: true }
                    } else {
                        let end = (self.loop_start + self.loop_len - 1.0).rem_euclid(flen);
                        Head { pos: end, fwd: false }
                    };
                    self.old_head = Some(self.head);
                    self.head = h;
                    self.xfade_left = WRAP_FADE;
                    self.xfade_total = WRAP_FADE;
                }
                self.loop_phase = (rel / self.loop_len).min(1.0) as f32;
            } else {
                let dist = (self.write as f64 - self.head.pos).rem_euclid(flen);
                self.loop_phase = (dist / self.cur_delay.max(1.0) as f64).min(1.0) as f32;
                // Reverse delay plays in segments: the backwards head
                // falls behind the advancing write head at 2 samples per
                // sample; once it trails by 2x the delay, re-anchor it at
                // the write head with a crossfade. Without this seam the
                // heads CROSS and the content jump at the crossing is an
                // unfadeable click (every reverse delay does some form
                // of this).
                if !self.head.fwd && dist > 2.0 * self.cur_delay as f64 {
                    let h = Head { pos: self.write as f64, fwd: false };
                    self.old_head = Some(self.head);
                    self.head = h;
                    self.xfade_left = XFADE;
                    self.xfade_total = XFADE;
                }
            }

            // read (crossfading trajectories when a move is in flight)
            let mut wet = tap(&self.buf, self.head.pos);
            if let Some(mut old) = self.old_head.take() {
                if self.xfade_left > 0 {
                    let t = 1.0 - self.xfade_left as f32 / self.xfade_total as f32;
                    wet = wet * t + tap(&self.buf, old.pos) * (1.0 - t);
                    old.step(len);
                    self.old_head = Some(old);
                    self.xfade_left -= 1;
                }
            }

            // write (delay mode only)
            if !p.hold {
                self.buf[self.write] = soft_clip(x * self.s_feed + wet * self.s_feedback);
                self.write = (self.write + 1) % len;
            }

            self.head.step(len);
            *o = x * (1.0 - self.s_mix) + wet * self.s_mix;
        }
    }
}

/// Time arithmetic: knob (1–16) and switch applied to one beat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeSwitch {
    /// Eighth-notes: knob/8 beats.
    Eighth,
    /// Plain beats.
    #[default]
    Beats,
    /// knob + 16 beats.
    Plus16,
}

impl TimeSwitch {
    pub fn name(self) -> &'static str {
        match self {
            TimeSwitch::Eighth => "/8",
            TimeSwitch::Beats => "=",
            TimeSwitch::Plus16 => "+16",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "/8" | "eighth" => Some(TimeSwitch::Eighth),
            "=" | "beats" => Some(TimeSwitch::Beats),
            "+16" | "plus16" => Some(TimeSwitch::Plus16),
            _ => None,
        }
    }

    pub fn beats(self, knob: f32) -> f32 {
        match self {
            TimeSwitch::Eighth => knob / 8.0,
            TimeSwitch::Beats => knob,
            TimeSwitch::Plus16 => knob + 16.0,
        }
    }
}

/// Delay/loop time in samples for a knob+switch against a beat.
pub fn delay_samples(knob: f32, switch: TimeSwitch, beat_secs: f32, sample_rate: f32) -> f32 {
    switch.beats(knob) * beat_secs * sample_rate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(delay: f32) -> ChannelParams {
        ChannelParams {
            delay_samples: delay,
            feedback: 0.0,
            feed: 1.0,
            mix: 1.0,
            hold: false,
            reverse: false,
            window: 0.0,
        }
    }

    fn run(ch: &mut Channel, input: &[f32], p: &ChannelParams) -> Vec<f32> {
        let mut out = vec![0.0; input.len()];
        for (i_chunk, o_chunk) in input.chunks(64).zip(out.chunks_mut(64)) {
            ch.process(i_chunk, o_chunk, p);
        }
        out
    }

    #[test]
    fn time_arithmetic_matches_the_manual() {
        // manual example: beat = 0.5 s, knob 8, center → 4 s
        assert_eq!(delay_samples(8.0, TimeSwitch::Beats, 0.5, 48_000.0), 192_000.0);
        // switch down: eight 1/8th notes = 0.5 s
        assert_eq!(delay_samples(8.0, TimeSwitch::Eighth, 0.5, 48_000.0), 24_000.0);
        // knob 2, eighth: 0.125 s
        assert_eq!(delay_samples(2.0, TimeSwitch::Eighth, 0.5, 48_000.0), 6_000.0);
        // switch up: 18 beats = 9 s
        assert_eq!(delay_samples(2.0, TimeSwitch::Plus16, 0.5, 48_000.0), 432_000.0);
    }

    #[test]
    fn echoes_arrive_exactly_on_time() {
        let mut ch = Channel::new(48_000);
        let delay = 4_800.0; // 100 ms
        let mut input = vec![0.0_f32; 9_600];
        input[0] = 1.0;
        let p = ChannelParams { mix: 1.0, ..params(delay) };
        let out = run(&mut ch, &input, &p);
        // wet-only: the impulse must appear at exactly `delay` samples
        let peak_at = out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .unwrap()
            .0;
        assert!(
            (peak_at as i64 - 4_800).unsigned_abs() <= 1,
            "echo at {peak_at}, wanted 4800"
        );
    }

    #[test]
    fn feedback_repeats_decay_and_110_blooms_bounded() {
        let mut ch = Channel::new(48_000);
        let mut input = vec![0.0_f32; 48_000];
        input[0] = 0.8;
        let p = ChannelParams { feedback: 0.5, ..params(2_400.0) };
        let out = run(&mut ch, &input, &p);
        let e1 = out[2_400].abs();
        let e2 = out[4_800].abs();
        let e3 = out[7_200].abs();
        assert!(e1 > 0.5, "first echo present, got {e1}");
        assert!(e2 < e1 && e3 < e2, "echoes decay: {e1} {e2} {e3}");

        let mut ch = Channel::new(48_000);
        let p = ChannelParams { feedback: 1.1, ..params(2_400.0) };
        let out = run(&mut ch, &input, &p);
        let late = out[40_000..].iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        assert!(late > 0.3, "110% bloom sustains, got {late}");
        assert!(late < 1.2, "soft clip bounds the bloom, got {late}");
    }

    #[test]
    fn no_head_move_ever_clicks() {
        // continuous loud sine in; mid-stream we change time, reverse,
        // and toggle hold — the output must never step more than the
        // signal's own slope plus the crossfade slope allows.
        let sr = 48_000.0;
        let mut ch = Channel::new(48_000);
        let input: Vec<f32> = (0..48_000)
            .map(|i| (i as f32 * 220.0 * std::f32::consts::TAU / sr).sin() * 0.9)
            .collect();
        let mut out = Vec::new();
        let mut p = ChannelParams { feedback: 0.6, mix: 1.0, ..params(3_000.0) };
        for (n, chunk) in input.chunks(64).enumerate() {
            let mut o = vec![0.0; chunk.len()];
            match n {
                200 => p.delay_samples = 9_000.0, // time jump
                350 => p.reverse = true,          // reverse mid-flight
                500 => p.hold = true,             // freeze into a loop
                650 => p.window = 0.5,            // scroll the window
                _ => {}
            }
            ch.process(chunk, &mut o, &p);
            out.extend(o);
        }
        // 220 Hz at 0.9 moves at most ~0.026/sample; the crossfade can
        // add the full difference of two such signals over XFADE samples
        // (~0.007/sample). Anything over 0.12 is a click.
        let (at, max_step) = out
            .windows(2)
            .enumerate()
            .skip(100)
            .map(|(i, w)| (i, (w[1] - w[0]).abs()))
            .fold((0, 0.0_f32), |acc, x| if x.1 > acc.1 { x } else { acc });
        assert!(
            max_step < 0.12,
            "head moves click: max step {max_step} at sample {at} (chunk {})",
            at / 64
        );
    }

    #[test]
    fn hold_loops_the_right_length_and_window_scrolls() {
        let sr = 48_000;
        let mut ch = Channel::new(sr);
        // record a ramp so positions are identifiable by value
        let input: Vec<f32> = (0..sr).map(|i| (i % 4800) as f32 / 4800.0).collect();
        let p = ChannelParams { feed: 1.0, mix: 1.0, ..params(4_800.0) };
        let _ = run(&mut ch, &input, &p);
        // enter hold: the loop should cycle with period 4800
        let hold = ChannelParams { hold: true, ..p };
        let silence = vec![0.0_f32; 14_400];
        let out = run(&mut ch, &silence, &hold);
        let a = &out[1_000..2_000];
        let b = &out[1_000 + 4_800..2_000 + 4_800];
        let diff: f32 = a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / 1_000.0;
        assert!(diff < 0.05, "hold loop period off: mean diff {diff}");
    }

    #[test]
    fn clear_silences_without_a_bang() {
        let mut ch = Channel::new(48_000);
        let input: Vec<f32> = (0..9_600).map(|i| ((i % 100) as f32 / 50.0) - 1.0).collect();
        let p = ChannelParams { feedback: 0.9, ..params(2_400.0) };
        let _ = run(&mut ch, &input, &p);
        ch.clear();
        let out = run(&mut ch, &vec![0.0; 4_800], &p);
        let peak = out[600..].iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        assert!(peak < 1e-3, "buffer clear leaves audio: {peak}");
    }
}
