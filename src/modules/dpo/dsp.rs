//! The DPO's voice: two triangle-core oscillators, bidirectional FM,
//! sync/lock, and VCO B's timbre chain (shape → angle → fold), after
//! the Make Noise DPO (itself after the Buchla 259).
//!
//! Pure Rust phase cores — sync, lock, follow lag, and the Strike
//! vactrol are all control-flow on phase accumulators, which Rust
//! states plainly. The folder is the only "circuit": an iterative
//! reflector with the drive staged like the hardware (fold 0 ≈ clean,
//! 1 ≈ five reflections deep).

/// VCO A core behavior (the manual's LED modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AMode {
    /// Standard triangle core, free.
    #[default]
    Free,
    /// Weak sync: A resets with B only when near an integer ratio.
    Lock,
    /// Hard sync: B's wrap restarts A every cycle.
    Sync,
    /// A runs at LFO rate (its sine published on the modbus).
    Lfo,
}

impl AMode {
    pub fn name(self) -> &'static str {
        match self {
            AMode::Free => "free",
            AMode::Lock => "lock",
            AMode::Sync => "sync",
            AMode::Lfo => "lfo",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "free" => Some(AMode::Free),
            "lock" => Some(AMode::Lock),
            "sync" => Some(AMode::Sync),
            "lfo" => Some(AMode::Lfo),
            _ => None,
        }
    }

    pub fn cycle(self, by: i32) -> Self {
        const ALL: [AMode; 4] = [AMode::Free, AMode::Lock, AMode::Sync, AMode::Lfo];
        let i = ALL.iter().position(|m| *m == self).unwrap_or(0) as i32;
        ALL[(i + by).rem_euclid(4) as usize]
    }
}

/// Per-block control snapshot.
#[derive(Debug, Clone, Copy)]
pub struct DpoParams {
    /// VCO B base frequency (the note), Hz.
    pub freq_b: f32,
    /// VCO A frequency as a ratio of B (0.25–8; the FM-ratio knob).
    pub ratio: f32,
    /// Follow lag 0–1: 1 = A tracks B's pitch instantly, 0 = A frozen.
    pub follow: f32,
    /// FM bus index 0–1 (master depth, both directions).
    pub index: f32,
    /// Per-direction attenuators: B→A and A→B.
    pub fm_a: f32,
    pub fm_b: f32,
    pub mode: AMode,
    /// Timbre: shape morph (sine→spike→glitched tri), angle (harmonic
    /// tilt), fold (the folder), mod bus index scaling A-sine into all
    /// three.
    pub shape: f32,
    pub angle: f32,
    pub fold: f32,
    pub mod_index: f32,
    /// Strike gate level this block (the vactrol snaps fold open).
    pub strike: bool,
    /// Output blend: 0 = all B FINAL, 1 = all A.
    pub mix: f32,
    pub level: f32,
}

impl Default for DpoParams {
    fn default() -> Self {
        Self {
            freq_b: 110.0,
            ratio: 1.0,
            follow: 1.0,
            index: 0.0,
            fm_a: 0.5,
            fm_b: 0.5,
            mode: AMode::Free,
            shape: 0.0,
            angle: 0.0,
            fold: 0.0,
            mod_index: 0.0,
            strike: false,
            mix: 0.0,
            level: 0.8,
        }
    }
}

pub struct Dpo {
    sample_rate: f32,
    phase_a: f32,
    phase_b: f32,
    /// Follow-lagged base frequency for A's note tracking.
    freq_followed: f32,
    /// Strike vactrol state (0–1, fast rise / slow fall).
    strike_env: f32,
    /// Last sines, for the one-sample cross-FM feedback.
    sine_a: f32,
    sine_b: f32,
    /// UI taps.
    pub a_out: f32,
    pub b_out: f32,
}

fn tri(p: f32) -> f32 {
    1.0 - 4.0 * (p - 0.5).abs()
}

/// The folder: reflect into ±1 up to five times; drive staged so fold 0
/// passes clean and fold 1 is deep in the reflections.
pub fn fold(x: f32, amount: f32) -> f32 {
    let mut y = x * (1.0 + amount * 5.0);
    for _ in 0..5 {
        if y > 1.0 {
            y = 2.0 - y;
        } else if y < -1.0 {
            y = -2.0 - y;
        } else {
            break;
        }
    }
    y.clamp(-1.0, 1.0)
}

/// VCO B's pre-fold shape: sine → spike → glitched triangle.
pub fn shape_morph(p: f32, shape: f32) -> f32 {
    let sine = (p * std::f32::consts::TAU).sin();
    // spike: a narrow bipolar pulse at the cycle's start
    let s = 1.0 - (2.0 * p - 1.0).abs();
    let spike = (s * s * s * s * s * s) * 2.0 - 1.0;
    // glitched triangle: tri plus an off-phase third-partial kink
    let glitch = 0.7 * tri(p) + 0.3 * tri((p * 3.0 + 0.21).fract());
    if shape < 0.5 {
        let t = shape * 2.0;
        sine * (1.0 - t) + spike * t
    } else {
        let t = shape * 2.0 - 1.0;
        spike * (1.0 - t) + glitch * t
    }
}

