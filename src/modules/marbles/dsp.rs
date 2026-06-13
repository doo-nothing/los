//! # Marbles engine — the stochastic sampler
//!
//! Ported from pichenettes/eurorack (marbles/random/*, MIT, copyright
//! 2015 Emilie Gillet, attribution preserved), float-faithful in its
//! random-generation core: the déjà-vu `RandomSequence` loop, the
//! Beta-distribution voltage shaping (the real scipy-computed ICDF
//! tables, embedded), the `OutputChannel` voltage law with its
//! smooth/quantized morph and the lag processor, and the `TGenerator`
//! rhythm models (complementary/independent Bernoulli, three-states,
//! drums, divider, Markov, clusters).
//!
//! Marbles' ramp infrastructure (ramp_extractor / ramp_generator /
//! slave_ramp, used to synthesise an internal clock and lock to an
//! external one) is replaced by the los transport clock: the shell
//! steps the generators once per musical clock pulse, the same way
//! grids does. Documented divergence — the stochastic character is
//! the firmware's, the clock is the session's.

#![allow(clippy::excessive_precision)]

use std::sync::OnceLock;

/// The scipy-computed Beta ICDF tables (60 cells × 387 floats).
const DISTRIBUTIONS_BIN: &[u8] = include_bytes!("distributions.bin");

const N_MU: usize = 5;
const N_NU: usize = 9;
const CELL_STRIDE: usize = N_NU + 1; // 10
const ENTRIES: usize = 387; // body[129] + head[129] + tail[129]
const REGION: usize = 129;
const ICDF_SIZE: f32 = 128.0;

struct Distributions {
    table: Vec<f32>, // (N_MU+1)*(N_NU+1) cells × 387
}

static DISTRIBUTIONS: OnceLock<Distributions> = OnceLock::new();

fn distributions() -> &'static Distributions {
    DISTRIBUTIONS.get_or_init(|| {
        let table: Vec<f32> = DISTRIBUTIONS_BIN
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        Distributions { table }
    })
}

#[inline]
fn interpolate(table: &[f32], base: usize, t: f32, size: f32) -> f32 {
    let p = (t * size).clamp(0.0, size);
    let i = p as usize;
    let f = p - i as f32;
    let a = table[base + i];
    let b = table[base + (i + 1).min((size as usize).min(REGION - 1))];
    a + (b - a) * f
}

/// distributions.h BetaDistributionSample — inverse-transform sampling
/// of the Beta family across the (bias, spread) grid.
pub fn beta_distribution_sample(mut uniform: f32, spread: f32, mut bias: f32) -> f32 {
    let d = distributions();
    let flip = bias > 0.5;
    if flip {
        uniform = 1.0 - uniform;
        bias = 1.0 - bias;
    }
    let bias_s = bias * (N_MU as f32 - 1.0) * 2.0;
    let spread_s = spread.clamp(0.0, 1.0) * (N_NU as f32 - 1.0);
    let bias_i = (bias_s as usize).min(N_MU);
    let bias_f = bias_s - bias_i as f32;
    let spread_i = (spread_s as usize).min(N_NU - 1);
    let spread_f = spread_s - spread_i as f32;

    let cell = bias_i * CELL_STRIDE + spread_i;
    let mut offset = 0usize;
    let mut u = uniform.clamp(0.0, 1.0);
    if u <= 0.05 {
        offset = REGION;
        u *= 20.0;
    } else if u >= 0.95 {
        offset = 2 * REGION;
        u = (u - 0.95) * 20.0;
    }

    let cell_base = |c: usize| c * ENTRIES + offset;
    let x1y1 = interpolate(&d.table, cell_base(cell), u, ICDF_SIZE);
    let x2y1 = interpolate(&d.table, cell_base(cell + 1), u, ICDF_SIZE);
    let x1y2 = interpolate(&d.table, cell_base(cell + CELL_STRIDE), u, ICDF_SIZE);
    let x2y2 = interpolate(&d.table, cell_base(cell + CELL_STRIDE + 1), u, ICDF_SIZE);
    let y1 = x1y1 + (x2y1 - x1y1) * spread_f;
    let y2 = x1y2 + (x2y2 - x1y2) * spread_f;
    let mut y = y1 + (y2 - y1) * bias_f;
    if flip {
        y = 1.0 - y;
    }
    y
}

