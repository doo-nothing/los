// The firmware's fixed-point idiom is `a + (b * c >> s)` everywhere;
// shift-after-multiply is the contract.
#![allow(clippy::precedence)]
//! # Peaks modulation & pulse engines
//!
//! The non-drum processor functions, fixed-point faithful: the
//! multistage envelope, the LFO (sine/triangle/square/steps/noise
//! with waveshaping), the tap LFO (stmlib PatternPredictor), pulse
//! shaper, pulse randomizer, bouncing ball, mini sequencer, and the
//! number station (digits.bin, masks and all).
//!
//! Ported from pichenettes/eurorack (peaks/modulations/*,
//! peaks/pulse_processor/*, peaks/number_station/*,
//! stmlib/algorithms/pattern_predictor.h), copyright 2013 Emilie
//! Gillet, MIT license; attribution preserved.

use super::dsp::{
    tables, Rng, GATE_FLAG_AUXILIARY_RISING, GATE_FLAG_FALLING, GATE_FLAG_FROM_BUTTON,
    GATE_FLAG_HIGH, GATE_FLAG_RISING, NATIVE_SR,
};

#[inline]
fn clip16(x: i32) -> i32 {
    x.clamp(-32768, 32767)
}

#[inline]
fn interpolate824_u16(table: &[u16], phase: u32) -> u16 {
    let i = (phase >> 24) as usize;
    let a = table[i] as i64;
    let b = table[(i + 1).min(table.len() - 1)] as i64;
    (a + ((b - a) * ((phase >> 8) & 0xffff) as i64 >> 16)) as u16
}

#[inline]
fn interpolate88_u16(table: &[u16], index: u16) -> u16 {
    let i = (index >> 8) as usize;
    let a = table[i] as i32;
    let b = table[(i + 1).min(table.len() - 1)] as i32;
    (a + ((b - a) * (index & 0xff) as i32 >> 8)) as u16
}

#[inline]
fn interpolate1022(table: &[i16], phase: u32) -> i16 {
    let i = (phase >> 22) as usize;
    let a = table[i] as i32;
    let b = table[(i + 1).min(table.len() - 1)] as i32;
    (a + ((b - a) * ((phase >> 6) & 0xffff) as i32 >> 16)) as i16
}

/// Per-sample u32 increment retargeted from 48 kHz.
#[inline]
fn retarget(inc: u32, sample_rate: f64) -> u32 {
    if (sample_rate - NATIVE_SR).abs() < 1.0 {
        inc
    } else {
        ((inc as f64) * NATIVE_SR / sample_rate) as u32
    }
}

// ── multistage envelope ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvShape {
    Linear,
    Exponential,
    Quartic,
}

/// peaks MultistageEnvelope with the set_ad / set_adsr configs.
pub struct MultistageEnvelope {
    level: [i32; 4],
    time: [u16; 3],
    shape: [EnvShape; 3],
    segment: usize,
    num_segments: usize,
    sustain_point: usize,
    loop_start: usize,
    loop_end: usize,
    phase: u32,
    phase_increment: u32,
    start_value: i32,
    value: i32,
    hard_reset: bool,
    sample_rate: f64,
}

impl MultistageEnvelope {
    pub fn new(sample_rate: f64) -> Self {
        let mut e = MultistageEnvelope {
            level: [0; 4],
            time: [0; 3],
            shape: [EnvShape::Linear; 3],
            segment: 0,
            num_segments: 0,
            sustain_point: 0,
            loop_start: 0,
            loop_end: 0,
            phase: 0,
            phase_increment: 0,
            start_value: 0,
            value: 0,
            hard_reset: false,
            sample_rate,
        };
        e.set_adsr(0, 8192, 16384, 32767);
        e.segment = e.num_segments;
        e
    }

    pub fn set_adsr(&mut self, attack: u16, decay: u16, sustain: i16, release: u16) {
        self.num_segments = 3;
        self.sustain_point = 2;
        self.level = [0, 32767, sustain as i32, 0];
        self.time = [attack, decay, release];
        self.shape = [
            EnvShape::Quartic,
            EnvShape::Exponential,
            EnvShape::Exponential,
        ];
        self.loop_start = 0;
        self.loop_end = 0;
    }

    pub fn set_ad(&mut self, attack: u16, decay: u16) {
        self.num_segments = 2;
        self.sustain_point = 0;
        self.level = [0, 32767, 0, 0];
        self.time = [attack, decay, 0];
        self.shape = [EnvShape::Linear, EnvShape::Exponential, EnvShape::Linear];
        self.loop_start = 0;
        self.loop_end = 0;
    }

    pub fn reset_if_needed(&mut self) {
        if self.segment > self.num_segments {
            self.segment = 0;
            self.phase = 0;
            self.value = 0;
        }
    }

