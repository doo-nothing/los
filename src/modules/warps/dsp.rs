//! # Warps engine — the meta-modulator
//!
//! Ported from pichenettes/eurorack (warps/dsp/*, MIT, copyright 2014
//! Emilie Gillet, attribution preserved): the cross-modulation core
//! faithful — the six algorithms (cross-fade, wavefolder through the
//! real timbre-derived fold table, analog and digital ring
//! modulation, XOR, comparator) with the adjacent-pair morph, the
//! saturating amplifier with its noise gate and overdrive, the
//! internal carrier oscillator, and a channel vocoder for the top of
//! the algorithm sweep.
//!
//! The cross-modulation algorithms are 6× oversampled through the
//! firmware's 48-tap polyphase SRC (so aliasing matches the hardware).
//! The hidden single-sideband frequency-shifter easter egg is ported as
//! carrier shape "freq_shifter" (external-input path; 17-pole Hilbert
//! all-pass quadrature pair). One documented divergence remains: the
//! vocoder uses a simplified SVF band bank rather than the firmware's
//! 20 tuned bands.

#![allow(clippy::excessive_precision)]

use std::sync::OnceLock;

const BIPOLAR_FOLD_BIN: &[u8] = include_bytes!("bipolar_fold.bin");

pub struct Tables {
    pub bipolar_fold: Vec<f32>, // 4097, centre at 2048
    pub xfade_in: Vec<f32>,     // 257
    pub xfade_out: Vec<f32>,
    pub sin: Vec<f32>, // 1281 (256-guarded for cos via +256)
}

static TABLES: OnceLock<Tables> = OnceLock::new();

pub fn tables() -> &'static Tables {
    TABLES.get_or_init(|| {
        let bipolar_fold: Vec<f32> = BIPOLAR_FOLD_BIN
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let xfade_in: Vec<f32> = (0..257)
            .map(|i| {
                let t = (i as f32 / 256.0).min(1.0) * std::f32::consts::FRAC_PI_2;
                t.sin() * std::f32::consts::FRAC_1_SQRT_2
            })
            .collect();
        let xfade_out: Vec<f32> = (0..257)
            .map(|i| {
                let t = (i as f32 / 256.0).min(1.0) * std::f32::consts::FRAC_PI_2;
                t.cos() * std::f32::consts::FRAC_1_SQRT_2
            })
            .collect();
        // lut_sin: one period over 1024 + a quarter-period guard, so
        // sin+256 reads cosine.
        let sin: Vec<f32> = (0..1281)
            .map(|i| (i as f32 / 1024.0 * std::f32::consts::TAU).sin())
            .collect();
        Tables {
            bipolar_fold,
            xfade_in,
            xfade_out,
            sin,
        }
    })
}

#[inline]
fn interpolate(table: &[f32], t: f32, size: f32) -> f32 {
    let p = t * size;
    let i = p.floor();
    let frac = p - i;
    let idx = (i as i64).clamp(0, table.len() as i64 - 2) as usize;
    table[idx] + (table[idx + 1] - table[idx]) * frac
}

/// stmlib SoftLimit — the gentle cubic saturator.
#[inline]
pub fn soft_limit(x: f32) -> f32 {
    x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
}

/// stmlib SoftClip — soft up to ±3, hard beyond.
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    if x < -3.0 {
        -1.0
    } else if x > 3.0 {
        1.0
    } else {
        soft_limit(x)
    }
}

#[inline]
fn clip16(x: f32) -> f32 {
    (x * 32768.0).clamp(-32768.0, 32767.0)
}

// ── the cross-modulation algorithms ─────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Xfade,
    Fold,
    AnalogRing,
    DigitalRing,
    Xor,
    Comparator,
    Nop,
}

/// modulator.cc Diode — Parker's diode ring-modulator nonlinearity.
#[inline]
fn diode(x: f32) -> f32 {
    let sign = if x > 0.0 { 1.0 } else { -1.0 };
    let mut dead_zone = x.abs() - 0.667;
    dead_zone += dead_zone.abs();
    dead_zone *= dead_zone;
    0.043_247_658_227_260_63 * dead_zone * sign
}