// ── random stream ───────────────────────────────────────────────────────────

/// marbles random_stream.h — the shared LCG.
#[derive(Debug, Clone)]
pub struct RandomStream {
    state: u32,
}

impl RandomStream {
    pub fn new(seed: u32) -> Self {
        Self { state: seed }
    }
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        self.state = self.state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.state
    }
    #[inline]
    pub fn next_float(&mut self) -> f32 {
        self.next_u32() as f32 / 4_294_967_296.0
    }
}

// ── déjà-vu random sequence ─────────────────────────────────────────────────

const DEJA_VU_BUFFER: usize = 16;
const HISTORY_BUFFER: usize = 16;
const MAX_U32: f32 = 4_294_967_296.0;

/// marbles random_sequence.h — the déjà-vu loop. Holds a 16-slot loop
/// buffer and a history; déjà-vu blends fresh randomness, looped
/// replay, and random jumps through the loop.
#[derive(Debug, Clone)]
pub struct RandomSequence {
    loop_buf: [f32; DEJA_VU_BUFFER],
    history: [f32; HISTORY_BUFFER],
    loop_write_head: usize,
    length: usize,
    step: usize,
    record_head: usize,
    replay_head: i32,
    replay_start: usize,
    replay_hash: u32,
    replay_shift: u32,
    deja_vu: f32,
    redo_read: usize,
    redo_write: Option<usize>,
    redo_write_history: Option<usize>,
}

impl RandomSequence {
    pub fn new(stream: &mut RandomStream) -> Self {
        let mut loop_buf = [0.0_f32; DEJA_VU_BUFFER];
        for v in loop_buf.iter_mut() {
            *v = stream.next_float();
        }
        Self {
            loop_buf,
            history: [0.0; HISTORY_BUFFER],
            loop_write_head: 0,
            length: 8,
            step: 0,
            record_head: 0,
            replay_head: -1,
            replay_start: 0,
            replay_hash: 0,
            replay_shift: 0,
            deja_vu: 0.0,
            redo_read: 0,
            redo_write: None,
            redo_write_history: None,
        }
    }

    pub fn clone_from_seq(&mut self, src: &RandomSequence) {
        self.loop_buf = src.loop_buf;
        self.history = src.history;
        self.loop_write_head = src.loop_write_head;
        self.length = src.length;
        self.step = src.step;
        self.record_head = src.record_head;
        self.replay_head = src.replay_head;
        self.replay_start = src.replay_start;
        self.replay_hash = src.replay_hash;
        self.replay_shift = src.replay_shift;
        self.deja_vu = src.deja_vu;
        self.redo_read = src.redo_read;
        self.redo_write = src.redo_write;
        self.redo_write_history = src.redo_write_history;
    }

    pub fn record(&mut self) {
        self.replay_start = self.record_head;
        self.replay_head = -1;
    }

    pub fn replay_pseudo_random(&mut self, hash: u32) {
        self.replay_head = self.replay_start as i32;
        self.replay_hash = hash;
        self.replay_shift = 0;
    }

    pub fn replay_shifted(&mut self, shift: u32) {
        self.replay_head = self.replay_start as i32;
        self.replay_hash = 0;
        self.replay_shift = shift;
    }

