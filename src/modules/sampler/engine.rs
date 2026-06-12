//! The sampler's playback engine: reels, voices, splices, and grains.
//!
//! Tape mode (`gene = 0`) is a varispeed splice player — one-shot,
//! loop, gated, or hold, forwards or backwards, repitched by note and
//! knob. Raising `gene` turns the voice into a two-headed granular
//! reader (the Morphagene move): raised-cosine windowed grains of
//! gene-length, 50% overlap, cycling at `slide`'s position inside the
//! splice. Same religion as the DLD engine: if any seam or grain edge
//! clicks, that is a bug in this file.

/// Polyphony per module instance.
pub const NUM_VOICES: usize = 6;
/// Per-slot reel cap, seconds (RAM: 120 s mono f32 ≈ 23 MB).
pub const MAX_REEL_SECS: f32 = 120.0;

/// A loaded sample: mono, at the engine's sample rate.
#[derive(Debug, Clone, Default)]
pub struct Reel {
    pub data: Vec<f32>,
    pub name: String,
}

/// Trigger behavior for a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    OneShot,
    Loop,
    Gated,
    Hold,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::OneShot => "oneshot",
            Mode::Loop => "loop",
            Mode::Gated => "gated",
            Mode::Hold => "hold",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "oneshot" | "one-shot" | "1shot" => Some(Mode::OneShot),
            "loop" => Some(Mode::Loop),
            "gated" | "gate" => Some(Mode::Gated),
            "hold" => Some(Mode::Hold),
            _ => None,
        }
    }

    pub fn cycle(self, by: i32) -> Self {
        const ALL: [Mode; 4] = [Mode::OneShot, Mode::Loop, Mode::Gated, Mode::Hold];
        let i = ALL.iter().position(|m| *m == self).unwrap_or(0) as i32;
        ALL[(i + by).rem_euclid(4) as usize]
    }
}

/// One slot's designer settings (the UI rows, engine-side).
#[derive(Debug, Clone, Copy)]
pub struct SlotParams {
    pub mode: Mode,
    /// Splice window into the reel, both 0–1 (len is relative to the
    /// remainder after start).
    pub start: f32,
    pub len: f32,
    /// Semitone offset, ±24.
    pub pitch: f32,
    /// Varispeed, −2…+2; negative reads backwards.
    pub speed: f32,
    /// 0 = tape playback; >0 = grain size, log 1 s → 10 ms.
    pub gene: f32,
    /// Grain position inside the splice, 0–1.
    pub slide: f32,
    /// AD envelope knobs, 0–1 (log 1 ms → 4 s).
    pub atk: f32,
    pub dec: f32,
    pub level: f32,
}

impl Default for SlotParams {
    fn default() -> Self {
        Self {
            mode: Mode::OneShot,
            start: 0.0,
            len: 1.0,
            pitch: 0.0,
            speed: 1.0,
            gene: 0.0,
            slide: 0.0,
            atk: 0.1,
            dec: 0.5,
            level: 0.8,
        }
    }
}

/// Envelope knob (0–1) → seconds, logarithmic 1 ms – 4 s.
pub fn env_secs(knob: f32) -> f32 {
    0.001 * 4000.0_f32.powf(knob.clamp(0.0, 1.0))
}

/// Gene knob (0–1) → grain length in seconds, logarithmic 1 s → 10 ms.
pub fn gene_secs(knob: f32) -> f32 {
    1.0 * 0.01_f32.powf(knob.clamp(0.0, 1.0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Attack,
    Sustain,
    Decay,
    Done,
}

#[derive(Debug, Clone, Copy, Default)]
struct Grain {
    pos: f64,
    age: f64,
    len: f64,
    active: bool,
}

#[derive(Debug, Clone)]
pub struct Voice {
    pub slot: usize,
    pub note: u8,
    active: bool,
    gate: bool,
    pos: f64,
    env: f32,
    stage: Stage,
    grains: [Grain; 2],
    /// Frames until the next grain head spawns.
    spawn_in: f64,
    vel: f32,
    age: u64,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            slot: 0,
            note: 60,
            active: false,
            gate: false,
            pos: 0.0,
            env: 0.0,
            stage: Stage::Done,
            grains: [Grain::default(); 2],
            spawn_in: 0.0,
            vel: 1.0,
            age: 0,
        }
    }
}

pub struct Engine {
    pub voices: [Voice; NUM_VOICES],
    sample_rate: f32,
    counter: u64,
    /// Loudest live envelope after the last block (modbus `env` out).
    pub env_out: f32,
}

fn hann(t: f64) -> f32 {
    // raised cosine over 0..1
    (0.5 - 0.5 * (std::f64::consts::TAU * t).cos()) as f32
}