    pub fn process(&mut self, gate_flags: &[u8], out: &mut [i16]) {
        let t = tables();
        for (g, o) in gate_flags.iter().zip(out.iter_mut()) {
            if g & GATE_FLAG_RISING != 0 {
                self.start_value = if self.segment == self.num_segments || self.hard_reset {
                    self.level[0]
                } else {
                    self.value
                };
                self.segment = 0;
                self.phase = 0;
            } else if g & GATE_FLAG_FALLING != 0 && self.sustain_point != 0 {
                self.start_value = self.value;
                self.segment = self.sustain_point;
                self.phase = 0;
            } else if self.phase < self.phase_increment {
                self.start_value = self.level[(self.segment + 1).min(3)];
                self.segment += 1;
                self.phase = 0;
                if self.loop_end != 0 && self.segment == self.loop_end {
                    self.segment = self.loop_start;
                }
            }
            let done = self.segment >= self.num_segments;
            let sustained = self.sustain_point != 0
                && self.segment == self.sustain_point
                && g & GATE_FLAG_HIGH != 0;
            self.phase_increment = if sustained || done {
                0
            } else {
                retarget(
                    t.env_increments[(self.time[self.segment.min(2)] >> 8) as usize],
                    self.sample_rate,
                )
            };
            let a = self.start_value;
            let b = self.level[(self.segment + 1).min(3)];
            let curve = match self.shape[self.segment.min(2)] {
                EnvShape::Linear => &t.env_linear,
                EnvShape::Exponential => &t.env_expo,
                EnvShape::Quartic => &t.env_quartic,
            };
            let tt = interpolate824_u16(curve, self.phase) as i32;
            self.value = a + ((b - a) * (tt >> 1) >> 15);
            self.phase = self.phase.wrapping_add(self.phase_increment);
            *o = self.value as i16;
        }
    }
}

// ── pattern predictor (stmlib) ─────────────────────────────────────────────

const HISTORY_SIZE: usize = 32;
const MAX_CANDIDATE_PERIOD: usize = 8;

#[derive(Debug, Clone)]
pub struct PatternPredictor {
    history: [u32; HISTORY_SIZE],
    prediction_error: [i32; MAX_CANDIDATE_PERIOD + 1],
    predicted_period: [i32; MAX_CANDIDATE_PERIOD + 1],
    history_pointer: usize,
}

impl Default for PatternPredictor {
    fn default() -> Self {
        PatternPredictor {
            history: [0; HISTORY_SIZE],
            prediction_error: [0; MAX_CANDIDATE_PERIOD + 1],
            predicted_period: [0; MAX_CANDIDATE_PERIOD + 1],
            history_pointer: 0,
        }
    }
}

impl PatternPredictor {
    pub fn predict(&mut self, value: u32) -> u32 {
        self.history[self.history_pointer] = value;
        let mut best_period = 0;
        for i in 0..=MAX_CANDIDATE_PERIOD {
            let error = (self.predicted_period[i] - value as i32).abs();
            let delta = error - self.prediction_error[i];
            if delta > 0 {
                self.prediction_error[i] += delta >> 1;
            } else {
                self.prediction_error[i] += delta >> 3;
            }
            if i == 0 {
                self.predicted_period[i] = (value as i32 + self.predicted_period[i]) >> 1;
            } else {
                let t = self.history_pointer + 1 + HISTORY_SIZE - i;
                self.predicted_period[i] = self.history[t % HISTORY_SIZE] as i32;
            }
            if self.prediction_error[i] < self.prediction_error[best_period] {
                best_period = i;
            }
        }
        self.history_pointer = (self.history_pointer + 1) % HISTORY_SIZE;
        self.predicted_period[best_period].max(1) as u32
    }
}

// ── LFO / tap LFO ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LfoShape {
    Sine,
    Triangle,
    Square,
    Steps,
    Noise,
}

pub const LFO_SHAPES: [LfoShape; 5] = [
    LfoShape::Sine,
    LfoShape::Triangle,
    LfoShape::Square,
    LfoShape::Steps,
    LfoShape::Noise,
];

const SLOPE_BITS: u32 = 12;

pub struct Lfo {
    rate: u16,
    shape: LfoShape,
    parameter: i16,
    reset_phase: u32,
    pub sync: bool,
    previous_parameter: i32,
    sync_counter: u32,
    sync_counter_max: u32,
    pattern_predictor: PatternPredictor,
    period: u32,
    phase: u32,
    phase_increment: u32,
    level: i32,
    value: i16,
    next_value: i16,
    decay_factor: u32,
    attack_factor: u32,
    end_of_attack: u32,
    rng: Rng,
    sample_rate: f64,
    /// ~40 ms in samples — the "tighten the period" window
    short_press: u32,
}

impl Lfo {
    pub fn new(sample_rate: f64, seed: u32) -> Self {
        Lfo {
            rate: 0,
            shape: LfoShape::Square,
            parameter: 0,
            reset_phase: 0,
            sync: false,
            previous_parameter: 32767,
            sync_counter: (8.0 * sample_rate) as u32,
            sync_counter_max: (8.0 * sample_rate) as u32,
            pattern_predictor: PatternPredictor::default(),
            period: 1,
            phase: 0,
            phase_increment: 0,
            level: 32767,
            value: 0,
            next_value: 0,
            decay_factor: 0,
            attack_factor: 0,
            end_of_attack: 0,
            rng: Rng::new(seed),
            sample_rate,
            short_press: (1920.0 * sample_rate / NATIVE_SR) as u32,
        }
    }