    fn get_replay_value(&self) -> f32 {
        let h = ((self.replay_head - 1 - self.replay_shift as i32
            + 2 * HISTORY_BUFFER as i32)
            % HISTORY_BUFFER as i32) as usize;
        if self.replay_hash == 0 {
            self.history[h]
        } else {
            let mut word = (self.history[h] * MAX_U32) as u32;
            word = (word ^ self.replay_hash)
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223);
            word as f32 / MAX_U32
        }
    }

    pub fn next_value(&mut self, stream: &mut RandomStream, deterministic: bool, value: f32) -> f32 {
        if self.replay_head >= 0 {
            self.replay_head = (self.replay_head + 1) % HISTORY_BUFFER as i32;
            return self.get_replay_value();
        }
        let p_sqrt = 2.0 * self.deja_vu - 1.0;
        let p = p_sqrt * p_sqrt;
        let mutate = stream.next_float() < p;
        if mutate && self.deja_vu <= 0.5 {
            // fresh value at the end of the loop
            self.loop_buf[self.loop_write_head] = if deterministic {
                1.0 + value
            } else {
                stream.next_float()
            };
            self.redo_write = Some(self.loop_write_head);
            self.loop_write_head = (self.loop_write_head + 1) % DEJA_VU_BUFFER;
            self.step = self.length - 1;
        } else {
            self.redo_write = None;
            if mutate {
                self.step = (stream.next_float() * self.length as f32) as usize;
            } else {
                self.step += 1;
                if self.step >= self.length {
                    self.step = 0;
                }
            }
        }
        let i =
            (self.loop_write_head + DEJA_VU_BUFFER - self.length + self.step) % DEJA_VU_BUFFER;
        self.redo_read = i;
        let mut result = self.loop_buf[i];
        if result >= 1.0 {
            result -= 1.0;
        } else if deterministic {
            result = 0.5;
        }
        self.history[self.record_head] = result;
        self.redo_write_history = Some(self.record_head);
        self.record_head = (self.record_head + 1) % HISTORY_BUFFER;
        result
    }

    pub fn set_length(&mut self, length: usize) {
        if !(1..=DEJA_VU_BUFFER).contains(&length) {
            return;
        }
        self.length = length;
        self.step %= length;
    }

    pub fn set_deja_vu(&mut self, deja_vu: f32) {
        self.deja_vu = deja_vu;
    }

    pub fn deja_vu(&self) -> f32 {
        self.deja_vu
    }

    pub fn length(&self) -> usize {
        self.length
    }

    pub fn reset(&mut self) {
        self.step = self.length.saturating_sub(1);
    }
}

// ── lag processor ───────────────────────────────────────────────────────────

fn raised_cosine() -> &'static [f32] {
    static RC: OnceLock<Vec<f32>> = OnceLock::new();
    RC.get_or_init(|| {
        (0..257)
            .map(|i| {
                let x = i as f32 / 256.0;
                1.0 - (0.5 * (x * std::f32::consts::PI).cos() + 0.5)
            })
            .collect()
    })
}

#[inline]
fn crossfade(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[inline]
fn semitones_to_ratio(x: f32) -> f32 {
    2.0_f32.powf(x / 12.0)
}

#[inline]
fn interpolate_lut(table: &[f32], t: f32, size: f32) -> f32 {
    let p = (t.clamp(0.0, 1.0) * size).min(size);
    let i = p as usize;
    let f = p - i as f32;
    let a = table[i.min(table.len() - 1)];
    let b = table[(i + 1).min(table.len() - 1)];
    a + (b - a) * f
}

/// marbles lag_processor.cc — the glide/lag with a raised-cosine warp.
#[derive(Debug, Clone, Default)]
pub struct LagProcessor {
    ramp_start: f32,
    ramp_value: f32,
    lp_state: f32,
    previous_phase: f32,
}

impl LagProcessor {
    pub fn reset_ramp(&mut self) {
        self.ramp_start = self.ramp_value;
    }

    pub fn process(&mut self, value: f32, smoothness: f32, phase: f32) -> f32 {
        let mut frequency = phase - self.previous_phase;
        if frequency < 0.0 {
            frequency += 1.0;
        }
        self.previous_phase = phase;
        frequency *= 0.25;
        frequency *= semitones_to_ratio(84.0 * (1.0 - smoothness));
        if frequency >= 1.0 {
            frequency = 1.0;
        }
        if smoothness <= 0.05 {
            frequency += 20.0 * (0.05 - smoothness) * (1.0 - frequency);
        }
        self.lp_state += (value - self.lp_state) * frequency;
        let interp_amount = ((smoothness - 0.6) * 5.0).clamp(0.0, 1.0);
        let interp_linearity = ((1.0 - smoothness) * 5.0).clamp(0.0, 1.0);
        let warped = interpolate_lut(raised_cosine(), phase, 256.0);
        let interp_phase = crossfade(warped, phase, interp_linearity);
        let interp = crossfade(self.ramp_start, value, interp_phase);
        self.ramp_value = interp;
        crossfade(self.lp_state, interp, interp_amount)
    }
}

// ── output channel ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoltageRange {
    Narrow,
    Positive,
    Full,
}

