//! # Streams engines — the six dynamics processors
//!
//! Ported from pichenettes/eurorack (streams/*, MIT, copyright 2014
//! Emilie Gillet, attribution preserved), fixed-point faithful with
//! i64-widened products: the multistage envelope with excite-driven
//! rate modulation and gate-level distortion, the vactrol model
//! (both the smooth hysteresis machine with its sensitized state and
//! the plucked dual-decay mode through the Gompertz waveshaper), the
//! three-band peak follower with spectral centroid, the compressor
//! (log2/exp2 fixed-point, soft knee, adaptive makeup), the filter
//! controller, and the Lorenz generator (24-bit fixed-point
//! attractor).
//!
//! Each processor maps `(audio, excite) → (gain dac, frequency cv)`
//! exactly like the firmware; the module shell applies the gain to
//! audio and publishes both CVs on the bus.

#![allow(clippy::excessive_precision)]
// The firmware's fixed-point idiom is `a + (b * c >> s)` everywhere.
#![allow(clippy::precedence)]

use std::sync::OnceLock;

pub const NATIVE_SR: f64 = 31_250.0; // streams' control rate (~31.25 kHz)
pub const SCHMITT_THRESHOLD: i32 = 32768 * 5 * 2 / 3 / 8;
pub const UNITY_GAIN: i32 = 32767;
pub const ABOVE_UNITY_GAIN: i32 = 32896;
pub const MAX_EXPONENTIAL_GAIN: i32 = 218_453;

pub struct Tables {
    pub lp_coefficients: Vec<u32>,
    pub env_increments: Vec<u32>,
    pub env_linear: Vec<u16>,
    pub env_expo: Vec<u16>,
    pub env_quartic: Vec<u16>,
    pub square_root: Vec<u16>,
    pub gompertz: Vec<i16>,
    pub log2: Vec<u32>,
    pub exp2: Vec<u32>,
    pub compressor_ratio: Vec<u16>,
    pub soft_knee: Vec<u16>,
    pub lorenz_rate: Vec<u32>,
}

static TABLES: OnceLock<Tables> = OnceLock::new();

pub fn tables() -> &'static Tables {
    TABLES.get_or_init(build_tables)
}

fn build_tables() -> Tables {
    let excursion = 4_294_967_296.0_f64;
    // vactrol time constants: 0.1 ms .. 100 s, 5 decades over 640
    let lp_coefficients: Vec<u32> = (0..640)
        .map(|i| {
            let mut time = 0.001 * 10.0_f64.powf(i as f64 / 128.0);
            time = match i {
                0 => 0.0001,
                1 => 0.0002,
                2 => 0.0005,
                3 => 0.001,
                _ => time,
            };
            let c = 1.0 - (-1.0 / (time * NATIVE_SR)).exp();
            (excursion / 2.0 * c) as u32
        })
        .collect();
    // envelope increments (same gamma law family as peaks at this rate)
    let gamma = 0.175_f64;
    let min_inc = excursion / (8.0 * NATIVE_SR);
    let max_inc = excursion / (0.0005 * NATIVE_SR);
    let env_increments: Vec<u32> = (0..257)
        .map(|i| {
            let t = i as f64 / 256.0;
            let r = max_inc.powf(-gamma) + (min_inc.powf(-gamma) - max_inc.powf(-gamma)) * t;
            r.powf(-1.0 / gamma) as u32
        })
        .collect();
    let lin: Vec<f64> = (0..257).map(|i| (i.min(255)) as f64 / 256.0).collect();
    let env_linear: Vec<u16> = lin.iter().map(|&x| (x / lin[256] * 65535.0) as u16).collect();
    let qmax = lin[256].powf(3.32);
    let env_quartic: Vec<u16> = lin
        .iter()
        .map(|&x| (x.powf(3.32) / qmax * 65535.0) as u16)
        .collect();
    let emax = 1.0 - (-4.0 * lin[256]).exp();
    let env_expo: Vec<u16> = lin
        .iter()
        .map(|&x| ((1.0 - (-4.0 * x).exp()) / emax * 65535.0) as u16)
        .collect();
    let square_root: Vec<u16> = lin
        .iter()
        .map(|&x| ((x / lin[256]).sqrt() * 65535.0) as u16)
        .collect();
    // gompertz: exp(-10 exp(-13x)) + slope, normalized (waveforms.py)
    let raw: Vec<f64> = (0..1025)
        .map(|i| {
            let x = i as f64 / 1024.0;
            let g = (-10.0 * (-13.0 * x).exp()).exp();
            g + 0.1 * g.powf(0.05)
        })
        .collect();
    let min = raw.iter().cloned().fold(f64::MAX, f64::min);
    let shifted: Vec<f64> = raw.iter().map(|v| v - min).collect();
    let max = shifted.iter().cloned().fold(f64::MIN, f64::max);
    let gompertz: Vec<i16> = shifted
        .iter()
        .map(|v| (32767.0 * v / max) as i16)
        .collect();
    let log2: Vec<u32> = (0..257)
        .map(|i| {
            let t = i as f64 / 256.0;
            (65536.0 * (256.0 + 256.0 * t).log2()) as u32
        })
        .collect();
    let exp2: Vec<u32> = (0..257)
        .map(|i| {
            let t = i as f64 / 256.0;
            (65536.0 * 2.0_f64.powf(t)) as u32
        })
        .collect();
    let compressor_ratio: Vec<u16> = (0..257)
        .map(|i| {
            let t = i as f64 / 256.0;
            (256.0 / (24.0 * t * t + 1.0)) as u16
        })
        .collect();
    let soft_knee: Vec<u16> = (0..257)
        .map(|i| {
            let t = i as f64 / 256.0;
            (t.powi(3) * 65535.0) as u16
        })
        .collect();
    let lorenz_rate: Vec<u32> = (0..257)
        .map(|i| {
            let mut t = i as f64 / 256.0;
            t /= 20.0 / 100.0 / 3.3;
            let f = 2.0_f64.powf(t);
            let fmax = 2.0_f64.powf((256.0 / 256.0) / (20.0 / 100.0 / 3.3));
            (f / fmax * 0.02 * (1 << 24) as f64) as u32
        })
        .collect();
    Tables {
        lp_coefficients,
        env_increments,
        env_linear,
        env_expo,
        env_quartic,
        square_root,
        gompertz,
        log2,
        exp2,
        compressor_ratio,
        soft_knee,
        lorenz_rate,
    }
}

