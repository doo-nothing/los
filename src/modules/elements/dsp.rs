//! Elements DSP primitives, exciters, resonator, tube, and envelope.
//!
//! Ported from Mutable Instruments Elements (pichenettes/eurorack)
//! and stmlib by Emilie Gillet.
//!
//! Copyright 2014 Emilie Gillet (emilie.o.gillet@gmail.com).
//! Permission is hereby granted, free of charge, to any person
//! obtaining a copy of this software and associated documentation
//! files (the "Software"), to deal in the Software without
//! restriction, including without limitation the rights to use, copy,
//! modify, merge, publish, distribute, sublicense, and/or sell copies
//! of the Software (MIT license; full text in the upstream repo).
//! The above copyright notice and this permission notice shall be
//! included in all copies or substantial portions of the Software.
//!
//! Port notes: retargeted from 32 kHz/fixed-point-adjacent STM32 code
//! to sample-rate-agnostic f32 Rust. Lookup tables are computed
//! analytically from the formulas in `elements/resources/
//! lookup_tables.py` rather than carried as 32 kHz-baked arrays; the
//! strike/noise sample data ships verbatim in `samples.bin`.

/// The firmware's resolution choices.
pub const MAX_MODES: usize = 64;
pub const MAX_BOWED_MODES: usize = 8;
pub const MAX_DELAY_SIZE: usize = 1024;
pub const RESOLUTION: usize = 52;
const TUBE_DELAY_SIZE: usize = 2048;

// ── lookup-table formulas (lookup_tables.py, analytic) ─────────────────────

/// `lut_stiffness`: geometry → inter-partial stretch.
pub fn stiffness(geometry: f32) -> f32 {
    let g = geometry.clamp(0.0, 1.0);
    if g < 0.25 {
        -(0.25 - g) * 0.25
    } else if g < 0.3 {
        0.0
    } else if g < 0.9 {
        let t = (g - 0.3) / 0.6;
        0.01 * 10.0_f32.powf(t * 2.005) - 0.01
    } else {
        let t = ((g - 0.9) / 0.1).min(1.0);
        let t = t * t;
        1.5 - (t * std::f32::consts::PI).cos() / 2.0
    }
}

/// `lut_4_decades`: 0–1 → 1 … 10⁴.
pub fn four_decades(x: f32) -> f32 {
    10.0_f32.powf(4.0 * x.clamp(0.0, 1.0))
}

/// Accent gain (±3 dB/V law): `lut_accent_gain_coarse/fine` combined.
pub fn accent_gain(strength: f32) -> f32 {
    10.0_f32.powf(1.5 * (strength.clamp(0.0, 1.0) - 0.5))
}

/// `lut_approx_svf_gain`: pulse amplitude compensation for a cutoff
/// knob (the 32 Hz–16 kHz exponential law, normalized at 32 kHz like
/// the firmware so excitation levels match).
pub fn pulse_amplitude(cutoff: f32) -> f32 {
    let f = (32.0 * 10.0_f32.powf(2.7 * cutoff.clamp(0.0, 1.0)) / 32_000.0).min(0.499);
    (0.42 / f) * 4.0_f32.powf(f * f)
}

/// The exciter LP cutoff law (same exponential), normalized for OUR
/// sample rate.
pub fn exciter_cutoff(timbre: f32, sample_rate: f32) -> f32 {
    (32.0 * 10.0_f32.powf(2.7 * timbre.clamp(0.0, 1.0)) / sample_rate).min(0.499)
}

/// 2^(semitones/12).
pub fn semitones_to_ratio(semitones: f32) -> f32 {
    2.0_f32.powf(semitones / 12.0)
}

// ── stmlib::Svf ────────────────────────────────────────────────────────────

/// State-variable filter, stmlib topology (g/r/h form).
#[derive(Debug, Clone, Copy, Default)]
pub struct Svf {
    g: f32,
    r: f32,
    h: f32,
    state1: f32,
    state2: f32,
}

impl Svf {
    pub fn init(&mut self) {
        *self = Svf::default();
        self.set_f_q(0.01, 100.0);
    }

    /// f is normalized frequency (hz/sr), q the quality factor.
    pub fn set_f_q(&mut self, f: f32, q: f32) {
        let f = f.clamp(1e-5, 0.49);
        self.g = (std::f32::consts::PI * f).tan();
        self.r = 1.0 / q.max(0.5);
        self.h = 1.0 / (1.0 + self.r * self.g + self.g * self.g);
    }

    pub fn set_g_q(&mut self, g: f32, q: f32) {
        self.g = g;
        self.r = 1.0 / q.max(0.5);
        self.h = 1.0 / (1.0 + self.r * self.g + self.g * self.g);
    }

    /// Direct g/r (and optional h) — the exciter LP path.
    pub fn set_g_r(&mut self, g: f32, r: f32) {
        self.g = g;
        self.r = r;
        self.h = 1.0 / (1.0 + self.r * self.g + self.g * self.g);
    }