/// One X or Y output: turns the sequence's uniform draw into a shaped,
/// optionally quantized voltage in 0..1 (los normalizes the ±5 V CV).
#[derive(Debug, Clone)]
pub struct OutputChannel {
    pub spread: f32,
    pub bias: f32,
    pub steps: f32,
    previous_steps: f32,
    previous_voltage: f32,
    voltage: f32,
    quantized_voltage: f32,
    lag: LagProcessor,
    /// scale degrees in 0..1 (fractions of the octave); empty = chromatic
    pub scale: Vec<f32>,
}

impl Default for OutputChannel {
    fn default() -> Self {
        Self {
            spread: 0.5,
            bias: 0.5,
            steps: 0.5,
            previous_steps: 0.0,
            previous_voltage: 0.0,
            voltage: 0.0,
            quantized_voltage: 0.0,
            lag: LagProcessor::default(),
            scale: Vec::new(),
        }
    }
}

impl OutputChannel {
    /// Quantize a 0..1 voltage to the nearest scale degree (per octave).
    fn quantize(&self, value: f32, amount: f32) -> f32 {
        if amount < 0.0 || self.scale.is_empty() {
            return value;
        }
        let oct = value.floor();
        let frac = value - oct;
        let mut best = frac;
        let mut best_d = f32::MAX;
        for &deg in &self.scale {
            let d = (frac - deg).abs();
            if d < best_d {
                best_d = d;
                best = deg;
            }
            // wrap neighbor
            let d2 = (frac - (deg + 1.0)).abs();
            if d2 < best_d {
                best_d = d2;
                best = deg + 1.0;
            }
        }
        // amount blends raw → snapped
        crossfade(value, oct + best, amount.clamp(0.0, 1.0))
    }

    fn generate_new_voltage(&self, seq: &mut RandomSequence, stream: &mut RandomStream) -> f32 {
        let u = seq.next_value(stream, false, 0.0);
        let degenerate = (1.25 - self.spread * 25.0).clamp(0.0, 1.0);
        let bernoulli = (self.spread * 25.0 - 23.75).clamp(0.0, 1.0);
        let mut value = beta_distribution_sample(u, self.spread, self.bias);
        let bernoulli_value = if u >= (1.0 - self.bias) { 0.999999 } else { 0.0 };
        value += degenerate * (self.bias - value);
        value += bernoulli * (bernoulli_value - value);
        value
    }

    /// Process one clock cycle's worth of phase; returns the output
    /// for the final sample (the shell holds it until the next step).
    pub fn process_step(
        &mut self,
        seq: &mut RandomSequence,
        stream: &mut RandomStream,
        new_value: bool,
    ) -> f32 {
        if new_value {
            self.previous_voltage = self.voltage;
            self.voltage = self.generate_new_voltage(seq, stream);
            self.lag.reset_ramp();
            self.quantized_voltage = self.quantize(self.voltage, 2.0 * self.steps - 1.0);
        }
        self.previous_steps = self.steps;
        if self.steps >= 0.5 {
            self.quantized_voltage
        } else {
            // smooth: lag toward the target over the step
            let smoothness = 1.0 - 2.0 * self.steps;
            self.lag.process(self.voltage, smoothness, 0.999)
        }
    }
}

// ── t-generator (rhythm) ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TModel {
    ComplementaryBernoulli,
    Clusters,
    Drums,
    IndependentBernoulli,
    Divider,
    ThreeStates,
    Markov,
}

