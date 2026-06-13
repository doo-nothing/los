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

    /// `set_f_q` with a precomputed `g` (the tangent), for the FAST/DIRTY
    /// frequency approximations the string voice uses.
    #[inline]
    pub fn set_g_q(&mut self, g: f32, q: f32) {
        self.g = g;
        self.r = 1.0 / q.max(0.01);
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

    /// One pass returning band-pass and low-pass together (the firmware's
    /// two-mode `Process`).
    #[inline]
    pub fn process_bp_lp(&mut self, input: f32) -> (f32, f32) {
        let hp = (input - self.r * self.state_1 - self.g * self.state_1 - self.state_2) * self.h;
        let bp = self.g * hp + self.state_1;
        self.state_1 = self.g * hp + bp;
        let lp = self.g * bp + self.state_2;
        self.state_2 = self.g * bp + lp;
        (bp, lp)
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

// ── shared sine LUT + FM ratio quantizer ─────────────────────────────────────

use std::sync::OnceLock;

const SINE_BITS: u32 = 9; // 512-entry sine table

struct FmTables {
    sine: Vec<f32>,     // 641 (512 + quarter guard + 1)
    fm_ratio: Vec<f32>, // 256+2, semitone offsets
}

static FM_TABLES: OnceLock<FmTables> = OnceLock::new();

fn fm_tables() -> &'static FmTables {
    FM_TABLES.get_or_init(|| {
        let size = 512usize;
        let sine: Vec<f32> = (0..(size + size / 4 + 1))
            .map(|i| (2.0 * std::f32::consts::PI * i as f32 / size as f32).sin())
            .collect();
        let ratios: [f64; 24] = [
            0.5,
            0.5 * 2.0_f64.powf(16.0 / 1200.0),
            std::f64::consts::SQRT_2 / 2.0,
            std::f64::consts::PI / 4.0,
            1.0,
            2.0_f64.powf(16.0 / 1200.0),
            std::f64::consts::SQRT_2,
            std::f64::consts::PI / 2.0,
            7.0 / 4.0,
            2.0,
            2.0 * 2.0_f64.powf(16.0 / 1200.0),
            9.0 / 4.0,
            11.0 / 4.0,
            2.0 * std::f64::consts::SQRT_2,
            3.0,
            std::f64::consts::PI,
            3.0_f64.sqrt() * 2.0,
            4.0,
            std::f64::consts::SQRT_2 * 3.0,
            std::f64::consts::PI * 3.0 / 2.0,
            5.0,
            std::f64::consts::SQRT_2 * 4.0,
            8.0,
            8.0,
        ];
        let mut scale: Vec<f64> = Vec::new();
        for r in ratios.iter() {
            let s = 12.0 * r.log2();
            scale.push(s);
            scale.push(s);
            scale.push(s);
        }
        let target = 256usize;
        while scale.len() < target {
            let mut gap = 0usize;
            let mut best = f64::MIN;
            for i in 0..scale.len() - 1 {
                let d = scale[i + 1] - scale[i];
                if d > best {
                    best = d;
                    gap = i;
                }
            }
            let mid = (scale[gap] + scale[gap + 1]) / 2.0;
            scale.insert(gap + 1, mid);
        }
        scale.truncate(target);
        scale.push(*scale.last().unwrap());
        scale.push(*scale.last().unwrap());
        let fm_ratio: Vec<f32> = scale.iter().map(|&x| x as f32).collect();
        FmTables { sine, fm_ratio }
    })
}

/// Phase-modulated sine lookup (sine_oscillator.h SinePM).
#[inline]
fn sine_pm(mut phase: u32, pm: f32) -> f32 {
    let t = fm_tables();
    let max_u32 = 4_294_967_296.0_f32;
    let max_index = 32i64;
    let offset = max_index as f32;
    let scale = max_u32 / (max_index as f32 * 2.0);
    phase = phase
        .wrapping_add((((pm + offset) * scale) as i64 as u32).wrapping_mul(max_index as u32 * 2));
    let integral = (phase >> (32 - SINE_BITS)) as usize;
    let fractional = (phase << SINE_BITS) as f32 / max_u32;
    let a = t.sine[integral.min(t.sine.len() - 1)];
    let b = t.sine[(integral + 1).min(t.sine.len() - 1)];
    a + (b - a) * fractional
}

#[inline]
fn fm_quantize_ratio(harmonics: f32) -> f32 {
    let t = fm_tables();
    let p = (harmonics.clamp(0.0, 1.0) * 256.0).min(256.0);
    let i = p as usize;
    let frac = p - i as f32;
    let a = t.fm_ratio[i.min(t.fm_ratio.len() - 1)];
    let b = t.fm_ratio[(i + 1).min(t.fm_ratio.len() - 1)];
    a + (b - a) * frac
}

// ── the 2-operator FM engine ─────────────────────────────────────────────────

/// A 2-operator FM voice with feedback and a sub-oscillator on aux.
/// Runs at the session rate (the firmware oversamples 4× through a FIR;
/// the soft sine and the HF-taming bound the aliasing — documented).
pub struct FmEngine {
    carrier_phase: u32,
    modulator_phase: u32,
    sub_phase: u32,
    prev_carrier_f: f32,
    prev_modulator_f: f32,
    prev_amount: f32,
    prev_feedback: f32,
    prev_sample: f32,
}

impl FmEngine {
    pub fn new() -> Self {
        let a0 = (440.0 / 8.0) / SAMPLE_RATE;
        Self {
            carrier_phase: 0,
            modulator_phase: 0,
            sub_phase: 0,
            prev_carrier_f: a0,
            prev_modulator_f: a0,
            prev_amount: 0.0,
            prev_feedback: 0.0,
            prev_sample: 0.0,
        }
    }
}

impl Default for FmEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine for FmEngine {
    fn render(&mut self, p: &EngineParameters, out: &mut [f32], aux: &mut [f32]) -> bool {
        let size = out.len();
        let note = p.note - 24.0;
        let ratio = fm_quantize_ratio(p.harmonics);
        let modulator_note = note + ratio;
        let target_mod_f = note_to_frequency(modulator_note).clamp(0.0, 0.5);
        let mut hf_taming = (1.0 - (modulator_note - 72.0) * 0.025).clamp(0.0, 1.0);
        hf_taming *= hf_taming;

        let carrier_f_target = note_to_frequency(note);
        let amount_target = 2.0 * p.timbre * p.timbre * hf_taming;
        let feedback_target = 2.0 * p.morph - 1.0;

        let cf_step = (carrier_f_target - self.prev_carrier_f) / size.max(1) as f32;
        let mf_step = (target_mod_f - self.prev_modulator_f) / size.max(1) as f32;
        let am_step = (amount_target - self.prev_amount) / size.max(1) as f32;
        let fb_step = (feedback_target - self.prev_feedback) / size.max(1) as f32;
        let (mut cf, mut mf, mut am, mut fb) = (
            self.prev_carrier_f,
            self.prev_modulator_f,
            self.prev_amount,
            self.prev_feedback,
        );
        let max_u32 = 4_294_967_296.0_f32;

        for i in 0..size {
            cf += cf_step;
            mf += mf_step;
            am += am_step;
            fb += fb_step;
            let phase_feedback = if fb < 0.0 { 0.5 * fb * fb } else { 0.0 };
            let carrier_increment = (max_u32 * cf) as i64 as u32;
            self.modulator_phase = self.modulator_phase.wrapping_add(
                (max_u32 * mf * (1.0 + self.prev_sample * phase_feedback)) as i64 as u32,
            );
            self.carrier_phase = self.carrier_phase.wrapping_add(carrier_increment);
            self.sub_phase = self.sub_phase.wrapping_add(carrier_increment >> 1);
            let modulator_fb = if fb > 0.0 { 0.25 * fb * fb } else { 0.0 };
            let modulator = sine_pm(self.modulator_phase, modulator_fb * self.prev_sample);
            let carrier = sine_pm(self.carrier_phase, am * modulator);
            let sub = sine_pm(self.sub_phase, am * carrier * 0.25);
            self.prev_sample += (carrier - self.prev_sample) * 0.05;
            out[i] = carrier;
            aux[i] = sub;
        }
        self.prev_carrier_f = carrier_f_target;
        self.prev_modulator_f = target_mod_f;
        self.prev_amount = amount_target;
        self.prev_feedback = feedback_target;
        false
    }
}

// ── variable-shape oscillator (polyblep) + the virtual-analog engine ─────────

pub const MAX_FREQUENCY: f32 = 0.25;

#[inline]
fn pb_this(t: f32) -> f32 {
    0.5 * t * t
}
#[inline]
fn pb_next(t: f32) -> f32 {
    let t = 1.0 - t;
    -0.5 * t * t
}
#[inline]
fn pb_next_int(t: f32) -> f32 {
    let t1 = 0.5 * t;
    let t2 = t1 * t1;
    let t4 = t2 * t2;
    0.1875 - t1 + 1.5 * t2 - t4
}
#[inline]
fn pb_this_int(t: f32) -> f32 {
    pb_next_int(1.0 - t)
}

#[inline]
fn compute_naive_sample(
    phase: f32,
    pw: f32,
    slope_up: f32,
    slope_down: f32,
    triangle_amount: f32,
    square_amount: f32,
) -> f32 {
    let mut saw = phase;
    let square = if phase < pw { 0.0 } else { 1.0 };
    let triangle = if phase < pw {
        phase * slope_up
    } else {
        1.0 - (phase - pw) * slope_down
    };
    saw += (square - saw) * square_amount;
    saw += (triangle - saw) * triangle_amount;
    saw
}

/// plaits variable_shape_oscillator.h — a band-limited oscillator that
/// morphs saw → triangle → square by `waveshape`, with pulse width and
/// optional hard sync from a master phase.
#[derive(Debug, Clone)]
pub struct VariableShapeOscillator {
    master_phase: f32,
    slave_phase: f32,
    next_sample: f32,
    previous_pw: f32,
    high: bool,
    master_frequency: f32,
    slave_frequency: f32,
    pw: f32,
    waveshape: f32,
}

impl Default for VariableShapeOscillator {
    fn default() -> Self {
        Self {
            master_phase: 0.0,
            slave_phase: 0.0,
            next_sample: 0.0,
            previous_pw: 0.5,
            high: false,
            master_frequency: 0.0,
            slave_frequency: 0.01,
            pw: 0.5,
            waveshape: 0.0,
        }
    }
}

impl VariableShapeOscillator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_master_phase(&mut self, p: f32) {
        self.master_phase = p;
    }

    /// Render without sync.
    pub fn render(&mut self, frequency: f32, pw: f32, waveshape: f32, out: &mut [f32]) {
        self.render_inner(false, 0.0, frequency, pw, waveshape, out);
    }

    /// Render with hard sync to `master_frequency`.
    pub fn render_sync(
        &mut self,
        master_frequency: f32,
        frequency: f32,
        pw: f32,
        waveshape: f32,
        out: &mut [f32],
    ) {
        self.render_inner(true, master_frequency, frequency, pw, waveshape, out);
    }

    fn render_inner(
        &mut self,
        enable_sync: bool,
        master_frequency: f32,
        mut frequency: f32,
        mut pw: f32,
        waveshape: f32,
        out: &mut [f32],
    ) {
        let master_frequency = master_frequency.min(MAX_FREQUENCY);
        frequency = frequency.min(MAX_FREQUENCY);
        if frequency >= 0.25 {
            pw = 0.5;
        } else {
            pw = pw.clamp(frequency * 2.0, 1.0 - 2.0 * frequency);
        }
        let size = out.len();
        let mf_step = (master_frequency - self.master_frequency) / size.max(1) as f32;
        let sf_step = (frequency - self.slave_frequency) / size.max(1) as f32;
        let pw_step = (pw - self.pw) / size.max(1) as f32;
        let ws_step = (waveshape - self.waveshape) / size.max(1) as f32;
        let mut next_sample = self.next_sample;

        for o in out.iter_mut() {
            let mut reset = false;
            let mut transition_during_reset = false;
            let mut reset_time = 0.0;
            let mut this_sample = next_sample;
            next_sample = 0.0;

            self.master_frequency += mf_step;
            self.slave_frequency += sf_step;
            self.pw += pw_step;
            self.waveshape += ws_step;
            let mf = self.master_frequency;
            let sf = self.slave_frequency;
            let pw = self.pw;
            let ws = self.waveshape;

            let square_amount = (ws - 0.5).max(0.0) * 2.0;
            let triangle_amount = (1.0 - ws * 2.0).max(0.0);
            let slope_up = 1.0 / pw;
            let slope_down = 1.0 / (1.0 - pw);

            if enable_sync {
                self.master_phase += mf;
                if self.master_phase >= 1.0 {
                    self.master_phase -= 1.0;
                    reset_time = self.master_phase / mf.max(1e-9);
                    let mut slave_phase_at_reset = self.slave_phase + (1.0 - reset_time) * sf;
                    reset = true;
                    if slave_phase_at_reset >= 1.0 {
                        slave_phase_at_reset -= 1.0;
                        transition_during_reset = true;
                    }
                    if !self.high && slave_phase_at_reset >= pw {
                        transition_during_reset = true;
                    }
                    let value = compute_naive_sample(
                        slave_phase_at_reset,
                        pw,
                        slope_up,
                        slope_down,
                        triangle_amount,
                        square_amount,
                    );
                    this_sample -= value * pb_this(reset_time);
                    next_sample -= value * pb_next(reset_time);
                }
            }

            self.slave_phase += sf;
            loop {
                if !transition_during_reset && reset {
                    break;
                }
                if !self.high {
                    if self.slave_phase < pw {
                        break;
                    }
                    let t = (self.slave_phase - pw) / (self.previous_pw - pw + sf).max(1e-9);
                    let triangle_step = (slope_up + slope_down) * sf * triangle_amount;
                    this_sample += square_amount * pb_this(t);
                    next_sample += square_amount * pb_next(t);
                    this_sample -= triangle_step * pb_this_int(t);
                    next_sample -= triangle_step * pb_next_int(t);
                    self.high = true;
                }
                if self.high {
                    if self.slave_phase < 1.0 {
                        break;
                    }
                    self.slave_phase -= 1.0;
                    let t = self.slave_phase / sf.max(1e-9);
                    let triangle_step = (slope_up + slope_down) * sf * triangle_amount;
                    this_sample -= (1.0 - triangle_amount) * pb_this(t);
                    next_sample -= (1.0 - triangle_amount) * pb_next(t);
                    this_sample += triangle_step * pb_this_int(t);
                    next_sample += triangle_step * pb_next_int(t);
                    self.high = false;
                }
            }

            if enable_sync && reset {
                self.slave_phase = reset_time * sf;
                self.high = false;
            }

            next_sample += compute_naive_sample(
                self.slave_phase,
                pw,
                slope_up,
                slope_down,
                triangle_amount,
                square_amount,
            );
            self.previous_pw = pw;
            *o = 2.0 * this_sample - 1.0;
        }
        self.next_sample = next_sample;
        self.master_frequency = master_frequency;
        self.slave_frequency = frequency;
        self.pw = pw;
        self.waveshape = waveshape;
    }
}