#[inline]
fn interp824_u16(table: &[u16], phase: u32) -> u16 {
    let i = (phase >> 24) as usize;
    let a = table[i.min(table.len() - 1)] as i64;
    let b = table[(i + 1).min(table.len() - 1)] as i64;
    (a + ((b - a) * ((phase >> 8) & 0xffff) as i64 >> 16)) as u16
}

#[inline]
fn interp1022_i16(table: &[i16], phase: u32) -> i16 {
    let i = (phase >> 22) as usize;
    let a = table[i.min(table.len() - 1)] as i32;
    let b = table[(i + 1).min(table.len() - 1)] as i32;
    (a + ((b - a) * ((phase >> 6) & 0xffff) as i32 >> 16)) as i16
}

/// meta_parameters.h ComputeAmountOffset.
pub fn compute_amount_offset(value: i32) -> (i32, i32) {
    if value < 32768 {
        let v = 32767 - value;
        let v = v * v >> 15;
        ((32767 - v) << 1, 0)
    } else {
        (65535 - ((value - 32768) << 1), (value - 32768) << 1)
    }
}

/// meta_parameters.h ComputeAttackDecay.
pub fn compute_attack_decay(shape: i32) -> (u16, u16) {
    if shape < 32768 {
        (0, (13 * (shape >> 3) + 12288).min(65535) as u16)
    } else if shape < 49152 {
        (
            ((shape - 32768) << 1).min(65535) as u16,
            (65535 - ((shape - 32768) >> 1) * 3).max(0) as u16,
        )
    } else {
        (
            (32768 - ((shape - 49152) >> 2) * 5).max(0) as u16,
            (65535 - ((shape - 32768) >> 1) * 3).max(0) as u16,
        )
    }
}

/// Common processor surface: (audio, excite) → (gain, frequency).
pub trait Processor {
    fn process(&mut self, audio: i16, excite: i16) -> (u16, u16);
}

// ── envelope ───────────────────────────────────────────────────────────────

pub struct Envelope {
    level: [i32; 4],
    time: [u16; 3],
    shape: [usize; 3], // 0 linear, 1 expo, 2 quartic
    segment: usize,
    num_segments: usize,
    sustain_point: usize,
    phase: u32,
    phase_increment: u32,
    start_value: i32,
    value: i32,
    rate_modulation: i32,
    gate_level: i32,
    gate: bool,
    hard_reset: bool,
    pub frequency_amount: i32,
    pub frequency_offset: i32,
    target_frequency_amount: i32,
    target_frequency_offset: i32,
}

impl Envelope {
    pub fn new() -> Self {
        let mut e = Envelope {
            level: [0; 4],
            time: [0; 3],
            shape: [0; 3],
            segment: 0,
            num_segments: 0,
            sustain_point: 0,
            phase: 0,
            phase_increment: 0,
            start_value: 0,
            value: 0,
            rate_modulation: 0,
            gate_level: 0,
            gate: false,
            hard_reset: false,
            frequency_amount: 0,
            frequency_offset: 0,
            target_frequency_amount: 0,
            target_frequency_offset: 0,
        };
        e.set_ad(0, 8192);
        e.segment = e.num_segments;
        e
    }

    pub fn set_ad(&mut self, attack: u16, decay: u16) {
        self.num_segments = 2;
        self.sustain_point = 0;
        self.level = [0, 32767, 0, 0];
        self.time = [attack, decay, 0];
        self.shape = [0, 1, 0];
    }