    pub fn g(&self) -> f32 {
        self.g
    }

    #[inline]
    pub fn process_bp(&mut self, input: f32) -> f32 {
        let hp = (input - self.r * self.state1 - self.g * self.state1 - self.state2) * self.h;
        let bp = self.g * hp + self.state1;
        self.state1 = self.g * hp + bp;
        let lp = self.g * bp + self.state2;
        self.state2 = self.g * bp + lp;
        bp
    }

    /// Band-pass normalized (×r) — the banded-waveguide path.
    #[inline]
    pub fn process_bp_norm(&mut self, input: f32) -> f32 {
        self.process_bp(input) * self.r
    }

    #[inline]
    pub fn process_lp(&mut self, input: f32) -> f32 {
        let hp = (input - self.r * self.state1 - self.g * self.state1 - self.state2) * self.h;
        let bp = self.g * hp + self.state1;
        self.state1 = self.g * hp + bp;
        let lp = self.g * bp + self.state2;
        self.state2 = self.g * bp + lp;
        lp
    }
}

// ── stmlib::CosineOscillator (exact recurrence) ────────────────────────────

#[derive(Debug, Clone, Copy, Default)]
pub struct CosineOscillator {
    y1: f32,
    y0: f32,
    coeff: f32,
    initial: f32,
}

#[allow(clippy::should_implement_trait)] // upstream API name, ported as-is
impl CosineOscillator {
    pub fn init(&mut self, frequency: f32) {
        self.coeff = 2.0 * (std::f32::consts::TAU * frequency).cos();
        self.initial = self.coeff * 0.25;
        self.start();
    }

    #[inline]
    pub fn start(&mut self) {
        self.y1 = self.initial;
        self.y0 = 0.5;
    }

    #[inline]
    pub fn next(&mut self) -> f32 {
        let temp = self.y0;
        self.y0 = self.coeff * self.y0 - self.y1;
        self.y1 = temp;
        temp + 0.5
    }
}

// ── the embedded sample data ───────────────────────────────────────────────

/// `samples.bin`: u32 n_bounds · bounds · u32 n_smp · i16 smp ·
/// u32 n_noise · i16 noise (all little-endian), extracted from the
/// MIT-licensed `elements/resources.cc`.
pub struct SampleData {
    pub boundaries: Vec<u32>,
    pub sample_data: Vec<f32>,
    pub noise_sample: Vec<f32>,
}

static SAMPLES_BIN: &[u8] = include_bytes!("samples.bin");

impl SampleData {
    pub fn load() -> SampleData {
        let b = SAMPLES_BIN;
        let rd_u32 = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let mut o = 0;
        let nb = rd_u32(o) as usize;
        o += 4;
        let boundaries: Vec<u32> = (0..nb)
            .map(|i| rd_u32(o + i * 4))
            .collect();
        o += nb * 4;
        let ns = rd_u32(o) as usize;
        o += 4;
        let rd_i16 =
            |o: usize| i16::from_le_bytes([b[o], b[o + 1]]) as f32 / 32_768.0;
        let sample_data: Vec<f32> = (0..ns).map(|i| rd_i16(o + i * 2)).collect();
        o += ns * 2;
        let nn = rd_u32(o) as usize;
        o += 4;
        let noise_sample: Vec<f32> = (0..nn).map(|i| rd_i16(o + i * 2)).collect();
        SampleData {
            boundaries,
            sample_data,
            noise_sample,
        }
    }
}

// ── random (the firmware's xorshift-flavored generator) ────────────────────

#[derive(Debug, Clone, Copy)]
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u32) -> Self {
        Rng(seed | 1)
    }

    #[inline]
    pub fn word(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    #[inline]
    pub fn sample(&mut self) -> f32 {
        (self.word() >> 8) as f32 / 16_777_216.0
    }
}

// ── exciter ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd)]
pub enum ExciterModel {
    GranularSamplePlayer,
    SamplePlayer,
    Mallet,
    Plectrum,
    Particles,
    Flow,
    Noise,
}

pub const FLAG_GATE: u8 = 1;
pub const FLAG_RISING: u8 = 2;
pub const FLAG_FALLING: u8 = 4;

pub struct Exciter {
    pub model: ExciterModel,
    pub parameter: f32,
    pub timbre: f32,
    pub signature: f32,
    damping: f32,
    lp: Svf,
    damp_state: f32,
    delay: u32,
    plectrum_delay: u32,
    particle_state: f32,
    particle_range: f32,
    phase: u32,
    rng: Rng,
    sample_rate: f32,
}