/// The virtual-analog engine (VA_VARIANT 0): two variable-shape
/// oscillators (the second detuned by harmonics) summed on the main
/// output; the aux mixes the first with a hard-synced second.
pub struct VirtualAnalogEngine {
    primary: VariableShapeOscillator,
    auxiliary: VariableShapeOscillator,
    sync: VariableShapeOscillator,
    temp: Vec<f32>,
}

const VA_INTERVALS: [f32; 5] = [0.0, 7.01, 12.01, 19.01, 24.01];

#[inline]
fn squash(x: f32) -> f32 {
    x * x * (3.0 - 2.0 * x)
}

impl VirtualAnalogEngine {
    pub fn new() -> Self {
        let mut auxiliary = VariableShapeOscillator::new();
        auxiliary.set_master_phase(0.25);
        Self {
            primary: VariableShapeOscillator::new(),
            auxiliary,
            sync: VariableShapeOscillator::new(),
            temp: Vec::new(),
        }
    }

    fn compute_detuning(detune: f32) -> f32 {
        let mut detune = (2.05 * detune - 1.025).clamp(-1.0, 1.0);
        let sign = if detune < 0.0 { -1.0 } else { 1.0 };
        detune = detune * sign * 3.9999;
        let i = detune as usize;
        let frac = detune - i as f32;
        let a = VA_INTERVALS[i.min(4)];
        let b = VA_INTERVALS[(i + 1).min(4)];
        (a + (b - a) * squash(squash(frac))) * sign
    }
}

impl Default for VirtualAnalogEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine for VirtualAnalogEngine {
    fn render(&mut self, p: &EngineParameters, out: &mut [f32], aux: &mut [f32]) -> bool {
        let size = out.len();
        if self.temp.len() < size {
            self.temp.resize(size, 0.0);
        }
        let auxiliary_detune = Self::compute_detuning(p.harmonics);
        let primary_f = note_to_frequency(p.note);
        let auxiliary_f = note_to_frequency(p.note + auxiliary_detune);
        let sync_f = note_to_frequency(p.note + p.harmonics * 48.0);
        let shape_1 = (p.timbre * 1.5).clamp(0.0, 1.0);
        let pw_1 = (0.5 + (p.timbre - 0.66) * 1.4).clamp(0.5, 0.99);
        let shape_2 = (p.morph * 1.5).clamp(0.0, 1.0);
        let pw_2 = (0.5 + (p.morph - 0.66) * 1.4).clamp(0.5, 0.99);

        self.primary.render(primary_f, pw_1, shape_1, &mut self.temp[..size]);
        self.auxiliary.render(auxiliary_f, pw_2, shape_2, aux);
        for (o, (a, &tmp)) in out.iter_mut().zip(aux.iter().zip(self.temp.iter())).take(size) {
            *o = (a + tmp) * 0.5;
        }
        self.sync.render_sync(primary_f, sync_f, pw_2, shape_2, aux);
        for (a, &tmp) in aux.iter_mut().zip(self.temp.iter()).take(size) {
            *a = (*a + tmp) * 0.5;
        }
        false
    }
}

// ── the chord engine ─────────────────────────────────────────────────────────

const CHORD_NUM_NOTES: usize = 4;
const CHORD_NUM_VOICES: usize = CHORD_NUM_NOTES + 1; // 5
const CHORD_NUM_CHORDS: usize = 17;

/// chord_bank.cc — the 17 chord types as semitone intervals.
const CHORDS: [[f32; CHORD_NUM_NOTES]; CHORD_NUM_CHORDS] = [
    [0.00, 0.01, 11.99, 12.00], // Octave
    [0.00, 7.00, 7.01, 12.00],  // Fifth
    [0.00, 3.00, 7.00, 12.00],  // Minor
    [0.00, 3.00, 7.00, 10.00],  // Minor 7th
    [0.00, 3.00, 10.00, 14.00], // Minor 9th
    [0.00, 3.00, 10.00, 17.00], // Minor 11th
    [0.00, 4.00, 7.00, 12.00],  // Major
    [0.00, 4.00, 7.00, 11.00],  // Major 7th
    [0.00, 4.00, 11.00, 14.00], // Major 9th
    [0.00, 5.00, 7.00, 12.00],  // Sus4
    [0.00, 2.00, 9.00, 16.00],  // 69
    [0.00, 4.00, 7.00, 9.00],   // 6th
    [0.00, 7.00, 16.00, 23.00], // 10th (spread maj7)
    [0.00, 4.00, 7.00, 10.00],  // Dominant 7th
    [0.00, 7.00, 10.00, 13.00], // Dominant 7th b9
    [0.00, 3.00, 6.00, 10.00],  // Half diminished
    [0.00, 3.00, 6.00, 9.00],   // Fully diminished
];

/// A five-voice chord engine: the note's pitch fans out into one of 17
/// chord types (harmonics), inverted/voiced by timbre, the waveform
/// morphed saw→square by morph. Voices are variable-shape oscillators
/// (a documented simplification of the firmware's divide-down +
/// wavetable blend — the chords and inversions are exact).
pub struct ChordEngine {
    voices: Vec<VariableShapeOscillator>,
    morph_lp: f32,
    timbre_lp: f32,
}

impl ChordEngine {
    pub fn new() -> Self {
        Self {
            voices: (0..CHORD_NUM_VOICES).map(|_| VariableShapeOscillator::new()).collect(),
            morph_lp: 0.0,
            timbre_lp: 0.0,
        }
    }

    fn chord_ratio(chord: usize, note: usize) -> f32 {
        semitones_to_ratio(CHORDS[chord.min(CHORD_NUM_CHORDS - 1)][note])
    }

    /// chord_bank.cc ComputeChordInversion: distribute the four chord
    /// notes across five voices per the inversion knob; returns the
    /// aux-routing bitmask.
    fn compute_inversion(
        chord: usize,
        inversion: f32,
        ratios: &mut [f32; CHORD_NUM_VOICES],
        amplitudes: &mut [f32; CHORD_NUM_VOICES],
    ) -> u32 {
        let inv = inversion * (CHORD_NUM_NOTES * CHORD_NUM_VOICES) as f32;
        let inv_i = inv as i32;
        let inv_f = inv - inv_i as f32;
        let num_rotations = inv_i / CHORD_NUM_NOTES as i32;
        let rotated_note = (inv_i % CHORD_NUM_NOTES as i32) as usize;
        const BASE_GAIN: f32 = 0.25;
        let mut mask = 0u32;
        for i in 0..CHORD_NUM_NOTES {
            let transposition = 0.25
                * (1i32 << (((CHORD_NUM_NOTES as i32 - 1 + inv_i - i as i32)
                    / CHORD_NUM_NOTES as i32)
                    .clamp(0, 6))) as f32;
            let target = ((i as i32 - num_rotations + CHORD_NUM_VOICES as i32)
                % CHORD_NUM_VOICES as i32) as usize;
            let previous = (target + CHORD_NUM_VOICES - 1) % CHORD_NUM_VOICES;
            let base = Self::chord_ratio(chord, i);
            if i == rotated_note {
                ratios[target] = base * transposition;
                ratios[previous] = ratios[target] * 2.0;
                amplitudes[previous] = BASE_GAIN * inv_f;
                amplitudes[target] = BASE_GAIN * (1.0 - inv_f);
            } else if i < rotated_note {
                ratios[previous] = base * transposition;
                amplitudes[previous] = BASE_GAIN;
            } else {
                ratios[target] = base * transposition;
                amplitudes[target] = BASE_GAIN;
            }
            if i == 0 {
                if i >= rotated_note {
                    mask |= 1 << target;
                }
                if i <= rotated_note {
                    mask |= 1 << previous;
                }
            }
        }
        mask
    }
}

impl Default for ChordEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine for ChordEngine {
    fn render(&mut self, p: &EngineParameters, out: &mut [f32], aux: &mut [f32]) -> bool {
        let size = out.len();
        self.morph_lp += (p.morph - self.morph_lp) * 0.1;
        self.timbre_lp += (p.timbre - self.timbre_lp) * 0.1;
        let chord = (p.harmonics.clamp(0.0, 1.0) * (CHORD_NUM_CHORDS as f32 - 1.0)).round() as usize;

        let mut ratios = [1.0_f32; CHORD_NUM_VOICES];
        let mut amplitudes = [0.0_f32; CHORD_NUM_VOICES];
        let aux_mask = Self::compute_inversion(chord, self.timbre_lp, &mut ratios, &mut amplitudes);

        out[..size].fill(0.0);
        aux[..size].fill(0.0);

        let f0 = note_to_frequency(p.note) * 0.998;
        // morph past the midpoint sweeps saw → square
        let waveshape = ((self.morph_lp - 0.5) * 2.0).clamp(0.0, 1.0);
        let pw = 0.5;
        let mut scratch = vec![0.0_f32; size];
        for note in 0..CHORD_NUM_VOICES {
            let amp = amplitudes[note];
            if amp <= 0.0 {
                continue;
            }
            let note_f0 = (f0 * ratios[note]).min(MAX_FREQUENCY);
            self.voices[note].render(note_f0, pw, waveshape, &mut scratch);
            let dest = if (1 << note) & aux_mask != 0 { &mut *aux } else { &mut *out };
            for (d, &s) in dest.iter_mut().zip(scratch.iter()).take(size) {
                *d += s * amp;
            }
        }
        for i in 0..size {
            out[i] += aux[i];
            aux[i] *= 3.0;
        }
        false
    }
}

// ── the waveshaping engine ───────────────────────────────────────────────────

const WAVESHAPER_BIN: &[u8] = include_bytes!("waveshaper_tables.bin");

struct WaveshaperTables {
    ws: Vec<Vec<i16>>, // 5 × 257
    fold: Vec<f32>,    // 516
    fold_2: Vec<f32>,  // 516
}

static WS_TABLES: OnceLock<WaveshaperTables> = OnceLock::new();

fn ws_tables() -> &'static WaveshaperTables {
    WS_TABLES.get_or_init(|| {
        let mut off = 0usize;
        let ws: Vec<Vec<i16>> = (0..5)
            .map(|_| {
                let v: Vec<i16> = WAVESHAPER_BIN[off..off + 257 * 2]
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();
                off += 257 * 2;
                v
            })
            .collect();
        let fold: Vec<f32> = WAVESHAPER_BIN[off..off + 516 * 4]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        off += 516 * 4;
        let fold_2: Vec<f32> = WAVESHAPER_BIN[off..off + 516 * 4]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        WaveshaperTables { ws, fold, fold_2 }
    })
}

#[inline]
fn interpolate_hermite(table: &[f32], base: usize, t: f32, size: f32) -> f32 {
    let p = (t.clamp(0.0, 1.0) * size).min(size - 1.0);
    let i = p as usize;
    let frac = p - i as f32;
    let idx = |k: isize| {
        let j = (base as isize + i as isize + k).clamp(0, table.len() as isize - 1) as usize;
        table[j]
    };
    let xm1 = idx(-1);
    let x0 = idx(0);
    let x1 = idx(1);
    let x2 = idx(2);
    let c = (x1 - xm1) * 0.5;
    let v = x0 - x1;
    let w = c + v;
    let a = w + v + (x2 - x0) * 0.5;
    let b = w + a;
    ((((a * frac) - b) * frac + c) * frac) + x0
}

#[inline]
fn ws_sine(phase: f32) -> f32 {
    let t = fm_tables();
    let p = (phase - phase.floor()) * 512.0;
    let i = p as usize;
    let frac = p - i as f32;
    let a = t.sine[i.min(t.sine.len() - 1)];
    let b = t.sine[(i + 1).min(t.sine.len() - 1)];
    a + (b - a) * frac
}

#[inline]
fn tame(f0: f32, harmonics: f32, order: f32) -> f32 {
    let f0 = f0 * harmonics;
    let max_f = 0.5 / order;
    let amount = (1.0 - (f0 - max_f) / (0.5 - max_f)).clamp(0.0, 1.0);
    amount * amount * amount
}

/// A waveshaping + wavefolding engine: a slope oscillator through one
/// of five waveshaper transfer curves and a wavefolder. The slope
/// source uses the variable-shape oscillator (documented simplification
/// of the firmware's dedicated slope oscillator); the waveshaper and
/// folder tables are extracted byte-exact.
pub struct WaveshapingEngine {
    slope: VariableShapeOscillator,
    triangle: VariableShapeOscillator,
    prev_shape: f32,
    prev_wf_gain: f32,
    prev_overtone: f32,
    temp: Vec<f32>,
}

impl WaveshapingEngine {
    pub fn new() -> Self {
        Self {
            slope: VariableShapeOscillator::new(),
            triangle: VariableShapeOscillator::new(),
            prev_shape: 0.0,
            prev_wf_gain: 0.0,
            prev_overtone: 0.0,
            temp: Vec::new(),
        }
    }
}

impl Default for WaveshapingEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine for WaveshapingEngine {
    fn render(&mut self, p: &EngineParameters, out: &mut [f32], aux: &mut [f32]) -> bool {
        let t = ws_tables();
        let size = out.len();
        if self.temp.len() < size {
            self.temp.resize(size, 0.0);
        }
        let f0 = note_to_frequency(p.note);
        let pw = p.morph * 0.45 + 0.5;
        // slope source (variable-shape saw) and a triangle reference
        self.slope.render(f0, pw, 0.0, &mut self.temp[..size]);
        self.triangle.render(f0, 0.5, 0.5, aux);

        let slope = 3.0 + (p.morph - 0.5).abs() * 5.0;
        let shape_amount = (p.harmonics - 0.5).abs() * 2.0;
        let shape_atten = tame(f0, slope, 16.0);
        let wf_gain = p.timbre;
        let wf_gain_atten = tame(f0, slope * (3.0 + shape_amount * shape_atten * 5.0), 12.0);

        let shape_target = 0.5 + (p.harmonics - 0.5) * shape_atten;
        let wf_target = 0.03 + 0.46 * wf_gain * wf_gain_atten;
        let overtone = p.timbre * (2.0 - p.timbre);
        let overtone_target = overtone * (2.0 - overtone);

        let shape_step = (shape_target - self.prev_shape) / size.max(1) as f32;
        let wf_step = (wf_target - self.prev_wf_gain) / size.max(1) as f32;
        let ot_step = (overtone_target - self.prev_overtone) / size.max(1) as f32;
        let (mut shape_m, mut wf_m, mut ot_m) =
            (self.prev_shape, self.prev_wf_gain, self.prev_overtone);

        for i in 0..size {
            shape_m += shape_step;
            wf_m += wf_step;
            ot_m += ot_step;
            let shape = (shape_m * 3.9999).clamp(0.0, 3.9999);
            let shape_i = shape as usize;
            let shape_f = shape - shape_i as f32;
            let s1 = &t.ws[shape_i.min(4)];
            let s2 = &t.ws[(shape_i + 1).min(4)];
            let ws_index = 127.0 * self.temp[i] + 128.0;
            let wi = (ws_index as usize) & 255;
            let wf = ws_index - ws_index.floor();
            let x = (s1[wi] as f32 + (s1[(wi + 1).min(256)] as f32 - s1[wi] as f32) * wf) / 32768.0;
            let y = (s2[wi] as f32 + (s2[(wi + 1).min(256)] as f32 - s2[wi] as f32) * wf) / 32768.0;
            let mix = x + (y - x) * shape_f;
            let index = (mix * wf_m + 0.5).clamp(0.0, 1.0);
            let fold = interpolate_hermite(&t.fold, 1, index, 512.0);
            let fold_2 = -interpolate_hermite(&t.fold_2, 1, index, 512.0);
            let sine = ws_sine(aux[i] * 0.25 + 0.5);
            out[i] = fold;
            aux[i] = sine + (fold_2 - sine) * ot_m;
        }
        self.prev_shape = shape_target;
        self.prev_wf_gain = wf_target;
        self.prev_overtone = overtone_target;
        false
    }
}

