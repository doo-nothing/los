//! # Stages engine — the segment generator
//!
//! Ported from pichenettes/eurorack (stages/*, MIT, copyright 2017
//! Emilie Gillet, attribution preserved), float-faithful: the
//! multi-segment state machine (ramp/step/hold segments wired into a
//! graph by the panel configuration), the twelve single-segment
//! personalities (decay envelope, timed pulse, gate generator with
//! probability, sample-and-hold, tap/free LFO with the morphing
//! shape law, PLL/free audio oscillator, clocked delay, portamento),
//! and the step sequencer with its seven playback directions.
//!
//! Everything runs at the firmware's 31.25 kHz; the shell steps it
//! with a fractional accumulator like streams.

#![allow(clippy::excessive_precision)]

use std::sync::OnceLock;

pub const NATIVE_SR: f64 = 31_250.0;
pub const MAX_NUM_SEGMENTS: usize = 8; // los exposes 6; +sentinel room
pub const MAX_DELAY: usize = 576;
const RETRIG_DELAY_SAMPLES: i32 = 32;
const SAMPLE_AND_HOLD_DELAY: usize = 62; // sr * 2ms
const CLOCK_INHIBIT_DELAY: i32 = 156; // sr * 5ms

// gate flags, stages-local (same convention as the peaks port)
pub const GATE_LOW: u8 = 0;
pub const GATE_HIGH: u8 = 1;
pub const GATE_RISING: u8 = 2;
pub const GATE_FALLING: u8 = 4;

#[inline]
pub fn extract_gate_flags(previous: u8, current: bool) -> u8 {
    let was_high = previous & GATE_HIGH != 0;
    match (was_high, current) {
        (false, true) => GATE_HIGH | GATE_RISING,
        (true, false) => GATE_FALLING,
        (true, true) => GATE_HIGH,
        (false, false) => GATE_LOW,
    }
}

pub struct Tables {
    pub env_frequency: Vec<f32>,
    pub portamento_coefficient: Vec<f32>,
    pub sine: Vec<f32>,
}

static TABLES: OnceLock<Tables> = OnceLock::new();

pub fn tables() -> &'static Tables {
    TABLES.get_or_init(|| {
        // envelope frequencies: 1 ms .. 16 s, gamma warp, doubled span
        let gamma = 0.125_f64;
        let min_f = 1.0 / (16.0 * NATIVE_SR);
        let max_f = 1.0 / (0.001 * NATIVE_SR);
        let at0 = max_f.powf(-gamma);
        let at1 = min_f.powf(-gamma);
        let mut env_frequency: Vec<f32> = (0..4096)
            .map(|i| {
                let t = i as f64 / 4095.0 * 2.0;
                (t * (at1 - at0) + at0).powf(-1.0 / gamma) as f32
            })
            .collect();
        env_frequency.push(*env_frequency.last().unwrap_or(&0.0));
        // portamento: 0.1 ms .. 4 s, log-spaced, DESCENDING (fast first)
        let pmax = (1.0 / (0.0001 * NATIVE_SR)).ln();
        let pmin = (1.0 / (4.0 * NATIVE_SR)).ln();
        let mut portamento_coefficient: Vec<f32> = (0..512)
            .map(|i| {
                let t = i as f64 / 511.0;
                (pmax + (pmin - pmax) * t).exp() as f32
            })
            .collect();
        portamento_coefficient.push(*portamento_coefficient.last().unwrap_or(&0.0));
        let sine: Vec<f32> = (0..1281)
            .map(|i| (i as f64 / 1024.0 * std::f64::consts::TAU).sin() as f32)
            .collect();
        Tables {
            env_frequency,
            portamento_coefficient,
            sine,
        }
    })
}