/// One sample of a single algorithm: f(x1, x2, parameter).
pub fn xmod(algo: Algorithm, x1: f32, x2: f32, parameter: f32) -> f32 {
    let t = tables();
    match algo {
        Algorithm::Xfade => {
            let fade_in = interpolate(&t.xfade_in, parameter, 256.0);
            let fade_out = interpolate(&t.xfade_out, parameter, 256.0);
            x1 * fade_in + x2 * fade_out
        }
        Algorithm::Fold => {
            let mut sum = x1 + x2 + x1 * x2 * 0.25;
            sum *= 0.02 + parameter;
            let k_scale = 2048.0 / ((1.0 + 1.0 + 0.25) * 1.02);
            // table centred at 2048: index = 2048 + sum*kScale
            let p = 2048.0 + sum * k_scale;
            let i = p.floor();
            let frac = p - i;
            let idx = (i as i64).clamp(0, t.bipolar_fold.len() as i64 - 2) as usize;
            t.bipolar_fold[idx] + (t.bipolar_fold[idx + 1] - t.bipolar_fold[idx]) * frac
        }
        Algorithm::AnalogRing => {
            let carrier = x2 * 2.0;
            let mut ring = diode(x1 + carrier) + diode(x1 - carrier);
            ring *= 4.0 + parameter * 24.0;
            soft_limit(ring)
        }
        Algorithm::DigitalRing => {
            let ring = 4.0 * x1 * x2 * (1.0 + parameter * 8.0);
            ring / (1.0 + ring.abs())
        }
        Algorithm::Xor => {
            let x1s = clip16(x1) as i32 as i16;
            let x2s = clip16(x2) as i32 as i16;
            let m = (x1s ^ x2s) as f32 / 32768.0;
            let sum = (x1 + x2) * 0.7;
            sum + (m - sum) * parameter
        }
        Algorithm::Comparator => {
            let x = parameter * 2.995;
            let xi = x.floor();
            let xf = x - xi;
            let modu = x1;
            let carrier = x2;
            let direct = modu.min(carrier);
            let window = if modu.abs() > carrier.abs() { modu } else { carrier };
            let window2 = if modu.abs() > carrier.abs() {
                modu.abs()
            } else {
                -carrier.abs()
            };
            let threshold = if carrier > 0.05 { carrier } else { modu };
            let seq = [direct, threshold, window, window2];
            let i = (xi as usize).min(2);
            seq[i] + (seq[i + 1] - seq[i]) * xf
        }
        Algorithm::Nop => x1,
    }
}

const ALGO_ORDER: [Algorithm; 7] = [
    Algorithm::Xfade,
    Algorithm::Fold,
    Algorithm::AnalogRing,
    Algorithm::DigitalRing,
    Algorithm::Xor,
    Algorithm::Comparator,
    Algorithm::Nop,
];

/// parameters.h skewed_modulation_parameter — a non-linear response on
/// the parameter for the middle algorithms.
fn skewed_parameter(algorithm_01: f32, parameter: f32) -> f32 {
    let a = algorithm_01;
    let skew = if a <= 1.0 {
        a
    } else if a >= 5.0 {
        1.0
    } else if a >= 4.0 {
        5.0 - a
    } else {
        0.0
    };
    parameter * (1.0 + skew * (parameter - 1.0))
}

// ── saturating amplifier ────────────────────────────────────────────────────

/// modulator.h SaturatingAmplifier — the per-channel noise gate +
/// overdrive front end.
#[derive(Debug, Clone, Default)]
pub struct SaturatingAmplifier {
    level: f32,
    pre_gain: f32,
    post_gain: f32,
}

impl SaturatingAmplifier {
    /// Returns the gated/gain-staged signal; `out_raw` accumulates the
    /// dry-with-drive copy (the firmware's aux feed).
    pub fn process(&mut self, drive: f32, limit: f32, input: &[f32], out: &mut [f32], out_raw: &mut [f32]) {
        let size = input.len();
        for i in 0..size {
            let s = input[i];
            let error = s * s - self.level;
            self.level += error * if error > 0.0 { 0.1 } else { 0.0001 };
            let gated = s * if self.level <= 0.0001 {
                (1.0 / 0.0001) * self.level
            } else {
                1.0
            };
            out[i] = gated;
            out_raw[i] += gated * drive;
        }
        let drive_2 = drive * drive;
        let pre_gain_a = drive * 0.5;
        let pre_gain_b = drive_2 * drive_2 * drive * 24.0;
        let pre_gain = pre_gain_a + (pre_gain_b - pre_gain_a) * drive_2;
        let drive_squished = drive * (2.0 - drive);
        let post_gain = 1.0 / soft_clip(0.33 + drive_squished * (pre_gain - 0.33));
        self.pre_gain = pre_gain;
        self.post_gain = post_gain;
        for o in out.iter_mut().take(size) {
            let pre = pre_gain * *o;
            let post = soft_clip(pre) * post_gain;
            *o = pre + (post - pre) * limit;
        }
    }
}

// ── internal oscillator ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscShape {
    Sine,
    Triangle,
    Saw,
    Pulse,
    NoiseLp,
}

/// A small internal carrier oscillator (the firmware's xmod/vocoder
/// oscillators, used when no external carrier is patched).
#[derive(Debug, Clone)]
pub struct Oscillator {
    phase: f32,
    sample_rate: f32,
    lp: f32,
    rng: u32,
}