// ── the additive (harmonic) engine ───────────────────────────────────────────

#[inline]
fn sine_no_wrap(phase: f32) -> f32 {
    let t = fm_tables();
    let p = (phase * 512.0).clamp(0.0, (t.sine.len() - 2) as f32);
    let i = p as usize;
    let frac = p - i as f32;
    t.sine[i] + (t.sine[i + 1] - t.sine[i]) * frac
}

const ADD_BATCH: usize = 12;

/// plaits harmonic_oscillator.h — a bank of `ADD_BATCH` sine partials
/// from `first_harmonic`, summed by a Chebyshev recurrence.
#[derive(Debug, Clone, Default)]
struct HarmonicOscillator {
    phase: f32,
    frequency: f32,
    amplitude: [f32; ADD_BATCH],
}

impl HarmonicOscillator {
    fn render(&mut self, first_harmonic: usize, frequency: f32, amps: &[f32], out: &mut [f32], add: bool) {
        let frequency = frequency.min(0.5);
        let size = out.len();
        let f_step = (frequency - self.frequency) / size.max(1) as f32;
        let mut targets = [0.0_f32; ADD_BATCH];
        let mut am_step = [0.0_f32; ADD_BATCH];
        for i in 0..ADD_BATCH {
            let f = (frequency * (first_harmonic + i) as f32).min(0.5);
            targets[i] = amps.get(i).copied().unwrap_or(0.0) * (1.0 - f * 2.0);
            am_step[i] = (targets[i] - self.amplitude[i]) / size.max(1) as f32;
        }
        for o in out.iter_mut() {
            self.frequency += f_step;
            self.phase += self.frequency;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            let two_x = 2.0 * sine_no_wrap(self.phase);
            let (mut previous, mut current) = if first_harmonic == 1 {
                (1.0, two_x * 0.5)
            } else {
                let k = first_harmonic as f32;
                (
                    ws_sine(self.phase * (k - 1.0) + 0.25),
                    ws_sine(self.phase * k),
                )
            };
            let mut sum = 0.0;
            for (amp, &step) in self.amplitude.iter_mut().zip(am_step.iter()) {
                *amp += step;
                sum += *amp * current;
                let temp = current;
                current = two_x * current - previous;
                previous = temp;
            }
            if add {
                *o += sum;
            } else {
                *o = sum;
            }
        }
        self.frequency = frequency;
        self.amplitude = targets;
    }
}

const INTEGER_HARMONICS: [usize; 24] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
];
const ORGAN_HARMONICS: [usize; 8] = [0, 1, 2, 3, 5, 7, 9, 11];

/// The additive engine: a spectrum shaped by a moving centroid (timbre),
/// a falloff slope (morph) and resonant bumps (harmonics), rendered by
/// banks of harmonic oscillators.
pub struct AdditiveEngine {
    amplitudes: [f32; 36],
    osc: [HarmonicOscillator; 3],
}

impl AdditiveEngine {
    pub fn new() -> Self {
        Self {
            amplitudes: [0.0; 36],
            osc: Default::default(),
        }
    }

    fn update_amplitudes(
        centroid: f32,
        slope: f32,
        bumps: f32,
        amplitudes: &mut [f32],
        harmonic_indices: &[usize],
    ) {
        let num = harmonic_indices.len();
        let n = num as f32 - 1.0;
        let margin = (1.0 / slope - 1.0) / (1.0 + bumps);
        let center = centroid * (n + margin) - 0.5 * margin;
        let mut sum = 0.001;
        for (i, &j) in harmonic_indices.iter().enumerate() {
            let order = (i as f32 - center).abs() * slope;
            let mut gain = 1.0 - order;
            gain += gain.abs();
            gain *= gain;
            let b = 0.25 + order * bumps;
            let bump_factor = 1.0 + ws_sine(b);
            gain *= bump_factor;
            gain *= gain;
            gain *= gain;
            amplitudes[j] += (gain - amplitudes[j]) * 0.001;
            sum += amplitudes[j];
        }
        let inv = 1.0 / sum;
        for &j in harmonic_indices.iter() {
            amplitudes[j] *= inv;
        }
    }
}

impl Default for AdditiveEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine for AdditiveEngine {
    fn render(&mut self, p: &EngineParameters, out: &mut [f32], aux: &mut [f32]) -> bool {
        let f0 = note_to_frequency(p.note);
        let centroid = p.timbre;
        let raw_bumps = p.harmonics;
        let raw_slope = (1.0 - 0.6 * raw_bumps) * p.morph;
        let slope = 0.01 + 1.99 * raw_slope * raw_slope * raw_slope;
        let bumps = 16.0 * raw_bumps * raw_bumps;

        Self::update_amplitudes(centroid, slope, bumps, &mut self.amplitudes, &INTEGER_HARMONICS);
        let (a0, rest) = self.amplitudes.split_at(12);
        let a0: Vec<f32> = a0.to_vec();
        let a1: Vec<f32> = rest[..12].to_vec();
        self.osc[0].render(1, f0, &a0, out, false);
        self.osc[1].render(13, f0, &a1, out, true);

        Self::update_amplitudes(
            centroid,
            slope,
            bumps,
            &mut self.amplitudes[24..],
            &ORGAN_HARMONICS,
        );
        let a2: Vec<f32> = self.amplitudes[24..36].to_vec();
        self.osc[2].render(1, f0, &a2, aux, false);
        false
    }
}

// ── the swarm engine ─────────────────────────────────────────────────────────

/// A small xorshift PRNG, standing in for stmlib's `Random` — the swarm
/// grains randomize their pitch and duration from it.
#[derive(Debug, Clone)]
struct Rng {
    state: u32,
}

impl Rng {
    fn new(seed: u32) -> Self {
        Self { state: seed | 1 }
    }
    #[inline]
    fn get_float(&mut self) -> f32 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 17;
        self.state ^= self.state << 5;
        (self.state >> 8) as f32 / 16_777_216.0
    }
}

/// The grain envelope shared by both halves of a swarm voice: a window
/// that ramps a frequency from grain to grain and shapes amplitude,
/// morphing between a "grain cloud" and a "swarm of glissandi" as the
/// size ratio crosses 1.
#[derive(Debug, Clone)]
struct GrainEnvelope {
    from: f32,
    interval: f32,
    phase: f32,
    fm: f32,
    amplitude: f32,
    previous_size_ratio: f32,
    filter_coefficient: f32,
}

impl GrainEnvelope {
    fn new() -> Self {
        Self {
            from: 0.0,
            interval: 1.0,
            phase: 1.0,
            fm: 0.0,
            amplitude: 0.5,
            previous_size_ratio: 0.0,
            filter_coefficient: 0.0,
        }
    }

    #[inline]
    fn step(&mut self, rate: f32, burst_mode: bool, start_burst: bool, rng: &mut Rng) {
        let mut randomize = false;
        if start_burst {
            self.phase = 0.5;
            self.fm = 16.0;
            randomize = true;
        } else {
            self.phase += rate * self.fm;
            if self.phase >= 1.0 {
                self.phase -= (self.phase as i32) as f32;
                randomize = true;
            }
        }
        if randomize {
            self.from += self.interval;
            self.interval = rng.get_float() - self.from;
            if burst_mode {
                self.fm *= 0.8 + 0.2 * rng.get_float();
            } else {
                self.fm = 0.5 + 1.5 * rng.get_float();
            }
        }
    }

    #[inline]
    fn frequency(&self, size_ratio: f32) -> f32 {
        if size_ratio < 1.0 {
            2.0 * (self.from + self.interval * self.phase) - 1.0
        } else {
            self.from
        }
    }

    #[inline]
    fn amplitude(&mut self, size_ratio: f32) -> f32 {
        let mut target_amplitude = 1.0;
        if size_ratio >= 1.0 {
            let phase = ((self.phase - 0.5) * size_ratio).clamp(-1.0, 1.0);
            let e = ws_sine(0.5 * phase + 1.25);
            target_amplitude = 0.5 * (e + 1.0);
        }
        if (size_ratio >= 1.0) ^ (self.previous_size_ratio >= 1.0) {
            self.filter_coefficient = 0.5;
        }
        self.filter_coefficient *= 0.95;
        self.previous_size_ratio = size_ratio;
        let coeff = 0.5 - self.filter_coefficient;
        self.amplitude += (target_amplitude - self.amplitude) * coeff;
        self.amplitude
    }
}

/// A band-limited (polyblep) saw that interpolates frequency and gain
/// across the block — the swarm voice's sawtooth half.
#[derive(Debug, Clone)]
struct AdditiveSawOscillator {
    phase: f32,
    next_sample: f32,
    frequency: f32,
    gain: f32,
}

impl AdditiveSawOscillator {
    fn new() -> Self {
        Self {
            phase: 0.0,
            next_sample: 0.0,
            frequency: 0.01,
            gain: 0.0,
        }
    }

    fn render(&mut self, frequency: f32, level: f32, out: &mut [f32]) {
        let frequency = frequency.min(0.25);
        let size = out.len().max(1) as f32;
        let f_step = (frequency - self.frequency) / size;
        let g_step = (level - self.gain) / size;
        let mut next_sample = self.next_sample;
        let mut phase = self.phase;
        for o in out.iter_mut() {
            self.frequency += f_step;
            self.gain += g_step;
            let mut this_sample = next_sample;
            next_sample = 0.0;
            let f = self.frequency;
            phase += f;
            if phase >= 1.0 {
                phase -= 1.0;
                let t = phase / f.max(1e-9);
                this_sample -= this_blep(t);
                next_sample -= next_blep(t);
            }
            next_sample += phase;
            *o += (2.0 * this_sample - 1.0) * self.gain;
        }
        self.frequency = frequency;
        self.gain = level;
        self.phase = phase;
        self.next_sample = next_sample;
    }
}

/// stmlib's `FastSineOscillator` — a quadrature recurrence (a magic-circle
/// oscillator) that gives a cheap sine with periodic renormalization.
#[derive(Debug, Clone)]
struct FastSineOscillator {
    x: f32,
    y: f32,
    epsilon: f32,
    amplitude: f32,
}

impl FastSineOscillator {
    fn new() -> Self {
        Self {
            x: 1.0,
            y: 0.0,
            epsilon: 0.0,
            amplitude: 0.0,
        }
    }

    #[inline]
    fn fast_2_sin(f: f32) -> f32 {
        let f_pi = f * std::f32::consts::PI;
        f_pi * (2.0 - (2.0 * 0.96 / 6.0) * f_pi * f_pi)
    }

    fn render_additive(&mut self, frequency: f32, amplitude: f32, out: &mut [f32]) {
        let (frequency, amplitude) = if frequency >= 0.25 {
            (0.25, 0.0)
        } else {
            (frequency, amplitude * (1.0 - frequency * 4.0))
        };
        let size = out.len().max(1) as f32;
        let target_epsilon = Self::fast_2_sin(frequency);
        let e_step = (target_epsilon - self.epsilon) / size;
        let a_step = (amplitude - self.amplitude) / size;
        let mut x = self.x;
        let mut y = self.y;
        let norm = x * x + y * y;
        if !(0.5..2.0).contains(&norm) && norm > 0.0 {
            let scale = 1.0 / norm.sqrt();
            x *= scale;
            y *= scale;
        }
        for o in out.iter_mut() {
            self.epsilon += e_step;
            self.amplitude += a_step;
            x += self.epsilon * y;
            y -= self.epsilon * x;
            *o += self.amplitude * x;
        }
        self.epsilon = target_epsilon;
        self.amplitude = amplitude;
        self.x = x;
        self.y = y;
    }
}

/// One swarm voice: a grain envelope driving a detuned saw + sine pair.
#[derive(Debug, Clone)]
struct SwarmVoice {
    rank: f32,
    envelope: GrainEnvelope,
    saw: AdditiveSawOscillator,
    sine: FastSineOscillator,
}

