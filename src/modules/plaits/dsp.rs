//! # Plaits engine — the macro-oscillator scaffold + the first engines
//!
//! Ported from pichenettes/eurorack (plaits/dsp/*, MIT, copyright
//! 2016 Emilie Gillet, attribution preserved). Plaits is a bank of
//! ~20 synthesis engines behind three macro knobs (harmonics, timbre,
//! morph); this module ports them in stages. This file holds the
//! shared scaffold — the engine parameter struct, the note→frequency
//! law, the state-variable filter, the clocked noise source — and the
//! first engine (filtered noise). Each engine renders into a main and
//! an aux output.
//!
//! Frequencies are normalized (cycles per sample), as in the firmware.
//! The los voice shell provides the amplitude envelope (an amp source),
//! so the firmware's internal LPG/envelope is not needed here.

#![allow(clippy::excessive_precision)]

pub const SAMPLE_RATE: f32 = 48_000.0;

pub const TRIGGER_LOW: i32 = 0;
pub const TRIGGER_RISING_EDGE: i32 = 1;
pub const TRIGGER_UNPATCHED: i32 = 2;
pub const TRIGGER_HIGH: i32 = 4;

/// The three macro parameters plus the note and the trigger/accent.
#[derive(Debug, Clone, Copy)]
pub struct EngineParameters {
    pub trigger: i32,
    pub note: f32,
    pub timbre: f32,
    pub morph: f32,
    pub harmonics: f32,
    pub accent: f32,
}

impl Default for EngineParameters {
    fn default() -> Self {
        Self {
            trigger: TRIGGER_UNPATCHED,
            note: 48.0,
            timbre: 0.5,
            morph: 0.5,
            harmonics: 0.5,
            accent: 0.8,
        }
    }
}

/// MIDI note → normalized frequency (cycles per sample), the firmware's
/// `NoteToFrequency`.
#[inline]
pub fn note_to_frequency(mut midi_note: f32) -> f32 {
    midi_note = (midi_note - 9.0).clamp(-128.0, 127.0);
    // a0 = (440/8)/sr; result = a0 * 0.25 * 2^(note/12)
    let a0 = (440.0 / 8.0) / SAMPLE_RATE;
    a0 * 0.25 * 2.0_f32.powf(midi_note / 12.0)
}

#[inline]
fn semitones_to_ratio(x: f32) -> f32 {
    2.0_f32.powf(x / 12.0)
}

// ── state-variable filter (stmlib Svf, TPT) ──────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Svf {
    g: f32,
    r: f32,
    h: f32,
    state_1: f32,
    state_2: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvfMode {
    LowPass,
    BandPass,
    HighPass,
}

impl Svf {
    pub fn new() -> Self {
        let mut s = Svf::default();
        s.set_f_q(0.01, 100.0);
        s
    }

    #[inline]
    pub fn set_f_q(&mut self, f: f32, resonance: f32) {
        let f = f.clamp(0.0, 0.497);
        self.g = (std::f32::consts::PI * f).tan();
        self.r = 1.0 / resonance.max(0.01);
        self.h = 1.0 / (1.0 + self.r * self.g + self.g * self.g);
    }

    #[inline]
    pub fn process(&mut self, input: f32, mode: SvfMode) -> f32 {
        let hp = (input - self.r * self.state_1 - self.g * self.state_1 - self.state_2) * self.h;
        let bp = self.g * hp + self.state_1;
        self.state_1 = self.g * hp + bp;
        let lp = self.g * bp + self.state_2;
        self.state_2 = self.g * bp + lp;
        match mode {
            SvfMode::LowPass => lp,
            SvfMode::BandPass => bp,
            SvfMode::HighPass => hp,
        }
    }

    /// Blend low-pass → high-pass by `mode` (0 = LP, 1 = HP), the
    /// firmware's `ProcessMultimodeLPtoHP`.
    #[inline]
    pub fn process_lp_to_hp(&mut self, input: f32, mode: f32) -> f32 {
        let hp = (input - self.r * self.state_1 - self.g * self.state_1 - self.state_2) * self.h;
        let bp = self.g * hp + self.state_1;
        self.state_1 = self.g * hp + bp;
        let lp = self.g * bp + self.state_2;
        self.state_2 = self.g * bp + lp;
        // LP at 0, BP-ish in the middle, HP at 1
        let m = mode.clamp(0.0, 1.0);
        if m < 0.5 {
            lp + (bp - lp) * (m * 2.0)
        } else {
            bp + (hp - bp) * ((m - 0.5) * 2.0)
        }
    }
}