    pub fn set_ar(&mut self, attack: u16, release: u16) {
        self.num_segments = 2;
        self.sustain_point = 1;
        self.level = [0, 32767, 0, 0];
        self.time = [attack, release, 0];
        self.shape = [0, 1, 0];
    }

    /// shape knob → AD/AR config (alternate = AR), per Configure.
    pub fn configure(&mut self, alternate: bool, shape: i32, amount: i32) {
        let (a, d) = compute_attack_decay(shape);
        if alternate {
            self.set_ar(a, d);
        } else {
            self.set_ad(a, d);
        }
        self.hard_reset = true;
        let (am, off) = compute_amount_offset(amount);
        self.target_frequency_amount = am;
        self.target_frequency_offset = off;
    }
}

impl Default for Envelope {
    fn default() -> Self {
        Self::new()
    }
}

impl Processor for Envelope {
    fn process(&mut self, _audio: i16, excite: i16) -> (u16, u16) {
        let t = tables();
        self.frequency_amount += (self.target_frequency_amount - self.frequency_amount) >> 8;
        self.frequency_offset += (self.target_frequency_offset - self.frequency_offset) >> 8;

        let excite = excite as i32;
        let mut trigger = false;
        if !self.gate {
            if excite > SCHMITT_THRESHOLD {
                trigger = true;
                self.gate = true;
                self.hard_reset = false;
            }
        } else if excite < (SCHMITT_THRESHOLD >> 1) {
            self.gate = false;
        } else {
            self.gate_level += (excite - self.gate_level) >> 8;
        }

        if trigger {
            self.start_value = if self.segment == self.num_segments || self.hard_reset {
                self.level[0]
            } else {
                self.value
            };
            self.segment = 0;
            self.phase = 0;
        } else if self.phase < self.phase_increment {
            self.start_value = self.level[(self.segment + 1).min(3)];
            self.segment += 1;
            self.phase = 0;
        }
        let done = self.segment >= self.num_segments;
        let sustained =
            self.sustain_point != 0 && self.segment == self.sustain_point && self.gate;
        let mut increment: u32 = if sustained || done {
            0
        } else {
            t.env_increments[(self.time[self.segment.min(2)] >> 8) as usize]
        };
        // rate modulation by the excitation pulse
        self.rate_modulation +=
            ((if excite > SCHMITT_THRESHOLD { excite } else { 0 }) - self.rate_modulation) >> 12;
        increment =
            increment.wrapping_add(((increment >> 7) as i64 * (self.rate_modulation >> 7) as i64) as u32);
        self.phase_increment = increment;

        let a = self.start_value;
        let b = self.level[(self.segment + 1).min(3)];
        let curve = match self.shape[self.segment.min(2)] {
            1 => &t.env_expo,
            2 => &t.env_quartic,
            _ => &t.env_linear,
        };
        let tt = interp824_u16(curve, self.phase) as i32;
        self.value = a + ((b - a) * (tt >> 1) >> 15);
        self.phase = self.phase.wrapping_add(self.phase_increment);

        // level-dependent distortion toward the gate level
        let mut compressed = 32767 - ((32767 - self.value) as i64 * (32767 - self.value) as i64 >> 15) as i32;
        compressed = 32767 - ((32767 - compressed) as i64 * (32767 - compressed) as i64 >> 15) as i32;
        let scaled = self.value + ((compressed - self.value) as i64 * self.gate_level as i64 >> 15) as i32;
        let scaled = (scaled as i64 * (28672 + (self.gate_level >> 3)) as i64 >> 15) as i32;
        let gain = (scaled as i64 * ABOVE_UNITY_GAIN as i64 >> 15).clamp(0, 65535) as u16;
        let frequency = (self.frequency_offset as i64
            + (scaled as i64 * self.frequency_amount as i64 >> 15))
            .clamp(0, 65535) as u16;
        (gain, frequency)
    }
}

// ── vactrol ────────────────────────────────────────────────────────────────

pub struct Vactrol {
    state: [i64; 4],
    excite_state: i64,
    gate: bool,
    pub plucked: bool,
    attack_coefficient: i64,
    fast_attack_coefficient: i64,
    decay_coefficient: i64,
    fast_decay_coefficient: i64,
    frequency_amount: i32,
    frequency_offset: i32,
    target_frequency_amount: i32,
    target_frequency_offset: i32,
}

impl Vactrol {
    pub fn new() -> Self {
        Vactrol {
            state: [0; 4],
            excite_state: 0,
            gate: false,
            plucked: false,
            attack_coefficient: 0,
            fast_attack_coefficient: 0,
            decay_coefficient: 0,
            fast_decay_coefficient: 0,
            frequency_amount: 0,
            frequency_offset: 0,
            target_frequency_amount: 0,
            target_frequency_offset: 0,
        }
    }