impl SwarmVoice {
    fn new(rank: f32) -> Self {
        Self {
            rank,
            envelope: GrainEnvelope::new(),
            saw: AdditiveSawOscillator::new(),
            sine: FastSineOscillator::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        mut f0: f32,
        density: f32,
        burst_mode: bool,
        start_burst: bool,
        spread: f32,
        size_ratio: f32,
        saw: &mut [f32],
        sine: &mut [f32],
        rng: &mut Rng,
    ) {
        self.envelope.step(density, burst_mode, start_burst, rng);
        let scale = 1.0 / NUM_SWARM_VOICES as f32;
        let amplitude = self.envelope.amplitude(size_ratio) * scale;
        let expo_amount = self.envelope.frequency(size_ratio);
        f0 *= semitones_to_ratio(48.0 * expo_amount * spread * self.rank);
        let linear_amount = self.rank * (self.rank + 0.01) * spread * 0.25;
        f0 *= 1.0 + linear_amount;
        self.saw.render(f0, amplitude, saw);
        self.sine.render_additive(f0, amplitude, sine);
    }
}

const NUM_SWARM_VOICES: usize = 8;

/// A swarm of sawtooths and sines — 8 grain-windowed voices spread around
/// the root, ramping from a grain cloud to a swarm of glissandi.
pub struct SwarmEngine {
    voices: Vec<SwarmVoice>,
    rng: Rng,
}

impl Default for SwarmEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SwarmEngine {
    pub fn new() -> Self {
        let n = (NUM_SWARM_VOICES - 1) as f32 / 2.0;
        let voices = (0..NUM_SWARM_VOICES)
            .map(|i| SwarmVoice::new((i as f32 - n) / n))
            .collect();
        Self {
            voices,
            rng: Rng::new(0x420_1337),
        }
    }
}

impl Engine for SwarmEngine {
    fn render(&mut self, p: &EngineParameters, out: &mut [f32], aux: &mut [f32]) -> bool {
        let f0 = note_to_frequency(p.note);
        let control_rate = out.len() as f32;
        let density = note_to_frequency(p.timbre * 120.0) * 0.025 * control_rate;
        let spread = p.harmonics * p.harmonics * p.harmonics;
        let mut size_ratio = 0.25 * semitones_to_ratio((1.0 - p.morph) * 84.0);
        let burst_mode = (p.trigger & TRIGGER_UNPATCHED) == 0;
        let start_burst = (p.trigger & TRIGGER_RISING_EDGE) != 0;

        out.iter_mut().for_each(|o| *o = 0.0);
        aux.iter_mut().for_each(|a| *a = 0.0);

        for voice in self.voices.iter_mut() {
            voice.render(
                f0,
                density,
                burst_mode,
                start_burst,
                spread,
                size_ratio,
                out,
                aux,
                &mut self.rng,
            );
            size_ratio *= 0.97;
        }
        false
    }
}

// ── the grain engine ─────────────────────────────────────────────────────────

/// stmlib's `ParameterInterpolator` — a linear ramp from the previous
/// block-end value to a target across the block, with a `subsample`
/// read used by the polyblep reset paths.
#[derive(Debug, Clone, Default)]
struct ParamInterp {
    value: f32,
    increment: f32,
}

impl ParamInterp {
    fn new(state: f32, new_value: f32, size: usize) -> Self {
        Self {
            value: state,
            increment: (new_value - state) / size.max(1) as f32,
        }
    }
    #[inline]
    fn next(&mut self) -> f32 {
        self.value += self.increment;
        self.value
    }
    #[inline]
    fn subsample(&self, t: f32) -> f32 {
        self.value + self.increment * t
    }
}

/// stmlib's `OnePole`, used here as a high-pass DC blocker.
#[derive(Debug, Clone, Default)]
struct OnePole {
    g: f32,
    gi: f32,
    state: f32,
}

impl OnePole {
    fn new() -> Self {
        let mut p = OnePole::default();
        p.set_f(0.01);
        p
    }
    #[inline]
    fn set_f(&mut self, f: f32) {
        let f = f.clamp(0.0, 0.497);
        self.g = (std::f32::consts::PI * f).tan();
        self.gi = 1.0 / (1.0 + self.g);
    }
    #[inline]
    fn process_high_pass(&mut self, input: f32) -> f32 {
        let lp = (self.g * input + self.state) * self.gi;
        self.state = self.g * (input - lp) + lp;
        input - lp
    }
}

/// A grainlet oscillator: a phase-distorted carrier sine windowed by a
/// formant sine, with a polyblep correction at each carrier reset.
#[derive(Debug, Clone, Default)]
struct GrainletOscillator {
    carrier_phase: f32,
    formant_phase: f32,
    next_sample: f32,
    carrier_frequency: f32,
    formant_frequency: f32,
    carrier_shape: f32,
    carrier_bleed: f32,
}

impl GrainletOscillator {
    fn new() -> Self {
        Self::default()
    }

    #[inline]
    fn carrier(phase: f32, shape: f32) -> f32 {
        let shape = shape * 3.0;
        let shape_integral = shape.floor();
        let shape_fractional = shape - shape_integral;
        let shape_integral = shape_integral as i32;
        let t = 1.0 - shape_fractional;
        let mut phase = phase;
        if shape_integral == 0 {
            phase *= 1.0 + t * t * t * 15.0;
            if phase >= 1.0 {
                phase = 1.0;
            }
            phase += 0.75;
        } else if shape_integral == 1 {
            let breakpoint = 0.001 + 0.499 * t * t * t;
            if phase < breakpoint {
                phase *= 0.5 / breakpoint;
            } else {
                phase = 0.5 + (phase - breakpoint) * 0.5 / (1.0 - breakpoint);
            }
            phase += 0.75;
        } else {
            let t = 1.0 - t;
            phase = 0.25 + phase * (0.5 + t * t * t * 14.5);
            if phase >= 0.75 {
                phase = 0.75;
            }
        }
        (ws_sine(phase) + 1.0) * 0.25
    }

    #[inline]
    fn grainlet(carrier_phase: f32, formant_phase: f32, shape: f32, bleed: f32) -> f32 {
        let carrier = Self::carrier(carrier_phase, shape);
        let formant = ws_sine(formant_phase);
        carrier * (formant + bleed) / (1.0 + bleed)
    }

    fn render(
        &mut self,
        carrier_frequency: f32,
        formant_frequency: f32,
        carrier_shape: f32,
        carrier_bleed: f32,
        out: &mut [f32],
    ) {
        let carrier_frequency = carrier_frequency.min(0.25 * 0.5);
        let formant_frequency = formant_frequency.min(0.25);
        let size = out.len();
        let mut cfm = ParamInterp::new(self.carrier_frequency, carrier_frequency, size);
        let mut ffm = ParamInterp::new(self.formant_frequency, formant_frequency, size);
        let mut csm = ParamInterp::new(self.carrier_shape, carrier_shape, size);
        let mut cbm = ParamInterp::new(self.carrier_bleed, carrier_bleed, size);
        let mut next_sample = self.next_sample;
        for o in out.iter_mut() {
            let mut this_sample = next_sample;
            next_sample = 0.0;
            let f0 = cfm.next();
            let f1 = ffm.next();
            self.carrier_phase += f0;
            let reset = self.carrier_phase >= 1.0;
            if reset {
                self.carrier_phase -= 1.0;
                let reset_time = self.carrier_phase / f0;
                let before = Self::grainlet(
                    1.0,
                    self.formant_phase + (1.0 - reset_time) * f1,
                    csm.subsample(1.0 - reset_time),
                    cbm.subsample(1.0 - reset_time),
                );
                let after = Self::grainlet(0.0, 0.0, csm.subsample(1.0), cbm.subsample(1.0));
                let discontinuity = after - before;
                this_sample += discontinuity * this_blep(reset_time);
                next_sample += discontinuity * next_blep(reset_time);
                self.formant_phase = reset_time * f1;
            } else {
                self.formant_phase += f1;
                if self.formant_phase >= 1.0 {
                    self.formant_phase -= 1.0;
                }
            }
            next_sample +=
                Self::grainlet(self.carrier_phase, self.formant_phase, csm.next(), cbm.next());
            *o = this_sample;
        }
        self.next_sample = next_sample;
        self.carrier_frequency = carrier_frequency;
        self.formant_frequency = formant_frequency;
        self.carrier_shape = carrier_shape;
        self.carrier_bleed = carrier_bleed;
    }
}

/// A "Z" oscillator: a ramp-down-windowed pair of sines with a mode knob
/// sweeping the formant offset, polyblep-corrected at its discontinuity.
#[derive(Debug, Clone, Default)]
struct ZOscillator {
    carrier_phase: f32,
    discontinuity_phase: f32,
    formant_phase: f32,
    next_sample: f32,
    carrier_frequency: f32,
    formant_frequency: f32,
    carrier_shape: f32,
    mode: f32,
}

impl ZOscillator {
    fn new() -> Self {
        Self::default()
    }

    #[inline]
    fn z(c: f32, d: f32, f: f32, shape: f32, mode: f32) -> f32 {
        let mut ramp_down = 0.5 * (1.0 + ws_sine(0.5 * d + 0.25));
        let offset;
        let phase_shift;
        if mode < 0.333 {
            offset = 1.0;
            phase_shift = 0.25 + mode * 1.50;
        } else if mode < 0.666 {
            phase_shift = 0.7495 - (mode - 0.33) * 0.75;
            offset = -ws_sine(phase_shift);
        } else {
            phase_shift = 0.7495 - (mode - 0.33) * 0.75;
            offset = 0.001;
        }
        let discontinuity = ws_sine(f + phase_shift);
        let contour = if shape < 0.5 {
            let shape = shape * 2.0;
            if c >= 0.5 {
                ramp_down *= shape;
            }
            1.0 + (ws_sine(c + 0.25) - 1.0) * shape
        } else {
            ws_sine(c + shape * 0.5)
        };
        (ramp_down * (offset + discontinuity) - offset) * contour
    }

    fn render(
        &mut self,
        carrier_frequency: f32,
        formant_frequency: f32,
        carrier_shape: f32,
        mode: f32,
        out: &mut [f32],
    ) {
        let carrier_frequency = carrier_frequency.min(0.25 * 0.5);
        let formant_frequency = formant_frequency.min(0.25);
        let size = out.len();
        let mut cfm = ParamInterp::new(self.carrier_frequency, carrier_frequency, size);
        let mut ffm = ParamInterp::new(self.formant_frequency, formant_frequency, size);
        let mut csm = ParamInterp::new(self.carrier_shape, carrier_shape, size);
        let mut mm = ParamInterp::new(self.mode, mode, size);
        let mut next_sample = self.next_sample;
        for o in out.iter_mut() {
            let mut this_sample = next_sample;
            next_sample = 0.0;
            let f0 = cfm.next();
            let f1 = ffm.next();
            self.discontinuity_phase += 2.0 * f0;
            self.carrier_phase += f0;
            let reset = self.discontinuity_phase >= 1.0;
            if reset {
                self.discontinuity_phase -= 1.0;
                let reset_time = self.discontinuity_phase / (2.0 * f0);
                let carrier_phase_before = if self.carrier_phase >= 1.0 { 1.0 } else { 0.5 };
                let carrier_phase_after = if self.carrier_phase >= 1.0 { 0.0 } else { 0.5 };
                let before = Self::z(
                    carrier_phase_before,
                    1.0,
                    self.formant_phase + (1.0 - reset_time) * f1,
                    csm.subsample(1.0 - reset_time),
                    mm.subsample(1.0 - reset_time),
                );
                let after = Self::z(carrier_phase_after, 0.0, 0.0, csm.subsample(1.0), mm.subsample(1.0));
                let discontinuity = after - before;
                this_sample += discontinuity * this_blep(reset_time);
                next_sample += discontinuity * next_blep(reset_time);
                self.formant_phase = reset_time * f1;
                if self.carrier_phase > 1.0 {
                    self.carrier_phase = self.discontinuity_phase * 0.5;
                }
            } else {
                self.formant_phase += f1;
                if self.formant_phase >= 1.0 {
                    self.formant_phase -= 1.0;
                }
            }
            if self.carrier_phase >= 1.0 {
                self.carrier_phase -= 1.0;
            }
            next_sample += Self::z(
                self.carrier_phase,
                self.discontinuity_phase,
                self.formant_phase,
                csm.next(),
                mm.next(),
            );
            *o = this_sample;
        }
        self.next_sample = next_sample;
        self.carrier_frequency = carrier_frequency;
        self.formant_frequency = formant_frequency;
        self.carrier_shape = carrier_shape;
        self.mode = mode;
    }
}

/// The grain engine: windowed sine segments. Two grainlet oscillators
/// summed (and DC-blocked) into the main output, a Z oscillator into aux.
pub struct GrainEngine {
    grainlet: [GrainletOscillator; 2],
    z_oscillator: ZOscillator,
    dc_blocker: [OnePole; 2],
    aux_scratch: Vec<f32>,
}

impl Default for GrainEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl GrainEngine {
    pub fn new() -> Self {
        Self {
            grainlet: [GrainletOscillator::new(), GrainletOscillator::new()],
            z_oscillator: ZOscillator::new(),
            dc_blocker: [OnePole::new(), OnePole::new()],
            aux_scratch: Vec::new(),
        }
    }
}

impl Engine for GrainEngine {
    fn render(&mut self, p: &EngineParameters, out: &mut [f32], aux: &mut [f32]) -> bool {
        let root = p.note;
        let f0 = note_to_frequency(root);
        let f1 = note_to_frequency(24.0 + 84.0 * p.timbre);
        let ratio = semitones_to_ratio(-24.0 + 48.0 * p.harmonics);
        let carrier_bleed = if p.harmonics < 0.5 {
            1.0 - 2.0 * p.harmonics
        } else {
            0.0
        };
        let carrier_bleed_fixed = carrier_bleed * (2.0 - carrier_bleed);
        let carrier_shape = 0.33 + (p.morph - 0.33) * (1.0 - f0 * 24.0).max(0.0);

        self.grainlet[0].render(f0, f1, carrier_shape, carrier_bleed_fixed, out);
        self.aux_scratch.resize(aux.len(), 0.0);
        self.grainlet[1].render(f0, f1 * ratio, carrier_shape, carrier_bleed_fixed, &mut self.aux_scratch);
        self.dc_blocker[0].set_f(0.3 * f0);
        for (o, &a) in out.iter_mut().zip(self.aux_scratch.iter()) {
            *o = self.dc_blocker[0].process_high_pass(*o + a);
        }

        let cutoff = note_to_frequency(root + 96.0 * p.timbre);
        self.z_oscillator.render(f0, cutoff, p.morph, p.harmonics, aux);
        self.dc_blocker[1].set_f(0.3 * f0);
        for a in aux.iter_mut() {
            *a = self.dc_blocker[1].process_high_pass(*a);
        }
        false
    }
}

// ── the wavetable engine ─────────────────────────────────────────────────────

const WAVETABLE_BIN: &[u8] = include_bytes!("wavetable_waves.bin");
const WT_NUM_WAVES: usize = 192;
const WT_TABLE_SIZE: usize = 128;
const WT_STRIDE: usize = WT_TABLE_SIZE + 4; // 4 guard samples per integrated wave

static WT_WAVES: OnceLock<Vec<i16>> = OnceLock::new();

fn wt_waves() -> &'static Vec<i16> {
    WT_WAVES.get_or_init(|| {
        WAVETABLE_BIN
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect()
    })
}

/// The wave map: 4 banks × 64 waves, each an index into the integrated
/// wave table. Banks 0–2 are identity; bank 3 is the firmware's shuffled
/// `w * 101 % 192` map. (No user data — the factory layout.)
fn wt_wave_map() -> &'static Vec<usize> {
    static MAP: OnceLock<Vec<usize>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut map = vec![0usize; 4 * 64];
        for (i, slot) in map.iter_mut().enumerate() {
            let bank = i / 64;
            *slot = if bank == 3 {
                (i * 101) % WT_NUM_WAVES
            } else {
                i
            };
        }
        map
    })
}

/// stmlib's `Differentiator` — a one-pole high-pass that differentiates
/// the integrated wavetable back to the audio waveform.
#[derive(Debug, Clone, Default)]
struct Differentiator {
    lp: f32,
    previous: f32,
}

impl Differentiator {
    fn new() -> Self {
        Self::default()
    }
    #[inline]
    fn process(&mut self, coefficient: f32, s: f32) -> f32 {
        self.lp += (s - self.previous - self.lp) * coefficient;
        self.previous = s;
        self.lp
    }
}

#[inline]
fn interpolate_wave_hermite(table: &[i16], base: usize, index_integral: usize, frac: f32) -> f32 {
    let at = |k: usize| table[(base + index_integral + k).min(table.len() - 1)] as f32;
    let xm1 = at(0);
    let x0 = at(1);
    let x1 = at(2);
    let x2 = at(3);
    let c = (x1 - xm1) * 0.5;
    let v = x0 - x1;
    let w = c + v;
    let a = w + v + (x2 - x0) * 0.5;
    let b_neg = w + a;
    (((a * frac) - b_neg) * frac + c) * frac + x0
}

#[inline]
fn wt_clamp(x: f32, amount: f32) -> f32 {
    let mut x = (x - 0.5) * amount;
    x = x.clamp(-0.5, 0.5);
    x + 0.5
}