impl Oscillator {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            phase: 0.0,
            sample_rate,
            lp: 0.0,
            rng: 0x1234_5678,
        }
    }

    /// note is a MIDI-ish pitch (0..127); pm is per-sample phase
    /// modulation (the firmware feeds input L here).
    pub fn render(&mut self, shape: OscShape, note: f32, pm: &[f32], out: &mut [f32]) {
        let freq = 440.0 * 2.0_f32.powf((note - 69.0) / 12.0) / self.sample_rate;
        for (i, o) in out.iter_mut().enumerate() {
            self.phase += freq;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            let p = (self.phase + pm[i] * 0.5).rem_euclid(1.0);
            *o = match shape {
                OscShape::Sine => (p * std::f32::consts::TAU).sin(),
                OscShape::Triangle => 4.0 * (p - 0.5).abs() - 1.0,
                OscShape::Saw => 2.0 * p - 1.0,
                OscShape::Pulse => {
                    if p < 0.5 {
                        1.0
                    } else {
                        -1.0
                    }
                }
                OscShape::NoiseLp => {
                    self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let n = (self.rng >> 8) as f32 / 8_388_608.0 - 1.0;
                    self.lp += (n - self.lp) * 0.2;
                    self.lp
                }
            };
        }
    }
}

// ── channel vocoder ─────────────────────────────────────────────────────────

const VOCODER_BANDS: usize = 16;

/// A band-pass + envelope follower pair (one vocoder band).
#[derive(Debug, Clone, Default)]
struct VocBand {
    // SVF state for analysis (carrier + modulator share frequency)
    f: f32,
    damp: f32,
    c_lp: f32,
    c_bp: f32,
    m_lp: f32,
    m_bp: f32,
    env: f32,
    last_carrier: f32,
}

impl VocBand {
    fn set_frequency(&mut self, hz: f32, sr: f32) {
        let f = (hz / sr).min(0.45);
        self.f = 2.0 * (std::f32::consts::PI * f).sin();
        self.damp = 0.18; // moderate resonance
    }
    #[inline]
    fn bandpass(lp: &mut f32, bp: &mut f32, f: f32, damp: f32, input: f32) -> f32 {
        let notch = input - *bp * damp;
        *lp += f * *bp;
        let hp = notch - *lp;
        *bp += f * hp;
        *bp
    }
}

/// A simplified channel vocoder: VOCODER_BANDS log-spaced bands,
/// modulator envelope imposed on the carrier per band. The exact band
/// count and tuning are a documented simplification of the firmware's
/// 20-band tuned bank.
#[derive(Debug, Clone)]
pub struct Vocoder {
    bands: Vec<VocBand>,
    release_time: f32,
    formant_shift: f32,
    attack: f32,
    decay: f32,
}

impl Vocoder {
    pub fn new(sample_rate: f32) -> Self {
        let mut bands = vec![VocBand::default(); VOCODER_BANDS];
        // 110 Hz .. ~7 kHz, third-octave-ish
        for (i, b) in bands.iter_mut().enumerate() {
            let hz = 110.0 * 1.35_f32.powi(i as i32);
            b.set_frequency(hz, sample_rate);
        }
        Self {
            bands,
            release_time: 0.5,
            formant_shift: 0.5,
            attack: 0.05,
            decay: 0.01,
        }
    }

    pub fn set_release_time(&mut self, t: f32) {
        self.release_time = t.clamp(0.0, 1.0);
        // faster release for low values
        self.decay = 0.02 * 2.0_f32.powf(-4.0 * self.release_time) + 0.0005;
        self.attack = self.decay * 4.0;
    }

    pub fn set_formant_shift(&mut self, f: f32) {
        self.formant_shift = f.clamp(0.0, 1.0);
    }

    pub fn process(&mut self, modulator: &[f32], carrier: &[f32], out: &mut [f32]) {
        // formant shift maps which modulator band drives which carrier band
        let shift = ((self.formant_shift - 0.5) * 2.0 * VOCODER_BANDS as f32 * 0.5) as i32;
        for (n, o) in out.iter_mut().enumerate() {
            let m = modulator[n];
            let c = carrier[n];
            // analyze both, track modulator envelope
            for b in self.bands.iter_mut() {
                let cb = VocBand::bandpass(&mut b.c_lp, &mut b.c_bp, b.f, b.damp, c);
                let mb = VocBand::bandpass(&mut b.m_lp, &mut b.m_bp, b.f, b.damp, m);
                let rect = mb.abs();
                let coeff = if rect > b.env { self.attack } else { self.decay };
                b.env += (rect - b.env) * coeff;
                b.c_bp_store(cb);
            }
            let mut acc = 0.0;
            for (i, b) in self.bands.iter().enumerate() {
                let src = (i as i32 - shift).clamp(0, VOCODER_BANDS as i32 - 1) as usize;
                acc += b.last_carrier * self.bands[src].env;
            }
            *o = soft_limit(acc * (2.5 / VOCODER_BANDS as f32).sqrt() * 4.0);
        }
    }
}