impl Exciter {
    pub fn new(model: ExciterModel, sample_rate: f32, seed: u32) -> Self {
        let mut lp = Svf::default();
        lp.init();
        Exciter {
            model,
            parameter: 0.0,
            timbre: 0.99,
            signature: 0.0,
            damping: 0.0,
            lp,
            damp_state: 0.0,
            delay: 0,
            plectrum_delay: 0,
            particle_state: 0.5,
            particle_range: 1.0,
            phase: 0,
            rng: Rng::new(seed),
            sample_rate,
        }
    }

    pub fn damping(&self) -> f32 {
        self.damping
    }

    /// `set_meta`: spread the meta knob across a model range; the
    /// fractional part becomes the model's parameter.
    pub fn set_meta(&mut self, meta: f32, first: ExciterModel, last: ExciterModel) {
        let first_i = first as i32;
        let last_i = last as i32;
        let span = (last_i - first_i + 1) as f32;
        let scaled = meta.clamp(0.0, 0.999) * span;
        let idx = (first_i + scaled as i32).min(ExciterModel::Noise as i32);
        self.model = [
            ExciterModel::GranularSamplePlayer,
            ExciterModel::SamplePlayer,
            ExciterModel::Mallet,
            ExciterModel::Plectrum,
            ExciterModel::Particles,
            ExciterModel::Flow,
            ExciterModel::Noise,
        ][idx as usize];
        self.parameter = scaled.fract();
    }

    pub fn process(&mut self, flags: u8, out: &mut [f32], samples: &SampleData) {
        self.damping = 0.0;
        match self.model {
            ExciterModel::GranularSamplePlayer => self.granular(out, samples),
            ExciterModel::SamplePlayer => self.sample_player(flags, out, samples),
            ExciterModel::Mallet => self.mallet(flags, out),
            ExciterModel::Plectrum => self.plectrum(flags, out),
            ExciterModel::Particles => self.particles(flags, out),
            ExciterModel::Flow => self.flow(flags, out),
            ExciterModel::Noise => self.noise(out),
        }
        // post LP (skipped for the two sample players, like the firmware)
        if self.model != ExciterModel::GranularSamplePlayer
            && self.model != ExciterModel::SamplePlayer
        {
            let f = exciter_cutoff(self.timbre, self.sample_rate);
            let g = (std::f32::consts::PI * f).tan();
            if self.model == ExciterModel::Noise {
                // resonance from the parameter knob (Q 0.5 … 500)
                let r = 1.0 / (0.5 * 10.0_f32.powf(3.0 * self.parameter));
                self.lp.set_g_r(g, r);
            } else {
                self.lp.set_g_r(g, 2.0);
            }
            for v in out.iter_mut() {
                *v = self.lp.process_lp(*v);
            }
        }
    }

    fn granular(&mut self, out: &mut [f32], samples: &SampleData) {
        // grain restart probability 1%, restart point from parameter,
        // pitch from timbre (−60…+12 st), reading the noise sample
        let restart_prob = (0.01 * 4_294_967_296.0) as u32;
        let restart_point = ((self.parameter * 32_767.0) as u32) << 17;
        let increment =
            (131_072.0 * semitones_to_ratio(72.0 * self.timbre - 60.0)) as u32;
        let base = (self.signature * 8_192.0) as usize;
        let n = samples.noise_sample.len();
        for v in out.iter_mut() {
            let idx = (self.phase >> 17) as usize;
            let frac = (self.phase & 0x1ffff) as f32 / 131_072.0;
            let a = samples.noise_sample[(base + idx) % n];
            let b = samples.noise_sample[(base + idx + 1) % n];
            *v = a + (b - a) * frac;
            self.phase = self.phase.wrapping_add(increment);
            if self.rng.word() < restart_prob {
                self.phase = restart_point;
            }
        }
    }

    fn sample_player(&mut self, flags: u8, out: &mut [f32], samples: &SampleData) {
        // parameter morphs across the nine percussive samples (two
        // adjacent ones crossfaded); timbre repitches −36…+43 st
        let index = (1.0 - self.parameter) * 8.0;
        let mut ii = index as usize;
        let mut frac = index.fract();
        if ii >= 8 {
            ii = 7;
            frac = 1.0;
        }
        let b = &samples.boundaries;
        let off1 = b[ii] as usize;
        let off2 = b[ii + 1] as usize;
        let len1 = off2 - off1 - 1;
        let len2 = b[(ii + 2).min(b.len() - 1)] as usize - off2 - 1;
        let increment =
            (65_536.0 * semitones_to_ratio(72.0 * self.timbre - 36.0 + 7.0)) as u32;
        let mut damp = self.damp_state;
        if flags & FLAG_RISING != 0 {
            damp = 0.0;
            self.phase = 0;
        }
        if flags & FLAG_GATE == 0 {
            damp = 1.0 - 0.95 * (1.0 - damp);
        }
        for v in out.iter_mut() {
            let pi = (self.phase >> 16) as usize;
            let frac_p = (self.phase & 0xffff) as f32 / 65_536.0;
            let mut s1 = 0.0;
            let mut s2 = 0.0;
            let mut step = false;
            if pi < len1 {
                let d = &samples.sample_data;
                s1 = d[off1 + pi] + (d[off1 + pi + 1] - d[off1 + pi]) * frac_p;
                step = true;
            }
            if pi < len2 {
                let d = &samples.sample_data;
                s2 = d[off2 + pi] + (d[off2 + pi + 1] - d[off2 + pi]) * frac_p;
                step = true;
            }
            if step {
                self.phase = self.phase.wrapping_add(increment);
            }
            *v = (s1 + (s2 - s1) * frac) * 0.5;
        }
        self.damping = damp
            * if self.parameter >= 0.8 {
                self.parameter * 5.0 - 4.0
            } else {
                0.0
            };
        self.damp_state = damp;
    }