    pub fn set_rate(&mut self, rate: u16) {
        self.rate = rate;
    }

    pub fn set_shape(&mut self, shape: LfoShape) {
        self.shape = shape;
    }

    pub fn set_parameter(&mut self, parameter: i16) {
        self.parameter = parameter;
    }

    pub fn set_level(&mut self, level: u16) {
        self.level = (level >> 1) as i32;
    }

    pub fn process(&mut self, gate_flags: &[u8], out: &mut [i16]) {
        let t = tables();
        if !self.sync {
            let a = t.lfo_increments[(self.rate >> 8) as usize] as i64;
            let b = t.lfo_increments[((self.rate >> 8) + 1).min(256) as usize] as i64;
            self.phase_increment = retarget(
                (a + (((b - a) >> 1) * (self.rate & 0xff) as i64 >> 7)) as u32,
                self.sample_rate,
            );
        }
        for (g, o) in gate_flags.iter().zip(out.iter_mut()) {
            self.sync_counter = self.sync_counter.saturating_add(1);
            if g & GATE_FLAG_RISING != 0 {
                let mut reset_phase = true;
                if self.sync {
                    if self.sync_counter < self.sync_counter_max {
                        let period;
                        if g & GATE_FLAG_FROM_BUTTON != 0 {
                            period = self.sync_counter;
                        } else if self.sync_counter < self.short_press {
                            period = (3 * self.period + self.sync_counter) >> 2;
                            reset_phase = false;
                        } else {
                            period = self.pattern_predictor.predict(self.sync_counter);
                        }
                        if period != self.period {
                            self.period = period.max(1);
                            self.phase_increment = 0xffff_ffff / self.period;
                        }
                    }
                    self.sync_counter = 0;
                }
                if reset_phase {
                    self.phase = self.reset_phase;
                }
            }
            self.phase = self.phase.wrapping_add(self.phase_increment);
            let sample = match self.shape {
                LfoShape::Sine => self.compute_sine(),
                LfoShape::Triangle => self.compute_triangle(),
                LfoShape::Square => self.compute_square(),
                LfoShape::Steps => self.compute_steps(),
                LfoShape::Noise => self.compute_noise(),
            } as i32;
            *o = (sample * self.level >> 15) as i16;
        }
    }

    fn compute_sine(&mut self) -> i16 {
        let t = tables();
        let phase = self.phase;
        let sine = interpolate1022(&t.wav_sine, phase) as i32;
        if self.parameter > 0 {
            let wf_balance = self.parameter as i32;
            let wf_gain = 2048 + ((self.parameter as i32) * (65535 - 2048) >> 15);
            let folded = interpolate1022(
                &t.wav_fold_sine,
                ((sine * wf_gain) as i64 + (1_i64 << 31)) as u32,
            ) as i32;
            (sine + ((folded - sine) * wf_balance >> 15)) as i16
        } else {
            let wf_balance = -(self.parameter as i32);
            let phase = phase.wrapping_add(1 << 30);
            let tri = if phase < (1 << 31) {
                phase << 1
            } else {
                !(phase << 1)
            };
            let folded = interpolate1022(&t.wav_fold_power, tri) as i32;
            (sine + ((folded - sine) * wf_balance >> 15)) as i16
        }
    }

    fn compute_triangle(&mut self) -> i16 {
        if self.parameter as i32 != self.previous_parameter {
            let slope_offset = (self.parameter as i32 + 32768) as u32;
            if slope_offset <= 1 {
                self.decay_factor = 32768 << SLOPE_BITS;
                self.attack_factor = 1 << (SLOPE_BITS - 1);
            } else {
                self.decay_factor = (32768 << SLOPE_BITS) / slope_offset;
                self.attack_factor = (32768 << SLOPE_BITS) / (65536 - slope_offset);
            }
            self.end_of_attack = slope_offset << 16;
            self.previous_parameter = self.parameter as i32;
        }
        let phase = self.phase;
        let skewed_phase = if phase < self.end_of_attack {
            (phase >> SLOPE_BITS).wrapping_mul(self.decay_factor)
        } else {
            ((phase - self.end_of_attack) >> SLOPE_BITS)
                .wrapping_mul(self.attack_factor)
                .wrapping_add(1 << 31)
        };
        // upstream: phase < 2^31 -> -32768 + (phase >> 15), else
        // 32767 - (phase >> 15) (the u32 >> 15 keeps the top bit, so
        // the subtraction wraps back down the triangle)
        if skewed_phase < 1 << 31 {
            (-32768 + (skewed_phase >> 15) as i32) as i16
        } else {
            (32767_i32.wrapping_sub((skewed_phase >> 15) as i32)) as i16
        }
    }

    fn compute_square(&mut self) -> i16 {
        let mut threshold = ((self.parameter as i32 + 32768) as u32) << 16;
        if threshold < (self.phase_increment << 1) {
            threshold = self.phase_increment << 1;
        } else if !threshold < (self.phase_increment << 1) {
            threshold = !(self.phase_increment << 1);
        }
        if self.phase < threshold {
            32767
        } else {
            -32767
        }
    }