    /// vactrol.h Configure: shape → attack/decay times (the three
    /// regions), plucked = alternate.
    pub fn configure(&mut self, alternate: bool, shape: i32, amount: i32) {
        let t = tables();
        let (attack_time, decay_time): (i32, i32) = if shape < 32768 {
            (128, 227 + (shape * 196 >> 15))
        } else if shape < 49512 {
            let s = shape - 32768;
            (128 + (s * 227 >> 15), 423 - (89 * s >> 15))
        } else {
            let s = shape - 49512;
            (355 - (s >> 7), 384 - (128 * s >> 15))
        };
        let idx = |i: i32| t.lp_coefficients[(i.clamp(0, 639)) as usize] as i64;
        self.attack_coefficient = idx(attack_time);
        self.fast_attack_coefficient = idx(attack_time - 128);
        self.decay_coefficient = idx(decay_time);
        self.fast_decay_coefficient = idx(decay_time - 128);
        self.plucked = alternate;
        if alternate {
            // <<4 like upstream, saturated at the int32 ceiling so the
            // error*coefficient products stay inside i64
            self.fast_attack_coefficient =
                (self.fast_attack_coefficient << 4).min((1_i64 << 31) - 1);
        } else {
            self.decay_coefficient >>= 1;
        }
        let (am, off) = compute_amount_offset(amount);
        self.target_frequency_amount = am;
        self.target_frequency_offset = off;
    }
}

impl Default for Vactrol {
    fn default() -> Self {
        Self::new()
    }
}

impl Processor for Vactrol {
    fn process(&mut self, _audio: i16, excite: i16) -> (u16, u16) {
        let t = tables();
        self.frequency_amount += (self.target_frequency_amount - self.frequency_amount) >> 8;
        self.frequency_offset += (self.target_frequency_offset - self.frequency_offset) >> 8;
        let excite = (excite.max(0)) as i64;

        if self.plucked {
            if !self.gate {
                if excite > SCHMITT_THRESHOLD as i64 {
                    self.gate = true;
                    self.state[0] = (32767_i64) << 16;
                    self.state[1] = (32767_i64) << 16;
                }
            } else if excite < (SCHMITT_THRESHOLD >> 1) as i64 {
                self.gate = false;
            }
            self.state[0] -= self.state[0] * self.fast_decay_coefficient >> 31;
            self.state[1] -= self.state[1] * self.decay_coefficient >> 31;
            let error = self.state[0] - self.state[2];
            let coefficient = if error > 0 {
                self.fast_attack_coefficient
            } else {
                self.fast_decay_coefficient
            };
            self.state[2] += error * coefficient >> 31;
            let error = self.state[1] - self.state[3];
            let coefficient = if error > 0 {
                self.fast_attack_coefficient
            } else {
                self.decay_coefficient
            };
            let strength = error.abs();
            let coefficient = (coefficient >> 1) + (coefficient * strength >> 31);
            self.state[3] += error * coefficient >> 31;
            let vcf_amount = (self.state[2] >> 16).clamp(0, 65535) as i64;
            let g_phase = ((self.state[3] >> 2) * 3).clamp(0, u32::MAX as i64) as u32;
            let vca_amount = interp1022_i16(&t.gompertz, g_phase).max(0) as i64;
            let gain = (ABOVE_UNITY_GAIN as i64 * vca_amount >> 15).clamp(0, 65535) as u16;
            let frequency = (self.frequency_offset as i64
                + (self.frequency_amount as i64 * vcf_amount >> 15))
                .clamp(0, 65535) as u16;
            return (gain, frequency);
        }

        // smooth mode: the hysteresis machine
        let error = excite - self.excite_state;
        let coefficient = if error > 0 {
            1_i64 << 30
        } else {
            self.decay_coefficient << 1
        };
        self.excite_state += error * coefficient >> 31;
        let excite = self.excite_state;

        let mut input: i64 = self.frequency_offset as i64;
        input += (self.frequency_amount >> 1) as i64;
        input = (65535 + input) >> 1;
        input *= excite;
        self.state[3] += (input - self.state[3]) * 67_976_239 >> 31;
        let error = input - self.state[0];
        let coefficient = if error > 0 {
            if self.state[1] > 0 {
                let c = self.attack_coefficient;
                c + (c * (255 - (self.state[2] >> 23)).max(0) >> 6)
            } else {
                self.fast_attack_coefficient
            }
        } else if self.state[1] < 0 {
            self.decay_coefficient
        } else {
            self.fast_decay_coefficient
        };
        self.state[0] += error * coefficient >> 31;
        self.state[1] += (error - self.state[1]) * coefficient >> 31;
        let sensitivity = if self.state[0] > (1 << 28) {
            1_i64 << 31
        } else {
            self.state[0] << 3
        };
        let error = sensitivity - self.state[2];
        if error > 0 {
            self.state[2] += error * 138_132 >> 31; // sensitize in ~1 s
        } else {
            self.state[2] += error * 1151 >> 31; // desensitize in ~60 s
        }
        let mut index = self.state[0] >> 1;
        index += (self.state[3] >> 15) * (self.state[1] >> 15) >> 1; // overshoot
        let index = index.clamp(0, (1_i64 << 30) - 1);
        let amplitude = if index < 536_870_912 {
            interp1022_i16(&t.gompertz, (index as u32) << 3).max(0) as i64
        } else {
            32767
        };
        let mut cutoff = (index >> 14).min(32767);
        cutoff = cutoff * cutoff >> 15;
        let gain = (ABOVE_UNITY_GAIN as i64 * amplitude >> 15).clamp(0, 65535) as u16;
        let frequency = (self.frequency_offset as i64
            + (self.frequency_amount as i64 * cutoff >> 15))
            .clamp(0, 65535) as u16;
        (gain, frequency)
    }
}