/// The wavetable engine: an 8×8×4 wave terrain. Three smoothed coordinates
/// (timbre→X, morph→Y, harmonics→Z) index a trilinear blend of integrated
/// wavetables, differentiated back to audio. Aux is a 5-bit-crushed copy.
pub struct WavetableEngine {
    phase: f32,
    x_pre_lp: f32,
    y_pre_lp: f32,
    z_pre_lp: f32,
    x_lp: f32,
    y_lp: f32,
    z_lp: f32,
    previous_x: f32,
    previous_y: f32,
    previous_z: f32,
    previous_f0: f32,
    diff_out: Differentiator,
}

impl Default for WavetableEngine {
    fn default() -> Self {
        Self::new()
    }
}

const WT_A0: f32 = (440.0 / 8.0) / SAMPLE_RATE;

impl WavetableEngine {
    pub fn new() -> Self {
        Self {
            phase: 0.0,
            x_pre_lp: 0.0,
            y_pre_lp: 0.0,
            z_pre_lp: 0.0,
            x_lp: 0.0,
            y_lp: 0.0,
            z_lp: 0.0,
            previous_x: 0.0,
            previous_y: 0.0,
            previous_z: 0.0,
            previous_f0: WT_A0,
            diff_out: Differentiator::new(),
        }
    }

    #[inline]
    fn read_wave(waves: &[i16], map: &[usize], x: i32, y: i32, z: i32, pi: usize, pf: f32) -> f32 {
        let slot = (x + y * 8 + z * 64) as usize;
        let base = map[slot.min(map.len() - 1)] * WT_STRIDE;
        interpolate_wave_hermite(waves, base, pi, pf)
    }
}

impl Engine for WavetableEngine {
    fn render(&mut self, p: &EngineParameters, out: &mut [f32], aux: &mut [f32]) -> bool {
        let waves = wt_waves();
        let map = wt_wave_map();
        let f0 = note_to_frequency(p.note);
        let table_size_f = WT_TABLE_SIZE as f32;

        self.x_pre_lp += (p.timbre * 6.9999 - self.x_pre_lp) * 0.2;
        self.y_pre_lp += (p.morph * 6.9999 - self.y_pre_lp) * 0.2;
        self.z_pre_lp += (p.harmonics * 6.9999 - self.z_pre_lp) * 0.05;

        let z = self.z_pre_lp;
        let quantization = (z - 3.0).clamp(0.0, 1.0);
        let lp_coefficient = (2.0 * f0 * (4.0 - 3.0 * quantization)).clamp(0.01, 0.1);

        let blend = |pre: f32| {
            let integral = pre.floor();
            let mut frac = pre - integral;
            frac += quantization * (wt_clamp(frac, 16.0) - frac);
            integral + frac
        };
        let x_target = blend(self.x_pre_lp);
        let y_target = blend(self.y_pre_lp);
        let z_target = blend(self.z_pre_lp);

        let size = out.len();
        let mut x_mod = ParamInterp::new(self.previous_x, x_target, size);
        let mut y_mod = ParamInterp::new(self.previous_y, y_target, size);
        let mut z_mod = ParamInterp::new(self.previous_z, z_target, size);
        let mut f0_mod = ParamInterp::new(self.previous_f0, f0, size);

        for (o, a) in out.iter_mut().zip(aux.iter_mut()) {
            let f0 = f0_mod.next();
            let gain = (1.0 / (f0 * 131072.0)) * (0.95 - f0);
            let cutoff = (table_size_f * f0).min(1.0);

            self.x_lp += (x_mod.next() - self.x_lp) * lp_coefficient;
            self.y_lp += (y_mod.next() - self.y_lp) * lp_coefficient;
            self.z_lp += (z_mod.next() - self.z_lp) * lp_coefficient;

            let xi = self.x_lp.floor();
            let xf = self.x_lp - xi;
            let yi = self.y_lp.floor();
            let yf = self.y_lp - yi;
            let zi = self.z_lp.floor();
            let zf = self.z_lp - zi;

            self.phase += f0;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            let pp = self.phase * table_size_f;
            let pi = pp as usize;
            let pf = pp - pi as f32;

            let x0 = (xi as i32).clamp(0, 7);
            let x1 = (xi as i32 + 1).clamp(0, 7);
            let y0 = (yi as i32).clamp(0, 7);
            let y1 = (yi as i32 + 1).clamp(0, 7);
            let mut z0 = zi as i32;
            let mut z1 = zi as i32 + 1;
            if z0 >= 4 {
                z0 = 7 - z0;
            }
            if z1 >= 4 {
                z1 = 7 - z1;
            }
            let z0 = z0.clamp(0, 3);
            let z1 = z1.clamp(0, 3);

            let rd = |x: i32, y: i32, z: i32| Self::read_wave(waves, map, x, y, z, pi, pf);
            let x0y0z0 = rd(x0, y0, z0);
            let x1y0z0 = rd(x1, y0, z0);
            let xy0z0 = x0y0z0 + (x1y0z0 - x0y0z0) * xf;
            let x0y1z0 = rd(x0, y1, z0);
            let x1y1z0 = rd(x1, y1, z0);
            let xy1z0 = x0y1z0 + (x1y1z0 - x0y1z0) * xf;
            let xyz0 = xy0z0 + (xy1z0 - xy0z0) * yf;

            let x0y0z1 = rd(x0, y0, z1);
            let x1y0z1 = rd(x1, y0, z1);
            let xy0z1 = x0y0z1 + (x1y0z1 - x0y0z1) * xf;
            let x0y1z1 = rd(x0, y1, z1);
            let x1y1z1 = rd(x1, y1, z1);
            let xy1z1 = x0y1z1 + (x1y1z1 - x0y1z1) * xf;
            let xyz1 = xy0z1 + (xy1z1 - xy0z1) * yf;

            let mix = xyz0 + (xyz1 - xyz0) * zf;
            let mix = self.diff_out.process(cutoff, mix) * gain;
            *o = mix;
            *a = ((mix * 32.0) as i32) as f32 / 32.0;
        }
        self.previous_x = x_target;
        self.previous_y = y_target;
        self.previous_z = z_target;
        self.previous_f0 = f0;
        false
    }
}

// ── the modal engine ─────────────────────────────────────────────────────────

const MODAL_MAX_MODES: usize = 24;
const MODE_BATCH_SIZE: usize = 4;

/// plaits' `lut_stiffness`, extracted byte-exact from resources.cc — the
/// inharmonicity-to-stretch curve the resonator reads with `structure`.
#[rustfmt::skip]
const LUT_STIFFNESS: [f32; 65] = [
    -6.250000000e-02, -5.859375000e-02, -5.468750000e-02, -5.078125000e-02, -4.687500000e-02, -4.296875000e-02,
    -3.906250000e-02, -3.515625000e-02, -3.125000000e-02, -2.734375000e-02, -2.343750000e-02, -1.953125000e-02,
    -1.562500000e-02, -1.171875000e-02, -7.812500000e-03, -3.906250000e-03, 0.000000000e+00, 0.000000000e+00,
    0.000000000e+00, 0.000000000e+00, 1.009582073e-03, 2.416076364e-03, 4.002252878e-03, 5.791066350e-03,
    7.808404022e-03, 1.008346028e-02, 1.264915914e-02, 1.554263074e-02, 1.880574864e-02, 2.248573583e-02,
    2.663584813e-02, 3.131614488e-02, 3.659435812e-02, 4.254687278e-02, 4.925983210e-02, 5.683038428e-02,
    6.536808837e-02, 7.499649981e-02, 8.585495846e-02, 9.810060511e-02, 1.119106556e-01, 1.274849653e-01,
    1.450489216e-01, 1.648567056e-01, 1.871949702e-01, 2.123869891e-01, 2.407973346e-01, 2.728371538e-01,
    3.089701187e-01, 3.497191360e-01, 3.956739150e-01, 4.474995013e-01, 5.059459012e-01, 5.718589358e-01,
    6.461924814e-01, 7.300222738e-01, 8.245614757e-01, 9.311782340e-01, 1.000037649e+00, 1.005639154e+00,
    1.048005353e+00, 1.183990632e+00, 1.457101344e+00, 2.000000000e+00, 2.000000000e+00,
];

/// Linear interpolation into a LUT, stmlib `Interpolate` semantics:
/// `index = x * scale`, integral/fractional split.
#[inline]
fn interp_lut(table: &[f32], x: f32, scale: f32) -> f32 {
    let index = (x * scale).max(0.0);
    let i = (index as usize).min(table.len().saturating_sub(2));
    let f = index - i as f32;
    table[i] + (table[i + 1] - table[i]) * f
}

/// stmlib's `OnePole::tan<FREQUENCY_FAST>` — the 16 Hz–16 kHz optimized
/// tangent approximation the resonator uses for its mode frequencies.
#[inline]
fn tan_fast(f: f32) -> f32 {
    use std::f32::consts::PI;
    let a = 3.260e-01 * PI * PI * PI;
    let b = 1.823e-01 * PI * PI * PI * PI * PI;
    let f2 = f * f;
    f * (PI + f2 * (a + b * f2))
}

/// stmlib's `CosineOscillator` (approximate init) — a folded parabola
/// standing in for 2·cos(2πf), used to voice the mode-amplitude comb.
#[derive(Debug, Clone, Default)]
struct CosineOscillator {
    y1: f32,
    y0: f32,
    iir: f32,
    initial: f32,
}

impl CosineOscillator {
    fn init_approximate(&mut self, frequency: f32) {
        let mut sign = 16.0;
        let mut f = frequency - 0.25;
        if f < 0.0 {
            f = -f;
        } else if f > 0.5 {
            f -= 0.5;
        } else {
            sign = -16.0;
        }
        self.iir = sign * f * (1.0 - 2.0 * f);
        self.initial = self.iir * 0.25;
        self.y1 = self.initial;
        self.y0 = 0.5;
    }
    #[inline]
    fn next(&mut self) -> f32 {
        let temp = self.y0;
        self.y0 = self.iir * self.y0 - self.y1;
        self.y1 = temp;
        temp + 0.5
    }
}

/// A batched bank of `N` band-pass SVFs (stmlib `ResonatorSvf`), summed
/// (optionally added) into the output.
#[derive(Debug, Clone)]
struct ResonatorSvf<const N: usize> {
    state_1: [f32; N],
    state_2: [f32; N],
}

impl<const N: usize> ResonatorSvf<N> {
    fn new() -> Self {
        Self {
            state_1: [0.0; N],
            state_2: [0.0; N],
        }
    }

    /// `band_pass` selects BP (true) vs LP (false); `add` accumulates.
    #[allow(clippy::too_many_arguments)]
    fn process(
        &mut self,
        f: &[f32; N],
        q: &[f32; N],
        gain: &[f32; N],
        band_pass: bool,
        add: bool,
        input: &[f32],
        out: &mut [f32],
    ) {
        let mut g = [0.0f32; N];
        let mut r_plus_g = [0.0f32; N];
        let mut h = [0.0f32; N];
        for i in 0..N {
            g[i] = tan_fast(f[i]);
            let r = 1.0 / q[i];
            h[i] = 1.0 / (1.0 + r * g[i] + g[i] * g[i]);
            r_plus_g[i] = r + g[i];
        }
        let mut s1 = self.state_1;
        let mut s2 = self.state_2;
        for (n, &s_in) in input.iter().enumerate() {
            let mut s_out = 0.0;
            for i in 0..N {
                let hp = (s_in - r_plus_g[i] * s1[i] - s2[i]) * h[i];
                let bp = g[i] * hp + s1[i];
                s1[i] = g[i] * hp + bp;
                let lp = g[i] * bp + s2[i];
                s2[i] = g[i] * bp + lp;
                s_out += gain[i] * if band_pass { bp } else { lp };
            }
            if add {
                out[n] += s_out;
            } else {
                out[n] = s_out;
            }
        }
        self.state_1 = s1;
        self.state_2 = s2;
    }
}

#[inline]
fn nth_harmonic_compensation(n: i32, mut stiffness: f32) -> f32 {
    let mut stretch_factor = 1.0;
    for _ in 0..(n - 1) {
        stretch_factor += stiffness;
        if stiffness < 0.0 {
            stiffness *= 0.93;
        } else {
            stiffness *= 0.98;
        }
    }
    1.0 / stretch_factor
}

/// plaits' modal `Resonator`: up to 24 band-pass modes (in batches of 4),
/// their frequencies stretched by an inharmonicity curve and their Q
/// shaped by damping and brightness.
struct Resonator {
    resolution: usize,
    mode_amplitude: [f32; MODAL_MAX_MODES],
    mode_filters: [ResonatorSvf<MODE_BATCH_SIZE>; MODAL_MAX_MODES / MODE_BATCH_SIZE],
}

impl Resonator {
    fn new(position: f32, resolution: usize) -> Self {
        let mut amplitudes = CosineOscillator::default();
        amplitudes.init_approximate(position);
        let mut mode_amplitude = [0.0; MODAL_MAX_MODES];
        for a in mode_amplitude.iter_mut() {
            *a = amplitudes.next() * 0.25;
        }
        Self {
            resolution: resolution.min(MODAL_MAX_MODES),
            mode_amplitude,
            mode_filters: std::array::from_fn(|_| ResonatorSvf::new()),
        }
    }

    fn process(
        &mut self,
        f0: f32,
        structure: f32,
        brightness: f32,
        damping: f32,
        input: &[f32],
        out: &mut [f32],
    ) {
        let mut stiffness = interp_lut(&LUT_STIFFNESS, structure, 64.0);
        let f0 = f0 * nth_harmonic_compensation(3, stiffness);

        let mut harmonic = f0;
        let mut stretch_factor = 1.0;
        let q_sqrt = semitones_to_ratio(damping * 79.7);
        let mut q = 500.0 * q_sqrt * q_sqrt;
        let brightness = brightness * (1.0 - structure * 0.3) * (1.0 - damping * 0.3);
        let q_loss = brightness * (2.0 - brightness) * 0.85 + 0.15;

        let mut mode_q = [0.0f32; MODE_BATCH_SIZE];
        let mut mode_f = [0.0f32; MODE_BATCH_SIZE];
        let mut mode_a = [0.0f32; MODE_BATCH_SIZE];
        let mut batch_counter = 0;
        let mut batch_index = 0;

        for i in 0..self.resolution {
            let mode_frequency = (harmonic * stretch_factor).min(0.499);
            let mode_attenuation = 1.0 - mode_frequency * 2.0;
            mode_f[batch_counter] = mode_frequency;
            mode_q[batch_counter] = 1.0 + mode_frequency * q;
            mode_a[batch_counter] = self.mode_amplitude[i] * mode_attenuation;
            batch_counter += 1;
            if batch_counter == MODE_BATCH_SIZE {
                batch_counter = 0;
                self.mode_filters[batch_index]
                    .process(&mode_f, &mode_q, &mode_a, true, true, input, out);
                batch_index += 1;
            }
            stretch_factor += stiffness;
            if stiffness < 0.0 {
                stiffness *= 0.93;
            } else {
                stiffness *= 0.98;
            }
            harmonic += f0;
            q *= q_loss;
        }
    }
}

