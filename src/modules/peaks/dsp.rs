//! # Peaks DSP — the twelve processor functions, fixed-point faithful
//!
//! A port of Mutable Instruments Peaks keeping the firmware's 16/32-bit
//! fixed-point arithmetic intact — the 808 models' character lives in
//! the integer SVF clipping and the >> shifts, so floats would be a
//! different instrument. Engines: multistage envelope, LFO (5 shapes),
//! tap LFO (pattern predictor), 808 bass/snare/hat, FM drum, pulse
//! shaper, pulse randomizer, bouncing ball, mini sequencer, and the
//! number station easter egg (digits.bin embedded, like the elements
//! samples).
//!
//! Ported from pichenettes/eurorack (peaks/*, stmlib/*), copyright
//! 2013 Emilie Gillet, MIT license; attribution preserved. Lookup
//! tables are generated at startup from the laws in
//! peaks/resources/lookup_tables.py and waveforms.py.
//!
//! Sample-rate note: the firmware runs at 48 kHz. Phase-increment
//! tables and per-sample decay constants are retargeted at configure
//! time when the host rate differs (exact at 48 kHz).

#![allow(clippy::excessive_precision)]
// The firmware's fixed-point idiom is `a + (b * c >> s)` everywhere;
// shift-after-multiply is the contract, parenthesized per-site would
// bury the laws in noise.
#![allow(clippy::precedence)]

use std::sync::OnceLock;

pub const NATIVE_SR: f64 = 48_000.0;

// ── gate flags (peaks/gate_processor.h) ────────────────────────────────────

pub const GATE_FLAG_LOW: u8 = 0;
pub const GATE_FLAG_HIGH: u8 = 1;
pub const GATE_FLAG_RISING: u8 = 2;
pub const GATE_FLAG_FALLING: u8 = 4;
pub const GATE_FLAG_FROM_BUTTON: u8 = 8;
pub const GATE_FLAG_AUXILIARY_RISING: u8 = 32;

#[inline]
pub fn extract_gate_flags(previous: u8, current: bool) -> u8 {
    let was_high = previous & GATE_FLAG_HIGH != 0;
    match (was_high, current) {
        (false, true) => GATE_FLAG_HIGH | GATE_FLAG_RISING,
        (true, false) => GATE_FLAG_FALLING,
        (true, true) => GATE_FLAG_HIGH,
        (false, false) => GATE_FLAG_LOW,
    }
}

#[inline]
fn clip16(x: i32) -> i32 {
    x.clamp(-32768, 32767)
}

// ── lookup tables (lookup_tables.py / waveforms.py, generated) ─────────────

pub struct Tables {
    pub lfo_increments: Vec<u32>,
    pub env_increments: Vec<u32>,
    pub oscillator_increments: Vec<u32>,
    pub delay_times: Vec<u16>,
    pub gravity: Vec<u16>,
    pub env_linear: Vec<u16>,
    pub env_expo: Vec<u16>,
    pub env_quartic: Vec<u16>,
    pub raised_cosine: Vec<u16>,
    pub svf_cutoff: Vec<u16>,
    pub svf_damp: Vec<u16>,
    pub wav_sine: Vec<i16>,
    pub wav_fold_sine: Vec<i16>,
    pub wav_fold_power: Vec<i16>,
    pub wav_overdrive: Vec<i16>,
    pub wav_digits: &'static [u8],
}

static TABLES: OnceLock<Tables> = OnceLock::new();

pub fn tables() -> &'static Tables {
    TABLES.get_or_init(build_tables)
}