// ── follower ───────────────────────────────────────────────────────────────

/// Chamberlin SVF, the streams flavor (svf.cc) for the band split.
#[derive(Debug, Clone, Default)]
struct BandSvf {
    f: i32,
    damp: i32,
    lp: i32,
    bp: i32,
    hp: i32,
}

impl BandSvf {
    fn set_frequency(&mut self, code: i32) {
        // svf_cutoff law: 2 sin(pi f) over the 0..127.5 semitone code
        let cutoff = 440.0 * 2.0_f64.powf(((code >> 7) as f64 - 69.0) / 12.0);
        let f = (cutoff / NATIVE_SR).min(0.125);
        self.f = ((2.0 * (std::f64::consts::PI * f).sin()) * 32767.0) as i32;
        self.damp = 32767; // resonance 0 -> damp ~2.0 (q=0.5)
    }

    #[inline]
    fn process(&mut self, input: i32) {
        let notch = input - (self.bp * self.damp >> 15);
        self.lp += self.f * self.bp >> 15;
        self.lp = self.lp.clamp(-32768, 32767);
        self.hp = notch - self.lp;
        self.bp += self.f * self.hp >> 15;
        self.bp = self.bp.clamp(-32768, 32767);
    }
}

pub struct Follower {
    analysis_low: BandSvf,
    analysis_medium: BandSvf,
    energy: [[i64; 2]; 3],
    follower: [i64; 3],
    follower_lp: [i64; 3],
    spectrum: [i64; 3],
    centroid: i32,
    pub only_filter: bool,
    attack_coefficient: [i64; 3],
    decay_coefficient: [i64; 3],
    frequency_amount: i32,
    frequency_offset: i32,
    target_frequency_amount: i32,
    target_frequency_offset: i32,
}

impl Follower {
    pub fn new() -> Self {
        let mut f = Follower {
            analysis_low: BandSvf::default(),
            analysis_medium: BandSvf::default(),
            energy: [[0; 2]; 3],
            follower: [0; 3],
            follower_lp: [0; 3],
            spectrum: [0; 3],
            centroid: 0,
            only_filter: false,
            attack_coefficient: [0; 3],
            decay_coefficient: [0; 3],
            frequency_amount: 0,
            frequency_offset: 0,
            target_frequency_amount: 0,
            target_frequency_offset: 0,
        };
        f.analysis_low.set_frequency(45 << 7);
        f.analysis_medium.set_frequency(86 << 7);
        f
    }

    /// follower.h Configure: shape → attack/decay per band.
    pub fn configure(&mut self, alternate: bool, shape: i32, amount: i32) {
        let t = tables();
        let (attack_time, decay_time): (i32, i32) = if shape < 32768 {
            ((shape * 39 >> 15), 128 + (shape * 128 >> 15))
        } else {
            let s = shape - 32768;
            (39 + (s * 89 >> 15), 256 + (s * 89 >> 15))
        };
        for i in 0..3 {
            let band_shift = (i as i32) * 13;
            let a = (attack_time - band_shift).clamp(0, 639);
            let d = (decay_time - band_shift).clamp(0, 639);
            self.attack_coefficient[i] = t.lp_coefficients[a as usize] as i64;
            self.decay_coefficient[i] = t.lp_coefficients[d as usize] as i64;
        }
        self.only_filter = alternate;
        let (am, off) = compute_amount_offset(amount);
        self.target_frequency_amount = am;
        self.target_frequency_offset = off;
    }
}

impl Default for Follower {
    fn default() -> Self {
        Self::new()
    }
}