    fn compute_steps(&mut self) -> i16 {
        let quantization_levels = (2 + (((self.parameter as i32 + 32768) * 15) >> 16)) as u32;
        let scale = 65535 / (quantization_levels - 1);
        let tri = if self.phase < (1 << 31) {
            self.phase << 1
        } else {
            !(self.phase << 1)
        };
        ((((tri >> 16) * quantization_levels >> 16) * scale) as i32 - 32768) as i16
    }

    fn compute_noise(&mut self) -> i16 {
        let t = tables();
        let phase = self.phase;
        if phase < self.phase_increment {
            self.value = self.next_value;
            self.next_value = self.rng.sample();
        }
        let value = self.value as i32;
        let next = self.next_value as i32;
        let linear = value + ((next - value) * ((phase >> 17) as i32) >> 15);
        if self.parameter < 0 {
            let balance = self.parameter as i32 + 32767;
            (value + ((linear - value) * balance >> 15)) as i16
        } else {
            let raised_cosine = (interpolate824_u16(&t.raised_cosine, phase) >> 1) as i32;
            let smooth = value + ((next - value) * raised_cosine >> 15);
            (linear + ((smooth - linear) * self.parameter as i32 >> 15)) as i16
        }
    }
}

// ── pulse shaper ───────────────────────────────────────────────────────────

const PULSE_BUFFER_SIZE: usize = 32;

#[derive(Debug, Clone, Copy, Default)]
struct Pulse {
    initial_delay_counter: u16,
    duration_counter: u16,
    delay_counter: u16,
    repetition_counter: u16,
}

pub struct PulseShaper {
    initial_delay: u16,
    duration: u16,
    delay: u16,
    num_repetitions: u16,
    pulse_buffer: [Pulse; PULSE_BUFFER_SIZE],
    previous_num_pulses: u32,
    retrig_counter: u32,
    rate_scale: f64,
}

impl PulseShaper {
    pub fn new(sample_rate: f64) -> Self {
        PulseShaper {
            initial_delay: 0,
            duration: 0,
            delay: 0,
            num_repetitions: 0,
            pulse_buffer: [Pulse::default(); PULSE_BUFFER_SIZE],
            previous_num_pulses: 0,
            retrig_counter: 0,
            // delay-time table is in 6 kHz control ticks at 48 kHz;
            // the engine here runs per CONTROL BLOCK (one tick per
            // process call at the host block rate) — scale the times
            rate_scale: sample_rate / NATIVE_SR,
        }
    }

    pub fn set_initial_delay(&mut self, v: u16) {
        self.initial_delay = v;
    }
    pub fn set_duration(&mut self, v: u16) {
        self.duration = v;
    }
    pub fn set_delay(&mut self, v: u16) {
        self.delay = v;
    }
    pub fn set_num_repetitions(&mut self, v: u16) {
        self.num_repetitions = v >> 12;
    }

    fn ticks(&self, index: u16) -> u16 {
        let t = tables();
        let base = interpolate88_u16(&t.delay_times, index) as f64 * self.rate_scale;
        base.min(65534.0) as u16
    }

    /// One control tick (the firmware ticks this at 6 kHz; we tick per
    /// 8-sample subgroup of the host block — see the channel driver).
    pub fn tick(&mut self, new_pulse: bool) -> i16 {
        let mut new_pulse = new_pulse;
        let mut num_pulses: u32 = 0;
        let duration = self.ticks(self.duration);
        let delay = self.ticks(self.delay).saturating_sub(1);
        let initial_delay = self.ticks(self.initial_delay);
        for p in self.pulse_buffer.iter_mut() {
            if p.repetition_counter > 0 {
                if p.delay_counter < p.duration_counter && p.repetition_counter > 1 {
                    p.duration_counter = p.delay_counter;
                }
                if p.initial_delay_counter == 0 {
                    if p.duration_counter > 0 {
                        p.duration_counter -= 1;
                        num_pulses += 1;
                    }
                    if p.delay_counter > 0 {
                        p.delay_counter -= 1;
                    } else {
                        p.repetition_counter -= 1;
                        p.duration_counter = duration;
                        p.delay_counter = delay;
                    }
                } else {
                    p.initial_delay_counter -= 1;
                }
            } else if new_pulse {
                p.repetition_counter = self.num_repetitions + 1;
                p.initial_delay_counter = initial_delay;
                p.duration_counter = duration;
                p.delay_counter = delay;
                new_pulse = false;
                if p.initial_delay_counter == 0 {
                    num_pulses += 1;
                }
            }
        }
        if self.previous_num_pulses > 0 && num_pulses > self.previous_num_pulses {
            self.retrig_counter = 6;
        }
        self.previous_num_pulses = num_pulses;
        if self.retrig_counter > 0 {
            self.retrig_counter -= 1;
        }
        if num_pulses > 0 && self.retrig_counter == 0 {
            20480
        } else {
            0
        }
    }
}

// ── pulse randomizer ───────────────────────────────────────────────────────

const TRIGGER_PULSE_BUFFER_SIZE: usize = 32;