fn build_tables() -> Tables {
    let n = 257;
    let excursion = 4_294_967_296.0_f64;

    // lfo increments: log-spaced 1/32 Hz .. 160 Hz at 48 kHz
    let min_inc = excursion / 32.0 / NATIVE_SR;
    let max_inc = excursion * 160.0 / NATIVE_SR;
    let lfo_increments: Vec<u32> = (0..n)
        .map(|i| {
            let t = i as f64 / (n - 1) as f64;
            (min_inc.ln() + (max_inc.ln() - min_inc.ln()) * t).exp() as u32
        })
        .collect();

    // envelope increments: gamma-warped 8 s .. 0.5 ms
    let gamma = 0.175_f64;
    let min_inc = excursion / (8.0 * NATIVE_SR);
    let max_inc = excursion / (0.0005 * NATIVE_SR);
    let env_increments: Vec<u32> = (0..n)
        .map(|i| {
            let t = i as f64 / (n - 1) as f64;
            let r = max_inc.powf(-gamma) + (min_inc.powf(-gamma) - max_inc.powf(-gamma)) * t;
            r.powf(-1.0 / gamma) as u32
        })
        .collect();

    // oscillator increments: notes 116*128 .. 128*128 step 16
    let oscillator_increments: Vec<u32> = (0..97)
        .map(|i| {
            let note = 116.0 * 128.0 + i as f64 * 16.0;
            let pitch = 440.0 * 2.0_f64.powf((note - 69.0 * 128.0) / (128.0 * 12.0));
            (excursion / NATIVE_SR * pitch) as u32
        })
        .collect();

    // pulse delay times at the 6 kHz control rate, gamma 0.3
    let g = 0.3_f64;
    let delay_times: Vec<u16> = (0..n)
        .map(|i| {
            let t = i as f64 / (n - 1) as f64;
            let time = (0.001_f64.powf(g) + (10.0_f64.powf(g) - 0.001_f64.powf(g)) * t)
                .powf(1.0 / g);
            (time * NATIVE_SR / 8.0).min(65535.0) as u16
        })
        .collect();

    // gravity factors, gamma 0.2 over 15 ms .. 2 s
    let g = 0.2_f64;
    let gravity: Vec<u16> = (0..n)
        .map(|i| {
            let t = i as f64 / (n - 1) as f64;
            let time = (0.015_f64.powf(g) + (2.0_f64.powf(g) - 0.015_f64.powf(g)) * t)
                .powf(1.0 / g);
            ((time * NATIVE_SR / (2.0 * 65536.0)).powi(-2)).min(65535.0) as u16
        })
        .collect();

    // envelope curves
    let lin: Vec<f64> = (0..n)
        .map(|i| (i.min(n - 2)) as f64 / 256.0)
        .collect();
    let env_linear: Vec<u16> = lin
        .iter()
        .map(|&x| (x / lin[n - 1] * 65535.0) as u16)
        .collect();
    let quart_max = lin[n - 1].powf(3.32);
    let env_quartic: Vec<u16> = lin
        .iter()
        .map(|&x| (x.powf(3.32) / quart_max * 65535.0) as u16)
        .collect();
    let expo_max = 1.0 - (-4.0 * lin[n - 1]).exp();
    let env_expo: Vec<u16> = lin
        .iter()
        .map(|&x| ((1.0 - (-4.0 * x).exp()) / expo_max * 65535.0) as u16)
        .collect();
    let raised_cosine: Vec<u16> = lin
        .iter()
        .map(|&x| ((0.5 - (x * std::f64::consts::PI).cos() / 2.0) * 65535.0) as u16)
        .collect();

    // SVF coefficients
    let svf_cutoff: Vec<u16> = (0..n)
        .map(|i| {
            let cutoff = 440.0 * 2.0_f64.powf((i as f64 - 69.0) / 12.0);
            let f = (cutoff / NATIVE_SR).min(1.0 / 8.0);
            ((2.0 * (std::f64::consts::PI * f).sin()) * 32767.0) as u16
        })
        .collect();
    let svf_damp: Vec<u16> = (0..n)
        .map(|i| {
            let resonance = i as f64 / 257.0;
            let cutoff = 440.0 * 2.0_f64.powf((i as f64 - 69.0) / 12.0);
            let f = 2.0 * (std::f64::consts::PI * (cutoff / NATIVE_SR).min(0.125)).sin();
            let damp = (2.0 * (1.0 - resonance.powf(0.25)))
                .min(2.0_f64.min(2.0 / f - f * 0.5));
            (damp * 32767.0).clamp(0.0, 65535.0) as u16
        })
        .collect();

    // waveforms (1025 entries, wrap included)
    let wn = 1025;
    let wav_sine: Vec<i16> = (0..wn)
        .map(|i| {
            let x = i as f64 / 1024.0;
            (32767.0 * (2.0 * std::f64::consts::PI * x).sin()) as i16
        })
        .collect();
    let xs: Vec<f64> = (0..wn).map(|i| i as f64 / 512.0 - 1.0).collect();
    let mut fold_sine: Vec<f64> = xs
        .iter()
        .map(|&x| {
            let sine = (8.0 * std::f64::consts::PI * x).sin();
            let window = (-x * x * 4.0).exp().powi(2);
            sine * window + (3.0 * x).atan() * (1.0 - window)
        })
        .collect();
    let max = fold_sine.iter().map(|v| v.abs()).fold(f64::MIN, f64::max);
    for v in fold_sine.iter_mut() {
        *v /= max;
    }
    let wav_fold_sine: Vec<i16> = fold_sine.iter().map(|&v| (32767.0 * v) as i16).collect();
    let wav_fold_power: Vec<i16> = xs.iter().map(|&x| (32767.0 * x.powi(7)) as i16).collect();
    let max = (5.0_f64).tanh();
    let wav_overdrive: Vec<i16> = xs
        .iter()
        .map(|&x| (32767.0 * (5.0 * x).tanh() / max) as i16)
        .collect();

    Tables {
        lfo_increments,
        env_increments,
        oscillator_increments,
        delay_times,
        gravity,
        env_linear,
        env_expo,
        env_quartic,
        raised_cosine,
        svf_cutoff,
        svf_damp,
        wav_sine,
        wav_fold_sine,
        wav_fold_power,
        wav_overdrive,
        wav_digits: include_bytes!("digits.bin"),
    }
}