impl Processor for Follower {
    fn process(&mut self, _audio: i16, excite: i16) -> (u16, u16) {
        let t = tables();
        self.frequency_amount += (self.target_frequency_amount - self.frequency_amount) >> 8;
        self.frequency_offset += (self.target_frequency_offset - self.frequency_offset) >> 8;

        self.analysis_low.process(excite as i32);
        self.analysis_medium.process(self.analysis_low.hp);
        let channel = [
            self.analysis_low.lp as i64,
            self.analysis_medium.lp as i64,
            self.analysis_medium.hp as i64,
        ];
        let mut envelope: i64 = 0;
        let mut centroid_numerator: i64 = 0;
        let mut centroid_denominator: i64 = 0;
        for i in 0..3 {
            let energy = channel[i] * channel[i];
            if self.energy[i][0] < self.energy[i][1]
                && self.energy[i][1] < energy
                && energy > self.follower[i]
            {
                self.follower[i] = energy;
            }
            if self.energy[i][0] <= self.energy[i][1] && self.energy[i][1] >= energy {
                self.follower[i] = self.energy[i][1];
            }
            self.energy[i][0] = self.energy[i][1];
            self.energy[i][1] = energy;
            let error = self.follower[i] - self.follower_lp[i];
            if error > 0 {
                self.follower_lp[i] += error * self.attack_coefficient[i] >> 31;
            } else {
                self.follower_lp[i] += error * self.decay_coefficient[i] >> 31;
            }
            envelope += self.follower_lp[i] >> 13;
            if self.only_filter {
                let error = self.follower_lp[i] - self.spectrum[i];
                self.spectrum[i] += error >> 6;
            } else {
                let error = self.follower[i] - self.spectrum[i];
                self.spectrum[i] += error >> 10;
            }
            centroid_numerator += (i as i64) * (self.spectrum[i] >> 1) >> 16;
            centroid_denominator += self.spectrum[i] >> 16;
        }
        let envelope = envelope.clamp(0, 65535) as u32;
        let gain_mod = (interp824_u16(&t.square_root, envelope << 16) >> 1) as i32;
        let centroid = ((centroid_numerator << 15) / (centroid_denominator + 1)) as i32;
        if gain_mod > 4096 {
            self.centroid = centroid;
        } else if gain_mod > 2048 {
            self.centroid += (centroid - self.centroid) >> 8;
        }
        let mut gain = (gain_mod as i64 * UNITY_GAIN as i64 >> 15).clamp(0, 65535) as u16;
        let mut frequency = (self.frequency_offset as i64
            + (self.centroid as i64 * self.frequency_amount as i64 >> 15))
            .clamp(0, 65535) as u16;
        if self.only_filter {
            gain = frequency;
            frequency = 65535;
        }
        (gain, frequency)
    }
}

// ── compressor ─────────────────────────────────────────────────────────────

pub struct Compressor {
    detector: i64,
    sidechain_signal_detector: i64,
    attack_coefficient: i64,
    decay_coefficient: i64,
    threshold: i32,
    ratio: i32,
    makeup_gain: i32,
    soft_knee: bool,
    pub gain_reduction: i32,
}

impl Compressor {
    pub fn new() -> Self {
        Compressor {
            detector: 0,
            sidechain_signal_detector: 0,
            attack_coefficient: -1,
            decay_coefficient: 0,
            threshold: 0,
            ratio: 256,
            makeup_gain: 0,
            soft_knee: false,
            gain_reduction: 0,
        }
    }

    fn log2_fp(value: i64) -> i32 {
        let t = tables();
        let mut value = value.max(1);
        let mut log_value: i32 = 0;
        while value >= 512 {
            value >>= 1;
            log_value += 65536;
        }
        while value < 256 {
            value <<= 1;
            log_value -= 65536;
        }
        log_value + t.log2[(value - 256) as usize] as i32 - t.log2[0] as i32 + 0
    }

    fn compress(squared_level: i64, threshold: i32, ratio: i32, soft: bool) -> i32 {
        let t = tables();
        let level = (Self::log2_fp(squared_level) >> 1) - 15 * 65536;
        let position = level - threshold;
        if position < 0 {
            return 0;
        }
        let mut attenuation = position - (position * ratio >> 8);
        if attenuation < 65535 && soft {
            let i = (attenuation >> 8).clamp(0, 255) as usize;
            let a = t.soft_knee[i] as i32;
            let b = t.soft_knee[i + 1] as i32;
            let sk = a + ((b - a) * (attenuation & 0xff) >> 8);
            attenuation += ((sk - attenuation) as i64 * ((65535 - attenuation) >> 1) as i64 >> 15) as i32;
        }
        -attenuation
    }

    /// compressor.h Configure (the panel mapping, alternate = soft knee).
    pub fn configure(&mut self, alternate: bool, threshold_knob: i32, amount: i32) {
        let t = tables();
        let attack_time: i32 = if !alternate { 1 } else { 40 };
        let decay_time: i32 = if !alternate { 279 } else { 236 };
        self.attack_coefficient = t.lp_coefficients[attack_time.clamp(0, 639) as usize] as i64;
        self.decay_coefficient = t.lp_coefficients[decay_time.clamp(0, 639) as usize] as i64;
        self.soft_knee = alternate;
        self.threshold = (-1280 + 5 * (threshold_knob >> 8)) << 8;
        if amount < 32768 {
            self.ratio = t.compressor_ratio[((32767 - amount) >> 7).clamp(0, 256) as usize] as i32;
            self.makeup_gain = 0;
        } else {
            let amount = amount - 32768;
            self.ratio = t.compressor_ratio[0] as i32;
            self.makeup_gain = amount * (MAX_EXPONENTIAL_GAIN >> 8) >> 7;
            let knee_gain = self.threshold + self.makeup_gain;
            if knee_gain >= 0 {
                self.makeup_gain -= knee_gain;
            }
        }
    }
}