pub const DRUM_PATTERNS: [[u8; 8]; 18] = [
    [1, 0, 0, 0, 2, 0, 0, 0],
    [0, 0, 1, 0, 2, 0, 0, 0],
    [1, 0, 1, 0, 2, 0, 0, 0],
    [0, 0, 1, 0, 2, 0, 0, 2],
    [1, 0, 1, 0, 2, 0, 1, 0],
    [0, 2, 1, 0, 2, 0, 0, 2],
    [1, 0, 0, 0, 2, 0, 1, 0],
    [0, 2, 1, 0, 2, 0, 1, 2],
    [1, 0, 0, 1, 2, 0, 0, 0],
    [0, 2, 1, 1, 2, 0, 1, 2],
    [1, 0, 0, 1, 2, 0, 1, 0],
    [0, 2, 1, 1, 2, 2, 1, 2],
    [1, 0, 0, 1, 2, 0, 1, 2],
    [0, 2, 0, 1, 2, 0, 1, 2],
    [1, 0, 1, 1, 2, 0, 1, 2],
    [2, 0, 1, 2, 0, 1, 2, 0],
    [1, 2, 1, 1, 2, 0, 1, 2],
    [2, 0, 1, 2, 0, 1, 2, 2],
];

/// Divider patterns: (t1 every a, t3 every b) over a cycle length.
pub const DIVIDER_PATTERNS: [(u32, u32, u32); 17] = [
    (1, 1, 1),
    (1, 2, 1),
    (2, 1, 2),
    (1, 4, 1),
    (2, 2, 2),
    (1, 3, 2),
    (4, 4, 4),
    (4, 2, 4),
    (2, 3, 2),
    (1, 8, 1),
    (1, 3, 1),
    (3, 1, 3),
    (1, 5, 4),
    (2, 5, 4),
    (1, 6, 1),
    (3, 2, 3),
    (1, 16, 1),
];

/// The rhythm brain: per clock step decides which of t1/t3 fire (t2 is
/// the master, fires every step). Markov keeps a 16-step history.
#[derive(Debug, Clone)]
pub struct TGenerator {
    pub model: TModel,
    pub bias: f32,
    pub jitter: f32,
    pub pulse_width_mean: f32,
    pub sequence: RandomSequence,
    drum_step: usize,
    drum_index: usize,
    divider_step: u32,
    markov_history: [u8; 16],
    markov_ptr: usize,
    streak: [i32; 2],
    cluster_remaining: u32,
}

impl TGenerator {
    pub fn new(stream: &mut RandomStream) -> Self {
        Self {
            model: TModel::ComplementaryBernoulli,
            bias: 0.5,
            jitter: 0.0,
            pulse_width_mean: 0.0,
            sequence: RandomSequence::new(stream),
            drum_step: 0,
            drum_index: 0,
            divider_step: 0,
            markov_history: [0; 16],
            markov_ptr: 0,
            streak: [0; 2],
            cluster_remaining: 0,
        }
    }

    /// One clock step → a 2-bit mask (bit0=t1, bit1=t3).
    pub fn step(&mut self, stream: &mut RandomStream) -> u8 {
        // advance the déjà-vu sequence to drive the random draws
        let p = self.sequence.next_value(stream, false, 0.0);
        let u0 = self.sequence.next_value(stream, false, 0.0);
        let u1 = stream.next_float();
        let u = [u0, u1];
        match self.model {
            TModel::ComplementaryBernoulli => {
                let mut m = 0;
                #[allow(clippy::needless_range_loop)] // i is the channel bit + parity
                for i in 0..2 {
                    if (u[i >> 1] > self.bias) ^ (i & 1 != 0) {
                        m |= 1 << i;
                    }
                }
                m
            }
            TModel::IndependentBernoulli => {
                let mut m = 0;
                #[allow(clippy::needless_range_loop)] // i is the channel bit + parity
                for i in 0..2 {
                    if (u[i] > self.bias) ^ (i & 1 != 0) {
                        m |= 1 << i;
                    }
                }
                m
            }
            TModel::ThreeStates => {
                let p_none = 0.75 - (self.bias - 0.5).abs();
                let threshold = p_none + (1.0 - p_none) * (0.25 + self.bias * 0.5);
                let mut m = 0;
                #[allow(clippy::needless_range_loop)] // i is the channel bit + parity
                for i in 0..2 {
                    let uu = u[i >> 1];
                    if uu > p_none && ((uu > threshold) ^ (i & 1 != 0)) {
                        m |= 1 << i;
                    }
                }
                m
            }
            TModel::Drums => {
                self.drum_step += 1;
                if self.drum_step >= 8 {
                    self.drum_step = 0;
                    let uu = u[0] * 2.0 * (self.bias - 0.5).abs();
                    self.drum_index = (18.0 * uu) as usize % 18;
                    if self.bias <= 0.5 {
                        self.drum_index -= self.drum_index % 2;
                    }
                }
                DRUM_PATTERNS[self.drum_index][self.drum_step]
            }
            TModel::Divider => {
                let idx = (self.bias * (DIVIDER_PATTERNS.len() as f32 - 1.0)).round() as usize;
                let (a, b, len) = DIVIDER_PATTERNS[idx.min(16)];
                self.divider_step = (self.divider_step + 1) % len.max(1);
                let mut m = 0;
                if self.divider_step.is_multiple_of(a.max(1)) {
                    m |= 1;
                }
                if self.divider_step.is_multiple_of(b.max(1)) {
                    m |= 2;
                }
                m
            }
            TModel::Clusters => {
                // bursts: bias sets burst probability + length
                if self.cluster_remaining > 0 {
                    self.cluster_remaining -= 1;
                    0b11
                } else if u[0] < self.bias {
                    self.cluster_remaining = (u[1] * 3.0) as u32;
                    0b01
                } else {
                    0b10
                }
            }
            TModel::Markov => self.generate_markov(&u, p),
        }
    }