// ── stmlib interpolators (utils/dsp.h, exact) ──────────────────────────────

#[inline]
fn interpolate824_u16(table: &[u16], phase: u32) -> u16 {
    let i = (phase >> 24) as usize;
    let a = table[i] as i64;
    let b = table[(i + 1).min(table.len() - 1)] as i64;
    (a + ((b - a) * ((phase >> 8) & 0xffff) as i64 >> 16)) as u16
}

#[inline]
fn interpolate1022(table: &[i16], phase: u32) -> i16 {
    let i = (phase >> 22) as usize;
    let a = table[i] as i32;
    let b = table[(i + 1).min(table.len() - 1)] as i32;
    (a + ((b - a) * ((phase >> 6) & 0xffff) as i32 >> 16)) as i16
}

#[inline]
fn mix16(a: i16, b: i16, balance: u16) -> i16 {
    (a as i32 + ((b as i32 - a as i32) * balance as i32 >> 16)) as i16
}

// ── rng (stmlib Random) ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Rng {
    state: u32,
}

impl Rng {
    pub fn new(seed: u32) -> Self {
        Rng { state: seed.max(1) }
    }

    #[inline]
    pub fn word(&mut self) -> u32 {
        self.state = self.state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.state
    }

    #[inline]
    pub fn sample(&mut self) -> i16 {
        (self.word() >> 16) as i16
    }
}

/// Per-sample fixed-point decay retargeted from 48 kHz: at other host
/// rates `state * decay >> bits` must shed the same dB per second.
fn retarget_decay(decay: u32, bits: u32, sample_rate: f64) -> u32 {
    if (sample_rate - NATIVE_SR).abs() < 1.0 {
        return decay;
    }
    let unit = (1u32 << bits) as f64;
    let c = (decay as f64 / unit).max(1e-9);
    ((c.powf(NATIVE_SR / sample_rate)) * unit).min(unit - 1.0) as u32
}

/// u32 phase increment retargeted from 48 kHz.
#[inline]
fn retarget_increment(inc: u32, sample_rate: f64) -> u32 {
    if (sample_rate - NATIVE_SR).abs() < 1.0 {
        inc
    } else {
        ((inc as f64) * NATIVE_SR / sample_rate) as u32
    }
}

// ── excitation (drums/excitation.h) ────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Excitation {
    delay: u32,
    decay: u32,
    counter: i32,
    state: i32,
    level: i32,
}

impl Excitation {
    pub fn set_delay(&mut self, delay: u32) {
        self.delay = delay;
    }

    pub fn set_decay(&mut self, decay: u32) {
        self.decay = decay;
    }

    pub fn trigger(&mut self, level: i32) {
        self.level = level;
        self.counter = self.delay as i32 + 1;
    }

    pub fn done(&self) -> bool {
        self.counter == 0
    }

    #[inline]
    pub fn process(&mut self) -> i32 {
        self.state = (self.state as i64 * self.decay as i64 >> 12) as i32;
        if self.counter > 0 {
            self.counter -= 1;
            if self.counter == 0 {
                self.state += self.level.abs();
            }
        }
        if self.level < 0 {
            -self.state
        } else {
            self.state
        }
    }
}

// ── the peaks SVF (drums/svf.h, fixed-point) ───────────────────────────────

#[derive(Debug, Clone)]
pub struct PeaksSvf {
    dirty: bool,
    frequency: i16,
    resonance: i16,
    punch: i32,
    f: i32,
    damp: i32,
    lp: i32,
    bp: i32,
}