/// A 1-mode excitation filter (the modal voice's input shaper).
type ExcitationFilter = ResonatorSvf<1>;

#[inline]
fn dust(frequency: f32, rng: &mut Rng) -> f32 {
    let inv = 1.0 / frequency;
    let u = rng.get_float();
    if u < frequency {
        u * inv
    } else {
        0.0
    }
}

/// The modal voice: an excitation (struck impulse or sustained dust)
/// through a low-pass into the modal resonator.
struct ModalVoice {
    excitation_filter: ExcitationFilter,
    resonator: Resonator,
    rng: Rng,
}

impl ModalVoice {
    fn new() -> Self {
        Self {
            excitation_filter: ExcitationFilter::new(),
            resonator: Resonator::new(0.015, MODAL_MAX_MODES),
            rng: Rng::new(0x2_9a3f),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        sustain: bool,
        trigger: bool,
        accent: f32,
        f0: f32,
        structure: f32,
        mut brightness: f32,
        mut damping: f32,
        temp: &mut [f32],
        out: &mut [f32],
        aux: &mut [f32],
    ) {
        let density = brightness * brightness;
        brightness += 0.25 * accent * (1.0 - brightness);
        damping += 0.25 * accent * (1.0 - damping);

        let range = if sustain { 36.0 } else { 60.0 };
        let f = if sustain { 4.0 * f0 } else { 2.0 * f0 };
        let cutoff =
            (f * semitones_to_ratio((brightness * (2.0 - brightness) - 0.5) * range)).min(0.499);
        let q = if sustain { 0.7 } else { 1.5 };

        if sustain {
            let dust_f = 0.00005 + 0.99995 * density * density;
            for t in temp.iter_mut() {
                *t = dust(dust_f, &mut self.rng) * (4.0 - dust_f * 3.0) * accent;
            }
        } else {
            temp.iter_mut().for_each(|t| *t = 0.0);
            if trigger {
                let attenuation = 1.0 - damping * 0.5;
                let amplitude = (0.12 + 0.08 * accent) * attenuation;
                temp[0] = amplitude * semitones_to_ratio(cutoff * cutoff * 24.0) / cutoff;
            }
        }
        let cutoff_arr = [cutoff];
        let q_arr = [q];
        let one = [1.0];
        let temp_in = temp.to_vec();
        self.excitation_filter
            .process(&cutoff_arr, &q_arr, &one, false, false, &temp_in, temp);
        for (a, &t) in aux.iter_mut().zip(temp.iter()) {
            *a += t;
        }
        self.resonator
            .process(f0, structure, brightness, damping, temp, out);
    }
}

/// The modal engine: a single modal voice driven by the macro knobs —
/// harmonics→mode-amplitude density, timbre→brightness, morph→damping.
pub struct ModalEngine {
    voice: ModalVoice,
    harmonics_lp: f32,
    temp: Vec<f32>,
}

impl Default for ModalEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ModalEngine {
    pub fn new() -> Self {
        Self {
            voice: ModalVoice::new(),
            harmonics_lp: 0.0,
            temp: Vec::new(),
        }
    }
}

impl Engine for ModalEngine {
    fn render(&mut self, p: &EngineParameters, out: &mut [f32], aux: &mut [f32]) -> bool {
        out.iter_mut().for_each(|o| *o = 0.0);
        aux.iter_mut().for_each(|a| *a = 0.0);
        self.harmonics_lp += (p.harmonics - self.harmonics_lp) * 0.01;
        self.temp.resize(out.len(), 0.0);
        let sustain = (p.trigger & TRIGGER_UNPATCHED) != 0;
        let trigger = (p.trigger & TRIGGER_RISING_EDGE) != 0;
        let mut temp = std::mem::take(&mut self.temp);
        self.voice.render(
            sustain,
            trigger,
            p.accent,
            note_to_frequency(p.note),
            self.harmonics_lp,
            p.timbre,
            p.morph,
            &mut temp,
            out,
            aux,
        );
        self.temp = temp;
        false
    }
}

// ── the string engine ────────────────────────────────────────────────────────

/// stmlib's `OnePole::tan<FREQUENCY_DIRTY>` — a cheaper tangent (good
/// below 4 kHz), used to voice the string's excitation filter.
#[inline]
fn tan_dirty(f: f32) -> f32 {
    use std::f32::consts::PI;
    let a = 3.736e-01 * PI * PI * PI;
    f * (PI + a * f * f)
}

/// `lut_svf_shift`: group-delay compensation for the damping filter,
/// `2·atan(2^(−i/12))/(2π)` evaluated continuously (the table is read
/// with scale 1.0, so this is the exact per-sample value).
#[inline]
fn svf_shift(index: f32) -> f32 {
    let i = index.clamp(0.0, 256.0);
    let ratio = (i / 12.0).exp2();
    2.0 * (1.0 / ratio).atan() / (2.0 * std::f32::consts::PI)
}

#[inline]
fn crossfade(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// stmlib's `DCBlocker` — a one-pole high-pass that removes DC drift
/// from the recirculating string.
#[derive(Debug, Clone)]
struct DcBlocker {
    pole: f32,
    x: f32,
    y: f32,
}

impl DcBlocker {
    fn new(pole: f32) -> Self {
        Self { pole, x: 0.0, y: 0.0 }
    }
    #[inline]
    fn process(&mut self, s: f32) -> f32 {
        let y = self.pole * self.y + s - self.x;
        self.x = s;
        self.y = y;
        y
    }
}

/// A delay line that does not own its buffer (stmlib `DelayLine`), with
/// linear, hermite, and allpass reads. `SIZE` is the ring length.
#[derive(Debug, Clone)]
struct DelayLine {
    line: Vec<f32>,
    write_ptr: usize,
    size: usize,
}

impl DelayLine {
    fn new(size: usize) -> Self {
        Self {
            line: vec![0.0; size],
            write_ptr: 0,
            size,
        }
    }

    fn reset(&mut self) {
        self.line.iter_mut().for_each(|s| *s = 0.0);
        self.write_ptr = 0;
    }

    #[inline]
    fn write(&mut self, sample: f32) {
        self.line[self.write_ptr] = sample;
        self.write_ptr = (self.write_ptr + self.size - 1) % self.size;
    }

    #[inline]
    fn allpass(&mut self, sample: f32, delay: usize, coefficient: f32) -> f32 {
        let read = self.line[(self.write_ptr + delay) % self.size];
        let write = sample + coefficient * read;
        self.write(write);
        -write * coefficient + read
    }

    #[inline]
    fn read(&self, delay: f32) -> f32 {
        let di = delay as usize;
        let df = delay - di as f32;
        let a = self.line[(self.write_ptr + di) % self.size];
        let b = self.line[(self.write_ptr + di + 1) % self.size];
        a + (b - a) * df
    }

    #[inline]
    fn read_hermite(&self, delay: f32) -> f32 {
        let di = delay as usize;
        let df = delay - di as f32;
        let t = self.write_ptr + di + self.size;
        let xm1 = self.line[(t - 1) % self.size];
        let x0 = self.line[t % self.size];
        let x1 = self.line[(t + 1) % self.size];
        let x2 = self.line[(t + 2) % self.size];
        let c = (x1 - xm1) * 0.5;
        let v = x0 - x1;
        let w = c + v;
        let a = w + v + (x2 - x0) * 0.5;
        let b_neg = w + a;
        (((a * df) - b_neg) * df + c) * df + x0
    }
}

const STRING_DELAY_LINE_SIZE: usize = 1024;

/// plaits' `String` — a comb-filter Karplus-Strong waveguide (the "lite"
/// version of the Rings string), with two non-linearity modes: a curved
/// bridge (negative `structure`) and string dispersion (positive).
struct PluckedString {
    string: DelayLine,
    stretch: DelayLine,
    iir_damping_filter: Svf,
    dc_blocker: DcBlocker,
    delay: f32,
    dispersion_noise: f32,
    curved_bridge: f32,
    src_phase: f32,
    out_sample: [f32; 2],
    rng: Rng,
}

impl PluckedString {
    fn new() -> Self {
        let mut s = Self {
            string: DelayLine::new(STRING_DELAY_LINE_SIZE),
            stretch: DelayLine::new(STRING_DELAY_LINE_SIZE / 4),
            iir_damping_filter: Svf::new(),
            dc_blocker: DcBlocker::new(1.0 - 20.0 / SAMPLE_RATE),
            delay: 100.0,
            dispersion_noise: 0.0,
            curved_bridge: 0.0,
            src_phase: 0.0,
            out_sample: [0.0; 2],
            rng: Rng::new(0x5_1d57),
        };
        s.reset();
        s
    }

    fn reset(&mut self) {
        self.string.reset();
        self.stretch.reset();
        self.dispersion_noise = 0.0;
        self.curved_bridge = 0.0;
        self.out_sample = [0.0; 2];
        self.src_phase = 0.0;
    }

    fn process(
        &mut self,
        f0: f32,
        non_linearity_amount: f32,
        brightness: f32,
        damping: f32,
        input: &[f32],
        out: &mut [f32],
    ) {
        // dispersion (true) vs curved bridge (false)
        let dispersion = non_linearity_amount > 0.0;
        let nl = non_linearity_amount.abs();

        let mut delay = (1.0 / f0).clamp(4.0, STRING_DELAY_LINE_SIZE as f32 - 4.0);
        let mut src_ratio = delay * f0;
        if src_ratio >= 0.9999 {
            self.src_phase = 1.0;
            src_ratio = 1.0;
        }

        let mut damping_cutoff = (12.0 + damping * damping * 60.0 + brightness * 24.0).min(84.0);
        let mut brightness = brightness;
        let mut damping_f = (f0 * semitones_to_ratio(damping_cutoff)).min(0.499);
        if damping >= 0.95 {
            let to_infinite = 20.0 * (damping - 0.95);
            brightness += to_infinite * (1.0 - brightness);
            damping_f += to_infinite * (0.4999 - damping_f);
            damping_cutoff += to_infinite * (128.0 - damping_cutoff);
        }
        self.iir_damping_filter.set_g_q(tan_fast(damping_f), 0.5);
        let damping_compensation = svf_shift(damping_cutoff);

        let delay_target = delay * damping_compensation;
        let size = input.len();
        let mut delay_mod = ParamInterp::new(self.delay, delay_target, size);

        let stretch_point = nl * (2.0 - nl) * 0.225;
        let stretch_correction = ((160.0 / SAMPLE_RATE) * delay).clamp(1.0, 2.1);
        let noise_amount_sqrt = if nl > 0.75 { 4.0 * (nl - 0.75) } else { 0.0 };
        let noise_amount = noise_amount_sqrt * noise_amount_sqrt * 0.1;
        let noise_filter = 0.06 + 0.94 * brightness * brightness;
        let bridge_curving = nl * nl * 0.01;
        let ap_gain = -0.618 * non_linearity_amount / (0.15 + non_linearity_amount.abs());

        for (o, &s_in) in out.iter_mut().zip(input.iter()) {
            self.src_phase += src_ratio;
            if self.src_phase > 1.0 {
                self.src_phase -= 1.0;
                delay = delay_mod.next();
                let mut s;
                if dispersion {
                    let noise = self.rng.get_float() - 0.5;
                    self.dispersion_noise += (noise - self.dispersion_noise) * noise_filter;
                    delay *= 1.0 + self.dispersion_noise * noise_amount;
                } else {
                    delay *= 1.0 - self.curved_bridge * bridge_curving;
                }

                if dispersion {
                    let ap_delay = delay * stretch_point;
                    let main_delay =
                        delay - ap_delay * (0.408 - stretch_point * 0.308) * stretch_correction;
                    if ap_delay >= 4.0 && main_delay >= 4.0 {
                        s = self.string.read(main_delay);
                        s = self.stretch.allpass(s, ap_delay as usize, ap_gain);
                    } else {
                        s = self.string.read_hermite(delay);
                    }
                } else {
                    s = self.string.read_hermite(delay);
                    let value = s.abs() - 0.025;
                    let sign = if s > 0.0 { 1.0 } else { -1.5 };
                    self.curved_bridge = (value.abs() + value) * sign;
                }

                s += s_in;
                s = s.clamp(-20.0, 20.0);
                s = self.dc_blocker.process(s);
                s = self.iir_damping_filter.process(s, SvfMode::LowPass);
                self.string.write(s);
                self.out_sample[1] = self.out_sample[0];
                self.out_sample[0] = s;
            }
            *o += crossfade(self.out_sample[1], self.out_sample[0], self.src_phase);
        }
        self.delay = delay_target;
    }
}

/// An extended Karplus-Strong voice: a band-limited noise/dust burst
/// through an excitation low-pass, into the plucked string.
struct StringVoice {
    excitation_filter: Svf,
    string: PluckedString,
    remaining_noise_samples: usize,
    rng: Rng,
}

impl StringVoice {
    fn new(seed: u32) -> Self {
        Self {
            excitation_filter: Svf::new(),
            string: PluckedString::new(),
            remaining_noise_samples: 0,
            rng: Rng::new(seed),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        sustain: bool,
        trigger: bool,
        accent: f32,
        f0: f32,
        structure: f32,
        mut brightness: f32,
        mut damping: f32,
        temp: &mut [f32],
        out: &mut [f32],
        aux: &mut [f32],
    ) {
        let density = brightness * brightness;
        brightness += 0.25 * accent * (1.0 - brightness);
        damping += 0.25 * accent * (1.0 - damping);

        if trigger || sustain {
            let range = 72.0;
            let f = 4.0 * f0;
            let cutoff =
                (f * semitones_to_ratio((brightness * (2.0 - brightness) - 0.5) * range)).min(0.499);
            let q = if sustain { 1.0 } else { 0.5 };
            self.remaining_noise_samples = (1.0 / f0) as usize;
            self.excitation_filter.set_g_q(tan_dirty(cutoff), q);
        }

        let size = temp.len();
        if sustain {
            let dust_f = 0.00005 + 0.99995 * density * density;
            for t in temp.iter_mut() {
                *t = dust(dust_f, &mut self.rng) * (8.0 - dust_f * 6.0) * accent;
            }
        } else if self.remaining_noise_samples > 0 {
            let noise_samples = self.remaining_noise_samples.min(size);
            self.remaining_noise_samples -= noise_samples;
            for (i, t) in temp.iter_mut().enumerate() {
                *t = if i < noise_samples {
                    2.0 * self.rng.get_float() - 1.0
                } else {
                    0.0
                };
            }
        } else {
            temp.iter_mut().for_each(|t| *t = 0.0);
        }

        for t in temp.iter_mut() {
            *t = self.excitation_filter.process(*t, SvfMode::LowPass);
        }
        for (a, &t) in aux.iter_mut().zip(temp.iter()) {
            *a += t;
        }

        let non_linearity = if structure < 0.24 {
            (structure - 0.24) * 4.166
        } else if structure > 0.26 {
            (structure - 0.26) * 1.35135
        } else {
            0.0
        };
        self.string
            .process(f0, non_linearity, brightness, damping, temp, out);
    }
}

const NUM_STRINGS: usize = 3;

/// The string engine: three Karplus-Strong voices round-robined on each
/// trigger so notes ring into one another. harmonics → non-linearity
/// (curved bridge ↔ dispersion), timbre² → brightness, morph → damping.
pub struct StringEngine {
    voices: Vec<StringVoice>,
    f0: [f32; NUM_STRINGS],
    f0_delay: DelayLine,
    active_string: usize,
    temp: Vec<f32>,
}

impl Default for StringEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl StringEngine {
    pub fn new() -> Self {
        Self {
            voices: (0..NUM_STRINGS)
                .map(|i| StringVoice::new(0x7_3a11 ^ (i as u32 * 0x9e37)))
                .collect(),
            f0: [0.01; NUM_STRINGS],
            f0_delay: DelayLine::new(16),
            active_string: NUM_STRINGS - 1,
            temp: Vec::new(),
        }
    }
}

impl Engine for StringEngine {
    fn render(&mut self, p: &EngineParameters, out: &mut [f32], aux: &mut [f32]) -> bool {
        let rising = (p.trigger & TRIGGER_RISING_EDGE) != 0;
        let unpatched = (p.trigger & TRIGGER_UNPATCHED) != 0;
        if rising {
            self.f0[self.active_string] = self.f0_delay.read(14.0);
            self.active_string = (self.active_string + 1) % NUM_STRINGS;
        }
        let f0 = note_to_frequency(p.note);
        self.f0[self.active_string] = f0;
        self.f0_delay.write(f0);

        out.iter_mut().for_each(|o| *o = 0.0);
        aux.iter_mut().for_each(|a| *a = 0.0);
        self.temp.resize(out.len(), 0.0);
        let mut temp = std::mem::take(&mut self.temp);

        for (i, voice) in self.voices.iter_mut().enumerate() {
            voice.render(
                unpatched && i == self.active_string,
                rising && i == self.active_string,
                p.accent,
                self.f0[i],
                p.harmonics,
                p.timbre * p.timbre,
                p.morph,
                &mut temp,
                out,
                aux,
            );
        }
        self.temp = temp;
        false
    }
}

// ── the bass drum engine ─────────────────────────────────────────────────────

#[inline]
fn soft_clip(x: f32) -> f32 {
    if x < -3.0 {
        -1.0
    } else if x > 3.0 {
        1.0
    } else {
        x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
    }
}

#[inline]
fn diode(x: f32) -> f32 {
    if x >= 0.0 {
        x
    } else {
        let x = x * 2.0;
        0.7 * x / (1.0 + x.abs())
    }
}

/// stmlib's `SLOPE` macro: an asymmetric slew toward `target`.
#[inline]
fn slope(state: &mut f32, target: f32, positive: f32, negative: f32) {
    let error = target - *state;
    *state += if error > 0.0 { positive } else { negative } * error;
}

/// stmlib's `Overdrive` — a pre-gain → soft-clip → post-gain saturator.
#[derive(Debug, Clone, Default)]
struct Overdrive {
    pre_gain: f32,
    post_gain: f32,
}

impl Overdrive {
    fn new() -> Self {
        Self::default()
    }
    fn process(&mut self, drive: f32, in_out: &mut [f32]) {
        let drive_2 = drive * drive;
        let pre_gain_a = drive * 0.5;
        let pre_gain_b = drive_2 * drive_2 * drive * 24.0;
        let pre_gain = pre_gain_a + (pre_gain_b - pre_gain_a) * drive_2;
        let drive_squashed = drive * (2.0 - drive);
        let post_gain = 1.0 / soft_clip(0.33 + drive_squashed * (pre_gain - 0.33));
        let size = in_out.len();
        let mut pre_mod = ParamInterp::new(self.pre_gain, pre_gain, size);
        let mut post_mod = ParamInterp::new(self.post_gain, post_gain, size);
        for s in in_out.iter_mut() {
            let pre = pre_mod.next() * *s;
            *s = soft_clip(pre) * post_mod.next();
        }
        self.pre_gain = pre_gain;
        self.post_gain = post_gain;
    }
}

/// A bare quadrature sine oscillator (stmlib `SineOscillator`), used by
/// the analog bass drum in its sustained (free-running) mode.
#[derive(Debug, Clone, Default)]
struct SineOscillator {
    phase: f32,
}

impl SineOscillator {
    fn new() -> Self {
        Self::default()
    }
    /// `sin = amp·sine(phase)`, `cos = amp·sine(phase+0.25)`.
    #[inline]
    fn next(&mut self, frequency: f32, amplitude: f32) -> (f32, f32) {
        let f = frequency.min(0.5);
        self.phase += f;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        (
            amplitude * ws_sine(self.phase),
            amplitude * ws_sine(self.phase + 0.25),
        )
    }
}

/// The 808 bass drum model, revisited (plaits `AnalogBassDrum`).
struct AnalogBassDrum {
    pulse_remaining_samples: i32,
    fm_pulse_remaining_samples: i32,
    pulse: f32,
    pulse_height: f32,
    pulse_lp: f32,
    fm_pulse_lp: f32,
    retrig_pulse: f32,
    lp_out: f32,
    tone_lp: f32,
    sustain_gain: f32,
    resonator: Svf,
    oscillator: SineOscillator,
}

impl AnalogBassDrum {
    fn new() -> Self {
        Self {
            pulse_remaining_samples: 0,
            fm_pulse_remaining_samples: 0,
            pulse: 0.0,
            pulse_height: 0.0,
            pulse_lp: 0.0,
            fm_pulse_lp: 0.0,
            retrig_pulse: 0.0,
            lp_out: 0.0,
            tone_lp: 0.0,
            sustain_gain: 0.0,
            resonator: Svf::new(),
            oscillator: SineOscillator::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        sustain: bool,
        trigger: bool,
        accent: f32,
        f0: f32,
        tone: f32,
        decay: f32,
        attack_fm_amount: f32,
        self_fm_amount: f32,
        out: &mut [f32],
    ) {
        let trigger_pulse_duration = (1.0e-3 * SAMPLE_RATE) as i32;
        let fm_pulse_duration = (6.0e-3 * SAMPLE_RATE) as i32;
        let pulse_decay_time = 0.2e-3 * SAMPLE_RATE;
        let pulse_filter_time = 0.1e-3 * SAMPLE_RATE;
        let retrig_pulse_duration = 0.05 * SAMPLE_RATE;

        let scale = 0.001 / f0;
        let q = 1500.0 * semitones_to_ratio(decay * 80.0);
        let tone_f = (4.0 * f0 * semitones_to_ratio(tone * 108.0)).min(1.0);
        let exciter_leak = 0.08 * (tone + 0.25);

        if trigger {
            self.pulse_remaining_samples = trigger_pulse_duration;
            self.fm_pulse_remaining_samples = fm_pulse_duration;
            self.pulse_height = 3.0 + 7.0 * accent;
            self.lp_out = 0.0;
        }

        let size = out.len();
        let mut sustain_gain = ParamInterp::new(self.sustain_gain, accent * decay, size);

        for o in out.iter_mut() {
            let mut pulse;
            if self.pulse_remaining_samples != 0 {
                self.pulse_remaining_samples -= 1;
                pulse = if self.pulse_remaining_samples != 0 {
                    self.pulse_height
                } else {
                    self.pulse_height - 1.0
                };
                self.pulse = pulse;
            } else {
                self.pulse *= 1.0 - 1.0 / pulse_decay_time;
                pulse = self.pulse;
            }
            if sustain {
                pulse = 0.0;
            }
            self.pulse_lp += (pulse - self.pulse_lp) * (1.0 / pulse_filter_time);
            pulse = diode((pulse - self.pulse_lp) + pulse * 0.044);

            let mut fm_pulse = 0.0;
            if self.fm_pulse_remaining_samples != 0 {
                self.fm_pulse_remaining_samples -= 1;
                fm_pulse = 1.0;
                self.retrig_pulse = if self.fm_pulse_remaining_samples != 0 {
                    0.0
                } else {
                    -0.8
                };
            } else {
                self.retrig_pulse *= 1.0 - 1.0 / retrig_pulse_duration;
            }
            if sustain {
                fm_pulse = 0.0;
            }
            self.fm_pulse_lp += (fm_pulse - self.fm_pulse_lp) * (1.0 / pulse_filter_time);

            let punch = 0.7 + diode(10.0 * self.lp_out - 1.0);
            let attack_fm = self.fm_pulse_lp * 1.7 * attack_fm_amount;
            let self_fm = punch * 0.08 * self_fm_amount;
            let f = (f0 * (1.0 + attack_fm + self_fm)).clamp(0.0, 0.4);

            let resonator_out;
            if sustain {
                let (s, c) = self.oscillator.next(f, sustain_gain.next());
                resonator_out = s;
                self.lp_out = c;
            } else {
                self.resonator.set_g_q(tan_dirty(f), 1.0 + q * f);
                let (bp, lp) = self.resonator.process_bp_lp((pulse - self.retrig_pulse * 0.2) * scale);
                resonator_out = bp;
                self.lp_out = lp;
            }
            self.tone_lp += (pulse * exciter_leak + resonator_out - self.tone_lp) * tone_f;
            *o = self.tone_lp;
        }
        self.sustain_gain = accent * decay;
    }
}

/// The transient click filter of the synthetic bass drum.
#[derive(Debug, Clone)]
struct SyntheticBassDrumClick {
    lp: f32,
    hp: f32,
    filter: Svf,
}

impl SyntheticBassDrumClick {
    fn new() -> Self {
        let mut filter = Svf::new();
        filter.set_g_q(tan_fast(5000.0 / SAMPLE_RATE), 2.0);
        Self {
            lp: 0.0,
            hp: 0.0,
            filter,
        }
    }
    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        slope(&mut self.lp, input, 0.5, 0.1);
        self.hp += (self.lp - self.hp) * 0.04;
        self.filter.process(self.lp - self.hp, SvfMode::LowPass)
    }
}

/// The attack-noise band of the synthetic bass drum.
#[derive(Debug, Clone, Default)]
struct SyntheticBassDrumAttackNoise {
    lp: f32,
    hp: f32,
}

impl SyntheticBassDrumAttackNoise {
    #[inline]
    fn render(&mut self, rng: &mut Rng) -> f32 {
        let sample = rng.get_float();
        self.lp += (sample - self.lp) * 0.05;
        self.hp += (self.lp - self.hp) * 0.005;
        self.lp - self.hp
    }
}

/// A naive (inadvertently 909-ish) bass drum: a distorted FM sine with
/// body/transient envelopes (plaits `SyntheticBassDrum`).
struct SyntheticBassDrum {
    f0: f32,
    phase: f32,
    phase_noise: f32,
    fm: f32,
    fm_lp: f32,
    body_env: f32,
    body_env_lp: f32,
    transient_env: f32,
    transient_env_lp: f32,
    sustain_gain: f32,
    tone_lp: f32,
    click: SyntheticBassDrumClick,
    noise: SyntheticBassDrumAttackNoise,
    body_env_pulse_width: i32,
    fm_pulse_width: i32,
    rng: Rng,
}

impl SyntheticBassDrum {
    fn new() -> Self {
        Self {
            f0: 0.0,
            phase: 0.0,
            phase_noise: 0.0,
            fm: 0.0,
            fm_lp: 0.0,
            body_env: 0.0,
            body_env_lp: 0.0,
            transient_env: 0.0,
            transient_env_lp: 0.0,
            sustain_gain: 0.0,
            tone_lp: 0.0,
            click: SyntheticBassDrumClick::new(),
            noise: SyntheticBassDrumAttackNoise::default(),
            body_env_pulse_width: 0,
            fm_pulse_width: 0,
            rng: Rng::new(0x9_b1c3),
        }
    }

    #[inline]
    fn distorted_sine(phase: f32, phase_noise: f32, dirtiness: f32) -> f32 {
        let mut phase = phase + phase_noise * dirtiness;
        phase -= phase.floor();
        let triangle = (if phase < 0.5 { phase } else { 1.0 - phase }) * 4.0 - 1.0;
        let sine = 2.0 * triangle / (1.0 + triangle.abs());
        let clean_sine = ws_sine(phase + 0.75);
        sine + (1.0 - dirtiness) * (clean_sine - sine)
    }

    #[inline]
    fn transistor_vca(s: f32, gain: f32) -> f32 {
        let s = (s - 0.6) * gain;
        3.0 * s / (2.0 + s.abs()) + gain * 0.3
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        sustain: bool,
        trigger: bool,
        accent: f32,
        f0: f32,
        tone: f32,
        mut decay: f32,
        mut dirtiness: f32,
        fm_envelope_amount: f32,
        mut fm_envelope_decay: f32,
        out: &mut [f32],
    ) {
        decay *= decay;
        fm_envelope_decay *= fm_envelope_decay;
        let size = out.len();
        let mut f0_mod = ParamInterp::new(self.f0, f0, size);
        dirtiness *= (1.0 - 8.0 * f0).max(0.0);

        let fm_decay = 1.0 - 1.0 / (0.008 * (1.0 + fm_envelope_decay * 4.0) * SAMPLE_RATE);
        let body_env_decay =
            1.0 - 1.0 / (0.02 * SAMPLE_RATE) * semitones_to_ratio(-decay * 60.0);
        let transient_env_decay = 1.0 - 1.0 / (0.005 * SAMPLE_RATE);
        let tone_f = (4.0 * f0 * semitones_to_ratio(tone * 108.0)).min(1.0);
        let transient_level = tone;

        if trigger {
            self.fm = 1.0;
            self.body_env = 0.3 + 0.7 * accent;
            self.transient_env = self.body_env;
            self.body_env_pulse_width = (SAMPLE_RATE * 0.001) as i32;
            self.fm_pulse_width = (SAMPLE_RATE * 0.0013) as i32;
        }

        let mut sustain_gain = ParamInterp::new(self.sustain_gain, accent * decay, size);

        for o in out.iter_mut() {
            self.phase_noise += (self.rng.get_float() - 0.5 - self.phase_noise) * 0.002;
            let mut mix = 0.0;
            if sustain {
                self.phase += f0_mod.next();
                if self.phase >= 1.0 {
                    self.phase -= 1.0;
                }
                let body = Self::distorted_sine(self.phase, self.phase_noise, dirtiness);
                mix -= Self::transistor_vca(body, sustain_gain.next());
            } else {
                if self.fm_pulse_width != 0 {
                    self.fm_pulse_width -= 1;
                    self.phase = 0.25;
                } else {
                    self.fm *= fm_decay;
                    let fm = 1.0 + fm_envelope_amount * 3.5 * self.fm_lp;
                    self.phase += (f0_mod.next() * fm).min(0.5);
                    if self.phase >= 1.0 {
                        self.phase -= 1.0;
                    }
                }
                if self.body_env_pulse_width != 0 {
                    self.body_env_pulse_width -= 1;
                } else {
                    self.body_env *= body_env_decay;
                    self.transient_env *= transient_env_decay;
                }
                let envelope_lp_f = 0.1;
                self.body_env_lp += (self.body_env - self.body_env_lp) * envelope_lp_f;
                self.transient_env_lp += (self.transient_env - self.transient_env_lp) * envelope_lp_f;
                self.fm_lp += (self.fm - self.fm_lp) * envelope_lp_f;

                let body = Self::distorted_sine(self.phase, self.phase_noise, dirtiness);
                let click_in = if self.body_env_pulse_width != 0 { 0.0 } else { 1.0 };
                let transient = self.click.process(click_in) + self.noise.render(&mut self.rng);
                mix -= Self::transistor_vca(body, self.body_env_lp);
                mix -= transient * self.transient_env_lp * transient_level;
            }
            self.tone_lp += (mix - self.tone_lp) * tone_f;
            *o = self.tone_lp;
        }
        self.f0 = f0;
        self.sustain_gain = accent * decay;
    }
}

/// The bass drum engine: an analog 808 model (out, overdriven) and a
/// synthetic 909-ish model (aux). harmonics → FM/drive, timbre → tone,
/// morph → decay.
pub struct BassDrumEngine {
    analog: AnalogBassDrum,
    synthetic: SyntheticBassDrum,
    overdrive: Overdrive,
}

impl Default for BassDrumEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl BassDrumEngine {
    pub fn new() -> Self {
        Self {
            analog: AnalogBassDrum::new(),
            synthetic: SyntheticBassDrum::new(),
            overdrive: Overdrive::new(),
        }
    }
}

impl Engine for BassDrumEngine {
    fn render(&mut self, p: &EngineParameters, out: &mut [f32], aux: &mut [f32]) -> bool {
        let f0 = note_to_frequency(p.note);
        let attack_fm_amount = (p.harmonics * 4.0).min(1.0);
        let self_fm_amount = (p.harmonics * 4.0 - 1.0).clamp(0.0, 1.0);
        let drive = (p.harmonics * 2.0 - 1.0).max(0.0) * (1.0 - 16.0 * f0).max(0.0);
        let sustain = (p.trigger & TRIGGER_UNPATCHED) != 0;
        let trigger = (p.trigger & TRIGGER_RISING_EDGE) != 0;

        self.analog.render(
            sustain,
            trigger,
            p.accent,
            f0,
            p.timbre,
            p.morph,
            attack_fm_amount,
            self_fm_amount,
            out,
        );
        self.overdrive.process(0.5 + 0.5 * drive, out);

        let synth_dirtiness = if sustain {
            p.harmonics
        } else {
            0.4 - 0.25 * p.morph * p.morph
        };
        self.synthetic.render(
            sustain,
            trigger,
            p.accent,
            f0,
            p.timbre,
            p.morph,
            synth_dirtiness,
            (p.harmonics * 2.0).min(1.0),
            (p.harmonics * 2.0 - 1.0).max(0.0),
            aux,
        );
        true
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
    fn bass_drum_engine_thumps() {
        let mut eng = BassDrumEngine::new();
        let mut out = vec![0.0_f32; 64];
        let mut aux = vec![0.0_f32; 64];
        for (h, tb, mo) in [(0.2, 0.4, 0.4), (0.6, 0.5, 0.6), (0.9, 0.7, 0.5)] {
            let mut energy = 0.0;
            for blk in 0..120 {
                let trig = if blk == 0 { TRIGGER_RISING_EDGE } else { 0 };
                let p = EngineParameters { trigger: trig, note: 36.0, harmonics: h, timbre: tb, morph: mo, ..Default::default() };
                eng.render(&p, &mut out, &mut aux);
                assert!(out.iter().chain(aux.iter()).all(|v| v.is_finite() && v.abs() <= 8.0), "h={h} bounded");
                energy += out.iter().chain(aux.iter()).map(|v| v * v).sum::<f32>();
            }
            assert!(energy > 1e-5, "bass drum thumps at h={h}: {energy}");
        }
    }

    #[test]
    fn string_engine_plucks() {
        let mut eng = StringEngine::new();
        let mut out = vec![0.0_f32; 64];
        let mut aux = vec![0.0_f32; 64];
        for (h, tb, mo) in [(0.1, 0.4, 0.4), (0.5, 0.5, 0.5), (0.9, 0.7, 0.6)] {
            let mut energy = 0.0;
            for blk in 0..200 {
                let trig = if blk == 0 { TRIGGER_RISING_EDGE | TRIGGER_HIGH } else { TRIGGER_HIGH };
                let p = EngineParameters { trigger: trig, note: 48.0, harmonics: h, timbre: tb, morph: mo, ..Default::default() };
                eng.render(&p, &mut out, &mut aux);
                assert!(out.iter().chain(aux.iter()).all(|v| v.is_finite() && v.abs() <= 24.0), "h={h} bounded");
                energy += out.iter().map(|v| v * v).sum::<f32>();
            }
            assert!(energy > 1e-7, "string sounds at h={h}: {energy}");
        }
    }

    #[test]
    fn modal_engine_rings() {
        let mut eng = ModalEngine::new();
        let mut out = vec![0.0_f32; 64];
        let mut aux = vec![0.0_f32; 64];
        for (h, tb, mo) in [(0.2, 0.3, 0.3), (0.5, 0.5, 0.5), (0.8, 0.7, 0.6)] {
            // strike on the first block, then let it ring
            let mut energy = 0.0;
            for blk in 0..200 {
                let trig = if blk == 0 { TRIGGER_RISING_EDGE | TRIGGER_HIGH } else { TRIGGER_HIGH };
                let p = EngineParameters { trigger: trig, note: 48.0, harmonics: h, timbre: tb, morph: mo, ..Default::default() };
                eng.render(&p, &mut out, &mut aux);
                assert!(out.iter().chain(aux.iter()).all(|v| v.is_finite() && v.abs() <= 8.0), "h={h} bounded");
                energy += out.iter().map(|v| v * v).sum::<f32>();
            }
            assert!(energy > 1e-7, "modal rings at h={h}: {energy}");
        }
    }

    #[test]
    fn wavetable_engine_scans_the_terrain() {
        let mut eng = WavetableEngine::new();
        let mut out = vec![0.0_f32; 64];
        let mut aux = vec![0.0_f32; 64];
        for (h, tb, mo) in [(0.1, 0.2, 0.3), (0.5, 0.5, 0.5), (0.9, 0.8, 0.7)] {
            let p = EngineParameters { note: 48.0, harmonics: h, timbre: tb, morph: mo, ..Default::default() };
            // warm up past the differentiator's init transient
            for _ in 0..4 {
                eng.render(&p, &mut out, &mut aux);
            }
            let mut energy = 0.0;
            for _ in 0..200 {
                eng.render(&p, &mut out, &mut aux);
                assert!(out.iter().chain(aux.iter()).all(|v| v.is_finite() && v.abs() <= 4.0), "h={h} bounded");
                energy += out.iter().map(|v| v * v).sum::<f32>();
            }
            assert!(energy > 0.0001, "wavetable sounds at h={h}: {energy}");
        }
    }

    #[test]
    fn grain_engine_makes_grains() {
        let mut eng = GrainEngine::new();
        let mut out = vec![0.0_f32; 64];
        let mut aux = vec![0.0_f32; 64];
        for (h, tb, mo) in [(0.2, 0.3, 0.4), (0.5, 0.5, 0.5), (0.8, 0.7, 0.7)] {
            let p = EngineParameters { note: 48.0, harmonics: h, timbre: tb, morph: mo, ..Default::default() };
            let mut energy = 0.0;
            for _ in 0..80 {
                eng.render(&p, &mut out, &mut aux);
                assert!(out.iter().chain(aux.iter()).all(|v| v.is_finite() && v.abs() <= 8.0), "h={h} bounded");
                energy += out.iter().chain(aux.iter()).map(|v| v * v).sum::<f32>();
            }
            assert!(energy > 0.001, "grain sounds at h={h}: {energy}");
        }
    }

    #[test]
    fn swarm_engine_makes_a_swarm() {
        let mut eng = SwarmEngine::new();
        let mut out = vec![0.0_f32; 64];
        let mut aux = vec![0.0_f32; 64];
        for (h, mo) in [(0.1, 0.2), (0.5, 0.5), (0.9, 0.8)] {
            let p = EngineParameters { note: 48.0, harmonics: h, timbre: 0.6, morph: mo, ..Default::default() };
            let mut energy = 0.0;
            for _ in 0..120 {
                eng.render(&p, &mut out, &mut aux);
                assert!(out.iter().chain(aux.iter()).all(|v| v.is_finite() && v.abs() <= 4.0), "h={h} bounded");
                energy += out.iter().chain(aux.iter()).map(|v| v * v).sum::<f32>();
            }
            assert!(energy > 0.001, "swarm sounds at h={h}: {energy}");
        }
    }

    #[test]
    fn additive_engine_makes_a_spectrum() {
        let mut eng = AdditiveEngine::new();
        let mut out = vec![0.0_f32; 64];
        let mut aux = vec![0.0_f32; 64];
        for (h, tb, mo) in [(0.2, 0.3, 0.4), (0.5, 0.5, 0.6), (0.8, 0.7, 0.8)] {
            let p = EngineParameters { note: 48.0, harmonics: h, timbre: tb, morph: mo, ..Default::default() };
            let mut energy = 0.0;
            for _ in 0..60 {
                eng.render(&p, &mut out, &mut aux);
                assert!(out.iter().chain(aux.iter()).all(|v| v.is_finite() && v.abs() <= 4.0), "h={h} bounded");
                energy += out.iter().map(|v| v * v).sum::<f32>();
            }
            assert!(energy > 0.001, "additive sounds at h={h}: {energy}");
        }
    }

    #[test]
    fn waveshaping_engine_makes_sound_and_folds() {
        let mut eng = WaveshapingEngine::new();
        let mut out = vec![0.0_f32; 64];
        let mut aux = vec![0.0_f32; 64];
        for (h, tb) in [(0.2, 0.3), (0.5, 0.6), (0.8, 0.9)] {
            let p = EngineParameters {
                note: 48.0,
                harmonics: h,
                timbre: tb,
                morph: 0.5,
                ..Default::default()
            };
            let mut energy = 0.0;
            for _ in 0..50 {
                eng.render(&p, &mut out, &mut aux);
                assert!(out.iter().chain(aux.iter()).all(|v| v.is_finite()), "h={h} finite");
                energy += out.iter().map(|v| v * v).sum::<f32>();
            }
            assert!(energy > 0.01, "waveshaper sounds at h={h}: {energy}");
        }
    }

    #[test]
    fn waveshaper_tables_load() {
        let t = ws_tables();
        assert_eq!(t.ws.len(), 5);
        assert!(t.ws.iter().all(|w| w.len() == 257));
        assert_eq!(t.fold.len(), 516);
        assert_eq!(t.fold_2.len(), 516);
    }

    #[test]
    fn chord_engine_makes_a_chord() {
        let mut eng = ChordEngine::new();
        let mut out = vec![0.0_f32; 64];
        let mut aux = vec![0.0_f32; 64];
        // a major chord (harmonics into the major region), root at MIDI 48
        for harm in [0.0, 0.35, 0.6, 1.0] {
            let p = EngineParameters {
                note: 48.0,
                harmonics: harm,
                timbre: 0.3,
                morph: 0.4,
                ..Default::default()
            };
            let mut energy = 0.0;
            for _ in 0..50 {
                eng.render(&p, &mut out, &mut aux);
                assert!(out.iter().chain(aux.iter()).all(|v| v.is_finite()), "chord {harm} finite");
                energy += out.iter().map(|v| v * v).sum::<f32>();
            }
            assert!(energy > 0.1, "chord {harm} sounds: {energy}");
        }
    }

    #[test]
    fn chord_table_has_all_seventeen_types() {
        assert_eq!(CHORDS.len(), 17);
        // the octave chord's top note is ~12 semitones
        assert!((CHORDS[0][3] - 12.0).abs() < 0.01);
        // the major chord has a major third
        assert!((CHORDS[6][1] - 4.0).abs() < 0.01);
    }

    #[test]
    fn virtual_analog_makes_a_detuned_pair() {
        let mut eng = VirtualAnalogEngine::new();
        let mut out = vec![0.0_f32; 64];
        let mut aux = vec![0.0_f32; 64];
        let p = EngineParameters {
            note: 48.0,
            harmonics: 0.6, // detune the second oscillator
            timbre: 0.4,
            morph: 0.6,
            ..Default::default()
        };
        let mut energy = 0.0;
        for _ in 0..60 {
            let enveloped = eng.render(&p, &mut out, &mut aux);
            assert!(!enveloped);
            assert!(
                out.iter().chain(aux.iter()).all(|v| v.is_finite() && v.abs() <= 2.0),
                "bounded"
            );
            energy += out.iter().map(|v| v * v).sum::<f32>();
        }
        assert!(energy > 0.1, "the analog pair sounds: {energy}");
    }

    #[test]
    fn variable_shape_oscillator_is_periodic() {
        let mut osc = VariableShapeOscillator::new();
        let mut out = vec![0.0_f32; 1024];
        // ~262 Hz at 48k → normalized 0.00545
        osc.render(0.00545, 0.5, 0.0, &mut out); // saw
        // warm up then measure crossings
        osc.render(0.00545, 0.5, 0.0, &mut out);
        let zc = out.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count();
        assert!(zc >= 2 && zc <= 12, "saw has a few crossings: {zc}");
        assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 2.0));
    }

    #[test]
    fn fm_engine_makes_a_tone_and_responds_to_timbre() {
        let mut eng = FmEngine::new();
        let mut out = vec![0.0_f32; 64];
        let mut aux = vec![0.0_f32; 64];
        // at timbre 0 the modulation amount is 0 → a near-pure carrier;
        // at timbre 1 the FM index is high → brighter (more energy in aux)
        let mut energy_at = |timbre: f32, eng: &mut FmEngine| -> f32 {
            let p = EngineParameters {
                note: 48.0,
                harmonics: 0.4,
                timbre,
                morph: 0.5,
                ..Default::default()
            };
            let mut e = 0.0;
            for _ in 0..40 {
                eng.render(&p, &mut out, &mut aux);
                e += out.iter().chain(aux.iter()).map(|v| v * v).sum::<f32>();
                assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 2.0));
            }
            e
        };
        let quiet = energy_at(0.0, &mut eng);
        let bright = energy_at(1.0, &mut eng);
        assert!(quiet > 0.0, "FM carrier sounds at timbre 0");
        assert!(bright > 0.0, "FM sounds at timbre 1");
    }

    #[test]
    fn fm_ratio_quantizer_is_monotone_musical() {
        // the ratio table maps harmonics 0→1 across a rising set of
        // musical FM ratios (in semitones); 1.0 ratio (0 st) sits mid-table
        let lo = fm_quantize_ratio(0.0);
        let hi = fm_quantize_ratio(1.0);
        assert!(hi > lo, "ratio rises with harmonics: {lo} -> {hi}");
        // 0.5 ratio = -12 st at the low end
        assert!((lo + 12.0).abs() < 0.5, "lowest ratio ~ -12 st: {lo}");
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