impl VocBand {
    #[inline]
    fn c_bp_store(&mut self, v: f32) {
        self.last_carrier = v;
    }
}

// ── the modulator (top-level process) ───────────────────────────────────────

/// The panel parameters the shell hands to the modulator each block.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub algorithm: f32,  // 0..1, the meta knob
    pub timbre: f32,     // 0..1, modulation_parameter
    pub drive1: f32,     // carrier channel drive
    pub drive2: f32,     // modulator channel drive
    pub carrier_shape: usize, // 0 = external, 1..5 = internal osc shape
    pub note: f32,       // internal-oscillator pitch
    pub frequency_shifter: bool, // the SSB freq-shifter easter egg
}

impl Default for Params {
    fn default() -> Self {
        Self {
            algorithm: 0.0,
            timbre: 0.5,
            drive1: 0.5,
            drive2: 0.5,
            carrier_shape: 0,
            note: 48.0,
            frequency_shifter: false,
        }
    }
}

/// Ties the cross-mod algorithms, the saturating amplifiers, the
/// internal oscillator and the vocoder into warps' Process.
/// The 6× / 48-tap polyphase sample-rate-conversion filters (warps
/// `sample_rate_conversion_filters.h`), stored half-length and mirrored.
const SRC_OVERSAMPLING: usize = 6;
#[rustfmt::skip]
const SRC_UP_HALF: [f32; 24] = [
     4.357278576e-04, -2.297029461e-03, -4.703810602e-03, -8.774604727e-03,
    -1.433899145e-02, -2.112793398e-02, -2.853108802e-02, -3.552868193e-02,
    -4.069862931e-02, -4.228981313e-02, -3.836519645e-02, -2.700780696e-02,
    -6.569014106e-03,  2.407089704e-02,  6.526452513e-02,  1.164165703e-01,
     1.758932961e-01,  2.410483237e-01,  3.083744498e-01,  3.737697127e-01,
     4.328923682e-01,  4.815728403e-01,  5.162355916e-01,  5.342582974e-01,
];
#[rustfmt::skip]
const SRC_DOWN_HALF: [f32; 24] = [
     7.262130960e-05, -3.828382434e-04, -7.839684337e-04, -1.462434121e-03,
    -2.389831909e-03, -3.521322331e-03, -4.755181337e-03, -5.921446989e-03,
    -6.783104885e-03, -7.048302188e-03, -6.394199409e-03, -4.501301159e-03,
    -1.094835684e-03,  4.011816173e-03,  1.087742085e-02,  1.940276171e-02,
     2.931554935e-02,  4.017472062e-02,  5.139574163e-02,  6.229495212e-02,
     7.214872804e-02,  8.026214006e-02,  8.603926526e-02,  8.904304957e-02,
];

#[inline]
fn src_fir(half: &[f32; 24], i: usize) -> f32 {
    if i < 24 {
        half[i]
    } else {
        half[47 - i]
    }
}

/// Polyphase 1→6 upsampler.
#[derive(Debug, Clone, Default)]
struct SrcUp {
    hist: [f32; 8], // 48 / 6
}

impl SrcUp {
    fn process(&mut self, x: f32, out: &mut [f32; SRC_OVERSAMPLING]) {
        for k in (1..8).rev() {
            self.hist[k] = self.hist[k - 1];
        }
        self.hist[0] = x;
        for (p, o) in out.iter_mut().enumerate() {
            let mut acc = 0.0;
            for k in 0..8 {
                acc += src_fir(&SRC_UP_HALF, p + k * SRC_OVERSAMPLING) * self.hist[k];
            }
            *o = acc;
        }
    }
}

/// Polyphase 6→1 downsampler (a 48-tap FIR evaluated at the decimation
/// points).
#[derive(Debug, Clone)]
struct SrcDown {
    hist: [f32; 48],
    write: usize,
    phase: usize,
}

impl Default for SrcDown {
    fn default() -> Self {
        Self { hist: [0.0; 48], write: 0, phase: 0 }
    }
}

impl SrcDown {
    /// Push one oversampled sample; returns `Some(out)` on every 6th.
    fn process(&mut self, x: f32) -> Option<f32> {
        let newest = self.write % 48;
        self.hist[newest] = x;
        self.write += 1;
        self.phase += 1;
        if self.phase == SRC_OVERSAMPLING {
            self.phase = 0;
            let mut acc = 0.0;
            for j in 0..48 {
                acc += src_fir(&SRC_DOWN_HALF, j) * self.hist[(newest + 96 - j) % 48];
            }
            Some(acc)
        } else {
            None
        }
    }
}