impl Default for Compressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Processor for Compressor {
    fn process(&mut self, audio: i16, excite: i16) -> (u16, u16) {
        const GAIN_CONSTANT: i64 =
            (1.0 / (1.55 / 6.0 * 65536.0 / 256.0) * 65536.0) as i64;
        let mut energy: i64 = excite as i64;
        energy *= energy;
        let error = energy - self.sidechain_signal_detector;
        if error > 0 {
            self.sidechain_signal_detector += error;
        } else {
            self.sidechain_signal_detector += error * 14174 >> 31;
        }
        if self.sidechain_signal_detector < 1024 * 1024 {
            energy = audio as i64;
            energy *= energy;
        }
        let error = energy - self.detector;
        if error > 0 {
            if self.attack_coefficient == -1 {
                self.detector += error;
            } else {
                self.detector += error * self.attack_coefficient >> 31;
            }
        } else {
            self.detector += error * self.decay_coefficient >> 31;
        }
        let g = Self::compress(self.detector, self.threshold, self.ratio, self.soft_knee);
        self.gain_reduction = g >> 3;
        let g = (UNITY_GAIN as i64 + ((g + self.makeup_gain) as i64 * GAIN_CONSTANT >> 16))
            .clamp(0, 65535) as u16;
        (g, 65535)
    }
}

// ── filter controller ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct FilterController {
    frequency_amount: i32,
    frequency_offset: i32,
    target_frequency_amount: i32,
    target_frequency_offset: i32,
}

impl FilterController {
    pub fn configure(&mut self, offset: i32, amount_knob: i32) {
        let mut amount = amount_knob - 32768;
        amount = amount * amount >> 15;
        self.target_frequency_amount = if amount_knob < 32768 { -amount } else { amount };
        self.target_frequency_offset = offset;
    }
}

impl Processor for FilterController {
    fn process(&mut self, _audio: i16, excite: i16) -> (u16, u16) {
        self.frequency_amount += (self.target_frequency_amount - self.frequency_amount) >> 8;
        self.frequency_offset += (self.target_frequency_offset - self.frequency_offset) >> 8;
        let f = (self.frequency_offset + ((excite as i32) * self.frequency_amount >> 14))
            .clamp(0, 65535) as u16;
        (0, f)
    }
}

// ── lorenz generator ───────────────────────────────────────────────────────

pub struct LorenzGenerator {
    x: i64,
    y: i64,
    z: i64,
    rate: i32,
    vcf_amount: i32,
    vca_amount: i32,
    target_vcf_amount: i32,
    target_vca_amount: i32,
    pub index: bool,
}

impl LorenzGenerator {
    pub fn new() -> Self {
        LorenzGenerator {
            x: (0.1 * (1 << 24) as f64) as i64,
            y: 0,
            z: 0,
            rate: 0,
            vcf_amount: 0,
            vca_amount: 0,
            target_vcf_amount: 0,
            target_vca_amount: 0,
            index: false,
        }
    }

    pub fn configure(&mut self, rate_knob: i32, balance: i32) {
        self.rate = rate_knob >> 8;
        let vcf = (65535 - balance).min(32767);
        let vca = balance.min(32767);
        self.target_vcf_amount = vcf;
        self.target_vca_amount = vca;
    }
}