    fn mallet(&mut self, flags: u8, out: &mut [f32]) {
        out.iter_mut().for_each(|v| *v = 0.0);
        if flags & FLAG_RISING != 0 {
            self.damp_state = 0.0;
            out[0] = pulse_amplitude(self.timbre);
        }
        if flags & FLAG_GATE == 0 {
            self.damp_state = 1.0 - 0.95 * (1.0 - self.damp_state);
        }
        self.damping = self.damp_state * (1.0 - self.parameter);
    }

    fn plectrum(&mut self, flags: u8, out: &mut [f32]) {
        let amplitude = pulse_amplitude(self.timbre);
        let mut damp = self.damp_state;
        let mut impulse = 0.0;
        if flags & FLAG_RISING != 0 {
            impulse = -amplitude * (0.05 + self.signature * 0.2);
            self.plectrum_delay = (4_096.0 * self.parameter * self.parameter) as u32 + 64;
        }
        for v in out.iter_mut() {
            if self.plectrum_delay > 0 {
                self.plectrum_delay -= 1;
                if self.plectrum_delay == 0 {
                    impulse = amplitude;
                    damp = 1.0;
                }
            }
            if damp > 0.005 {
                damp *= 0.9;
            }
            *v = impulse;
            impulse = 0.0;
        }
        self.damping = damp * 0.5;
        self.damp_state = damp;
    }

    fn particles(&mut self, flags: u8, out: &mut [f32]) {
        if flags & FLAG_RISING != 0 {
            let r = self.rng.sample();
            self.particle_state = 1.0 - 0.6 * r * r;
            self.delay = 0;
            self.particle_range = 1.0;
        }
        out.iter_mut().for_each(|v| *v = 0.0);
        if flags & FLAG_GATE != 0 {
            let up_probability = (0.7 * 4_294_967_296.0) as u32;
            let down_probability = (0.3 * 4_294_967_296.0) as u32;
            let amplitude = pulse_amplitude(self.timbre);
            for v in out.iter_mut() {
                if self.delay == 0 {
                    let r = self.rng.sample();
                    let amount = 1.05 + 0.5 * r * r;
                    if self.rng.word() > up_probability {
                        self.particle_state =
                            (self.particle_state * amount).min(self.particle_range + 0.25);
                    } else if self.rng.word() < down_probability {
                        self.particle_state = (self.particle_state / amount).max(0.02);
                    }
                    self.delay = (self.particle_state * 0.15 * self.sample_rate) as u32;
                    let mut gain = 1.0 - self.particle_range;
                    gain *= gain;
                    *v = self.particle_state * amplitude * (1.0 - gain);
                    let decay = 1.0 - self.parameter;
                    self.particle_range *= 1.0 - decay * decay * 0.5;
                } else {
                    self.delay -= 1;
                }
            }
        }
    }

    fn flow(&mut self, flags: u8, out: &mut [f32]) {
        let scale = self.parameter.powi(4);
        let threshold = 0.0001 + scale * 0.125;
        if flags & FLAG_RISING != 0 {
            self.particle_state = 0.5;
        }
        for v in out.iter_mut() {
            let sample = self.rng.sample();
            if sample < threshold {
                self.particle_state = -self.particle_state;
            }
            *v = self.particle_state + (sample - 0.5 - self.particle_state) * scale;
        }
    }

    fn noise(&mut self, out: &mut [f32]) {
        for v in out.iter_mut() {
            *v = self.rng.sample() - 0.5;
        }
    }
}

// ── the bow table (resonator.h, verbatim) ──────────────────────────────────

#[inline]
pub fn bow_table(x: f32, velocity: f32) -> f32 {
    let x = 0.13 * velocity - x;
    let mut bow = x * 6.0;
    bow = bow.abs() + 0.75;
    bow *= bow;
    bow *= bow;
    bow = 0.25 / bow;
    bow = bow.clamp(0.0025, 0.245);
    x * bow
}

// ── resonator ──────────────────────────────────────────────────────────────