pub struct PulseRandomizer {
    repetition_probability: u16,
    acceptance_probability: u16,
    delay_average: u16,
    delay_randomness: u16,
    delay_counter: [u16; TRIGGER_PULSE_BUFFER_SIZE],
    num_pulses: u32,
    retrig_counter: u32,
    rng: Rng,
    rate_scale: f64,
}

impl PulseRandomizer {
    pub fn new(sample_rate: f64, seed: u32) -> Self {
        PulseRandomizer {
            repetition_probability: 32767,
            acceptance_probability: 65535,
            delay_average: 32767,
            delay_randomness: 0,
            delay_counter: [0xffff; TRIGGER_PULSE_BUFFER_SIZE],
            num_pulses: 0,
            retrig_counter: 0,
            rng: Rng::new(seed),
            rate_scale: sample_rate / NATIVE_SR,
        }
    }

    pub fn set_repetition_probability(&mut self, v: u16) {
        self.repetition_probability = v;
    }
    pub fn set_acceptance_probability(&mut self, v: u16) {
        self.acceptance_probability = v;
    }
    pub fn set_delay_average(&mut self, v: u16) {
        self.delay_average = v;
    }
    pub fn set_delay_randomness(&mut self, v: u16) {
        self.delay_randomness = v;
    }

    fn delay(&mut self) -> u16 {
        let t = tables();
        let mut delay = self.delay_average as i32;
        delay += (self.rng.sample() as i32 * self.delay_randomness as i32) >> 16;
        let delay = delay.clamp(0, 0xffff) as u16;
        let base = interpolate88_u16(&t.delay_times, delay) as f64 * self.rate_scale;
        base.min(65534.0) as u16
    }

    pub fn tick(&mut self, mut new_pulse: bool) -> i16 {
        if (self.rng.word() >> 16) as u16 > self.acceptance_probability {
            new_pulse = false;
        }
        if new_pulse {
            self.num_pulses += 1;
        }
        let mut consume = new_pulse;
        for i in 0..TRIGGER_PULSE_BUFFER_SIZE {
            if self.delay_counter[i] == 0xffff {
                if consume {
                    self.delay_counter[i] = self.delay();
                    consume = false;
                }
            } else if self.delay_counter[i] > 0 {
                self.delay_counter[i] -= 1;
            } else if ((self.rng.word() >> 16) as u16) < self.repetition_probability {
                self.num_pulses += 1;
                self.delay_counter[i] = self.delay();
            } else {
                self.delay_counter[i] = 0xffff;
            }
        }
        if self.retrig_counter > 0 {
            self.retrig_counter -= 1;
        } else if self.num_pulses > 0 {
            self.retrig_counter = 12;
            self.num_pulses -= 1;
        }
        if self.retrig_counter > 6 {
            20480
        } else {
            0
        }
    }
}

// ── bouncing ball ──────────────────────────────────────────────────────────

pub struct BouncingBall {
    gravity: i32,
    bounce_loss: i32,
    initial_amplitude: i32,
    initial_velocity: i32,
    velocity: i32,
    position: i64,
    rate_scale2: f64,
}

impl BouncingBall {
    pub fn new(sample_rate: f64) -> Self {
        let s = NATIVE_SR / sample_rate;
        BouncingBall {
            gravity: 40,
            bounce_loss: 4095,
            initial_amplitude: 65535 << 14,
            initial_velocity: 0,
            velocity: 0,
            position: 0,
            rate_scale2: s * s,
        }
    }

    pub fn set_gravity(&mut self, gravity: u16) {
        let t = tables();
        // gravity is per-sample² at 48 kHz; scale by (48k/sr)²
        self.gravity =
            (interpolate88_u16(&t.gravity, gravity) as f64 * self.rate_scale2).max(1.0) as i32;
    }

    pub fn set_bounce_loss(&mut self, bounce_loss: u16) {
        let b = (65535 - bounce_loss as u32) as u64;
        let b = (b * b) >> 16;
        self.bounce_loss = (4095 - (b >> 4) as i32).max(0);
    }

    pub fn set_initial_amplitude(&mut self, v: u16) {
        self.initial_amplitude = (v as i32) << 14;
    }

    pub fn set_initial_velocity(&mut self, v: i16) {
        self.initial_velocity = (v as i32) << 4;
    }

    pub fn process(&mut self, gate_flags: &[u8], out: &mut [i16]) {
        for (g, o) in gate_flags.iter().zip(out.iter_mut()) {
            if g & GATE_FLAG_RISING != 0 {
                self.velocity = self.initial_velocity;
                self.position = self.initial_amplitude as i64;
            }
            self.velocity -= self.gravity;
            self.position += self.velocity as i64;
            if self.position < 0 {
                self.position = 0;
                self.velocity = -(self.velocity >> 12) * self.bounce_loss;
            }
            if self.position > (32767_i64) << 15 {
                self.position = 32767 << 15;
                self.velocity = -(self.velocity >> 12) * self.bounce_loss;
            }
            *o = (self.position >> 15) as i16;
        }
    }
}

// ── mini sequencer ─────────────────────────────────────────────────────────