impl Default for LorenzGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl Processor for LorenzGenerator {
    fn process(&mut self, _audio: i16, excite: i16) -> (u16, u16) {
        const SIGMA: i64 = (10.0 * (1 << 24) as f64) as i64;
        const RHO: i64 = (28.0 * (1 << 24) as f64) as i64;
        const BETA: i64 = (8.0 / 3.0 * (1 << 24) as f64) as i64;
        let t = tables();
        self.vcf_amount += (self.target_vcf_amount - self.vcf_amount) >> 8;
        self.vca_amount += (self.target_vca_amount - self.vca_amount) >> 8;
        let rate = (self.rate + (excite as i32 >> 8)).clamp(0, 256);
        let dt = t.lorenz_rate[rate as usize] as i64;
        let x = self.x + (dt * ((SIGMA * (self.y - self.x)) >> 24) >> 24);
        let y = self.y + (dt * ((self.x * (RHO - self.z) >> 24) - self.y) >> 24);
        let z = self.z + (dt * ((self.x * self.y >> 24) - (BETA * self.z >> 24)) >> 24);
        self.x = x;
        self.y = y;
        self.z = z;
        let mut z_scaled = (z >> 14) as i32;
        let mut x_scaled = ((x >> 14) + 32768) as i32;
        if self.index {
            std::mem::swap(&mut z_scaled, &mut x_scaled);
        }
        let gain = ((z_scaled as i64 * self.vca_amount as i64) >> 15).clamp(0, 65535) as u16;
        let frequency = (65535 + (((x_scaled - 65535) as i64 * self.vcf_amount as i64) >> 15))
            .clamp(0, 65535) as u16;
        (gain, frequency)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn excite_pulse(p: &mut impl Processor, len: usize, high: usize) -> Vec<(u16, u16)> {
        (0..len)
            .map(|i| p.process(0, if i < high { 28000 } else { 0 }))
            .collect()
    }

    #[test]
    fn meta_parameter_laws_match_upstream() {
        // amount/offset: knob low = negative-ish amount law, knob high
        // = offset rises
        let (am0, off0) = compute_amount_offset(0);
        assert!(am0 < 4096 && off0 == 0, "{am0} {off0}");
        let (am_mid, off_mid) = compute_amount_offset(32768);
        assert!(am_mid > 60000 && off_mid == 0);
        let (am1, off1) = compute_amount_offset(65535);
        assert!(am1 < 4096 && off1 > 60000);
        // attack/decay: low knob = snappy attack
        let (a, d) = compute_attack_decay(0);
        assert_eq!(a, 0);
        assert!(d > 10_000);
    }

    #[test]
    fn envelope_fires_and_decays() {
        let mut e = Envelope::new();
        e.configure(false, 20_000, 32768);
        let out = excite_pulse(&mut e, 31_250, 300);
        let peak = out.iter().map(|(g, _)| *g).max().unwrap_or(0);
        assert!(peak > 20_000, "envelope peaks: {peak}");
        let tail = out[out.len() - 1].0;
        assert!(tail < peak / 4, "and decays: {peak} -> {tail}");
    }

    #[test]
    fn vactrol_smooth_responds_and_relaxes() {
        let mut v = Vactrol::new();
        v.configure(false, 16_000, 40_000);
        let out = excite_pulse(&mut v, 31_250, 3_000);
        let peak = out.iter().map(|(g, _)| *g).max().unwrap_or(0);
        let tail = out[out.len() - 1].0;
        assert!(peak > 8_000, "vactrol lights up: {peak}");
        assert!(tail < peak, "and relaxes: {peak} -> {tail}");
    }

    #[test]
    fn vactrol_plucked_rings_through_gompertz() {
        let mut v = Vactrol::new();
        v.configure(true, 20_000, 40_000);
        let out = excite_pulse(&mut v, 31_250, 300);
        let peak = out.iter().map(|(g, _)| *g).max().unwrap_or(0);
        assert!(peak > 16_000, "pluck snaps open: {peak}");
        let tail = out[out.len() - 1].0;
        assert!(tail < peak / 2, "pluck dies: {peak} -> {tail}");
    }

    #[test]
    fn follower_tracks_signal_energy() {
        let mut f = Follower::new();
        f.configure(false, 20_000, 32768);
        // loud noise burst then silence
        let mut rng = 1u32;
        let mut peak = 0u16;
        for i in 0..31_250 {
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let x = if i < 8_000 { (rng >> 17) as i16 - 16384 } else { 0 };
            let (g, _) = f.process(0, x);
            if i < 8_000 {
                peak = peak.max(g);
            }
        }
        assert!(peak > 8_000, "follower rises on energy: {peak}");
        let (tail, _) = f.process(0, 0);
        assert!(tail < peak / 2, "and falls in silence: {peak} -> {tail}");
    }

    #[test]
    fn compressor_attenuates_hot_signals() {
        let mut c = Compressor::new();
        c.configure(false, 10_000, 8_000);
        let mut quiet = 65535u16;
        let mut loud = 65535u16;
        for i in 0..31_250 {
            let (g, _) = c.process(if i < 15_000 { 800 } else { 30_000 }, 0);
            if i == 14_000 {
                quiet = g;
            }
            if i == 31_000 {
                loud = g;
            }
        }
        assert!(loud < quiet, "hot input pulls gain down: {quiet} -> {loud}");
    }

    #[test]
    fn lorenz_wanders_within_bounds() {
        let mut l = LorenzGenerator::new();
        l.configure(40_000, 20_000);
        let mut min = u16::MAX;
        let mut max = 0u16;
        for _ in 0..62_500 {
            let (g, f) = l.process(0, 0);
            min = min.min(g);
            max = max.max(g);
            assert!(f <= 65535);
        }
        assert!(max > min, "the attractor moves: {min}..{max}");
    }

    #[test]
    fn filter_controller_scales_excite() {
        let mut fc = FilterController::default();
        fc.configure(20_000, 65_535);
        // let smoothing settle
        let mut hi = 0u16;
        let mut lo = 0u16;
        for _ in 0..4_096 {
            lo = fc.process(0, 0).1;
            hi = fc.process(0, 20_000).1;
        }
        assert!(hi > lo, "positive amount tracks excite: {lo} -> {hi}");
    }
}