/// The 17 all-pass pole coefficients of the Hilbert transform network
/// (warps `lut_ap_poles` — an elliptic half-band all-pass decomposition).
#[rustfmt::skip]
const AP_POLES: [f32; 17] = [
    0.9999174437, 0.9997160329, 0.9993897602, 0.9987952776,
    0.9976718129, 0.9955280098, 0.9914315323, 0.9836199785,
    0.9688016569, 0.9409767040, 0.8897147107, 0.7984785110,
    0.6454684139, 0.4118108699, 0.0972566715, -0.2775386379,
    -0.7176356738,
];

/// A polyphase all-pass IIR Hilbert transform (warps `QuadratureTransform`):
/// 17 first-order all-passes split into an in-phase (I) and quadrature (Q)
/// chain, producing a 90°-shifted pair for single-sideband modulation.
#[derive(Debug, Clone)]
struct QuadratureTransform {
    coef: [f32; 17],
    x: [f32; 17],
    y: [f32; 17],
}

impl Default for QuadratureTransform {
    fn default() -> Self {
        Self {
            coef: std::array::from_fn(|i| -AP_POLES[i]),
            x: [0.0; 17],
            y: [0.0; 17],
        }
    }
}

impl QuadratureTransform {
    #[inline]
    fn process(&mut self, input: f32) -> (f32, f32) {
        let mut i_out = 0.0;
        let mut q_out = 0.0;
        for k in 0..17 {
            let src = if k <= 1 {
                input
            } else if k & 1 == 1 {
                q_out
            } else {
                i_out
            };
            let y = self.coef[k] * (src - self.y[k]) + self.x[k];
            self.x[k] = src;
            self.y[k] = y;
            if k & 1 == 1 {
                q_out = y;
            } else {
                i_out = y;
            }
        }
        (i_out, q_out)
    }
}

pub struct Modulator {
    amp: [SaturatingAmplifier; 2],
    osc: Oscillator,
    vocoder: Vocoder,
    src_up: [SrcUp; 2],
    src_down: SrcDown,
    qt: [QuadratureTransform; 2],
    feedback_sample: f32,
    prev_algorithm: f32,
    prev_timbre: f32,
}