pub struct Resonator {
    pub frequency: f32, // normalized hz/sr
    pub geometry: f32,
    pub brightness: f32,
    pub damping: f32,
    pub position: f32,
    pub modulation_frequency: f32,
    pub modulation_offset: f32,
    resolution: usize,
    f: Vec<Svf>,
    f_bow: Vec<Svf>,
    d_bow: Vec<DelayLine>,
    previous_position: f32,
    lfo_phase: f32,
    bow_signal: f32,
    clock_divider: u32,
}

struct DelayLine {
    buf: Vec<f32>,
    write: usize,
    delay: usize,
}

impl DelayLine {
    fn new() -> Self {
        DelayLine {
            buf: vec![0.0; MAX_DELAY_SIZE],
            write: 0,
            delay: 100,
        }
    }

    #[inline]
    fn set_delay(&mut self, d: usize) {
        self.delay = d.clamp(1, MAX_DELAY_SIZE - 1);
    }

    #[inline]
    fn read(&self) -> f32 {
        self.buf[(self.write + MAX_DELAY_SIZE - self.delay) % MAX_DELAY_SIZE]
    }

    #[inline]
    fn write_sample(&mut self, v: f32) {
        self.buf[self.write] = v;
        self.write = (self.write + 1) % MAX_DELAY_SIZE;
    }
}

impl Default for Resonator {
    fn default() -> Self {
        Self::new()
    }
}

impl Resonator {
    pub fn new() -> Self {
        Resonator {
            frequency: 220.0 / 48_000.0,
            geometry: 0.25,
            brightness: 0.5,
            damping: 0.3,
            position: 0.999,
            modulation_frequency: 0.5 / 48_000.0,
            modulation_offset: 0.1,
            resolution: RESOLUTION,
            f: (0..MAX_MODES).map(|_| Svf::default()).collect(),
            f_bow: (0..MAX_BOWED_MODES).map(|_| Svf::default()).collect(),
            d_bow: (0..MAX_BOWED_MODES).map(|_| DelayLine::new()).collect(),
            previous_position: 0.0,
            lfo_phase: 0.0,
            bow_signal: 0.0,
            clock_divider: 0,
        }
    }

    /// The firmware's ComputeFilters: walk the partials with the
    /// stiffness/brightness/damping laws, return the live mode count.
    fn compute_filters(&mut self) -> usize {
        self.clock_divider += 1;
        let mut stiff = stiffness(self.geometry);
        let mut harmonic = self.frequency;
        let mut stretch_factor = 1.0_f32;
        let mut q = 500.0 * four_decades(self.damping * 0.8);
        let mut brightness_att = 1.0 - self.geometry;
        brightness_att *= brightness_att;
        brightness_att *= brightness_att;
        brightness_att *= brightness_att;
        let brightness = self.brightness * (1.0 - 0.2 * brightness_att);
        let mut q_loss = brightness * (2.0 - brightness) * 0.85 + 0.15;
        let q_loss_damping_rate = self.geometry * (2.0 - self.geometry) * 0.1;
        let mut num_modes = 0;
        for i in 0..self.resolution.min(MAX_MODES) {
            let update = i <= 24 || ((i as u32 & 1) == (self.clock_divider & 1));
            let partial_frequency = (harmonic * stretch_factor).min(0.49);
            if partial_frequency < 0.49 {
                num_modes = i + 1;
            }
            if update {
                self.f[i].set_f_q(partial_frequency, 1.0 + partial_frequency * q);
                if i < MAX_BOWED_MODES {
                    let mut period = (1.0 / partial_frequency) as usize;
                    while period >= MAX_DELAY_SIZE {
                        period >>= 1;
                    }
                    self.d_bow[i].set_delay(period);
                    let g = self.f[i].g();
                    self.f_bow[i].set_g_q(g, 1.0 + partial_frequency * 1500.0);
                }
            }
            stretch_factor += stiff;
            if stiff < 0.0 {
                stiff *= 0.93;
            } else {
                stiff *= 0.98;
            }
            q_loss += q_loss_damping_rate * (1.0 - q_loss);
            harmonic += self.frequency;
            q *= q_loss;
        }
        num_modes
    }