impl Engine {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            voices: std::array::from_fn(|_| Voice::default()),
            sample_rate,
            counter: 0,
            env_out: 0.0,
        }
    }

    /// Steal the oldest-done (else oldest) voice.
    fn alloc(&mut self) -> usize {
        let mut best = 0;
        let mut best_score = u64::MAX;
        for (i, v) in self.voices.iter().enumerate() {
            let score = if !v.active { v.age / 2 } else { v.age };
            if score < best_score {
                best_score = score;
                best = i;
            }
        }
        best
    }

    pub fn trigger(&mut self, slot: usize, note: u8, vel: f32, p: &SlotParams, reel_len: usize) {
        if reel_len == 0 {
            return;
        }
        let i = self.alloc();
        self.counter += 1;
        let splice = splice_of(p, reel_len);
        let v = &mut self.voices[i];
        v.slot = slot;
        v.note = note;
        v.active = true;
        v.gate = true;
        v.vel = vel;
        v.env = 0.0;
        v.stage = Stage::Attack;
        v.age = self.counter;
        v.grains = [Grain::default(); 2];
        v.spawn_in = 0.0;
        // start where the direction enters the splice
        v.pos = if p.speed < 0.0 {
            (splice.1 - 1) as f64
        } else {
            splice.0 as f64
        };
    }

    pub fn release(&mut self, slot: usize, note: u8) {
        for v in self.voices.iter_mut() {
            if v.active && v.slot == slot && v.note == note {
                v.gate = false;
            }
        }
    }

    /// Render one block, summing into `out` (mono).
    pub fn process(
        &mut self,
        out: &mut [f32],
        reels: &[Option<Reel>],
        params: &[SlotParams],
        kit_pitch_track: bool,
    ) {
        let sr = self.sample_rate as f64;
        let mut env_peak = 0.0_f32;
        for v in self.voices.iter_mut() {
            if !v.active {
                continue;
            }
            let Some(p) = params.get(v.slot) else {
                v.active = false;
                continue;
            };
            let Some(reel) = reels.get(v.slot).and_then(|r| r.as_ref()) else {
                v.active = false;
                continue;
            };
            let n = reel.data.len();
            if n == 0 {
                v.active = false;
                continue;
            }
            let (s0, s1) = splice_of(p, n);
            let splice_len = (s1 - s0) as f64;
            let semis = if kit_pitch_track {
                p.pitch
            } else {
                p.pitch + (v.note as f32 - 60.0)
            };
            let inc = (2.0_f64).powf(semis as f64 / 12.0) * p.speed as f64;
            let atk = (env_secs(p.atk) * sr as f32).max(1.0);
            let dec = (env_secs(p.dec) * sr as f32).max(1.0);
            let gene_frames = (gene_secs(p.gene) as f64 * sr).max(64.0);

            for o in out.iter_mut() {
                // envelope. Stage semantics per mode:
                //   oneshot: sustain through the splice, decay at its
                //            end (ignores note-off — a drum is a drum)
                //   loop:    sustain while the gate holds, release on
                //            note-off (wraps underneath)
                //   gated:   like loop, but the splice end also releases
                //   hold:    attack then decay on its own clock while
                //            looping — a self-fading loop, gate ignored
                match v.stage {
                    Stage::Attack => {
                        v.env += 1.0 / atk;
                        if v.env >= 1.0 {
                            v.env = 1.0;
                            v.stage = match p.mode {
                                Mode::Hold => Stage::Decay,
                                _ => Stage::Sustain,
                            };
                        }
                    }
                    Stage::Sustain => {}
                    Stage::Decay => {
                        v.env -= 1.0 / dec;
                        if v.env <= 0.0 {
                            v.env = 0.0;
                            v.stage = Stage::Done;
                            v.active = false;
                            break;
                        }
                    }
                    Stage::Done => {
                        v.active = false;
                        break;
                    }
                }
                // note-off releases loop/gated voices from any stage
                if !v.gate
                    && matches!(p.mode, Mode::Loop | Mode::Gated)
                    && v.stage != Stage::Decay
                    && v.stage != Stage::Done
                {
                    v.stage = Stage::Decay;
                }

                let sample = if p.gene <= 0.001 {
                    // ── tape playback ────────────────────────────────
                    v.pos += inc;
                    let end = s1 as f64;
                    let beg = s0 as f64;
                    if v.pos >= end || v.pos < beg {
                        match p.mode {
                            Mode::Loop | Mode::Gated => {
                                if p.mode == Mode::Gated && v.stage == Stage::Sustain {
                                    v.stage = Stage::Decay;
                                }
                                let span = (end - beg).max(1.0);
                                v.pos = if inc >= 0.0 {
                                    beg + (v.pos - end).rem_euclid(span)
                                } else {
                                    end - 1.0 - (beg - v.pos).rem_euclid(span)
                                };
                            }
                            Mode::Hold => {
                                let span = (end - beg).max(1.0);
                                v.pos = if inc >= 0.0 {
                                    beg + (v.pos - end).rem_euclid(span)
                                } else {
                                    end - 1.0 - (beg - v.pos).rem_euclid(span)
                                };
                            }
                            Mode::OneShot => {
                                if v.stage != Stage::Decay {
                                    v.stage = Stage::Decay;
                                }
                                v.pos = v.pos.clamp(beg, end - 1.0);
                            }
                        }
                    }
                    read(&reel.data, v.pos)
                } else {
                    // ── grains: two heads, hann windows, 50% overlap ─
                    v.spawn_in -= 1.0;
                    if v.spawn_in <= 0.0 {
                        let head = if v.grains[0].active { 1 } else { 0 };
                        let base =
                            s0 as f64 + p.slide as f64 * (splice_len - gene_frames).max(0.0);
                        v.grains[head] = Grain {
                            pos: base,
                            age: 0.0,
                            len: gene_frames,
                            active: true,
                        };
                        v.spawn_in = gene_frames / 2.0;
                    }
                    let mut acc = 0.0_f32;
                    for g in v.grains.iter_mut() {
                        if !g.active {
                            continue;
                        }
                        let w = hann((g.age / g.len).clamp(0.0, 1.0));
                        let pos = if inc >= 0.0 {
                            g.pos + g.age * inc
                        } else {
                            g.pos + g.len + g.age * inc
                        };
                        let wrapped = s0 as f64 + (pos - s0 as f64).rem_euclid(splice_len.max(1.0));
                        acc += read(&reel.data, wrapped) * w;
                        g.age += 1.0;
                        if g.age >= g.len {
                            g.active = false;
                        }
                    }
                    acc
                };

                *o += sample * v.env * v.env * p.level * v.vel;
                env_peak = env_peak.max(v.env);
            }
        }
        self.env_out = env_peak;
    }
}