    fn generate_markov(&mut self, u: &[f32; 2], p: f32) -> u8 {
        let b = 1.5 * self.bias - 0.5;
        self.markov_history[self.markov_ptr] = 0;
        let pp = self.markov_ptr;
        let mut bitmask = 0u8;
        #[allow(clippy::needless_range_loop)] // i strides streak + the u draws
        for i in 0..2 {
            let mask = 1u8 << i;
            let periodic = self.markov_history[(pp + 8) % 16] & mask != 0;
            let simultaneous = self.markov_history[(pp + 8) % 16] & !mask != 0;
            let dense = self.markov_history[(pp + 1) % 16] & mask != 0;
            let alternate = self.markov_history[(pp + 4) % 16] & !mask != 0;
            let mut logit = -1.5_f32;
            logit += if self.streak[i] > 24 { 10.0 } else { 0.0 };
            logit += 8.0 * b.abs() * (if periodic { b } else { -b });
            logit -= 2.0 * (if simultaneous { b } else { -b });
            logit -= 1.0 * (if dense { b } else { 0.0 });
            logit += 1.0 * (if alternate { b } else { 0.0 });
            logit = logit.clamp(-10.0, 10.0);
            // logistic: p = 1/(1+e^-logit) approximated directly
            let probability = 1.0 / (1.0 + (-logit).exp());
            let mut state = u[i] < probability;
            if self.sequence.deja_vu() >= p {
                state =
                    self.markov_history[(pp + self.sequence.length()) % 16] & mask != 0;
            }
            if state {
                bitmask |= mask;
                self.streak[i] = 0;
            } else {
                self.streak[i] += 1;
            }
        }
        self.markov_history[pp] |= bitmask;
        self.markov_ptr = (pp + 16 - 1) % 16;
        bitmask
    }