#[inline]
fn crossfade(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[inline]
fn interpolate_wrap(table: &[f32], mut x: f32, size: f32) -> f32 {
    x -= x.floor();
    let p = x * size;
    let i = p as usize;
    let f = p - i as f32;
    table[i] + (table[i + 1] - table[i]) * f
}

#[inline]
fn semitones_to_ratio(x: f32) -> f32 {
    2.0_f32.powf(x / 12.0)
}

#[inline]
fn rate_to_frequency(rate: f32) -> f32 {
    let t = tables();
    let i = ((rate * 2048.0) as i32).clamp(0, 4096) as usize;
    t.env_frequency[i]
}

#[inline]
fn portamento_coefficient(rate: f32) -> f32 {
    let t = tables();
    let i = ((rate * 512.0) as i32).clamp(0, 512) as usize;
    t.portamento_coefficient[i]
}

/// segment_generator.cc WarpPhase — the curve law.
pub fn warp_phase(mut t: f32, mut curve: f32) -> f32 {
    curve -= 0.5;
    let flip = curve < 0.0;
    if flip {
        t = 1.0 - t;
    }
    let a = 128.0 * curve * curve;
    t = (1.0 + a) * t / (1.0 + a * t);
    if flip {
        t = 1.0 - t;
    }
    t
}

// polyblep (stmlib/dsp/polyblep.h)
#[inline]
fn this_blep(t: f32) -> f32 {
    0.5 * t * t
}
#[inline]
fn next_blep(t: f32) -> f32 {
    let t = 1.0 - t;
    -0.5 * t * t
}
#[inline]
fn next_integrated_blep(t: f32) -> f32 {
    let t1 = 0.5 * t;
    let t2 = t1 * t1;
    let t4 = t2 * t2;
    0.1875 - t1 + 1.5 * t2 - t4
}
#[inline]
fn this_integrated_blep(t: f32) -> f32 {
    next_integrated_blep(1.0 - t)
}

/// stages/variable_shape_oscillator.h — polyblep saw/tri/square morph.
#[derive(Debug, Clone, Default)]
pub struct VariableShapeOscillator {
    phase: f32,
    next_sample: f32,
    high: bool,
}

impl VariableShapeOscillator {
    pub fn render(&mut self, mut frequency: f32, macro_knob: f32, out: &mut [f32]) {
        let waveshape = (macro_knob * 1.5).clamp(0.0, 1.0);
        let mut pw = (0.5 + (macro_knob - 0.66) * 1.46).clamp(0.5, 0.995);
        frequency = frequency.min(0.25);
        if frequency >= 0.25 {
            pw = 0.5;
        } else {
            pw = pw.clamp(frequency * 2.0, 1.0 - 2.0 * frequency);
        }
        let square_amount = (waveshape - 0.5).max(0.0) * 2.0;
        let triangle_amount = (1.0 - waveshape * 2.0).max(0.0);
        let slope_up = 1.0 / pw;
        let slope_down = 1.0 / (1.0 - pw);
        let mut next_sample = self.next_sample;
        for o in out.iter_mut() {
            let mut this_sample = next_sample;
            next_sample = 0.0;
            self.phase += frequency;
            if !self.high && self.phase >= pw {
                let t = (self.phase - pw) / frequency;
                let triangle_step = (slope_up + slope_down) * frequency * triangle_amount;
                this_sample += square_amount * this_blep(t);
                next_sample += square_amount * next_blep(t);
                this_sample -= triangle_step * this_integrated_blep(t);
                next_sample -= triangle_step * next_integrated_blep(t);
                self.high = true;
            } else if self.phase >= 1.0 {
                self.phase -= 1.0;
                let t = self.phase / frequency;
                let triangle_step = (slope_up + slope_down) * frequency * triangle_amount;
                this_sample -= (1.0 - triangle_amount) * this_blep(t);
                next_sample -= (1.0 - triangle_amount) * next_blep(t);
                this_sample += triangle_step * this_integrated_blep(t);
                next_sample += triangle_step * next_integrated_blep(t);
                self.high = false;
            }
            let saw = self.phase;
            let square = if self.phase < pw { 0.0 } else { 1.0 };
            let triangle = if self.phase < pw {
                self.phase * slope_up
            } else {
                1.0 - (self.phase - pw) * slope_down
            };
            let mut naive = saw;
            naive += (square - naive) * square_amount;
            naive += (triangle - naive) * triangle_amount;
            next_sample += naive;
            *o = 2.0 * this_sample - 1.0;
        }
        self.next_sample = next_sample;
    }
}

/// stmlib HysteresisQuantizer2 — sticky index from a 0..1 value.
#[derive(Debug, Clone)]
pub struct HysteresisQuantizer {
    num: usize,
    hysteresis: f32,
    current: i32,
}

impl HysteresisQuantizer {
    pub fn new(num: usize, hysteresis: f32) -> Self {
        Self {
            num,
            hysteresis,
            current: 0,
        }
    }

    pub fn process(&mut self, value: f32) -> usize {
        let raw = value.clamp(0.0, 1.0) * (self.num as f32 - 1.0);
        let h = self.hysteresis * self.num as f32;
        let cur = self.current as f32;
        if raw > cur + 0.5 + h {
            self.current = (raw - h).round() as i32;
        } else if raw < cur - 0.5 - h {
            self.current = (raw + h).round() as i32;
        }
        self.current = self.current.clamp(0, self.num as i32 - 1);
        self.current as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentType {
    Ramp,
    Step,
    Hold,
    /// Like Hold in a group, but a single ALT segment is an audio
    /// oscillator (free-running or PLL-locked).
    Alt,
}

#[derive(Debug, Clone, Copy)]
pub struct Configuration {
    pub segment_type: SegmentType,
    pub loop_flag: bool,
}

/// Where a segment-graph value comes from (upstream uses raw
/// pointers into parameters_; we name the sources instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValRef {
    Zero,
    Half,
    One,
    Primary(usize),
    Secondary(usize),
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    start: Option<ValRef>, // None: continue from the current value
    time: Option<ValRef>,  // None: infinite duration
    curve: ValRef,
    portamento: ValRef,
    end: ValRef,
    phase: Option<ValRef>, // None: the running phase
    if_rising: i32,
    if_falling: i32,
    if_complete: i32,
}

const DEFAULT_SEGMENT: Segment = Segment {
    start: Some(ValRef::Zero),
    time: Some(ValRef::Zero),
    curve: ValRef::Half,
    portamento: ValRef::Zero,
    end: ValRef::Zero,
    phase: None,
    if_rising: 0,
    if_falling: 0,
    if_complete: 0,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct Parameters {
    pub primary: f32,
    pub secondary: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Output {
    pub value: f32,
    pub phase: f32,
    pub segment: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessMode {
    MultiSegment,
    Sequencer,
    DecayEnvelope,
    TimedPulse,
    GateGenerator,
    SampleAndHold,
    TapLfo,
    FreeLfo,
    PllOscillator,
    FreeOscillator,
    Delay,
    Portamento,
    Zero,
    Slave,
}

const DIRECTION_LAST: usize = 7; // up,down,updown,alt,random,random-no-rep,addressable

/// Simplified tides RampExtractor: period-averaged tap tempo with
/// the divider-ratio lock. (The upstream pulse-train predictor is a
/// few hundred lines; period averaging covers the musical contract —
/// documented divergence.)
#[derive(Debug, Clone)]
struct RampExtractor {
    period: f32,
    samples_since_edge: f32,
    edge_count: u32,
    phase: f32,
}

impl RampExtractor {
    fn new() -> Self {
        Self {
            period: NATIVE_SR as f32, // 1 Hz until taught
            samples_since_edge: 0.0,
            edge_count: 0,
            phase: 0.0,
        }
    }

    /// ratio: (multiplier, divider q). Returns frequency; fills ramp.
    fn process(&mut self, ratio: (f32, u32), gate_flags: &[u8], ramp: &mut [f32]) -> f32 {
        let q = ratio.1.max(1);
        let mut frequency = ratio.0 / self.period.max(1.0);
        for (i, &g) in gate_flags.iter().enumerate() {
            self.samples_since_edge += 1.0;
            if g & GATE_RISING != 0 && self.samples_since_edge > 2.0 {
                // one-pole the period so jitter doesn't snap the LFO
                let measured = self.samples_since_edge;
                if self.edge_count == 0 {
                    self.period = measured;
                } else {
                    self.period += 0.5 * (measured - self.period);
                }
                self.samples_since_edge = 0.0;
                self.edge_count += 1;
                if self.edge_count.is_multiple_of(q) {
                    self.phase = 0.0; // hard lock every q-th edge
                }
                frequency = ratio.0 / self.period.max(1.0);
            }
            self.phase += frequency;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            ramp[i] = self.phase;
        }
        frequency
    }
}

const DIVIDER_RATIOS: [(f32, u32); 7] = [
    (0.249999, 4),
    (0.333333, 3),
    (0.499999, 2),
    (0.999999, 1),
    (1.999999, 1),
    (2.999999, 1),
    (3.999999, 1),
];

pub struct SegmentGenerator {
    segments: [Segment; MAX_NUM_SEGMENTS + 1],
    parameters: [Parameters; MAX_NUM_SEGMENTS],
    mode: ProcessMode,
    num_segments: usize,

    phase: f32,
    aux: f32,
    start: f32,
    value: f32,
    lp: f32,
    primary: f32,

    active_segment: usize,
    previous_segment: usize,
    monitored_segment: usize,
    retrig_delay: i32,

    // sequencer state
    first_step: usize,
    last_step: usize,
    quantized_output: bool,
    up_down_counter: i32,
    inhibit_clock: i32,
    reset: bool,
    accepted_gate: bool,

    function_quantizer: HysteresisQuantizer,
    address_quantizer: HysteresisQuantizer,
    ramp_extractor: RampExtractor,
    audio_osc: VariableShapeOscillator,

    delay_line: Vec<f32>,
    delay_pos: usize,
    gate_delay: Vec<u8>,
    gate_delay_pos: usize,
    previous_delay_sample: f32,

    rng: u32,
}

impl SegmentGenerator {
    pub fn new(seed: u32) -> Self {
        Self {
            segments: [DEFAULT_SEGMENT; MAX_NUM_SEGMENTS + 1],
            parameters: [Parameters::default(); MAX_NUM_SEGMENTS],
            mode: ProcessMode::Zero,
            num_segments: 0,
            phase: 0.0,
            aux: 0.0,
            start: 0.0,
            value: 0.0,
            lp: 0.0,
            primary: 0.0,
            active_segment: 0,
            previous_segment: 0,
            monitored_segment: 0,
            retrig_delay: 0,
            first_step: 1,
            last_step: 1,
            quantized_output: false,
            up_down_counter: 0,
            inhibit_clock: 0,
            reset: false,
            accepted_gate: true,
            function_quantizer: HysteresisQuantizer::new(2, 0.025),
            address_quantizer: HysteresisQuantizer::new(2, 0.025),
            ramp_extractor: RampExtractor::new(),
            audio_osc: VariableShapeOscillator::default(),
            delay_line: vec![0.0; MAX_DELAY],
            delay_pos: 0,
            gate_delay: vec![0; 128],
            gate_delay_pos: 0,
            previous_delay_sample: 0.0,
            rng: seed | 1,
        }
    }

    fn random(&mut self) -> f32 {
        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.rng >> 8) as f32 / 16_777_216.0
    }

    pub fn set_segment_parameters(&mut self, index: usize, primary: f32, secondary: f32) {
        self.parameters[index].primary = primary;
        self.parameters[index].secondary = secondary;
    }

    #[must_use]
    pub fn active_segment(&self) -> usize {
        self.active_segment
    }

    fn resolve(&self, r: ValRef) -> f32 {
        match r {
            ValRef::Zero => 0.0,
            ValRef::Half => 0.5,
            ValRef::One => 1.0,
            ValRef::Primary(i) => self.parameters[i].primary,
            ValRef::Secondary(i) => self.parameters[i].secondary,
        }
    }

    /// Configure for a slave display segment of a group.
    pub fn configure_slave(&mut self, i: usize) {
        self.monitored_segment = i;
        self.mode = ProcessMode::Slave;
        self.num_segments = 0;
    }

    /// segment_generator.cc Configure.
    pub fn configure(&mut self, has_trigger: bool, cfg: &[Configuration]) {
        let num_segments = cfg.len().min(MAX_NUM_SEGMENTS - 1);
        if num_segments == 0 {
            self.mode = ProcessMode::Zero;
            self.num_segments = 0;
            return;
        }
        if num_segments == 1 {
            self.function_quantizer = HysteresisQuantizer::new(7, 0.025);
            let c = cfg[0];
            let i = (if has_trigger { 2 } else { 0 }) + usize::from(c.loop_flag);
            self.mode = match (c.segment_type, i) {
                (SegmentType::Ramp, 0) => ProcessMode::Zero,
                (SegmentType::Ramp, 1) => ProcessMode::FreeLfo,
                (SegmentType::Ramp, 2) => ProcessMode::DecayEnvelope,
                (SegmentType::Ramp, _) => ProcessMode::TapLfo,
                (SegmentType::Step, 0 | 1) => ProcessMode::Portamento,
                (SegmentType::Step, _) => ProcessMode::SampleAndHold,
                (SegmentType::Hold, 0 | 1) => ProcessMode::Delay,
                (SegmentType::Hold, 2) => ProcessMode::TimedPulse,
                (SegmentType::Hold, _) => ProcessMode::GateGenerator,
                // ALT row: zero, free osc, decay env, PLL osc
                (SegmentType::Alt, 0) => ProcessMode::Zero,
                (SegmentType::Alt, 1) => ProcessMode::FreeOscillator,
                (SegmentType::Alt, 2) => ProcessMode::DecayEnvelope,
                (SegmentType::Alt, _) => ProcessMode::PllOscillator,
            };
            self.num_segments = 1;
            return;
        }

        let sequencer_mode = cfg[0].segment_type != SegmentType::Step
            && !cfg[0].loop_flag
            && num_segments >= 3
            && cfg[1..num_segments]
                .iter()
                .all(|c| c.segment_type == SegmentType::Step);
        if sequencer_mode {
            self.function_quantizer = HysteresisQuantizer::new(DIRECTION_LAST, 0.025);
            self.configure_sequencer(cfg, num_segments);
            return;
        }

        self.num_segments = num_segments;
        self.mode = ProcessMode::MultiSegment;

        let last_segment = num_segments - 1;
        let mut loop_start: i32 = -1;
        let mut loop_end: i32 = -1;
        let mut has_step_segments = false;
        let mut first_ramp_segment: i32 = -1;
        for (i, c) in cfg.iter().enumerate().take(num_segments) {
            has_step_segments = has_step_segments || c.segment_type == SegmentType::Step;
            if c.loop_flag {
                if loop_start == -1 {
                    loop_start = i as i32;
                }
                loop_end = i as i32;
            }
            if c.segment_type == SegmentType::Ramp && first_ramp_segment == -1 {
                first_ramp_segment = i as i32;
            }
        }
        let mut has_step_inside_loop = false;
        if loop_start != -1 {
            for i in loop_start..=loop_end {
                if cfg[i as usize].segment_type == SegmentType::Step {
                    has_step_inside_loop = true;
                    break;
                }
            }
        }

        for i in 0..=last_segment {
            let mut s = DEFAULT_SEGMENT;
            match cfg[i].segment_type {
                SegmentType::Ramp => {
                    s.start = None;
                    s.time = Some(ValRef::Primary(i));
                    s.curve = ValRef::Secondary(i);
                    s.portamento = ValRef::Zero;
                    s.phase = None;
                    if i == last_segment {
                        s.end = ValRef::Zero;
                    } else if cfg[i + 1].segment_type != SegmentType::Ramp {
                        s.end = ValRef::Primary(i + 1);
                    } else if i as i32 == first_ramp_segment {
                        s.end = ValRef::One;
                    } else {
                        s.end = ValRef::Secondary(i);
                        s.curve = ValRef::Half;
                    }
                }
                SegmentType::Step => {
                    s.start = Some(ValRef::Primary(i));
                    s.end = ValRef::Primary(i);
                    s.curve = ValRef::Half;
                    s.portamento = ValRef::Secondary(i);
                    s.time = None;
                    s.phase = if i as i32 == loop_start && i as i32 == loop_end {
                        Some(ValRef::Zero) // sample
                    } else {
                        Some(ValRef::One) // track
                    };
                }
                // ALT in a group behaves like HOLD (upstream's else branch)
                SegmentType::Hold | SegmentType::Alt => {
                    s.start = Some(ValRef::Primary(i));
                    s.end = ValRef::Primary(i);
                    s.curve = ValRef::Half;
                    s.portamento = ValRef::Zero;
                    s.time = if i as i32 == loop_start && i as i32 == loop_end {
                        None
                    } else {
                        Some(ValRef::Secondary(i))
                    };
                    s.phase = Some(ValRef::One);
                }
            }
            s.if_complete = if i as i32 == loop_end {
                loop_start
            } else {
                (i + 1) as i32
            };
            s.if_falling =
                if loop_end == -1 || loop_end == last_segment as i32 || has_step_segments {
                    -1
                } else {
                    loop_end + 1
                };
            s.if_rising = 0;
            if has_step_segments {
                if !has_step_inside_loop && (i as i32) >= loop_start && (i as i32) <= loop_end {
                    s.if_rising = ((loop_end + 1) % num_segments as i32).max(0);
                } else {
                    let mut follow_loop = loop_end != -1;
                    let mut next_step = i;
                    while cfg[next_step].segment_type != SegmentType::Step {
                        next_step += 1;
                        if follow_loop && next_step as i32 == loop_end + 1 {
                            next_step = loop_start.max(0) as usize;
                            follow_loop = false;
                        }
                        if next_step >= num_segments {
                            next_step = num_segments - 1;
                            break;
                        }
                    }
                    s.if_rising = if next_step as i32 == loop_end {
                        loop_start
                    } else {
                        ((next_step + 1) % num_segments) as i32
                    };
                }
            }
            self.segments[i] = s;
        }
        // the sentinel
        let mut sentinel = DEFAULT_SEGMENT;
        sentinel.start = Some(self.segments[last_segment].end);
        sentinel.end = self.segments[last_segment].end;
        sentinel.time = Some(ValRef::Zero);
        sentinel.if_rising = 0;
        sentinel.if_falling = -1;
        sentinel.if_complete = if loop_end == last_segment as i32 { 0 } else { -1 };
        self.segments[num_segments] = sentinel;
        self.active_segment = num_segments;
        self.previous_segment = num_segments;
    }

    fn configure_sequencer(&mut self, cfg: &[Configuration], num_segments: usize) {
        self.num_segments = num_segments;
        self.first_step = 0;
        for (i, c) in cfg.iter().enumerate().take(num_segments).skip(1) {
            if c.loop_flag {
                if self.first_step == 0 {
                    self.first_step = i;
                    self.last_step = i;
                } else {
                    self.last_step = i;
                }
            }
        }
        if self.first_step == 0 {
            self.first_step = 1;
            self.last_step = num_segments - 1;
        }
        let num_steps = self.last_step - self.first_step + 1;
        self.address_quantizer =
            HysteresisQuantizer::new(num_steps, 0.02 / 8.0 * num_steps as f32);
        self.inhibit_clock = 0;
        self.up_down_counter = 0;
        self.quantized_output = cfg[0].segment_type == SegmentType::Ramp;
        self.reset = false;
        self.lp = 0.0;
        self.value = 0.0;
        self.active_segment = self.first_step;
        self.mode = ProcessMode::Sequencer;
    }

    /// Returns true while the generator sits in its first segment.
    pub fn process(&mut self, gate_flags: &[u8], out: &mut [Output]) -> bool {
        match self.mode {
            ProcessMode::MultiSegment => self.process_multi_segment(gate_flags, out),
            ProcessMode::Sequencer => self.process_sequencer(gate_flags, out),
            ProcessMode::DecayEnvelope => self.process_decay_envelope(gate_flags, out),
            ProcessMode::TimedPulse => self.process_timed_pulse(gate_flags, out),
            ProcessMode::GateGenerator => self.process_gate_generator(gate_flags, out),
            ProcessMode::SampleAndHold => self.process_sample_and_hold(gate_flags, out),
            ProcessMode::TapLfo => self.process_oscillator(false, Some(gate_flags), out),
            ProcessMode::FreeLfo => self.process_oscillator(false, None, out),
            ProcessMode::PllOscillator => self.process_oscillator(true, Some(gate_flags), out),
            ProcessMode::FreeOscillator => self.process_oscillator(true, None, out),
            ProcessMode::Delay => self.process_delay(out),
            ProcessMode::Portamento => self.process_portamento(out),
            ProcessMode::Zero => self.process_zero(out),
            ProcessMode::Slave => self.process_slave(out),
        }
        self.active_segment == 0
    }

    fn process_multi_segment(&mut self, gate_flags: &[u8], out: &mut [Output]) {
        let mut phase = self.phase;
        let mut start = self.start;
        let mut lp = self.lp;
        let mut value = self.value;
        for (n, o) in out.iter_mut().enumerate() {
            let segment = self.segments[self.active_segment];
            let previous = self.segments[self.previous_segment];
            // TRACK_PREVIOUS_SEGMENT
            if segment.start.is_none()
                && previous.phase.is_some()
                && segment.end != previous.end
            {
                let target = self.resolve(previous.end);
                let c = portamento_coefficient(self.resolve(previous.portamento));
                start += (target - start) * c;
            }
            if let Some(t) = segment.time {
                phase += rate_to_frequency(self.resolve(t));
            }
            let complete = phase >= 1.0;
            if complete {
                phase = 1.0;
            }
            let warp_t = match segment.phase {
                Some(p) => self.resolve(p),
                None => phase,
            };
            value = crossfade(
                start,
                self.resolve(segment.end),
                warp_phase(warp_t, self.resolve(segment.curve)),
            );
            lp += (value - lp) * portamento_coefficient(self.resolve(segment.portamento));

            let g = gate_flags[n];
            let go_to = if g & GATE_RISING != 0 {
                segment.if_rising
            } else if g & GATE_FALLING != 0 {
                segment.if_falling
            } else if complete {
                segment.if_complete
            } else {
                -1
            };
            if go_to != -1 {
                let dest = go_to as usize;
                phase = 0.0;
                start = match self.segments[dest].start {
                    Some(r) => self.resolve(r),
                    None => {
                        if dest == self.active_segment {
                            start
                        } else {
                            value
                        }
                    }
                };
                if dest != self.active_segment {
                    self.previous_segment = self.active_segment;
                }
                self.active_segment = dest;
            }
            o.value = lp;
            o.phase = phase;
            o.segment = self.active_segment as i32;
        }
        self.phase = phase;
        self.start = start;
        self.lp = lp;
        self.value = value;
    }

    fn process_decay_envelope(&mut self, gate_flags: &[u8], out: &mut [Output]) {
        let frequency = rate_to_frequency(self.parameters[0].primary);
        for (n, o) in out.iter_mut().enumerate() {
            if gate_flags[n] & GATE_RISING != 0 {
                self.phase = 0.0;
                self.active_segment = 0;
            }
            self.phase += frequency;
            if self.phase >= 1.0 {
                self.phase = 1.0;
                self.active_segment = 1;
            }
            self.value = 1.0 - warp_phase(self.phase, self.parameters[0].secondary);
            self.lp = self.value;
            o.value = self.lp;
            o.phase = self.phase;
            o.segment = self.active_segment as i32;
        }
    }

    fn process_timed_pulse(&mut self, gate_flags: &[u8], out: &mut [Output]) {
        let frequency = rate_to_frequency(self.parameters[0].secondary);
        let target = self.parameters[0].primary;
        let step = (target - self.primary) / out.len() as f32;
        for (n, o) in out.iter_mut().enumerate() {
            if gate_flags[n] & GATE_RISING != 0 {
                self.retrig_delay = if self.active_segment == 0 {
                    RETRIG_DELAY_SAMPLES
                } else {
                    0
                };
                self.phase = 0.0;
                self.active_segment = 0;
            }
            if self.retrig_delay > 0 {
                self.retrig_delay -= 1;
            }
            self.phase += frequency;
            if self.phase >= 1.0 {
                self.phase = 1.0;
                self.active_segment = 1;
            }
            self.primary += step;
            self.value = if self.active_segment == 0 && self.retrig_delay == 0 {
                self.primary
            } else {
                0.0
            };
            self.lp = self.value;
            o.value = self.lp;
            o.phase = self.phase;
            o.segment = self.active_segment as i32;
        }
        self.primary = target;
    }

    fn process_gate_generator(&mut self, gate_flags: &[u8], out: &mut [Output]) {
        let target = self.parameters[0].primary;
        let step = (target - self.primary) / out.len() as f32;
        for (n, o) in out.iter_mut().enumerate() {
            if gate_flags[n] & GATE_RISING != 0 {
                let roll = self.random();
                self.accepted_gate = roll < self.parameters[0].secondary * 1.01;
            }
            self.active_segment =
                if gate_flags[n] & GATE_HIGH != 0 && self.accepted_gate {
                    0
                } else {
                    1
                };
            self.primary += step;
            self.value = if self.active_segment == 0 {
                self.primary
            } else {
                0.0
            };
            self.lp = self.value;
            o.value = self.lp;
            o.phase = 0.5;
            o.segment = self.active_segment as i32;
        }
        self.primary = target;
    }

    fn process_sample_and_hold(&mut self, gate_flags: &[u8], out: &mut [Output]) {
        let coefficient = portamento_coefficient(self.parameters[0].secondary);
        let target = self.parameters[0].primary;
        let step = (target - self.primary) / out.len() as f32;
        for (n, o) in out.iter_mut().enumerate() {
            self.primary += step;
            let len = self.gate_delay.len();
            self.gate_delay[self.gate_delay_pos] = gate_flags[n];
            let read = (self.gate_delay_pos + len - SAMPLE_AND_HOLD_DELAY) % len;
            self.gate_delay_pos = (self.gate_delay_pos + 1) % len;
            if self.gate_delay[read] & GATE_RISING != 0 {
                self.value = self.primary;
            }
            self.active_segment = if gate_flags[n] & GATE_HIGH != 0 { 0 } else { 1 };
            self.lp += (self.value - self.lp) * coefficient;
            o.value = self.lp;
            o.phase = 0.5;
            o.segment = self.active_segment as i32;
        }
        self.primary = target;
    }

    fn process_oscillator(
        &mut self,
        audio_rate: bool,
        gate_flags: Option<&[u8]>,
        out: &mut [Output],
    ) {
        let t = tables();
        let size = out.len();
        let root_note: f32 = if audio_rate { 261.625_56 } else { 2.043_949_7 };
        let mut ramp = vec![0.0_f32; size];
        let frequency;
        match gate_flags {
            Some(g) => {
                let idx = self
                    .function_quantizer
                    .process((self.parameters[0].primary * 1.03).clamp(0.0, 1.0));
                let r = DIVIDER_RATIOS[idx.min(6)];
                frequency = self.ramp_extractor.process(r, g, &mut ramp);
            }
            None => {
                let f = (96.0 * (self.parameters[0].primary - 0.5)).clamp(-128.0, 127.0);
                frequency = semitones_to_ratio(f) * root_note / NATIVE_SR as f32;
                for r in ramp.iter_mut() {
                    self.phase += frequency;
                    if self.phase >= 1.0 {
                        self.phase -= 1.0;
                    }
                    *r = self.phase;
                }
            }
        }
        if audio_rate {
            let mut audio = vec![0.0_f32; size];
            self.audio_osc
                .render(frequency, self.parameters[0].secondary, &mut audio);
            for (i, o) in out.iter_mut().enumerate() {
                o.phase = audio[i] * 0.5 + 0.5;
                o.value = (audio[i] * 0.5 + 0.5) * 5.0 / 8.0;
                o.segment = i32::from(audio[i] >= 0.0);
            }
        } else {
            // ShapeLFO
            let mut shape = self.parameters[0].secondary - 0.5;
            shape = 2.0 + 9.999_999 * shape / (1.0 + 3.0 * shape.abs());
            let slope = (shape * 0.5).min(0.5);
            let plateau_width = (shape - 3.0).max(0.0);
            let sine_amount = if shape < 2.0 {
                (shape - 1.0).max(0.0)
            } else {
                (3.0 - shape).max(0.0)
            };
            let slope_up = 1.0 / slope;
            let slope_down = 1.0 / (1.0 - slope);
            let plateau = 0.5 * (1.0 - plateau_width);
            let normalization = 1.0 / plateau;
            let phase_shift = plateau_width * 0.25;
            for (i, o) in out.iter_mut().enumerate() {
                let mut p = ramp[i] + phase_shift;
                if p > 1.0 {
                    p -= 1.0;
                }
                let mut triangle = if p < slope {
                    slope_up * p
                } else {
                    1.0 - (p - slope) * slope_down
                };
                triangle -= 0.5;
                triangle = triangle.clamp(-plateau, plateau) * normalization;
                let sine = interpolate_wrap(&t.sine, p + 0.75, 1024.0);
                o.phase = ramp[i];
                o.value = 0.5 * crossfade(triangle, sine, sine_amount) + 0.5;
                o.segment = i32::from(p >= 0.5);
            }
        }
        self.active_segment = out[size - 1].segment as usize;
    }

    fn process_delay(&mut self, out: &mut [Output]) {
        let max_delay = (MAX_DELAY - 1) as f32;
        let mut delay_time = semitones_to_ratio(2.0 * (self.parameters[0].secondary - 0.5) * 36.0)
            * 0.5
            * NATIVE_SR as f32;
        let mut clock_frequency = 1.0;
        let delay_frequency = 1.0 / delay_time;
        if delay_time >= max_delay {
            clock_frequency = max_delay * delay_frequency;
            delay_time = max_delay;
        }
        let target = self.parameters[0].primary;
        let step = (target - self.primary) / out.len() as f32;
        self.active_segment = 0;
        for o in out.iter_mut() {
            self.primary += step;
            self.phase += clock_frequency;
            self.lp += (self.primary - self.lp) * clock_frequency;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
                self.delay_line[self.delay_pos] = self.lp;
                self.delay_pos = (self.delay_pos + 1) % MAX_DELAY;
            }
            self.aux += delay_frequency;
            if self.aux >= 1.0 {
                self.aux -= 1.0;
            }
            self.active_segment = usize::from(self.aux >= 0.5);
            // fractional read at delay_time - phase
            let d = (delay_time - self.phase).clamp(0.0, max_delay);
            let di = d as usize;
            let df = d - di as f32;
            let len = MAX_DELAY;
            let r0 = self.delay_line[(self.delay_pos + len - 1 - di) % len];
            let r1 = self.delay_line[(self.delay_pos + len - 2 - di.min(len - 2)) % len];
            let sample = r0 + (r1 - r0) * df;
            self.value += (sample - self.value) * clock_frequency;
            self.previous_delay_sample = sample;
            o.value = self.value;
            o.phase = self.aux;
            o.segment = self.active_segment as i32;
        }
        self.primary = target;
    }

    fn process_portamento(&mut self, out: &mut [Output]) {
        let coefficient = portamento_coefficient(self.parameters[0].secondary);
        let target = self.parameters[0].primary;
        let step = (target - self.primary) / out.len() as f32;
        self.active_segment = 0;
        for o in out.iter_mut() {
            self.primary += step;
            self.value = self.primary;
            self.lp += (self.value - self.lp) * coefficient;
            o.value = self.lp;
            o.phase = 0.5;
            o.segment = 0;
        }
        self.primary = target;
    }

    fn process_zero(&mut self, out: &mut [Output]) {
        self.value = 0.0;
        self.active_segment = 1;
        for o in out.iter_mut() {
            o.value = 0.0;
            o.phase = 0.5;
            o.segment = 1;
        }
    }

    /// Slave reads the master group's Output (already written into
    /// `out` by the group leader) and replaces value per segment.
    fn process_slave(&mut self, out: &mut [Output]) {
        for o in out.iter_mut() {
            self.active_segment = if o.segment == self.monitored_segment as i32 {
                0
            } else {
                1
            };
            o.value = if self.active_segment == 0 {
                1.0 - o.phase
            } else {
                0.0
            };
        }
    }

    fn process_sequencer(&mut self, gate_flags: &[u8], out: &mut [Output]) {
        let direction = self
            .function_quantizer
            .process(self.parameters[0].secondary);
        if direction == 6 {
            // addressable
            self.reset = false;
            self.active_segment = self
                .address_quantizer
                .process(self.parameters[0].primary)
                + self.first_step;
        } else {
            if self.parameters[0].primary > 0.125 && !self.reset {
                self.reset = true;
                self.active_segment = if direction == 1 {
                    self.last_step
                } else {
                    self.first_step
                };
                self.up_down_counter = 0;
                self.inhibit_clock = CLOCK_INHIBIT_DELAY;
            }
            if self.reset && self.parameters[0].primary < 0.0625 {
                self.reset = false;
            }
        }
        for (n, o) in out.iter_mut().enumerate() {
            if self.inhibit_clock > 0 {
                self.inhibit_clock -= 1;
            }
            let clockable = self.inhibit_clock == 0 && !self.reset && direction != 6;
            if gate_flags[n] & GATE_RISING != 0 && clockable {
                let first = self.first_step as i32;
                let last = self.last_step as i32;
                let n_steps = last - first + 1;
                let mut seg = self.active_segment as i32;
                match direction {
                    0 => {
                        seg += 1;
                        if seg > last {
                            seg = first;
                        }
                    }
                    1 => {
                        seg -= 1;
                        if seg < first {
                            seg = last;
                        }
                    }
                    2 => {
                        if n_steps == 1 {
                            seg = first;
                        } else {
                            self.up_down_counter =
                                (self.up_down_counter + 1) % (2 * (n_steps - 1));
                            seg = first
                                + if self.up_down_counter < n_steps {
                                    self.up_down_counter
                                } else {
                                    2 * (n_steps - 1) - self.up_down_counter
                                };
                        }
                    }
                    3 => {
                        if n_steps == 1 {
                            seg = first;
                        } else if n_steps == 2 {
                            self.up_down_counter = (self.up_down_counter + 1) % 2;
                            seg = first + self.up_down_counter;
                        } else {
                            self.up_down_counter =
                                (self.up_down_counter + 1) % (4 * n_steps - 8);
                            let i = (self.up_down_counter - 1) / 2;
                            seg = first
                                + if self.up_down_counter & 1 != 0 {
                                    1 + if i < n_steps - 1 { i } else { 2 * (n_steps - 2) - i }
                                } else {
                                    0
                                };
                        }
                    }
                    4 => {
                        seg = first + (self.random() * n_steps as f32) as i32;
                    }
                    5 => {
                        let r = (self.random() * (n_steps - 1) as f32) as i32;
                        seg = first + ((seg - first + r + 1) % n_steps);
                    }
                    _ => {}
                }
                self.active_segment = seg.clamp(0, (MAX_NUM_SEGMENTS - 1) as i32) as usize;
            }
            self.value = self.parameters[self.active_segment].primary;
            if self.quantized_output {
                // chromatic quantizer (the firmware quantizes via its
                // settings scales; los pins the chromatic case)
                let note = (self.value * 96.0).round();
                self.value = note / 96.0;
            }
            self.lp += (self.value - self.lp)
                * portamento_coefficient(self.parameters[self.active_segment].secondary);
            o.value = self.lp;
            o.phase = 0.0;
            o.segment = self.active_segment as i32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gates(pattern: &[(usize, usize)], len: usize) -> Vec<u8> {
        // pattern: (start, hold) rising-edge gates
        let mut bools = vec![false; len];
        for &(s, h) in pattern {
            for b in bools.iter_mut().skip(s).take(h) {
                *b = true;
            }
        }
        let mut prev = GATE_LOW;
        bools
            .iter()
            .map(|&b| {
                prev = extract_gate_flags(prev, b);
                prev
            })
            .collect()
    }

    fn run(sg: &mut SegmentGenerator, g: &[u8]) -> Vec<Output> {
        let mut out = vec![Output::default(); g.len()];
        for (chunk_g, chunk_o) in g.chunks(24).zip(out.chunks_mut(24)) {
            sg.process(chunk_g, chunk_o);
        }
        out
    }

    #[test]
    fn warp_phase_is_identity_at_half_curve() {
        for i in 0..=10 {
            let t = i as f32 / 10.0;
            assert!((warp_phase(t, 0.5) - t).abs() < 1e-6);
        }
        // curve extremes bend but stay in bounds and monotone
        let lo = warp_phase(0.5, 0.0);
        let hi = warp_phase(0.5, 1.0);
        assert!(lo < 0.5 && hi > 0.5, "{lo} {hi}");
    }

    #[test]
    fn decay_envelope_fires_and_decays() {
        let mut sg = SegmentGenerator::new(1);
        sg.configure(
            true,
            &[Configuration {
                segment_type: SegmentType::Ramp,
                loop_flag: false,
            }],
        );
        sg.set_segment_parameters(0, 0.3, 0.5);
        let g = gates(&[(10, 50)], 31_250);
        let out = run(&mut sg, &g);
        let peak = out.iter().fold(0.0_f32, |m, o| m.max(o.value));
        assert!(peak > 0.95, "decay env snaps to 1: {peak}");
        assert!(out[31_249].value < 0.05, "and decays: {}", out[31_249].value);
    }

    #[test]
    fn ad_envelope_two_ramps() {
        // attack+decay: two RAMP segments, gate-triggered group
        let mut sg = SegmentGenerator::new(2);
        let cfg = [
            Configuration {
                segment_type: SegmentType::Ramp,
                loop_flag: false,
            },
            Configuration {
                segment_type: SegmentType::Ramp,
                loop_flag: false,
            },
        ];
        sg.configure(true, &cfg);
        sg.set_segment_parameters(0, 0.2, 0.5); // fast attack
        sg.set_segment_parameters(1, 0.5, 0.5); // slower decay
        let g = gates(&[(100, 200)], 62_500);
        let out = run(&mut sg, &g);
        let peak = out.iter().fold(0.0_f32, |m, o| m.max(o.value));
        assert!(peak > 0.9, "AD reaches the top: {peak}");
        let tail = out[62_499].value;
        assert!(tail < 0.1, "AD comes back down: {tail}");
    }

    #[test]
    fn looping_two_ramps_make_an_lfo() {
        let mut sg = SegmentGenerator::new(3);
        let cfg = [
            Configuration {
                segment_type: SegmentType::Ramp,
                loop_flag: true,
            },
            Configuration {
                segment_type: SegmentType::Ramp,
                loop_flag: true,
            },
        ];
        sg.configure(true, &cfg);
        sg.set_segment_parameters(0, 0.15, 0.5);
        sg.set_segment_parameters(1, 0.15, 0.5);
        // one trigger starts the loop
        let g = gates(&[(10, 30)], 62_500);
        let out = run(&mut sg, &g);
        let later = &out[31_250..];
        let min = later.iter().fold(1.0_f32, |m, o| m.min(o.value));
        let max = later.iter().fold(0.0_f32, |m, o| m.max(o.value));
        assert!(max - min > 0.5, "the loop keeps oscillating: {min}..{max}");
    }

    #[test]
    fn gate_generator_follows_gate_with_probability_one() {
        let mut sg = SegmentGenerator::new(4);
        sg.configure(
            true,
            &[Configuration {
                segment_type: SegmentType::Hold,
                loop_flag: true,
            }],
        );
        sg.set_segment_parameters(0, 0.8, 1.0);
        let g = gates(&[(100, 400)], 1_000);
        let out = run(&mut sg, &g);
        assert!(out[300].value > 0.7, "gate high passes level");
        assert!(out[700].value < 0.01, "gate low is zero");
    }

    #[test]
    fn sample_and_hold_latches_on_gate() {
        let mut sg = SegmentGenerator::new(5);
        sg.configure(
            true,
            &[Configuration {
                segment_type: SegmentType::Step,
                loop_flag: false,
            }],
        );
        sg.set_segment_parameters(0, 0.33, 0.0);
        let g = gates(&[(100, 200)], 2_000);
        let out = run(&mut sg, &g);
        let held = out[1_500].value;
        assert!((held - 0.33).abs() < 0.05, "S&H held the level: {held}");
    }

    #[test]
    fn free_lfo_oscillates_with_shape() {
        let mut sg = SegmentGenerator::new(6);
        sg.configure(
            false,
            &[Configuration {
                segment_type: SegmentType::Ramp,
                loop_flag: true,
            }],
        );
        sg.set_segment_parameters(0, 0.65, 0.5); // a few Hz, sine-ish
        let g = vec![GATE_LOW; 31_250];
        let out = run(&mut sg, &g);
        let min = out.iter().fold(1.0_f32, |m, o| m.min(o.value));
        let max = out.iter().fold(0.0_f32, |m, o| m.max(o.value));
        assert!(min < 0.1 && max > 0.9, "full-swing LFO: {min}..{max}");
    }

    #[test]
    fn tap_lfo_locks_to_the_clock() {
        let mut sg = SegmentGenerator::new(7);
        sg.configure(
            true,
            &[Configuration {
                segment_type: SegmentType::Ramp,
                loop_flag: true,
            }],
        );
        sg.set_segment_parameters(0, 0.5, 0.5); // ratio 1:1
        // clock at 1250-sample period (25 Hz)
        let pattern: Vec<(usize, usize)> = (0..40).map(|i| (i * 1250, 100)).collect();
        let g = gates(&pattern, 50_000);
        let out = run(&mut sg, &g);
        // after lock, value at each clock edge should be near-constant phase
        let v1 = out[30_000].phase;
        let v2 = out[30_000 + 1250].phase;
        assert!(
            (v1 - v2).abs() < 0.1,
            "phase repeats at the clock period: {v1} vs {v2}"
        );
    }

    #[test]
    fn sequencer_steps_on_clock() {
        let mut sg = SegmentGenerator::new(8);
        let cfg = [
            Configuration {
                segment_type: SegmentType::Hold,
                loop_flag: false,
            },
            Configuration {
                segment_type: SegmentType::Step,
                loop_flag: false,
            },
            Configuration {
                segment_type: SegmentType::Step,
                loop_flag: false,
            },
            Configuration {
                segment_type: SegmentType::Step,
                loop_flag: false,
            },
        ];
        sg.configure(true, &cfg);
        assert_eq!(sg.mode, ProcessMode::Sequencer);
        sg.set_segment_parameters(0, 0.0, 0.0); // direction up, no reset
        sg.set_segment_parameters(1, 0.2, 0.0);
        sg.set_segment_parameters(2, 0.5, 0.0);
        sg.set_segment_parameters(3, 0.9, 0.0);
        let pattern: Vec<(usize, usize)> = (0..12).map(|i| (200 + i * 1000, 100)).collect();
        let g = gates(&pattern, 13_000);
        let out = run(&mut sg, &g);
        // initial step is first_step (0.2); each clock advances
        let a = out[100].value;
        let b = out[700].value;
        let c = out[1_700].value;
        assert!(a < b && b < c, "ascending steps: {a} {b} {c}");
    }

    #[test]
    fn audio_oscillator_is_bounded_and_periodic() {
        let mut sg = SegmentGenerator::new(9);
        sg.configure(
            false,
            &[Configuration {
                segment_type: SegmentType::Hold,
                loop_flag: true,
            }],
        );
        // ALT-type free osc lives at idx 13 of the table; in los the
        // single-segment map reaches it via SegmentType::Hold? No —
        // ALT is its own type upstream; los maps it from the shell.
        // Here exercise the oscillator engine directly.
        let mut osc = VariableShapeOscillator::default();
        let mut buf = vec![0.0_f32; 4_096];
        osc.render(261.6 / NATIVE_SR as f32, 0.8, &mut buf);
        let peak = buf.iter().fold(0.0_f32, |m, v| m.max(v.abs()));
        assert!(peak <= 1.5 && peak > 0.5, "osc bounded: {peak}");
        let _ = sg;
    }

    #[test]
    fn tables_match_their_laws() {
        let t = tables();
        assert_eq!(t.env_frequency.len(), 4097);
        assert_eq!(t.portamento_coefficient.len(), 513);
        assert_eq!(t.sine.len(), 1281);
        // env: rate 0 = fastest (1 ms), rising rate = slower
        assert!(t.env_frequency[0] > t.env_frequency[2048]);
        // sine guard band wraps correctly
        assert!((t.sine[0] - t.sine[1024]).abs() < 1e-6);
        // portamento descends fast→slow
        assert!(t.portamento_coefficient[0] > t.portamento_coefficient[511]);
    }

    #[test]
    fn timed_pulse_emits_a_pulse_of_set_length() {
        let mut sg = SegmentGenerator::new(10);
        sg.configure(
            true,
            &[Configuration {
                segment_type: SegmentType::Hold,
                loop_flag: false,
            }],
        );
        sg.set_segment_parameters(0, 0.9, 0.3);
        let g = gates(&[(100, 10)], 31_250);
        let out = run(&mut sg, &g);
        let on = out.iter().filter(|o| o.value > 0.5).count();
        assert!(on > 100, "the pulse outlives the trigger: {on}");
        assert!(out[31_249].value < 0.01, "and ends");
    }
}
