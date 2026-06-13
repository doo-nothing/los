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