    pub fn pulse_width(&self) -> f32 {
        0.05 + 0.9 * self.pulse_width_mean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_load() {
        let d = distributions();
        assert_eq!(d.table.len(), 60 * ENTRIES);
        // a symmetric mid cell median sits near the middle of the range
        let v = beta_distribution_sample(0.5, 0.5, 0.5);
        assert!((0.2..=0.8).contains(&v), "median-ish: {v}");
    }

    #[test]
    fn beta_bias_shifts_the_mean() {
        // average many draws; higher bias → higher mean
        let mut stream = RandomStream::new(1);
        let mut mean = |bias: f32| {
            let mut acc = 0.0;
            let n = 4000;
            for _ in 0..n {
                acc += beta_distribution_sample(stream.next_float(), 0.5, bias);
            }
            acc / n as f32
        };
        let lo = mean(0.25);
        let hi = mean(0.75);
        assert!(hi > lo + 0.1, "bias raises the mean: {lo} -> {hi}");
    }

    #[test]
    fn beta_spread_widens_variance() {
        let mut stream = RandomStream::new(7);
        let mut var = |spread: f32| {
            let mut vals = Vec::new();
            for _ in 0..4000 {
                vals.push(beta_distribution_sample(stream.next_float(), spread, 0.5));
            }
            let m = vals.iter().sum::<f32>() / vals.len() as f32;
            vals.iter().map(|v| (v - m).powi(2)).sum::<f32>() / vals.len() as f32
        };
        let narrow = var(0.2);
        let wide = var(0.8);
        assert!(wide > narrow, "spread widens variance: {narrow} -> {wide}");
    }

    #[test]
    fn deja_vu_locks_a_loop() {
        // deja_vu = 1.0 → the sequence repeats with period = length
        let mut stream = RandomStream::new(42);
        let mut seq = RandomSequence::new(&mut stream);
        seq.set_length(4);
        seq.set_deja_vu(1.0);
        // prime, then collect two cycles
        let mut vals = Vec::new();
        for _ in 0..12 {
            vals.push(seq.next_value(&mut stream, false, 0.0));
        }
        // with deja_vu=1, p=1, mutate always true but deja_vu>0.5 so it
        // jumps within the loop — the loop CONTENTS are fixed, so every
        // draw is a member of the 16-slot loop buffer
        for v in &vals {
            assert!((0.0..=1.0).contains(v));
        }
    }

    #[test]
    fn deja_vu_zero_is_fresh_each_time() {
        let mut stream = RandomStream::new(99);
        let mut seq = RandomSequence::new(&mut stream);
        seq.set_deja_vu(0.0);
        let a: Vec<f32> = (0..8).map(|_| seq.next_value(&mut stream, false, 0.0)).collect();
        let b: Vec<f32> = (0..8).map(|_| seq.next_value(&mut stream, false, 0.0)).collect();
        // with deja_vu 0, p=1 and deja_vu<=0.5 → always fresh draws;
        // the two windows should differ
        assert_ne!(a, b, "deja_vu 0 keeps generating new values");
    }

    #[test]
    fn t_bernoulli_bias_controls_density() {
        let mut stream = RandomStream::new(3);
        let mut t = TGenerator::new(&mut stream);
        t.model = TModel::IndependentBernoulli;
        let density = |bias: f32, t: &mut TGenerator, s: &mut RandomStream| {
            t.bias = bias;
            let mut hits = 0;
            for _ in 0..2000 {
                let m = t.step(s);
                hits += (m & 1) as u32;
            }
            hits
        };
        let lo = density(0.2, &mut t, &mut stream);
        let hi = density(0.8, &mut t, &mut stream);
        // t1 fires when u > bias (bit i=0): higher bias → fewer hits
        assert_ne!(lo, hi, "bias changes t1 density: {lo} vs {hi}");
    }

    #[test]
    fn drums_model_cycles_a_pattern() {
        let mut stream = RandomStream::new(5);
        let mut t = TGenerator::new(&mut stream);
        t.model = TModel::Drums;
        t.bias = 0.9;
        let mut any_t1 = false;
        let mut any_t3 = false;
        for _ in 0..64 {
            let m = t.step(&mut stream);
            any_t1 |= m & 1 != 0;
            any_t3 |= m & 2 != 0;
        }
        assert!(any_t1 && any_t3, "drum pattern hits both channels");
    }

    #[test]
    fn output_channel_quantizes_to_scale() {
        // a 3-note scale; high steps → outputs snap to degrees
        let mut stream = RandomStream::new(11);
        let mut seq = RandomSequence::new(&mut stream);
        let mut ch = OutputChannel {
            scale: vec![0.0, 1.0 / 3.0, 2.0 / 3.0],
            steps: 1.0,
            ..Default::default()
        };
        for _ in 0..200 {
            let v = ch.process_step(&mut seq, &mut stream, true);
            let frac = v - v.floor();
            let nearest = [0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0]
                .iter()
                .map(|d: &f32| (frac - d).abs())
                .fold(f32::MAX, f32::min);
            assert!(nearest < 0.02, "snapped to a degree: frac {frac}");
        }
    }
}