impl Default for PeaksSvf {
    fn default() -> Self {
        PeaksSvf {
            dirty: true,
            frequency: 33 << 7,
            resonance: 16384,
            punch: 0,
            f: 0,
            damp: 0,
            lp: 0,
            bp: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvfMode {
    Lp,
    Bp,
    Hp,
}

impl PeaksSvf {
    pub fn set_frequency(&mut self, frequency: i16) {
        self.dirty = self.dirty || (self.frequency != frequency);
        self.frequency = frequency;
    }

    pub fn set_resonance(&mut self, resonance: i16) {
        self.resonance = resonance;
        self.dirty = true;
    }

    pub fn set_punch(&mut self, punch: u16) {
        self.punch = ((punch as u32 * punch as u32) >> 24) as i32;
    }

    #[inline]
    pub fn process(&mut self, mode: SvfMode, input: i32) -> i32 {
        let t = tables();
        if self.dirty {
            self.f = interpolate824_u16(&t.svf_cutoff, (self.frequency as u32) << 17) as i32;
            self.damp = interpolate824_u16(&t.svf_damp, (self.resonance as u32) << 17) as i32;
            self.dirty = false;
        }
        let mut f = self.f;
        let mut damp = self.damp;
        if self.punch != 0 {
            let punch_signal = if self.lp > 4096 { self.lp } else { 2048 };
            f += ((punch_signal >> 4) * self.punch) >> 9;
            damp += (punch_signal - 2048) >> 3;
        }
        let notch = input - (self.bp * damp >> 15);
        self.lp += f * self.bp >> 15;
        self.lp = clip16(self.lp);
        let hp = notch - self.lp;
        self.bp += f * hp >> 15;
        self.bp = clip16(self.bp);
        match mode {
            SvfMode::Bp => self.bp,
            SvfMode::Hp => hp,
            SvfMode::Lp => self.lp,
        }
    }
}

// ── 808 bass drum ──────────────────────────────────────────────────────────

pub struct BassDrum {
    pulse_up: Excitation,
    pulse_down: Excitation,
    attack_fm: Excitation,
    resonator: PeaksSvf,
    frequency: i32,
    lp_coefficient: i32,
    lp_state: i32,
}

impl BassDrum {
    pub fn new(sample_rate: f64) -> Self {
        let s = sample_rate / NATIVE_SR;
        let mut d = BassDrum {
            pulse_up: Excitation::default(),
            pulse_down: Excitation::default(),
            attack_fm: Excitation::default(),
            resonator: PeaksSvf::default(),
            frequency: 0,
            lp_coefficient: 0,
            lp_state: 0,
        };
        d.pulse_up.set_delay(0);
        d.pulse_up.set_decay(retarget_decay(3340, 12, sample_rate));
        d.pulse_down.set_delay((1.0e-3 * NATIVE_SR * s) as u32);
        d.pulse_down.set_decay(retarget_decay(3072, 12, sample_rate));
        d.attack_fm.set_delay((4.0e-3 * NATIVE_SR * s) as u32);
        d.attack_fm.set_decay(retarget_decay(4093, 12, sample_rate));
        d.resonator.set_punch(32768);
        d.set_frequency(0);
        d.set_decay(32768);
        d.set_tone(32768);
        d.set_punch(65535);
        d
    }

    pub fn set_frequency(&mut self, frequency: i16) {
        self.frequency = (31 << 7) + ((frequency as i32 * 896) >> 15);
    }

    pub fn set_decay(&mut self, decay: u16) {
        let scaled = 65535 - decay as u32;
        let squared = scaled * scaled >> 16;
        let scaled = squared * scaled >> 18;
        self.resonator.set_resonance((32768 - 128 - scaled as i32) as i16);
    }

    pub fn set_tone(&mut self, tone: u16) {
        let coefficient = (tone as u32 * tone as u32) >> 16;
        self.lp_coefficient = 512 + ((coefficient >> 2) * 3) as i32;
    }

    pub fn set_punch(&mut self, punch: u16) {
        self.resonator.set_punch(((punch as u32 * punch as u32) >> 16) as u16);
    }

    pub fn process(&mut self, gate_flags: &[u8], out: &mut [i16]) {
        for (g, o) in gate_flags.iter().zip(out.iter_mut()) {
            if g & GATE_FLAG_RISING != 0 {
                self.pulse_up.trigger((12.0 * 32768.0 * 0.7) as i32);
                self.pulse_down.trigger((-19662.0 * 0.7) as i32);
                self.attack_fm.trigger(18000);
            }
            let mut excitation = 0;
            excitation += self.pulse_up.process();
            excitation += if !self.pulse_down.done() { 16384 } else { 0 };
            excitation += self.pulse_down.process();
            self.attack_fm.process();
            self.resonator.set_frequency(
                (self.frequency + if self.attack_fm.done() { 0 } else { 17 << 7 }) as i16,
            );
            let resonator_output =
                (excitation >> 4) + self.resonator.process(SvfMode::Bp, excitation);
            self.lp_state += (resonator_output - self.lp_state) * self.lp_coefficient >> 15;
            *o = clip16(self.lp_state) as i16;
        }
    }
}

// ── 808 snare drum ─────────────────────────────────────────────────────────

pub struct SnareDrum {
    excitation_1_up: Excitation,
    excitation_1_down: Excitation,
    excitation_2: Excitation,
    excitation_noise: Excitation,
    body_1: PeaksSvf,
    body_2: PeaksSvf,
    noise: PeaksSvf,
    rng: Rng,
    gain_1: i32,
    gain_2: i32,
    snappy: i32,
    sample_rate: f64,
}

impl SnareDrum {
    pub fn new(sample_rate: f64, seed: u32) -> Self {
        let s = sample_rate / NATIVE_SR;
        let mut d = SnareDrum {
            excitation_1_up: Excitation::default(),
            excitation_1_down: Excitation::default(),
            excitation_2: Excitation::default(),
            excitation_noise: Excitation::default(),
            body_1: PeaksSvf::default(),
            body_2: PeaksSvf::default(),
            noise: PeaksSvf::default(),
            rng: Rng::new(seed),
            gain_1: 0,
            gain_2: 0,
            snappy: 0,
            sample_rate,
        };
        d.excitation_1_up.set_delay(0);
        d.excitation_1_up.set_decay(retarget_decay(1536, 12, sample_rate));
        d.excitation_1_down.set_delay((1e-3 * NATIVE_SR * s) as u32);
        d.excitation_1_down
            .set_decay(retarget_decay(3072, 12, sample_rate));
        d.excitation_2.set_delay((1e-3 * NATIVE_SR * s) as u32);
        d.excitation_2.set_decay(retarget_decay(1200, 12, sample_rate));
        d.excitation_noise.set_delay(0);
        d.noise.set_resonance(2000);
        d.set_tone(0);
        d.set_snappy(32768);
        d.set_decay(32768);
        d.set_frequency(0);
        d
    }

    pub fn set_tone(&mut self, tone: u16) {
        self.gain_1 = 22000 - (tone as i32 >> 2);
        self.gain_2 = 22000 + (tone as i32 >> 2);
    }

    pub fn set_snappy(&mut self, snappy: u16) {
        let snappy = (snappy >> 1).min(28672);
        self.snappy = 512 + snappy as i32;
    }

    pub fn set_decay(&mut self, decay: u16) {
        self.body_1
            .set_resonance((29000 + (decay as i32 >> 5)) as i16);
        self.body_2
            .set_resonance((26500 + (decay as i32 >> 5)) as i16);
        self.excitation_noise
            .set_decay(retarget_decay(4092 + (decay as u32 >> 14), 12, self.sample_rate));
    }

    pub fn set_frequency(&mut self, frequency: i16) {
        let mut base_note: i32 = 52 << 7;
        base_note += frequency as i32 * 896 >> 15;
        self.body_1.set_frequency(base_note as i16);
        self.body_2.set_frequency((base_note + (12 << 7)) as i16);
        self.noise.set_frequency((base_note + (48 << 7)) as i16);
    }

    pub fn process(&mut self, gate_flags: &[u8], out: &mut [i16]) {
        for (g, o) in gate_flags.iter().zip(out.iter_mut()) {
            if g & GATE_FLAG_RISING != 0 {
                self.excitation_1_up.trigger(15 * 32768);
                self.excitation_1_down.trigger(-32768);
                self.excitation_2.trigger(13107);
                self.excitation_noise.trigger(self.snappy);
            }
            let mut excitation_1 = 0;
            excitation_1 += self.excitation_1_up.process();
            excitation_1 += self.excitation_1_down.process();
            excitation_1 += if !self.excitation_1_down.done() { 2621 } else { 0 };
            let body_1 = self.body_1.process(SvfMode::Bp, excitation_1) + (excitation_1 >> 4);
            let mut excitation_2 = 0;
            excitation_2 += self.excitation_2.process();
            excitation_2 += if !self.excitation_2.done() { 13107 } else { 0 };
            let body_2 = self.body_2.process(SvfMode::Bp, excitation_2) + (excitation_2 >> 4);
            let noise_sample = self.rng.sample() as i32;
            let noise = self.noise.process(SvfMode::Bp, noise_sample);
            let noise_envelope = self.excitation_noise.process();
            // retriggers ACCUMULATE excitation state (the 808
            // machine-gun behavior); widen the products — upstream
            // leans on silent i32 wrap here
            let mut sd: i64 = 0;
            sd += body_1 as i64 * self.gain_1 as i64 >> 15;
            sd += body_2 as i64 * self.gain_2 as i64 >> 15;
            sd += noise_envelope as i64 * noise as i64 >> 15;
            *o = clip16(sd.clamp(i32::MIN as i64, i32::MAX as i64) as i32) as i16;
        }
    }
}

// ── 808 high hat ───────────────────────────────────────────────────────────

pub struct HighHat {
    noise: PeaksSvf,
    vca_coloration: PeaksSvf,
    vca_envelope: Excitation,
    phase: [u32; 6],
    sample_rate: f64,
}

impl HighHat {
    pub fn new(sample_rate: f64) -> Self {
        let mut h = HighHat {
            noise: PeaksSvf::default(),
            vca_coloration: PeaksSvf::default(),
            vca_envelope: Excitation::default(),
            phase: [0; 6],
            sample_rate,
        };
        h.noise.set_frequency(105 << 7); // 8 kHz
        h.noise.set_resonance(24000);
        h.vca_coloration.set_frequency(110 << 7); // 13 kHz
        h.vca_coloration.set_resonance(0);
        h.vca_envelope.set_delay(0);
        h.vca_envelope.set_decay(retarget_decay(4093, 12, sample_rate));
        h
    }

    pub fn process(&mut self, gate_flags: &[u8], out: &mut [i16]) {
        // the six square oscillators of the 808 metallic core
        const INCREMENTS: [u32; 6] = [
            48_318_382, 71_582_788, 37_044_092, 54_313_440, 66_214_079, 93_952_409,
        ];
        for (g, o) in gate_flags.iter().zip(out.iter_mut()) {
            if g & GATE_FLAG_RISING != 0 {
                self.vca_envelope.trigger(32768 * 15);
            }
            let mut noise: i32 = 0;
            for (p, inc) in self.phase.iter_mut().zip(INCREMENTS.iter()) {
                *p = p.wrapping_add(retarget_increment(*inc, self.sample_rate));
                noise += (*p >> 31) as i32;
            }
            let noise = clip16(noise << 12);
            // run the SVF at double rate for stability, like upstream
            let mut filtered_noise = 0;
            filtered_noise += self.noise.process(SvfMode::Bp, noise);
            filtered_noise += self.noise.process(SvfMode::Bp, noise);
            // the 808 VCA only amplifies the positive section
            let filtered_noise = filtered_noise.clamp(0, 32767);
            let envelope = self.vca_envelope.process() >> 4;
            let vca_noise = clip16((envelope as i64 * filtered_noise as i64 >> 14)
                .clamp(i32::MIN as i64, i32::MAX as i64) as i32);
            let mut hh = 0;
            hh += self.vca_coloration.process(SvfMode::Hp, vca_noise);
            hh += self.vca_coloration.process(SvfMode::Hp, vca_noise);
            hh <<= 1;
            *o = clip16(hh) as i16;
        }
    }
}

// ── FM drum ────────────────────────────────────────────────────────────────

pub struct FmDrum {
    phase: u32,
    fm_envelope_phase: u32,
    am_envelope_phase: u32,
    aux_envelope_phase: u32,
    phase_increment: u32,
    previous_sample: i32,
    frequency: i32,
    fm_amount: u32,
    am_decay: u16,
    fm_decay: u16,
    noise: u32,
    overdrive: u32,
    aux_envelope_strength: u32,
    pub sd_range: bool,
    rng: Rng,
    sample_rate: f64,
}

const BD_MAP: [[u16; 4]; 10] = [
    [4096, 0, 65535, 32768],
    [8192 + 4096, 0, 65535, 32768],
    [8192, 4096, 49512, 32768],
    [8192, 16384, 40960, 32768],
    [10240, 4096, 24576, 32768],
    [10240, 16384, 24576, 16384],
    [8192, 8192, 32768, 16384],
    [8192, 24576, 49152, 8192],
    [4096, 16384, 40960, 16384],
    [8192, 24576, 49152, 0],
];
const SD_MAP: [[u16; 4]; 10] = [
    [24576, 0, 24576, 36864],
    [24576, 0, 16384, 65535],
    [28672, 0, 16384, 36864],
    [28672, 0, 16384, 65535],
    [20488, 0, 32768, 57344],
    [28672, 0, 24576, 65535],
    [20488, 0, 24576, 65535],
    [28672, 0, 32768, 65535],
    [20488, 65535, 16384, 0],
    [65535, 0, 8192, 32768],
];

impl FmDrum {
    pub fn new(sample_rate: f64, seed: u32) -> Self {
        FmDrum {
            phase: 0,
            fm_envelope_phase: 0xffff_ffff,
            am_envelope_phase: 0xffff_ffff,
            aux_envelope_phase: 0xffff_ffff,
            phase_increment: 0,
            previous_sample: 0,
            frequency: 0,
            fm_amount: 0,
            am_decay: 0,
            fm_decay: 0,
            noise: 0,
            overdrive: 0,
            aux_envelope_strength: 0,
            sd_range: false,
            rng: Rng::new(seed),
            sample_rate,
        }
    }

    pub fn morph(&mut self, x: u16, y: u16) {
        let map: &[[u16; 4]; 10] = if self.sd_range { &SD_MAP } else { &BD_MAP };
        let mut parameters = [0u16; 4];
        for (i, p) in parameters.iter_mut().enumerate() {
            let x_integral = ((x >> 14) << 1) as usize;
            let x_fractional = (x << 2) as u32;
            let a = map[x_integral][i] as i32;
            let b = map[(x_integral + 2).min(9)][i] as i32;
            let c = map[(x_integral + 1).min(9)][i] as i32;
            let d = map[(x_integral + 3).min(9)][i] as i32;
            let e = a + ((b - a) * x_fractional as i32 >> 16);
            let f = c + ((d - c) * x_fractional as i32 >> 16);
            *p = (e + ((f - e) * y as i32 >> 16)) as u16;
        }
        self.set_frequency(parameters[0]);
        self.set_fm_amount((parameters[1] >> 2) * 3);
        self.set_decay(parameters[2]);
        self.set_noise(parameters[3]);
    }

    pub fn set_frequency(&mut self, frequency: u16) {
        self.aux_envelope_strength = if frequency <= 16384 {
            1024
        } else if frequency <= 32768 {
            2048 - (frequency as u32 >> 4)
        } else {
            0
        };
        self.frequency = (24 << 7) + (((72 << 7) * frequency as i32) >> 16);
    }

    pub fn set_fm_amount(&mut self, fm_amount: u16) {
        self.fm_amount = fm_amount as u32 >> 2;
    }

    pub fn set_decay(&mut self, decay: u16) {
        self.am_decay = 16384 + (decay >> 1);
        self.fm_decay = 8192 + (decay >> 2);
    }

    pub fn set_noise(&mut self, noise: u16) {
        let n = noise as u32;
        self.noise = if noise >= 32768 {
            (n - 32768) * (n - 32768) >> 15
        } else {
            0
        };
        let o = n;
        self.overdrive = if noise < 32768 {
            let inv = 32768 - o;
            inv * inv >> 15
        } else {
            0
        };
    }

    fn envelope_increment(&self, decay: u16) -> u32 {
        let t = tables();
        let a = t.env_increments[(decay >> 8) as usize];
        let b = t.env_increments[((decay >> 8) + 1).min(256) as usize];
        retarget_increment(
            a.wrapping_sub((a.wrapping_sub(b)).wrapping_mul((decay & 0xff) as u32) >> 8),
            self.sample_rate,
        )
    }

    fn phase_increment_for(&self, midi_pitch: i32) -> u32 {
        let t = tables();
        let highest = 128 * 128;
        let table_start = 116 * 128;
        let octave = 128 * 12;
        let pitch = midi_pitch.min(highest - 1);
        let mut ref_pitch = pitch - table_start;
        let mut num_shifts = 0;
        while ref_pitch < 0 {
            ref_pitch += octave;
            num_shifts += 1;
        }
        let a = t.oscillator_increments[(ref_pitch >> 4) as usize];
        let b = t.oscillator_increments[((ref_pitch >> 4) + 1).min(96) as usize];
        let inc = a.wrapping_add(
            ((b as i64 - a as i64) * (ref_pitch & 0xf) as i64 >> 4) as u32,
        );
        retarget_increment(inc >> num_shifts, self.sample_rate)
    }

    pub fn process(&mut self, gate_flags: &[u8], out: &mut [i16]) {
        let t = tables();
        let am_envelope_increment = self.envelope_increment(self.am_decay);
        let fm_envelope_increment = self.envelope_increment(self.fm_decay);
        let aux_increment = retarget_increment(4_473_924, self.sample_rate);
        let mut countdown = out.len();
        for (g, o) in gate_flags.iter().zip(out.iter_mut()) {
            countdown -= 1;
            if g & GATE_FLAG_RISING != 0 {
                self.fm_envelope_phase = 0;
                self.am_envelope_phase = 0;
                self.aux_envelope_phase = 0;
                self.phase = 0x3fff * self.fm_amount >> 16;
            }
            self.fm_envelope_phase = self.fm_envelope_phase.wrapping_add(fm_envelope_increment);
            if self.fm_envelope_phase < fm_envelope_increment {
                self.fm_envelope_phase = 0xffff_ffff;
            }
            self.aux_envelope_phase = self.aux_envelope_phase.wrapping_add(aux_increment);
            if self.aux_envelope_phase < aux_increment {
                self.aux_envelope_phase = 0xffff_ffff;
            }
            if countdown & 3 == 0 {
                let aux_envelope =
                    65535 - interpolate824_u16(&t.env_expo, self.aux_envelope_phase) as u32;
                let fm_envelope =
                    65535 - interpolate824_u16(&t.env_expo, self.fm_envelope_phase) as u32;
                self.phase_increment = self.phase_increment_for(
                    self.frequency
                        + (fm_envelope.wrapping_mul(self.fm_amount) >> 16) as i32
                        + (aux_envelope.wrapping_mul(self.aux_envelope_strength) >> 15) as i32
                        + (self.previous_sample >> 6),
                );
            }
            self.phase = self.phase.wrapping_add(self.phase_increment);
            let mut mix = interpolate1022(&t.wav_sine, self.phase);
            if self.noise != 0 {
                mix = mix16(mix, self.rng.sample(), self.noise.min(65535) as u16);
            }
            self.am_envelope_phase = self.am_envelope_phase.wrapping_add(am_envelope_increment);
            if self.am_envelope_phase < am_envelope_increment {
                self.am_envelope_phase = 0xffff_ffff;
            }
            let am_envelope =
                65535 - interpolate824_u16(&t.env_expo, self.am_envelope_phase) as u32;
            mix = ((mix as i32) * am_envelope as i32 >> 16) as i16;
            if self.overdrive != 0 {
                let phi = ((mix as i32) << 16).wrapping_add(1 << 31) as u32;
                let overdriven = interpolate1022(&t.wav_overdrive, phi);
                mix = mix16(mix, overdriven, self.overdrive.min(65535) as u16);
            }
            self.previous_sample = mix as i32;
            *o = mix;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gates_one_hit(len: usize) -> Vec<u8> {
        let mut g = vec![GATE_FLAG_LOW; len];
        g[0] = GATE_FLAG_HIGH | GATE_FLAG_RISING;
        for v in g.iter_mut().take(2400).skip(1) {
            *v = GATE_FLAG_HIGH;
        }
        if len > 2400 {
            g[2400] = GATE_FLAG_FALLING;
        }
        g
    }

    #[test]
    fn tables_have_expected_anchors() {
        let t = tables();
        assert_eq!(t.lfo_increments.len(), 257);
        // 1/32 Hz at index 0
        let hz0 = t.lfo_increments[0] as f64 * NATIVE_SR / 4_294_967_296.0;
        assert!((hz0 - 1.0 / 32.0).abs() < 0.002, "{hz0}");
        let hz256 = t.lfo_increments[256] as f64 * NATIVE_SR / 4_294_967_296.0;
        assert!((hz256 - 160.0).abs() < 1.0, "{hz256}");
        // env curves end at 65535-ish
        assert!(t.env_expo[256] > 65000);
        assert!(t.env_quartic[256] > 65000);
        // digits.bin embedded in full
        assert_eq!(t.wav_digits.len(), 36_824);
    }

    #[test]
    fn bass_drum_thumps_and_decays() {
        let mut bd = BassDrum::new(48_000.0);
        let gates = gates_one_hit(48_000);
        let mut out = vec![0i16; 48_000];
        bd.process(&gates, &mut out);
        let early: i64 = out[..4800].iter().map(|v| (*v as i64).pow(2)).sum();
        let late: i64 = out[43_200..].iter().map(|v| (*v as i64).pow(2)).sum();
        assert!(early > 0, "the kick speaks");
        assert!(late < early / 10, "and decays: {early} -> {late}");
    }

    #[test]
    fn bass_drum_pitch_rises_with_frequency() {
        let centroid = |freq: i16| -> f64 {
            let mut bd = BassDrum::new(48_000.0);
            bd.set_frequency(freq);
            let gates = gates_one_hit(24_000);
            let mut out = vec![0i16; 24_000];
            bd.process(&gates, &mut out);
            let crossings = out
                .windows(2)
                .filter(|w| (w[0] >= 0) != (w[1] >= 0))
                .count();
            crossings as f64
        };
        assert!(centroid(20_000) > centroid(-20_000), "freq knob tracks");
    }

    #[test]
    fn snare_has_more_highs_with_snappy_up() {
        let highs = |snappy: u16| -> i64 {
            let mut sd = SnareDrum::new(48_000.0, 0x5eed);
            sd.set_snappy(snappy);
            let gates = gates_one_hit(9600);
            let mut out = vec![0i16; 9600];
            sd.process(&gates, &mut out);
            // crude high-band energy: first difference
            out.windows(2)
                .map(|w| {
                    let d = (w[1] - w[0]) as i64;
                    d * d
                })
                .sum()
        };
        assert!(highs(65535) > highs(0), "snappy adds noise band");
    }

    #[test]
    fn high_hat_sizzles_then_chokes() {
        let mut hh = HighHat::new(48_000.0);
        let gates = gates_one_hit(24_000);
        let mut out = vec![0i16; 24_000];
        hh.process(&gates, &mut out);
        let early: i64 = out[..2400].iter().map(|v| (*v as i64).pow(2)).sum();
        let late: i64 = out[21_600..].iter().map(|v| (*v as i64).pow(2)).sum();
        assert!(early > 0);
        assert!(late < early / 4, "the hat decays: {early} -> {late}");
    }

    #[test]
    fn fm_drum_sounds_in_both_ranges_and_morphs() {
        for sd_range in [false, true] {
            let mut fm = FmDrum::new(48_000.0, 0xfeed);
            fm.sd_range = sd_range;
            fm.morph(20_000, 40_000);
            let gates = gates_one_hit(9600);
            let mut out = vec![0i16; 9600];
            fm.process(&gates, &mut out);
            let energy: i64 = out.iter().map(|v| (*v as i64).pow(2)).sum();
            assert!(energy > 0, "fm drum (sd={sd_range}) speaks");
        }
    }
}