pub struct MiniSequencer {
    steps: [i16; 4],
    num_steps: usize,
    step: usize,
    reset_at_next_clock: bool,
}

impl Default for MiniSequencer {
    fn default() -> Self {
        MiniSequencer {
            steps: [0; 4],
            num_steps: 4,
            step: 0,
            reset_at_next_clock: false,
        }
    }
}

impl MiniSequencer {
    pub fn set_step(&mut self, index: usize, value: i16) {
        self.steps[index.min(3)] = value;
    }

    pub fn set_num_steps(&mut self, n: usize) {
        self.num_steps = n.clamp(1, 4);
    }

    pub fn process(&mut self, gate_flags: &[u8], out: &mut [i16]) {
        for (g, o) in gate_flags.iter().zip(out.iter_mut()) {
            if g & GATE_FLAG_RISING != 0 {
                self.step += 1;
                if self.reset_at_next_clock {
                    self.reset_at_next_clock = false;
                    self.step = 0;
                }
            }
            if self.num_steps > 2 && g & GATE_FLAG_AUXILIARY_RISING != 0 {
                self.reset_at_next_clock = true;
            }
            *o = self.steps[self.step % self.num_steps.max(1)];
        }
    }
}

// ── number station ─────────────────────────────────────────────────────────

const VOICE_DIGITS: [usize; 11] = [
    0, 4913, 7830, 11306, 14601, 18651, 22308, 26438, 30495, 33296, 36801,
];

pub struct NumberStation {
    pub voice: bool,
    tone: u16,
    pitch_shift: u32,
    transition_probability: u16,
    noise: i32,
    distortion: i32,
    tone_amplitude: i32,
    phase: u32,
    digit: usize,
    gate: bool,
    drift: i32,
    lp_noise: i32,
    noise_phase: u32,
    interference_phase: u32,
    ringmod_phase: u32,
    lp: super::dsp::PeaksSvf,
    hp: super::dsp::PeaksSvf,
    previous_inner_sample: i32,
    previous_outer_sample: i32,
    rng: Rng,
    sample_rate: f64,
}

impl NumberStation {
    pub fn new(sample_rate: f64, seed: u32) -> Self {
        let mut n = NumberStation {
            voice: true,
            tone: 32768 + 8192,
            pitch_shift: 24576,
            transition_probability: 32768,
            noise: 16384,
            distortion: 16384,
            tone_amplitude: 0,
            phase: 0,
            digit: 0,
            gate: false,
            drift: 0,
            lp_noise: 0,
            noise_phase: 0,
            interference_phase: 0,
            ringmod_phase: 0,
            lp: super::dsp::PeaksSvf::default(),
            hp: super::dsp::PeaksSvf::default(),
            previous_inner_sample: 0,
            previous_outer_sample: 0,
            rng: Rng::new(seed),
            sample_rate,
        };
        n.lp.set_frequency(120 << 7);
        n.lp.set_resonance(16000);
        n.hp.set_frequency(70 << 7);
        n.hp.set_resonance(8000);
        n
    }

    pub fn set_tone(&mut self, tone: u16) {
        self.tone = (tone >> 2) + 32768 + 8192;
        self.pitch_shift = if tone < 32768 {
            24576 + (tone as u32 >> 2)
        } else {
            16384 + (tone as u32 >> 1)
        };
    }

    pub fn set_transition_probability(&mut self, p: u16) {
        self.transition_probability = p;
    }

    pub fn set_noise(&mut self, noise: u16) {
        self.noise = (noise >> 1) as i32;
    }

    pub fn set_distortion(&mut self, distortion: u16) {
        self.distortion = 8192 + (((32767 - 8192) as u32 * distortion as u32) >> 16) as i32;
    }