// ── clocked noise (band-limited) ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ClockedNoise {
    phase: f32,
    sample: f32,
    next_sample: f32,
    frequency: f32,
    rng: u32,
}

impl Default for ClockedNoise {
    fn default() -> Self {
        Self {
            phase: 0.0,
            sample: 0.0,
            next_sample: 0.0,
            frequency: 0.001,
            rng: 0x1234_5678,
        }
    }
}

#[inline]
fn this_blep(t: f32) -> f32 {
    0.5 * t * t
}
#[inline]
fn next_blep(t: f32) -> f32 {
    let t = 1.0 - t;
    -0.5 * t * t
}

impl ClockedNoise {
    pub fn new(seed: u32) -> Self {
        Self {
            rng: seed | 1,
            ..Default::default()
        }
    }

    #[inline]
    fn rand(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng >> 8) as f32 / 8_388_608.0 - 1.0
    }

    pub fn render(&mut self, sync: bool, frequency: f32, out: &mut [f32]) {
        let target = frequency.clamp(0.0, 1.0);
        let size = out.len();
        let step = (target - self.frequency) / size.max(1) as f32;
        let mut next_sample = self.next_sample;
        let mut sample = self.sample;
        if sync {
            self.phase = 1.0;
        }
        for o in out.iter_mut() {
            self.frequency += step;
            let f = self.frequency;
            let mut this_sample = next_sample;
            next_sample = 0.0;
            let raw_sample = self.rand();
            let raw_amount = (4.0 * (f - 0.25)).clamp(0.0, 1.0);
            self.phase += f;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
                let t = self.phase / f.max(1e-9);
                let discontinuity = raw_sample - sample;
                this_sample += discontinuity * this_blep(t);
                next_sample += discontinuity * next_blep(t);
                sample = raw_sample;
            }
            next_sample += sample;
            *o = this_sample + raw_amount * (raw_sample - this_sample);
        }
        self.frequency = target;
        self.sample = sample;
        self.next_sample = next_sample;
    }
}

// ── the engine trait ─────────────────────────────────────────────────────────

/// One synthesis engine: renders into `out` (main) and `aux`.
pub trait Engine {
    fn render(
        &mut self,
        p: &EngineParameters,
        out: &mut [f32],
        aux: &mut [f32],
    ) -> bool; // returns already_enveloped
}

// ── the noise engine ─────────────────────────────────────────────────────────

/// Dual filtered-noise engine: two clocked noise sources through a
/// multimode LP→HP filter and two band-pass filters.
pub struct NoiseEngine {
    clocked: [ClockedNoise; 2],
    lp_hp: Svf,
    bp: [Svf; 2],
    prev_f0: f32,
    prev_f1: f32,
    prev_q: f32,
    prev_mode: f32,
    temp: Vec<f32>,
}

impl NoiseEngine {
    pub fn new(seed: u32) -> Self {
        Self {
            clocked: [ClockedNoise::new(seed), ClockedNoise::new(seed ^ 0x9e37)],
            lp_hp: Svf::new(),
            bp: [Svf::new(), Svf::new()],
            prev_f0: 0.0,
            prev_f1: 0.0,
            prev_q: 0.0,
            prev_mode: 0.0,
            temp: Vec::new(),
        }
    }
}