    pub fn process(
        &mut self,
        bow_strength: &[f32],
        input: &[f32],
        center: &mut [f32],
        sides: &mut [f32],
    ) {
        let num_modes = self.compute_filters();
        let num_banded = MAX_BOWED_MODES.min(num_modes);
        let size = input.len();
        let position_increment = (self.position - self.previous_position) / size as f32;
        for n in 0..size {
            // 0.5 Hz LFO on the side-channel comb
            self.lfo_phase += self.modulation_frequency;
            if self.lfo_phase >= 1.0 {
                self.lfo_phase -= 1.0;
            }
            self.previous_position += position_increment;
            let lfo = if self.lfo_phase > 0.5 {
                1.0 - self.lfo_phase
            } else {
                self.lfo_phase
            };
            let mut amplitudes = CosineOscillator::default();
            let mut aux_amplitudes = CosineOscillator::default();
            amplitudes.init(self.previous_position);
            aux_amplitudes.init(self.modulation_offset + lfo);

            let in_sample = input[n] * 0.125;
            let mut sum_center = 0.0;
            let mut sum_side = 0.0;
            amplitudes.start();
            aux_amplitudes.start();
            for i in 0..num_modes {
                let s = self.f[i].process_bp(in_sample);
                sum_center += s * amplitudes.next();
                sum_side += s * aux_amplitudes.next();
            }
            sides[n] = sum_side - sum_center;

            // banded waveguides (the bow path)
            let mut bow_sig = 0.0;
            let input_bowed = in_sample + self.bow_signal;
            amplitudes.start();
            for i in 0..num_banded {
                let s = 0.99 * self.d_bow[i].read();
                bow_sig += s;
                let filtered = self.f_bow[i].process_bp_norm(input_bowed + s);
                self.d_bow[i].write_sample(filtered);
                sum_center += filtered * amplitudes.next() * 8.0;
            }
            self.bow_signal = bow_table(bow_sig, bow_strength[n]);
            center[n] = sum_center;
        }
    }
}

// ── tube (tube.cc, verbatim port) ──────────────────────────────────────────

pub struct Tube {
    delay_line: Vec<f32>,
    delay_ptr: i32,
    zero_state: f32,
    pole_state: f32,
}

impl Default for Tube {
    fn default() -> Self {
        Self::new()
    }
}