    pub fn process(&mut self, gate_flags: &[u8], out: &mut [i16]) {
        let t = tables();
        const DOWNSAMPLE: usize = 4;
        let phase_increment = if self.voice {
            retarget(self.pitch_shift, self.sample_rate)
        } else {
            let frequency = self.tone;
            let a = t.lfo_increments[(frequency >> 8) as usize] as i64;
            let b = t.lfo_increments[((frequency >> 8) + 1).min(256) as usize] as i64;
            let inc = (a + (((b - a) >> 1) * (frequency & 0xff) as i64 >> 7)) as u32;
            retarget((inc << 6).wrapping_mul(self.digit as u32 + 1), self.sample_rate)
        };
        let drift_target = self.rng.sample() as i32;
        if drift_target > self.drift {
            self.drift += (drift_target - self.drift) >> 13;
        } else {
            self.drift -= (self.drift - drift_target) >> 13;
        }

        let mut i = 0;
        while i + DOWNSAMPLE <= out.len() {
            for k in 0..DOWNSAMPLE {
                let g = gate_flags[i + k];
                if g & GATE_FLAG_RISING != 0 {
                    let random = self.rng.sample() as u16;
                    if random < self.transition_probability {
                        let d = (random >> 2) as usize;
                        self.digit = if self.voice { d % 10 } else { d & 3 };
                    }
                    if self.voice {
                        self.phase = (VOICE_DIGITS[self.digit] as u32) << 16;
                    }
                }
                if g & GATE_FLAG_HIGH != 0 {
                    self.tone_amplitude += (32767 - self.tone_amplitude) >> 6;
                } else {
                    self.tone_amplitude -= self.tone_amplitude >> 6;
                    if self.tone_amplitude < 64 && self.tone_amplitude > 0 {
                        self.tone_amplitude -= 1;
                    }
                }
            }

            let mut digit: i32;
            if self.voice {
                let integral = (self.phase >> 16) as usize;
                let fractional = (self.phase & 0xffff) as i32;
                if integral < VOICE_DIGITS[self.digit + 1]
                    && integral + 1 < t.wav_digits.len()
                {
                    let mask_a = (integral as u32).wrapping_mul(53) as u8;
                    let mask_b = mask_a.wrapping_add(53);
                    let a = (t.wav_digits[integral] ^ mask_a) as i32;
                    let b = (t.wav_digits[integral + 1] ^ mask_b) as i32;
                    digit = (a << 8) + (((b - a) * fractional) >> 8);
                    digit -= 32768;
                    self.phase = self.phase.wrapping_add(phase_increment);
                    self.gate = true;
                } else {
                    digit = 0;
                    self.gate = false;
                }
            } else {
                self.phase = self
                    .phase
                    .wrapping_add(phase_increment.wrapping_add((self.lp_noise << 10) as u32));
                digit = interpolate1022(&t.wav_sine, self.phase) as i32;
                digit = digit * self.tone_amplitude >> 16;
                self.gate = self.tone_amplitude > 0;
            }
            digit += (digit - ((digit + 4096) ^ 0x055a)) * self.distortion >> 15;

            let random_sample = self.rng.sample() as i32;
            self.lp_noise += (random_sample - self.lp_noise) >> 6;
            self.noise_phase = self
                .noise_phase
                .wrapping_add(retarget(238_370_685, self.sample_rate));
            let mut noise =
                self.lp_noise * interpolate1022(&t.wav_sine, self.noise_phase) as i32 >> 12;
            self.interference_phase = self
                .interference_phase
                .wrapping_add(retarget(710_101_260, self.sample_rate));
            noise += self.distortion
                * t.wav_sine[(self.interference_phase >> 22) as usize] as i32
                >> 18;

            let mut inner_sample = digit + ((noise - digit) * self.noise >> 15);
            if random_sample >= 32767 - (self.noise >> 7) {
                inner_sample = 0;
            }
            self.ringmod_phase = self.ringmod_phase.wrapping_add(retarget(
                (38_654_706_i64 + (38_654_706_i64 * self.drift as i64 >> 10)) as u32,
                self.sample_rate,
            ));
            let ringmod = (interpolate1022(&t.wav_sine, self.ringmod_phase) as i32) >> 1;
            let ringmod = ringmod * inner_sample >> 15;
            inner_sample += ringmod * self.distortion >> 15;
            let inner_sample = clip16(inner_sample);
            let inner_sample = interpolate1022(
                &t.wav_fold_sine,
                ((inner_sample as i64 * 8192) + (1_i64 << 31)) as u32,
            ) as i32;

            // 4x upsample through the band-pass coloration, like the
            // firmware's interleaved writes
            let mut outer = self.lp.process(
                super::dsp::SvfMode::Lp,
                self.hp.process(
                    super::dsp::SvfMode::Hp,
                    (inner_sample + self.previous_inner_sample) >> 1,
                ),
            );
            out[i] = clip16((self.previous_outer_sample + outer) >> 1) as i16;
            out[i + 1] = clip16(outer) as i16;
            self.previous_outer_sample = outer;
            outer = self.lp.process(
                super::dsp::SvfMode::Lp,
                self.hp.process(super::dsp::SvfMode::Hp, inner_sample),
            );
            out[i + 2] = clip16((self.previous_outer_sample + outer) >> 1) as i16;
            out[i + 3] = clip16(outer) as i16;
            self.previous_outer_sample = outer;
            self.previous_inner_sample = inner_sample;

            i += DOWNSAMPLE;
        }
        // any ragged tail keeps the last sample
        for k in i..out.len() {
            out[k] = if k > 0 { out[k - 1] } else { 0 };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::peaks::dsp::extract_gate_flags;

    fn gate_pattern(len: usize, f: impl Fn(usize) -> bool) -> Vec<u8> {
        let mut prev = 0;
        (0..len)
            .map(|i| {
                prev = extract_gate_flags(prev, f(i));
                prev
            })
            .collect()
    }

    #[test]
    fn envelope_adsr_rises_sustains_releases() {
        let mut e = MultistageEnvelope::new(48_000.0);
        e.set_adsr(8192, 8192, 16384, 8192);
        let gates = gate_pattern(48_000, |i| i < 24_000);
        let mut out = vec![0i16; 48_000];
        e.process(&gates, &mut out);
        let peak = out.iter().cloned().max().unwrap_or(0);
        assert!(peak > 30_000, "attack reaches the top: {peak}");
        let sustain = out[20_000];
        assert!(
            (sustain as i32 - 16384).abs() < 2000,
            "sustains near the set level: {sustain}"
        );
        let tail = out[47_900];
        assert!(tail < 1000, "releases to zero: {tail}");
    }

    #[test]
    fn lfo_every_shape_oscillates_finite() {
        for shape in LFO_SHAPES {
            let mut l = Lfo::new(48_000.0, 0xbeef);
            l.set_shape(shape);
            l.set_rate(40_000);
            // noise only draws a new value per cycle — give every
            // shape a few full cycles to show its swing
            let gates = gate_pattern(48_000, |_| false);
            let mut out = vec![0i16; 48_000];
            l.process(&gates, &mut out);
            let min = *out.iter().min().unwrap_or(&0);
            let max = *out.iter().max().unwrap_or(&0);
            assert!(
                max as i32 - min as i32 > 8000,
                "{shape:?} swings: {min}..{max}"
            );
        }
    }

    #[test]
    fn tap_lfo_locks_to_a_steady_clock() {
        let mut l = Lfo::new(48_000.0, 0x77);
        l.sync = true;
        l.set_shape(LfoShape::Triangle);
        // taps every 12000 samples (4 Hz at 48k)
        let gates = gate_pattern(96_000, |i| i % 12_000 < 100);
        let mut out = vec![0i16; 96_000];
        l.process(&gates, &mut out);
        // after several taps the period should be ~12000: count
        // triangle peaks in the second half
        let tail = &out[48_000..];
        let crossings = tail
            .windows(2)
            .filter(|w| (w[0] >= 0) != (w[1] >= 0))
            .count();
        let hz = crossings as f32 / 2.0;
        assert!(
            (3.0..=5.0).contains(&hz),
            "locked near 4 Hz over 1 s: {hz}"
        );
    }

    #[test]
    fn pulse_shaper_repeats_pulses() {
        let mut ps = PulseShaper::new(48_000.0);
        ps.set_initial_delay(0);
        ps.set_duration(10_000);
        ps.set_delay(20_000);
        ps.set_num_repetitions(3 << 12);
        let mut highs = 0;
        let mut transitions = 0;
        let mut last = 0i16;
        for tick in 0..30_000 {
            let v = ps.tick(tick == 0);
            if v > 0 {
                highs += 1;
            }
            if (v > 0) != (last > 0) {
                transitions += 1;
            }
            last = v;
        }
        assert!(highs > 0, "the pulse fires");
        assert!(transitions >= 4, "and repeats: {transitions} transitions");
    }

    #[test]
    fn pulse_randomizer_emits_with_full_acceptance() {
        let mut pr = PulseRandomizer::new(48_000.0, 0x1234);
        pr.set_acceptance_probability(65535);
        pr.set_repetition_probability(0);
        pr.set_delay_average(0);
        let mut highs = 0;
        for tick in 0..6000 {
            if pr.tick(tick % 1500 == 0) > 0 {
                highs += 1;
            }
        }
        assert!(highs > 0, "accepted pulses come out");
    }

    #[test]
    fn bouncing_ball_bounces_with_decreasing_peaks() {
        let mut bb = BouncingBall::new(48_000.0);
        bb.set_gravity(32768);
        bb.set_bounce_loss(20_000);
        bb.set_initial_amplitude(65535);
        bb.set_initial_velocity(0);
        let gates = gate_pattern(96_000, |i| i == 0);
        let mut out = vec![0i16; 96_000];
        bb.process(&gates, &mut out);
        // find the floor hits (position returns to 0 then rises)
        let mut peaks: Vec<i16> = Vec::new();
        let mut cur_max = 0i16;
        for w in out.windows(2) {
            cur_max = cur_max.max(w[1]);
            if w[0] > 0 && w[1] == 0 {
                peaks.push(cur_max);
                cur_max = 0;
            }
        }
        assert!(peaks.len() >= 2, "at least two bounces: {}", peaks.len());
        assert!(
            peaks[1] < peaks[0],
            "bounce loss shrinks the peaks: {peaks:?}"
        );
    }

    #[test]
    fn mini_sequencer_steps_through_values() {
        let mut ms = MiniSequencer::default();
        ms.set_step(0, 100);
        ms.set_step(1, 200);
        ms.set_step(2, 300);
        ms.set_step(3, 400);
        let gates = gate_pattern(4000, |i| i % 1000 < 10);
        let mut out = vec![0i16; 4000];
        ms.process(&gates, &mut out);
        let mut seen: Vec<i16> = vec![out[0]];
        for w in out.windows(2) {
            if w[1] != w[0] {
                seen.push(w[1]);
            }
        }
        assert!(seen.len() >= 3, "the sequence advances: {seen:?}");
    }

    #[test]
    fn number_station_speaks_in_both_modes() {
        for voice in [true, false] {
            let mut ns = NumberStation::new(48_000.0, 0x5eed);
            ns.voice = voice;
            let gates = gate_pattern(48_000, |i| (i / 6000) % 2 == 0);
            let mut out = vec![0i16; 48_000];
            ns.process(&gates, &mut out);
            let energy: i64 = out.iter().map(|v| (*v as i64).pow(2)).sum();
            assert!(energy > 0, "number station (voice={voice}) transmits");
        }
    }
}