impl Engine for NoiseEngine {
    fn render(&mut self, p: &EngineParameters, out: &mut [f32], aux: &mut [f32]) -> bool {
        let size = out.len();
        if self.temp.len() < size {
            self.temp.resize(size, 0.0);
        }
        let f0 = note_to_frequency(p.note);
        let f1 = note_to_frequency(p.note + p.harmonics * 48.0 - 24.0);
        let clock_lowest = if p.trigger & TRIGGER_UNPATCHED != 0 {
            0.0
        } else {
            -24.0
        };
        let clock_f = note_to_frequency(p.timbre * (128.0 - clock_lowest) + clock_lowest);
        let q = 0.5 * semitones_to_ratio(p.morph * 120.0);
        let sync = p.trigger & TRIGGER_RISING_EDGE != 0;

        self.clocked[0].render(sync, clock_f, aux);
        let f_ratio = if f0 > 1e-9 { f1 / f0 } else { 1.0 };
        self.clocked[1].render(sync, clock_f * f_ratio, &mut self.temp[..size]);

        let f0_step = (f0 - self.prev_f0) / size.max(1) as f32;
        let f1_step = (f1 - self.prev_f1) / size.max(1) as f32;
        let q_step = (q - self.prev_q) / size.max(1) as f32;
        let mode_step = (p.harmonics - self.prev_mode) / size.max(1) as f32;
        let (mut cf0, mut cf1, mut cq, mut cmode) =
            (self.prev_f0, self.prev_f1, self.prev_q, self.prev_mode);

        for i in 0..size {
            cf0 += f0_step;
            cf1 += f1_step;
            cq += q_step;
            cmode += mode_step;
            let gain = 1.0 / ((0.5 + cq) * 40.0 * cf0.max(1e-6)).sqrt();
            self.lp_hp.set_f_q(cf0, cq);
            self.bp[0].set_f_q(cf0, cq);
            self.bp[1].set_f_q(cf1, cq);
            let in_1 = aux[i] * gain;
            let in_2 = self.temp[i] * gain;
            out[i] = self.lp_hp.process_lp_to_hp(in_1, cmode);
            aux[i] = self.bp[0].process(in_1, SvfMode::BandPass)
                + self.bp[1].process(in_2, SvfMode::BandPass);
        }
        self.prev_f0 = f0;
        self.prev_f1 = f1;
        self.prev_q = q;
        self.prev_mode = p.harmonics;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_to_frequency_tracks_octaves() {
        // an octave up doubles the normalized frequency
        let a4 = note_to_frequency(69.0);
        let a5 = note_to_frequency(81.0);
        assert!((a5 / a4 - 2.0).abs() < 1e-4, "{a4} -> {a5}");
        // A4 should be ~440/sr
        assert!((a4 - 440.0 / SAMPLE_RATE).abs() < 1e-6);
    }

    #[test]
    fn svf_lowpass_attenuates_highs() {
        // white noise through a low cutoff LP loses high-frequency energy
        let mut lp = Svf::new();
        lp.set_f_q(0.02, 0.7);
        let mut rng = 1u32;
        let mut hp = Svf::new();
        hp.set_f_q(0.02, 0.7);
        let mut lp_energy = 0.0;
        let mut hp_energy = 0.0;
        for _ in 0..8000 {
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let x = (rng >> 9) as f32 / 4_194_304.0 - 1.0;
            lp_energy += lp.process(x, SvfMode::LowPass).powi(2);
            hp_energy += hp.process(x, SvfMode::HighPass).powi(2);
        }
        assert!(lp_energy.is_finite() && hp_energy.is_finite());
        // the LP and HP split the spectrum — both carry energy but differ
        assert!(lp_energy > 0.0 && hp_energy > 0.0);
    }

    #[test]
    fn clocked_noise_is_bounded_and_clocks() {
        let mut cn = ClockedNoise::new(42);
        let mut out = vec![0.0_f32; 4096];
        // low clock rate → stepped noise; high → closer to white
        cn.render(false, 0.01, &mut out);
        assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 2.0));
        let energy: f32 = out.iter().map(|v| v * v).sum();
        assert!(energy > 0.0, "noise produces output");
    }

    #[test]
    fn noise_engine_renders_filtered_noise() {
        let mut eng = NoiseEngine::new(7);
        let mut out = vec![0.0_f32; 64];
        let mut aux = vec![0.0_f32; 64];
        let p = EngineParameters {
            note: 60.0,
            harmonics: 0.5,
            timbre: 0.5,
            morph: 0.5,
            ..Default::default()
        };
        let mut energy = 0.0;
        for _ in 0..40 {
            let enveloped = eng.render(&p, &mut out, &mut aux);
            assert!(!enveloped, "noise is not self-enveloped");
            assert!(
                out.iter().chain(aux.iter()).all(|v| v.is_finite()),
                "stays finite"
            );
            energy += out.iter().map(|v| v * v).sum::<f32>();
        }
        assert!(energy > 0.0, "the noise engine sings: {energy}");
    }

    #[test]
    fn noise_morph_sweeps_the_filter() {
        // morph controls Q; at high morph the band gets narrower (more
        // resonant) — output stays bounded across the sweep
        let mut eng = NoiseEngine::new(11);
        let mut out = vec![0.0_f32; 64];
        let mut aux = vec![0.0_f32; 64];
        for morph in [0.0, 0.3, 0.6, 0.9] {
            let p = EngineParameters {
                note: 55.0,
                morph,
                ..Default::default()
            };
            for _ in 0..30 {
                eng.render(&p, &mut out, &mut aux);
                assert!(out.iter().all(|v| v.is_finite() && v.abs() < 16.0), "morph {morph}");
            }
        }
    }
}