impl Tube {
    pub fn new() -> Self {
        Tube {
            delay_line: vec![0.0; TUBE_DELAY_SIZE],
            delay_ptr: 0,
            zero_state: 0.0,
            pole_state: 0.0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &mut self,
        frequency: f32,
        envelope: f32,
        damping: f32,
        timbre: f32,
        input_output: &mut [f32],
        gain: f32,
    ) {
        let mut delay = 1.0 / frequency.max(1e-5);
        while delay >= TUBE_DELAY_SIZE as f32 {
            delay *= 0.5;
        }
        let delay_integral = delay as usize;
        let delay_fractional = delay - delay_integral as f32;
        let envelope = envelope.min(1.0);
        let damping = 3.6 - damping * 1.8;
        let lpf_coefficient =
            (frequency * (1.0 + timbre * timbre * 256.0)).min(0.995);
        let mut d = self.delay_ptr;
        for io in input_output.iter_mut() {
            let breath = *io * damping + 0.8;
            let a = self.delay_line
                [((d + delay_integral as i32) % TUBE_DELAY_SIZE as i32) as usize];
            let b = self.delay_line
                [((d + delay_integral as i32 + 1) % TUBE_DELAY_SIZE as i32) as usize];
            let in_s = a + (b - a) * delay_fractional;
            let pressure_delta = -0.95 * (in_s * envelope + self.zero_state) - breath;
            self.zero_state = in_s;
            let reed = pressure_delta * -0.2 + 0.8;
            let out = (pressure_delta * reed + breath).clamp(-5.0, 5.0);
            self.delay_line[d as usize] = out * 0.5;
            d -= 1;
            if d < 0 {
                d = TUBE_DELAY_SIZE as i32 - 1;
            }
            self.pole_state += lpf_coefficient * (out - self.pole_state);
            *io += gain * envelope * self.pole_state;
        }
        self.delay_ptr = d;
    }
}

// ── multistage envelope (ADSR as Elements wires it) ────────────────────────

/// The firmware's increment law: knob → per-control-tick increment,
/// 0.5 ms … 8 s over a gamma-warped curve (lookup_tables.py).
pub fn env_increment(knob: f32, sample_rate: f32) -> f32 {
    let control_rate = sample_rate / 16.0;
    let max_time = 8.0;
    let min_time = 0.0005;
    let gamma = 0.175_f32;
    let min_inc = 1.0 / (max_time * control_rate);
    let max_inc = 1.0 / (min_time * control_rate);
    let a = max_inc.powf(-gamma);
    let b = min_inc.powf(-gamma);
    (a + (b - a) * knob.clamp(0.0, 1.0)).powf(-1.0 / gamma)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvStage {
    Attack,
    Decay,
    Sustain,
    Release,
    Idle,
}

pub struct MultistageEnvelope {
    stage: EnvStage,
    phase: f32,
    start_value: f32,
    value: f32,
    attack: f32,
    decay: f32,
    sustain: f32,
    release: f32,
    sample_rate: f32,
}

impl MultistageEnvelope {
    pub fn new(sample_rate: f32) -> Self {
        MultistageEnvelope {
            stage: EnvStage::Idle,
            phase: 0.0,
            start_value: 0.0,
            value: 0.0,
            attack: 0.5,
            decay: 0.5,
            sustain: 0.5,
            release: 0.5,
            sample_rate,
        }
    }

    pub fn set_adsr(&mut self, a: f32, d: f32, s: f32, r: f32) {
        self.attack = a;
        self.decay = d;
        self.sustain = s;
        self.release = r;
    }

    /// One control tick per audio block of 16 samples (the firmware's
    /// control rate); `flags` are the exciter gate flags.
    pub fn process(&mut self, flags: u8, block_len: usize) -> f32 {
        if flags & FLAG_RISING != 0 {
            self.stage = EnvStage::Attack;
            self.start_value = self.value;
            self.phase = 0.0;
        }
        if flags & FLAG_FALLING != 0 {
            self.stage = EnvStage::Release;
            self.start_value = self.value;
            self.phase = 0.0;
        }
        let ticks = (block_len as f32 / 16.0).max(1.0);
        let (target, knob) = match self.stage {
            EnvStage::Attack => (1.0, self.attack),
            EnvStage::Decay => (self.sustain, self.decay),
            EnvStage::Sustain => {
                self.value = self.sustain;
                return self.value;
            }
            EnvStage::Release => (0.0, self.release),
            EnvStage::Idle => {
                self.value = 0.0;
                return 0.0;
            }
        };
        self.phase += env_increment(knob, self.sample_rate) * ticks;
        if self.phase >= 1.0 {
            self.phase = 0.0;
            self.value = target;
            self.start_value = target;
            self.stage = match self.stage {
                EnvStage::Attack => EnvStage::Decay,
                EnvStage::Decay => {
                    if self.sustain > 0.0 {
                        EnvStage::Sustain
                    } else {
                        EnvStage::Idle
                    }
                }
                EnvStage::Release => EnvStage::Idle,
                s => s,
            };
        } else {
            // expo-ish curve (the firmware's env_expo flavor)
            let t = self.phase;
            let curved = 1.0 - (-4.0 * t).exp() / (1.0 - (-4.0_f32).exp()).abs();
            let curved = curved.clamp(0.0, 1.0);
            self.value = self.start_value + (target - self.start_value) * curved;
        }
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stiffness_matches_the_python_table() {
        // endpoints and region boundaries from lookup_tables.py
        assert!((stiffness(0.0) - (-0.0625)).abs() < 1e-4);
        assert!(stiffness(0.27).abs() < 1e-6, "plateau region is zero");
        assert!((stiffness(0.9) - (0.01 * 10.0_f32.powf(2.005) - 0.01)).abs() < 0.05);
        assert!((stiffness(1.0) - 2.0).abs() < 0.01);
        assert!((four_decades(1.0) - 10_000.0).abs() < 1.0);
        assert!((accent_gain(0.5) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn samples_bin_roundtrips() {
        let s = SampleData::load();
        assert_eq!(s.boundaries.len(), 10);
        assert!(s.boundaries.windows(2).all(|w| w[0] < w[1]), "monotonic");
        assert_eq!(*s.boundaries.last().unwrap() as usize, s.sample_data.len());
        assert!(s.noise_sample.len() > 32_000);
        let peak = s.sample_data.iter().fold(0.0_f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.5 && peak <= 1.0, "PCM looks sane: {peak}");
    }

    #[test]
    fn bow_table_is_bounded_and_sticky() {
        for x in [-2.0, -0.5, 0.0, 0.5, 2.0] {
            for v in [0.0, 0.5, 1.0] {
                let y = bow_table(x, v);
                assert!(y.abs() < 1.0, "bow({x},{v}) = {y}");
            }
        }
    }

    #[test]
    fn resonator_rings_and_decays() {
        let sr = 48_000.0;
        let mut r = Resonator::new();
        r.frequency = 220.0 / sr;
        r.damping = 0.4;
        let mut center = vec![0.0; 64];
        let mut sides = vec![0.0; 64];
        let bow = vec![0.0; 64];
        // strike it once
        let mut imp = vec![0.0; 64];
        imp[0] = 1.0;
        r.process(&bow, &imp, &mut center, &mut sides);
        let silence = vec![0.0; 64];
        let mut early = 0.0_f32;
        let mut late = 0.0_f32;
        for blk in 0..200 {
            r.process(&bow, &silence, &mut center, &mut sides);
            let e: f32 = center.iter().map(|s| s * s).sum();
            if blk < 10 {
                early += e;
            }
            if blk >= 190 {
                late += e;
            }
        }
        assert!(early > 1e-6, "the strike rings: {early}");
        assert!(late < early, "and decays: {early} -> {late}");
        assert!(late.is_finite());
    }

    #[test]
    fn resonator_brightness_adds_modes_energy() {
        let sr = 48_000.0;
        let ring = |brightness: f32| -> f32 {
            let mut r = Resonator::new();
            r.frequency = 110.0 / sr;
            r.brightness = brightness;
            r.damping = 0.5;
            let mut center = vec![0.0; 64];
            let mut sides = vec![0.0; 64];
            let bow = vec![0.0; 64];
            let mut imp = vec![0.0; 64];
            imp[0] = 1.0;
            r.process(&bow, &imp, &mut center, &mut sides);
            let silence = vec![0.0; 64];
            let mut hf = 0.0_f32;
            for _ in 0..50 {
                r.process(&bow, &silence, &mut center, &mut sides);
                hf += center.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f32>();
            }
            hf
        };
        let dark = ring(0.1);
        let bright = ring(0.9);
        assert!(
            bright > dark * 1.2,
            "brightness brightens: {dark} vs {bright}"
        );
    }

    #[test]
    fn tube_is_stable_at_full_blow() {
        let mut t = Tube::new();
        let mut buf: Vec<f32> = (0..4_096).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
        for chunk in buf.chunks_mut(64) {
            t.process(110.0 / 48_000.0, 1.0, 0.8, 0.7, chunk, 1.0);
        }
        let peak = buf.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        assert!(peak.is_finite() && peak < 30.0, "tube bounded: {peak}");
        assert!(peak > 0.1, "tube speaks");
    }

    #[test]
    fn exciter_models_produce_and_stay_bounded() {
        let samples = SampleData::load();
        for model in [
            ExciterModel::GranularSamplePlayer,
            ExciterModel::SamplePlayer,
            ExciterModel::Mallet,
            ExciterModel::Plectrum,
            ExciterModel::Particles,
            ExciterModel::Flow,
            ExciterModel::Noise,
        ] {
            let mut e = Exciter::new(model, 48_000.0, 0xbeef);
            e.timbre = 0.6;
            e.parameter = 0.5;
            let mut energy = 0.0_f32;
            let mut peak = 0.0_f32;
            for blk in 0..100 {
                let flags = if blk == 0 {
                    FLAG_RISING | FLAG_GATE
                } else if blk < 50 {
                    FLAG_GATE
                } else if blk == 50 {
                    FLAG_FALLING
                } else {
                    0
                };
                let mut out = vec![0.0; 64];
                e.process(flags, &mut out, &samples);
                energy += out.iter().map(|s| s * s).sum::<f32>();
                peak = out.iter().fold(peak, |m, s| m.max(s.abs()));
            }
            assert!(energy > 1e-6, "{model:?} makes something");
            assert!(peak.is_finite() && peak < 50.0, "{model:?} bounded: {peak}");
        }
    }

    #[test]
    fn set_meta_walks_the_strike_range() {
        let mut e = Exciter::new(ExciterModel::Mallet, 48_000.0, 1);
        e.set_meta(0.0, ExciterModel::SamplePlayer, ExciterModel::Particles);
        assert_eq!(e.model, ExciterModel::SamplePlayer);
        e.set_meta(0.5, ExciterModel::SamplePlayer, ExciterModel::Particles);
        assert!(matches!(e.model, ExciterModel::Mallet | ExciterModel::Plectrum));
        e.set_meta(0.99, ExciterModel::SamplePlayer, ExciterModel::Particles);
        assert_eq!(e.model, ExciterModel::Particles);
    }

    #[test]
    fn envelope_shape_regions() {
        let sr = 48_000.0;
        // percussive: short attack, no sustain hold after decay completes
        let mut e = MultistageEnvelope::new(sr);
        e.set_adsr(0.2, 0.36, 0.0, 0.36);
        let mut v = e.process(FLAG_RISING | FLAG_GATE, 64);
        for _ in 0..4_000 {
            v = e.process(FLAG_GATE, 64);
        }
        assert!(v < 0.2, "percussive shape dies while gated: {v}");
        // sustained: holds at sustain level
        let mut e = MultistageEnvelope::new(sr);
        e.set_adsr(0.45, 0.81, 0.8, 0.81);
        let mut v = e.process(FLAG_RISING | FLAG_GATE, 64);
        for _ in 0..4_000 {
            v = e.process(FLAG_GATE, 64);
        }
        assert!((v - 0.8).abs() < 0.05, "sustained shape holds: {v}");
        let mut last = v;
        e.process(FLAG_FALLING, 64);
        for _ in 0..200 {
            last = e.process(0, 64);
        }
        assert!(last < v, "release releases: {v} -> {last}");
    }

    #[test]
    fn cosine_oscillator_tracks_cos() {
        let mut c = CosineOscillator::default();
        c.init(0.05);
        c.start();
        for n in 0..40 {
            let got = c.next();
            let want = 0.5 + 0.5 * (std::f32::consts::TAU * 0.05 * n as f32).cos();
            assert!(
                (got - want).abs() < 0.02,
                "cos recurrence at n={n}: {got} vs {want}"
            );
        }
    }
}