/// The splice window in frames.
fn splice_of(p: &SlotParams, n: usize) -> (usize, usize) {
    let s0 = ((p.start.clamp(0.0, 0.99) as f64) * n as f64) as usize;
    let span = (((p.len.clamp(0.01, 1.0)) as f64) * (n - s0) as f64).max(64.0) as usize;
    (s0, (s0 + span).min(n))
}

fn read(data: &[f32], pos: f64) -> f32 {
    let n = data.len();
    if n == 0 {
        return 0.0;
    }
    let i = (pos.floor() as usize).min(n - 1);
    let j = (i + 1).min(n - 1);
    let frac = (pos - pos.floor()) as f32;
    data[i] * (1.0 - frac) + data[j] * frac
}

/// Kit mode: note → slot (C→0 … G→7, any octave).
pub fn kit_slot(note: u8) -> Option<usize> {
    let pc = note % 12;
    (pc < 8).then_some(pc as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reel(n: usize) -> Option<Reel> {
        // identifiable ramp + sine mix
        Some(Reel {
            data: (0..n)
                .map(|i| (i as f32 * 0.05).sin() * 0.8)
                .collect(),
            name: "test".into(),
        })
    }

    fn engine() -> Engine {
        Engine::new(48_000.0)
    }

    fn slots(p: SlotParams) -> Vec<SlotParams> {
        vec![p; 8]
    }

    #[test]
    fn oneshot_plays_once_and_dies() {
        let mut e = engine();
        let reels: Vec<Option<Reel>> = (0..8).map(|_| reel(4_800)).collect();
        let p = SlotParams { dec: 0.2, ..Default::default() };
        e.trigger(0, 60, 1.0, &p, 4_800);
        let mut heard = 0.0_f32;
        for _ in 0..200 {
            let mut out = vec![0.0; 64];
            e.process(&mut out, &reels, &slots(p), true);
            heard += out.iter().map(|s| s.abs()).sum::<f32>();
        }
        assert!(heard > 1.0, "one-shot made sound: {heard}");
        assert!(!e.voices.iter().any(|v| v.active), "voice retired at splice end");
    }

    #[test]
    fn loop_mode_sustains_until_release() {
        let mut e = engine();
        let reels: Vec<Option<Reel>> = (0..8).map(|_| reel(2_400)).collect();
        let p = SlotParams { mode: Mode::Loop, atk: 0.0, dec: 0.3, ..Default::default() };
        e.trigger(2, 64, 1.0, &p, 2_400);
        for _ in 0..150 {
            let mut out = vec![0.0; 64];
            e.process(&mut out, &reels, &slots(p), true);
        }
        assert!(e.voices.iter().any(|v| v.active), "gated voice sustains past splice");
        e.release(2, 64);
        for _ in 0..2_000 {
            let mut out = vec![0.0; 64];
            e.process(&mut out, &reels, &slots(p), true);
            if !e.voices.iter().any(|v| v.active) {
                return;
            }
        }
        panic!("gated voice never released");
    }

    #[test]
    fn pitch_doubles_speed_per_octave() {
        // play a 4800-frame splice at +12 st: voice should exhaust the
        // splice in half the frames of the unpitched one
        let reels: Vec<Option<Reel>> = (0..8).map(|_| reel(4_800)).collect();
        let frames_until_done = |pitch: f32| {
            let mut e = engine();
            let p = SlotParams { pitch, dec: 0.05, ..Default::default() };
            e.trigger(0, 60, 1.0, &p, 4_800);
            let mut frames = 0;
            for _ in 0..400 {
                let mut out = vec![0.0; 64];
                e.process(&mut out, &reels, &slots(p), true);
                if !e.voices.iter().any(|v| v.active) {
                    break;
                }
                frames += 64;
            }
            frames
        };
        let base = frames_until_done(0.0);
        let up = frames_until_done(12.0);
        let ratio = base as f32 / up as f32;
        assert!((1.8..=2.2).contains(&ratio), "octave-up ratio {ratio} (base {base}, up {up})");
    }

    #[test]
    fn reverse_speed_reads_backwards_without_dying_instantly() {
        let mut e = engine();
        let reels: Vec<Option<Reel>> = (0..8).map(|_| reel(9_600)).collect();
        let p = SlotParams { speed: -1.0, dec: 0.9, ..Default::default() };
        e.trigger(0, 60, 1.0, &p, 9_600);
        let mut heard = 0.0_f32;
        for _ in 0..100 {
            let mut out = vec![0.0; 64];
            e.process(&mut out, &reels, &slots(p), true);
            heard += out.iter().map(|s| s.abs()).sum::<f32>();
        }
        assert!(heard > 1.0, "reverse playback audible: {heard}");
    }

    #[test]
    fn grains_never_click() {
        // gene playback over a loud sine: max sample step must stay
        // within window-slope bounds even while slide scrubs hard
        let n = 48_000;
        let reels: Vec<Option<Reel>> = (0..8)
            .map(|_| {
                Some(Reel {
                    data: (0..n).map(|i| (i as f32 * 0.06).sin() * 0.9).collect(),
                    name: "sine".into(),
                })
            })
            .collect();
        let mut e = engine();
        let mut p = SlotParams {
            mode: Mode::Gated,
            gene: 0.5,
            atk: 0.2,
            dec: 0.5,
            ..Default::default()
        };
        e.trigger(0, 60, 1.0, &p, n);
        let mut prev = 0.0_f32;
        let mut max_step = 0.0_f32;
        for blk in 0..600 {
            p.slide = (blk as f32 * 0.011) % 1.0; // hard scrub
            let mut out = vec![0.0; 64];
            e.process(&mut out, &reels, &slots(p), true);
            for &s in &out {
                max_step = max_step.max((s - prev).abs());
                prev = s;
            }
        }
        assert!(max_step < 0.3, "grain edges click: max step {max_step}");
    }

    #[test]
    fn kit_maps_white_keys_to_slots() {
        assert_eq!(kit_slot(60), Some(0)); // C
        assert_eq!(kit_slot(61), Some(1));
        assert_eq!(kit_slot(67), Some(7)); // G
        assert_eq!(kit_slot(68), None); // G# — out of the kit
        assert_eq!(kit_slot(72), Some(0)); // any octave
    }

    #[test]
    fn knob_curves_are_sane() {
        assert!((env_secs(0.0) - 0.001).abs() < 1e-4);
        assert!((env_secs(1.0) - 4.0).abs() < 0.01);
        assert!((gene_secs(0.0) - 1.0).abs() < 1e-3);
        assert!((gene_secs(1.0) - 0.01).abs() < 1e-3);
    }

    #[test]
    fn polyphony_steals_oldest() {
        let mut e = engine();
        let reels: Vec<Option<Reel>> = (0..8).map(|_| reel(48_000)).collect();
        let p = SlotParams { mode: Mode::Gated, ..Default::default() };
        for k in 0..NUM_VOICES + 2 {
            e.trigger(k % 8, 60 + k as u8, 1.0, &p, 48_000);
            let mut out = vec![0.0; 16];
            e.process(&mut out, &reels, &slots(p), true);
        }
        let live = e.voices.iter().filter(|v| v.active).count();
        assert_eq!(live, NUM_VOICES, "all voices in use, oldest stolen");
    }
}