/// Angle: warps phase so harmonics tilt toward one end of the cycle.
pub fn angle_warp(p: f32, angle: f32) -> f32 {
    p.powf(1.0 + angle * 2.5)
}

impl Dpo {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            phase_a: 0.0,
            phase_b: 0.0,
            freq_followed: 110.0,
            strike_env: 0.0,
            sine_a: 0.0,
            sine_b: 0.0,
            a_out: 0.0,
            b_out: 0.0,
        }
    }

    /// Render one block into `out` (mono), gain handled by the caller's
    /// envelope; returns A's sine at block end (the LFO tap).
    pub fn process(&mut self, out: &mut [f32], p: &DpoParams) -> f32 {
        let sr = self.sample_rate;
        // follow: one-pole lag of the note toward B's pitch. follow 1 =
        // instant, 0.5 = audible portamento-of-ratio, 0 = frozen.
        let lag = p.follow.clamp(0.0, 1.0);
        let coeff = if lag >= 0.999 {
            1.0
        } else {
            1.0 - (-(lag * lag) * 64.0 / sr * out.len() as f32).exp()
        };
        self.freq_followed += (p.freq_b - self.freq_followed) * coeff.clamp(0.0, 1.0);

        let freq_a_base = if p.mode == AMode::Lfo {
            // LFO range: the ratio knob spans ~0.05–13 Hz
            0.05 + p.ratio * 1.6
        } else {
            (self.freq_followed * p.ratio).clamp(0.1, 12_000.0)
        };

        // strike vactrol: ~2 ms rise, ~90 ms fall
        let rise = 1.0 - (-1.0 / (0.002 * sr)).exp();
        let fall = 1.0 - (-1.0 / (0.09 * sr)).exp();

        for o in out.iter_mut() {
            // strike envelope
            let target = if p.strike { 1.0 } else { 0.0 };
            let c = if target > self.strike_env { rise } else { fall };
            self.strike_env += (target - self.strike_env) * c;

            // bidirectional FM (one-sample feedback, the hardware's
            // hardwired sine bus)
            let fm_to_a = self.sine_b * p.index * p.fm_a;
            let fm_to_b = self.sine_a * p.index * p.fm_b;
            let fa = (freq_a_base * (1.0 + fm_to_a * 3.0)).max(0.0);
            let fb = (p.freq_b * (1.0 + fm_to_b * 3.0)).max(0.0);

            self.phase_b += fb / sr;
            let wrapped_b = self.phase_b >= 1.0;
            if wrapped_b {
                self.phase_b -= 1.0;
            }
            self.phase_a += fa / sr;
            if self.phase_a >= 1.0 {
                self.phase_a -= 1.0;
            }
            match p.mode {
                AMode::Sync => {
                    if wrapped_b {
                        self.phase_a = self.phase_b;
                    }
                }
                AMode::Lock => {
                    // weak: reset only near integer ratios
                    if wrapped_b {
                        let r = if p.freq_b > 0.0 { fa / fb.max(0.001) } else { 1.0 };
                        if (r - r.round()).abs() < 0.05 {
                            self.phase_a = self.phase_b;
                        }
                    }
                }
                AMode::Free | AMode::Lfo => {}
            }

            self.sine_a = (self.phase_a * std::f32::consts::TAU).sin();
            self.sine_b = (self.phase_b * std::f32::consts::TAU).sin();

            // mod bus: A's sine into shape/angle/fold
            let m = self.sine_a * p.mod_index;
            let shape = (p.shape + m * 0.5).clamp(0.0, 1.0);
            let angle = (p.angle + m * 0.5).clamp(0.0, 1.0);
            let fold_amt = (p.fold + m * 0.5 + self.strike_env).clamp(0.0, 1.0);

            // VCO A FINAL: triangle core (sync-rich in sync mode)
            let a_final = tri(self.phase_a);
            // VCO B FINAL: shape → angle → fold
            let warped = angle_warp(self.phase_b, angle);
            let b_final = fold(shape_morph(warped, shape), fold_amt);

            self.a_out = a_final;
            self.b_out = b_final;
            *o = (b_final * (1.0 - p.mix) + a_final * p.mix) * p.level;
        }
        self.sine_a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(p: &DpoParams, blocks: usize) -> Vec<f32> {
        let mut d = Dpo::new(48_000.0);
        let mut out = Vec::new();
        for _ in 0..blocks {
            let mut b = vec![0.0; 64];
            d.process(&mut b, p);
            out.extend(b);
        }
        out
    }

    fn dominant_period(x: &[f32]) -> usize {
        // zero-crossing estimate over the back half (settled)
        let h = &x[x.len() / 2..];
        let mut crossings = 0;
        let mut first = None;
        let mut last = 0;
        for i in 1..h.len() {
            if h[i - 1] < 0.0 && h[i] >= 0.0 {
                crossings += 1;
                if first.is_none() {
                    first = Some(i);
                }
                last = i;
            }
        }
        if crossings < 2 {
            return 0;
        }
        (last - first.unwrap()) / (crossings - 1)
    }

    #[test]
    fn vco_b_tracks_its_frequency() {
        let p = DpoParams { freq_b: 480.0, mix: 0.0, shape: 0.0, ..Default::default() };
        let out = render(&p, 100);
        let period = dominant_period(&out);
        assert!(
            (95..=105).contains(&period),
            "480 Hz at 48 k = 100 samples/cycle, got {period}"
        );
    }

    #[test]
    fn fm_index_brightens_b() {
        let hf = |x: &[f32]| -> f32 {
            x.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f32>() / x.len() as f32
        };
        let clean = render(&DpoParams { freq_b: 220.0, ..Default::default() }, 60);
        let fm = render(
            &DpoParams { freq_b: 220.0, index: 0.9, fm_b: 1.0, ratio: 2.5, ..Default::default() },
            60,
        );
        assert!(
            hf(&fm) > hf(&clean) * 1.3,
            "FM adds sidebands: clean {} fm {}",
            hf(&clean),
            hf(&fm)
        );
    }

    #[test]
    fn fold_adds_harmonics_and_stays_bounded() {
        let hf = |x: &[f32]| -> f32 {
            x.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f32>() / x.len() as f32
        };
        let clean = render(&DpoParams { freq_b: 220.0, ..Default::default() }, 60);
        let folded = render(
            &DpoParams { freq_b: 220.0, fold: 0.9, ..Default::default() },
            60,
        );
        assert!(hf(&folded) > hf(&clean) * 1.5, "folding brightens");
        let peak = folded.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        assert!(peak <= 1.0 + 1e-3, "folder bounded, got {peak}");
        // pure functions too
        assert!((fold(0.3, 0.0) - 0.3).abs() < 0.01, "fold 0 ≈ clean");
        assert!(fold(0.9, 1.0).abs() <= 1.0);
    }

    #[test]
    fn hard_sync_locks_a_to_b_period() {
        // A at a non-integer ratio, hard sync on: A's output must repeat
        // at B's period (the spectral content differs, the period locks)
        let p = DpoParams {
            freq_b: 300.0,
            ratio: 2.37,
            mode: AMode::Sync,
            mix: 1.0,
            ..Default::default()
        };
        let out = render(&p, 100);
        let h = &out[out.len() / 2..];
        let period_b = (48_000.0_f32 / 300.0) as usize;
        // compare one B-period against the next: sync makes them match
        let a = &h[0..period_b * 2];
        let b = &h[period_b..period_b * 3];
        let diff: f32 =
            a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / (period_b * 2) as f32;
        assert!(diff < 0.05, "hard-synced A repeats at B's period, diff {diff}");
    }

    #[test]
    fn strike_opens_the_fold_then_relaxes() {
        let mut d = Dpo::new(48_000.0);
        let mut p = DpoParams { freq_b: 220.0, fold: 0.0, ..Default::default() };
        let hf = |x: &[f32]| -> f32 {
            x.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f32>() / x.len() as f32
        };
        let mut quiet = vec![0.0; 512];
        d.process(&mut quiet, &p);
        let before = hf(&quiet);
        p.strike = true;
        let mut struck = vec![0.0; 512];
        d.process(&mut struck, &p);
        let during = hf(&struck);
        p.strike = false;
        // ~200 ms after release the vactrol has mostly closed
        let mut after = vec![0.0; 9_600];
        d.process(&mut after, &p);
        let tail = hf(&after[8_000..]);
        assert!(during > before * 1.4, "strike brightens: {before} -> {during}");
        assert!(tail < during * 0.8, "vactrol relaxes: {during} -> {tail}");
    }

    #[test]
    fn lfo_mode_runs_slow() {
        let p = DpoParams { mode: AMode::Lfo, ratio: 1.0, mix: 1.0, ..Default::default() };
        let mut d = Dpo::new(48_000.0);
        let mut b = vec![0.0; 4_800]; // 100 ms
        let lfo = d.process(&mut b, &p);
        assert!(lfo.abs() <= 1.0);
        // a ~1.6 Hz triangle moves < a full cycle in 100 ms
        let crossings = b.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        assert!(crossings <= 1, "LFO is slow, got {crossings} crossings in 100 ms");
    }

    #[test]
    fn shape_morph_endpoints_behave() {
        // sine endpoint: smooth, max near ±1
        let sine_peak = (0..100)
            .map(|i| shape_morph(i as f32 / 100.0, 0.0).abs())
            .fold(0.0_f32, f32::max);
        assert!((0.95..=1.01).contains(&sine_peak));
        // all morph positions bounded
        for s in [0.0, 0.25, 0.5, 0.75, 1.0] {
            for i in 0..200 {
                let v = shape_morph(i as f32 / 200.0, s);
                assert!(v.abs() <= 1.3, "shape {s} pos {i} out of range: {v}");
            }
        }
    }
}