impl Modulator {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            amp: [SaturatingAmplifier::default(), SaturatingAmplifier::default()],
            osc: Oscillator::new(sample_rate),
            vocoder: Vocoder::new(sample_rate),
            src_up: [SrcUp::default(), SrcUp::default()],
            src_down: SrcDown::default(),
            qt: [QuadratureTransform::default(), QuadratureTransform::default()],
            feedback_sample: 0.0,
            prev_algorithm: 0.0,
            prev_timbre: 0.5,
        }
    }

    /// The single-sideband frequency-shifter easter egg (warps
    /// `ProcessEasterEgg`, external-input path): Hilbert-transform the
    /// carrier (input 1) and modulator (input 2) into quadrature, rotate the
    /// carrier by `algorithm`, ring-modulate, and crossfade the upper/lower
    /// sideband by `timbre`. `drive1` = feedback, `drive2` = dry/wet.
    pub fn process_easter_egg(
        &mut self,
        p: &Params,
        carrier_in: &[f32],
        modulator_in: &[f32],
        main: &mut [f32],
        aux: &mut [f32],
    ) {
        let t = tables();
        let size = main.len();
        let phase_shift = p.algorithm;
        // carrier I/Q, rotated by the phase shift
        let r_sin = interpolate(&t.sin, phase_shift, 1024.0);
        let r_cos = interpolate(&t.sin[256..], phase_shift, 1024.0);
        let mut feedback = self.feedback_sample;
        for i in 0..size {
            let (ci0, cq0) = self.qt[0].process(carrier_in[i]);
            let carrier_i = r_sin * ci0 + r_cos * cq0;
            let carrier_q = r_sin * cq0 - r_cos * ci0;

            let timbre = p.timbre;
            let in_ = modulator_in[i];
            let mut amount = p.drive1;
            amount *= 2.0 - amount;
            amount *= 2.0 - amount;
            let max_fb = 1.0 + 2.0 * (timbre - 0.5) * (timbre - 0.5);
            let modulator =
                in_ + amount * (soft_clip(in_ + max_fb * feedback * amount) - in_);
            let (mi, mq) = self.qt[1].process(modulator);

            let a = carrier_i * mi;
            let b = carrier_q * mq;
            let up = a - b;
            let down = a + b;
            let fade_in = interpolate(&t.xfade_in, timbre, 256.0);
            let fade_out = interpolate(&t.xfade_out, timbre, 256.0);
            let mut m = up * fade_in + down * fade_out;
            let mut x = down * fade_in + up * fade_out;
            feedback += 0.2 * (m - feedback); // one-pole LP on the feedback
            let wet_dry = 1.0 - p.drive2;
            m += wet_dry * (in_ - m);
            x += wet_dry * (in_ - x);
            main[i] = m;
            aux[i] = x;
        }
        self.feedback_sample = feedback;
    }

    /// Process a block. `carrier_in`/`modulator_in` are the two audio
    /// inputs (the carrier is replaced by the internal oscillator when
    /// `carrier_shape != 0`). Writes `main` (the modulated output) and
    /// `aux` (the secondary/raw feed).
    pub fn process(
        &mut self,
        p: &Params,
        carrier_in: &[f32],
        modulator_in: &[f32],
        main: &mut [f32],
        aux: &mut [f32],
    ) {
        if p.frequency_shifter {
            self.process_easter_egg(p, carrier_in, modulator_in, main, aux);
            return;
        }
        let size = main.len();
        let vocoder_amount = ((p.algorithm - 0.7) * 20.0 + 0.5).clamp(0.0, 1.0);

        // channel amplifiers (the noise gate + overdrive)
        let mut carrier = vec![0.0_f32; size];
        let mut modulator = vec![0.0_f32; size];
        aux.iter_mut().for_each(|v| *v = 0.0);
        // channel 0 = carrier (skipped when an internal oscillator is used)
        if p.carrier_shape == 0 {
            self.amp[0].process(p.drive1, 1.0 - vocoder_amount, carrier_in, &mut carrier, aux);
        }
        self.amp[1].process(p.drive2, 1.0 - vocoder_amount, modulator_in, &mut modulator, aux);

        // internal oscillator replaces the carrier
        if p.carrier_shape != 0 {
            let shape = match p.carrier_shape {
                1 => OscShape::Sine,
                2 => OscShape::Triangle,
                3 => OscShape::Saw,
                4 => OscShape::Pulse,
                _ => OscShape::NoiseLp,
            };
            self.osc.render(shape, p.note, carrier_in, &mut carrier);
            for c in carrier.iter_mut() {
                *c *= 0.5;
            }
        }

        if vocoder_amount < 0.5 {
            // cross-modulation: morph between adjacent algorithms
            let algorithm = (p.algorithm * 8.0).min(5.999);
            let prev = (self.prev_algorithm * 8.0).min(5.999);
            let ai = algorithm.floor() as usize;
            let mut af = algorithm - ai as f32;
            let prev_i = prev.floor() as usize;
            let mut prev_f = prev - prev_i as f32;
            if ai != prev_i {
                prev_f = af;
            }
            let a = ALGO_ORDER[ai.min(5)];
            let b = ALGO_ORDER[(ai + 1).min(6)];
            let param0 = skewed_parameter(prev * 8.0 / 8.0, self.prev_timbre);
            let param1 = skewed_parameter(p.algorithm * 8.0, p.timbre);
            // 6× oversample the cross-modulation to suppress aliasing (the
            // firmware runs the xmod algorithms at kOscillatorSampleRate via a
            // polyphase SRC).
            let os_size = size * SRC_OVERSAMPLING;
            let mut up_c = [0.0f32; SRC_OVERSAMPLING];
            let mut up_m = [0.0f32; SRC_OVERSAMPLING];
            for i in 0..size {
                self.src_up[0].process(carrier[i], &mut up_c);
                self.src_up[1].process(modulator[i], &mut up_m);
                for p in 0..SRC_OVERSAMPLING {
                    let j = i * SRC_OVERSAMPLING + p;
                    let frac = j as f32 / os_size as f32;
                    let xfade = prev_f + (af - prev_f) * frac;
                    let param = param0 + (param1 - param0) * frac;
                    let ya = xmod(a, up_m[p], up_c[p], param);
                    let yb = xmod(b, up_m[p], up_c[p], param);
                    let os = ya + (yb - ya) * xfade;
                    if let Some(down) = self.src_down.process(os) {
                        main[i] = down;
                    }
                }
            }
            let _ = &mut af;
            // crossfade to raw modulator at the vocoder transition
            let transition = 2.0 * vocoder_amount;
            if transition != 0.0 {
                for i in 0..size {
                    main[i] += transition * (modulator[i] - main[i]);
                }
            }
        } else {
            let release_time = (4.0 * (p.algorithm - 0.75)).clamp(0.0, 1.0);
            self.vocoder.set_release_time(release_time * (2.0 - release_time));
            self.vocoder.set_formant_shift(p.timbre);
            self.vocoder.process(&modulator, &carrier, main);
            let transition = 2.0 * (1.0 - vocoder_amount);
            if transition != 0.0 {
                for i in 0..size {
                    main[i] += transition * (modulator[i] - main[i]);
                }
            }
        }
        self.prev_algorithm = p.algorithm;
        self.prev_timbre = p.timbre;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequency_shifter_easter_egg_shifts() {
        let mut m = Modulator::new(48000.0);
        let params = Params {
            algorithm: 0.3, // phase-shift / rotation amount
            timbre: 0.5,    // USB/LSB balance
            drive1: 0.0,    // no feedback
            drive2: 1.0,    // fully wet
            carrier_shape: 0,
            note: 60.0,
            frequency_shifter: true,
        };
        let n = 64;
        let mut energy = 0.0;
        for blk in 0..40 {
            let carrier: Vec<f32> =
                (0..n).map(|i| 0.5 * ((blk * n + i) as f32 * 0.20).sin()).collect();
            let modulator: Vec<f32> =
                (0..n).map(|i| 0.5 * ((blk * n + i) as f32 * 0.05).sin()).collect();
            let mut main = vec![0.0_f32; n];
            let mut aux = vec![0.0_f32; n];
            m.process(&params, &carrier, &modulator, &mut main, &mut aux);
            assert!(
                main.iter().chain(aux.iter()).all(|v| v.is_finite() && v.abs() <= 8.0),
                "freq shifter bounded"
            );
            if blk > 4 {
                energy += main.iter().map(|v| v * v).sum::<f32>();
            }
        }
        assert!(energy > 1e-5, "frequency shifter produces sound: {energy}");
    }

    #[test]
    fn oversampled_xmod_is_bounded_and_audible() {
        let mut m = Modulator::new(48000.0);
        // digital ring-mod region (algorithm ~0.45 < 0.5 = cross-mod, not vocoder)
        let params = Params {
            algorithm: 0.45,
            timbre: 0.6,
            drive1: 0.8,
            drive2: 0.8,
            carrier_shape: 0,
            note: 60.0,
            frequency_shifter: false,
        };
        let n = 64;
        let mut energy = 0.0;
        for blk in 0..40 {
            let carrier: Vec<f32> =
                (0..n).map(|i| 0.5 * ((blk * n + i) as f32 * 0.10).sin()).collect();
            let modulator: Vec<f32> =
                (0..n).map(|i| 0.5 * ((blk * n + i) as f32 * 0.013).sin()).collect();
            let mut main = vec![0.0_f32; n];
            let mut aux = vec![0.0_f32; n];
            m.process(&params, &carrier, &modulator, &mut main, &mut aux);
            assert!(main.iter().all(|v| v.is_finite() && v.abs() <= 8.0), "xmod bounded");
            if blk > 4 {
                energy += main.iter().map(|v| v * v).sum::<f32>();
            }
        }
        assert!(energy > 1e-4, "oversampled xmod produces sound: {energy}");
    }

    #[test]
    fn fold_table_loads_and_is_centred() {
        let t = tables();
        assert_eq!(t.bipolar_fold.len(), 4097);
        // a zero input through the fold sits near zero
        let y = xmod(Algorithm::Fold, 0.0, 0.0, 0.5);
        assert!(y.abs() < 0.2, "fold of zero ~ zero: {y}");
    }

    #[test]
    fn xfade_crosses_between_inputs() {
        // parameter 0 → all x2 (fade_out), parameter 1 → all x1 (fade_in)
        let lo = xmod(Algorithm::Xfade, 1.0, -1.0, 0.0);
        let hi = xmod(Algorithm::Xfade, 1.0, -1.0, 1.0);
        assert!(lo < -0.5, "param 0 favors x2: {lo}");
        assert!(hi > 0.5, "param 1 favors x1: {hi}");
    }

    #[test]
    fn ring_mod_multiplies() {
        // digital ring mod of two sines produces sum/difference content;
        // dc-free sines multiplied give zero mean but nonzero energy
        let mut energy = 0.0;
        for i in 0..1000 {
            let c = (i as f32 * 0.05).sin();
            let m = (i as f32 * 0.17).sin();
            let y = xmod(Algorithm::DigitalRing, m, c, 0.3);
            energy += y * y;
            assert!(y.abs() <= 1.0, "bounded: {y}");
        }
        assert!(energy > 1.0, "ring mod produces output");
    }

    #[test]
    fn analog_ring_is_bounded() {
        for i in 0..2000 {
            let c = (i as f32 * 0.03).sin() * 0.9;
            let m = (i as f32 * 0.11).sin() * 0.9;
            let y = xmod(Algorithm::AnalogRing, m, c, 0.8);
            // SoftLimit saturates softly (not a hard clip) — bounded but
            // can exceed 1; the module's final Clip16 hard-limits
            assert!(y.is_finite() && y.abs() <= 4.0, "{y}");
        }
    }

    #[test]
    fn xor_is_bitwise_and_bounded() {
        for i in 0..500 {
            let a = (i as f32 * 0.07).sin() * 0.8;
            let b = (i as f32 * 0.13).cos() * 0.8;
            let y = xmod(Algorithm::Xor, a, b, 1.0);
            assert!(y.is_finite() && y.abs() <= 2.0, "{y}");
        }
    }

    #[test]
    fn saturating_amplifier_gates_silence() {
        let mut amp = SaturatingAmplifier::default();
        let quiet = vec![0.0001_f32; 96];
        let mut out = vec![0.0; 96];
        let mut raw = vec![0.0; 96];
        // run several blocks so the gate level settles
        for _ in 0..50 {
            amp.process(0.5, 1.0, &quiet, &mut out, &mut raw);
        }
        let peak = out.iter().fold(0.0_f32, |m, v| m.max(v.abs()));
        assert!(peak < 0.05, "noise gate squashes quiet input: {peak}");
    }

    #[test]
    fn saturating_amplifier_passes_signal() {
        let mut amp = SaturatingAmplifier::default();
        let loud: Vec<f32> = (0..96).map(|i| (i as f32 * 0.3).sin() * 0.7).collect();
        let mut out = vec![0.0; 96];
        let mut raw = vec![0.0; 96];
        for _ in 0..20 {
            amp.process(0.5, 1.0, &loud, &mut out, &mut raw);
        }
        let peak = out.iter().fold(0.0_f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.2, "loud signal passes: {peak}");
    }

    #[test]
    fn skew_is_identity_at_edges() {
        // at parameter extremes the skew preserves 0 and 1
        assert!((skewed_parameter(0.5, 0.0)).abs() < 1e-6);
        assert!((skewed_parameter(0.5, 1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn modulator_processes_across_the_sweep() {
        // sweep the algorithm knob; output stays finite and the ring-mod
        // region produces real modulation of two tones
        let mut m = Modulator::new(48_000.0);
        let carrier: Vec<f32> = (0..96).map(|i| (i as f32 * 0.2).sin() * 0.6).collect();
        let modulator: Vec<f32> = (0..96).map(|i| (i as f32 * 0.31).sin() * 0.6).collect();
        let mut main = vec![0.0; 96];
        let mut aux = vec![0.0; 96];
        for algo in [0.0, 0.15, 0.3, 0.5, 0.65, 0.85, 1.0] {
            let p = Params {
                algorithm: algo,
                timbre: 0.6,
                drive1: 0.6,
                drive2: 0.6,
                carrier_shape: 0,
                note: 48.0,
            frequency_shifter: false,
        };
            // run a few blocks so the amp gates settle
            for _ in 0..30 {
                m.process(&p, &carrier, &modulator, &mut main, &mut aux);
            }
            assert!(main.iter().all(|v| v.is_finite()), "finite at algo {algo}");
            let energy: f32 = main.iter().map(|v| v * v).sum();
            assert!(energy > 0.0, "output present at algo {algo}");
        }
    }

    #[test]
    fn internal_oscillator_self_carries() {
        // carrier_shape != 0 → the internal oscillator is the carrier,
        // so even with a silent carrier input there's a modulated tone
        let mut m = Modulator::new(48_000.0);
        let silent = vec![0.0_f32; 96];
        let modulator: Vec<f32> = (0..96).map(|i| (i as f32 * 0.3).sin() * 0.6).collect();
        let mut main = vec![0.0; 96];
        let mut aux = vec![0.0; 96];
        let p = Params {
            algorithm: 0.25,
            timbre: 0.6,
            drive1: 0.5,
            drive2: 0.6,
            carrier_shape: 1, // sine
            note: 50.0,
            frequency_shifter: false,
        };
        for _ in 0..30 {
            m.process(&p, &silent, &modulator, &mut main, &mut aux);
        }
        let energy: f32 = main.iter().map(|v| v * v).sum();
        assert!(energy > 0.001, "internal carrier produces output: {energy}");
    }

    #[test]
    fn vocoder_imposes_envelope() {
        // carrier = steady tone, modulator = gated tone; output should
        // follow the modulator's envelope
        let mut voc = Vocoder::new(48_000.0);
        voc.set_release_time(0.3);
        let n = 4096;
        let carrier: Vec<f32> = (0..n).map(|i| (i as f32 * 0.2).sin() * 0.6).collect();
        let modulator: Vec<f32> = (0..n)
            .map(|i| {
                let gate = if (i / 512) % 2 == 0 { 1.0 } else { 0.0 };
                (i as f32 * 0.25).sin() * 0.6 * gate
            })
            .collect();
        let mut out = vec![0.0; n];
        voc.process(&modulator, &carrier, &mut out);
        let on: f32 = out[200..500].iter().map(|v| v * v).sum();
        let off: f32 = out[700..1000].iter().map(|v| v * v).sum();
        assert!(on > off, "voiced louder than gated-off: {on} vs {off}");
    }
}
