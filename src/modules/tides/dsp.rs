//! # Tides (2018) DSP — the PolySlopeGenerator and its ramps
//!
//! A faithful port of Mutable Instruments Tides 2's slope engine:
//! the four-channel ramp generator (AD / looping / AR, with the
//! polyrhythmic ratio tables), the band-limited ramp shaper, the
//! two-bank waveshaper wavetable (transcribed from the generators in
//! resources/lookup_tables.py rather than embedded), the wavefolders,
//! and the smoothness filter.
//!
//! Ported from pichenettes/eurorack (tides2/*, stmlib/dsp/*),
//! copyright 2017 Emilie Gillet, MIT license; attribution preserved.
//!
//! The firmware runs at 62.5 kHz in blocks of 8; everything here is
//! expressed in normalized frequency (cycles/sample), so the engine
//! is sample-rate honest by construction.

#![allow(clippy::excessive_precision)]

pub const NUM_CHANNELS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RampMode {
    Ad,
    #[default]
    Looping,
    Ar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    Gates,
    Amplitude,
    #[default]
    SlopePhase,
    Frequency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Range {
    #[default]
    Control,
    Audio,
}

// ── gate flags (stmlib/utils/gate_flags.h) ─────────────────────────────────

pub const GATE_FLAG_LOW: u8 = 0;
pub const GATE_FLAG_HIGH: u8 = 1;
pub const GATE_FLAG_RISING: u8 = 2;
pub const GATE_FLAG_FALLING: u8 = 4;

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

// ── polyblep (stmlib/dsp/polyblep.h) ───────────────────────────────────────

#[inline]
fn this_blep_sample(t: f32) -> f32 {
    0.5 * t * t
}

#[inline]
fn next_blep_sample(t: f32) -> f32 {
    let t = 1.0 - t;
    -0.5 * t * t
}

#[inline]
fn next_integrated_blep_sample(t: f32) -> f32 {
    let t1 = 0.5 * t;
    let t2 = t1 * t1;
    let t4 = t2 * t2;
    0.1875 - t1 + 1.5 * t2 - t4
}

#[inline]
fn this_integrated_blep_sample(t: f32) -> f32 {
    next_integrated_blep_sample(1.0 - t)
}

// ── hysteresis quantizer (stmlib) ──────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct HysteresisQuantizer {
    num_steps: i32,
    hysteresis: f32,
    scale: f32,
    offset: f32,
    quantized: i32,
}

impl HysteresisQuantizer {
    pub fn new(num_steps: usize, hysteresis: f32) -> Self {
        // non-symmetric flavor (tides2's ratio quantizer)
        HysteresisQuantizer {
            num_steps: num_steps as i32,
            hysteresis,
            scale: num_steps as f32,
            offset: -0.5,
            quantized: 0,
        }
    }

    pub fn process(&mut self, value: f32) -> usize {
        let value = value * self.scale + self.offset;
        let sign = if value > self.quantized as f32 {
            -1.0
        } else {
            1.0
        };
        let q = (value + sign * self.hysteresis + 0.5).floor() as i32;
        let q = q.clamp(0, self.num_steps - 1);
        self.quantized = q;
        q as usize
    }
}

// ── wavetables (resources/lookup_tables.py, transcribed) ───────────────────

const WS_SIZE: usize = 1024;
/// Per-shape stride in the wavetable: 1024 entries + a guard.
pub const SHAPE_STRIDE: usize = WS_SIZE + 1;
/// 5 audio shapes + 7 control shapes.
pub const NUM_SHAPES: usize = 12;

/// Build the 12-shape waveshaper bank exactly as the generator does
/// (audio: inverse_tan, inverse_sin, linear, sin, bump; control:
/// log, log-flipped, inverse_sin, linear, sin, expo-flipped, expo —
/// each control shape mirrored around its midpoint).
pub fn build_wavetable() -> Vec<f32> {
    let mut table: Vec<f32> = Vec::with_capacity(NUM_SHAPES * SHAPE_STRIDE);
    let pi = std::f32::consts::PI;

    // audio-rate shapes over 1024 points
    let xs: Vec<f32> = (0..WS_SIZE).map(|i| i as f32 / WS_SIZE as f32).collect();
    let tan_scale = {
        // max of atan(8 cos(pi x)) is atan(8)
        (8.0_f32).atan()
    };
    let mut push = |v: Vec<f32>| {
        let last = *v.last().unwrap_or(&0.0);
        table.extend(v);
        table.push(last);
    };

    // inverse_tan: acos(tan(tan_scale (1-2x)) / 8) / pi, [0] = 0
    let mut inverse_tan: Vec<f32> = xs
        .iter()
        .map(|&x| ((tan_scale * (1.0 - 2.0 * x)).tan() / 8.0).clamp(-1.0, 1.0).acos() / pi)
        .collect();
    inverse_tan[0] = 0.0;
    push(inverse_tan);
    // inverse_sin: acos(1-2x)/pi
    push(xs.iter().map(|&x| (1.0 - 2.0 * x).acos() / pi).collect());
    // linear
    push(xs.clone());
    // sin: 0.5 - 0.5 cos(pi x)
    push(xs.iter().map(|&x| 0.5 - 0.5 * (pi * x).cos()).collect());
    // bump
    push(
        xs.iter()
            .map(|&x| {
                let fade_crop = (4.0 - 4.0 * x).min(1.0);
                (1.0 - (pi * x * 1.5).cos()) * (1.0 - (pi * fade_crop).cos()) / 4.5
            })
            .collect(),
    );

    // control-rate shapes over 512 points, mirrored: shape + [1] + tail
    let half: Vec<f32> = (0..WS_SIZE / 2)
        .map(|i| i as f32 / (WS_SIZE / 2) as f32)
        .collect();
    let expo: Vec<f32> = half.iter().map(|&x| 1.0 - (-5.0 * x).exp()).collect();
    let expo_max = expo.iter().cloned().fold(f32::MIN, f32::max);
    let expo: Vec<f32> = expo.iter().map(|v| v / expo_max).collect();
    let log: Vec<f32> = half
        .iter()
        .map(|&x| (1.0 - x * expo_max).max(1e-9).ln() / -5.0)
        .collect();
    let lin = half.clone();
    let sin_c: Vec<f32> = half.iter().map(|&x| (1.0 - (pi * x).cos()) / 2.0).collect();
    let inv_sin: Vec<f32> = half.iter().map(|&x| (1.0 - 2.0 * x).acos() / pi).collect();

    // control shapes are already exactly SHAPE_STRIDE entries long
    // (512 + apex + 512) — no guard appended, unlike the audio shapes
    let mut push_exact = |v: Vec<f32>| {
        debug_assert_eq!(v.len(), SHAPE_STRIDE);
        table.extend(v);
    };
    let scale_flip = |v: &[f32], flip: bool| -> Vec<f32> {
        let mut out: Vec<f32> = v.to_vec();
        out.push(1.0);
        if flip {
            out.extend(v.iter().rev());
        } else {
            out.extend(v.iter().map(|x| 1.0 - x));
        }
        out
    };
    push_exact(scale_flip(&log, false));
    push_exact(scale_flip(&log, true));
    push_exact(scale_flip(&inv_sin, false));
    push_exact(scale_flip(&lin, false));
    push_exact(scale_flip(&sin_c, false));
    push_exact(scale_flip(&expo, true));
    push_exact(scale_flip(&expo, false));
    table
}

/// The two fold curves (1028 entries each, like the generator).
pub fn build_folds() -> (Vec<f32>, Vec<f32>) {
    let n = WS_SIZE + 4;
    let pi = std::f32::consts::PI;
    let mut bipolar: Vec<f32> = (0..n)
        .map(|i| {
            let x = i as f32 / (WS_SIZE as f32 / 2.0) - 1.0;
            let sine = (8.0 * pi * x).sin();
            let window = (-x * x * 4.0).exp().powi(2);
            sine * window + (3.0 * x).atan() * (1.0 - window)
        })
        .collect();
    bipolar[n - 1] = bipolar[n - 2];
    let max = bipolar.iter().map(|v| v.abs()).fold(f32::MIN, f32::max);
    for v in bipolar.iter_mut() {
        *v /= max;
    }

    let mut unipolar: Vec<f32> = (0..n)
        .map(|i| {
            let x = i as f32 / WS_SIZE as f32;
            let sine = (16.0 * pi * x).sin();
            let window = (-x * x * 4.0).exp().powi(2);
            (0.38 * sine + 4.0 * x) * window + (4.0 * x).atan() * (1.0 - window)
        })
        .collect();
    unipolar[n - 1] = unipolar[n - 3];
    unipolar[n - 2] = unipolar[n - 3];
    let max = unipolar.iter().map(|v| v.abs()).fold(f32::MIN, f32::max);
    for v in unipolar.iter_mut() {
        *v /= max;
    }
    (bipolar, unipolar)
}

#[inline]
fn interpolate(table: &[f32], x: f32, scale: f32) -> f32 {
    let index = (x * scale).max(0.0);
    let i = (index as usize).min(table.len().saturating_sub(2));
    let f = index - i as f32;
    table[i] + (table[i + 1] - table[i]) * f
}

// ── ratio tables (poly_slope_generator.cc) ─────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct Ratio {
    pub ratio: f32,
    pub q: i32,
}

const fn r(ratio: f32, q: i32) -> Ratio {
    Ratio { ratio, q }
}

pub const AUDIO_RATIOS: [[Ratio; 4]; 21] = [
    [r(1.0, 1), r(0.5, 2), r(0.25, 4), r(0.125, 8)],
    [r(1.0, 1), r(0.5, 2), r(0.33333333, 3), r(0.2, 5)],
    [r(1.0, 1), r(0.5, 2), r(0.33333333, 3), r(0.25, 4)],
    [r(1.0, 1), r(0.66666666, 3), r(0.44444444, 9), r(0.296296297, 27)],
    [r(1.0, 1), r(0.66666666, 3), r(0.5, 2), r(0.33333333, 3)],
    [r(1.0, 1), r(0.75, 4), r(0.66666666, 3), r(0.5, 2)],
    [r(1.0, 1), r(0.790123456, 81), r(0.66666666, 3), r(0.5, 2)],
    [r(1.0, 1), r(0.790123456, 81), r(0.75, 4), r(0.66666666, 3)],
    [r(1.0, 1), r(0.88888888, 9), r(0.790123456, 81), r(0.66666666, 3)],
    [r(1.0, 1), r(0.99090909091, 109), r(0.987341772, 79), r(0.9811320755, 53)],
    [r(1.0, 1), r(1.0, 1), r(1.0, 1), r(1.0, 1)],
    [r(1.0, 1), r(1.009174312, 109), r(1.01265823, 79), r(1.0188679245, 53)],
    [r(1.0, 1), r(1.125, 8), r(1.265625, 64), r(1.5, 2)],
    [r(1.0, 1), r(1.265625, 64), r(1.3333333, 3), r(1.5, 2)],
    [r(1.0, 1), r(1.265625, 64), r(1.5, 2), r(2.0, 1)],
    [r(1.0, 1), r(1.33333333, 3), r(1.5, 2), r(2.0, 1)],
    [r(1.0, 1), r(1.5, 2), r(2.0, 1), r(3.0, 1)],
    [r(1.0, 1), r(1.5, 2), r(2.25, 4), r(3.375, 8)],
    [r(1.0, 1), r(2.0, 1), r(3.0, 1), r(4.0, 1)],
    [r(1.0, 1), r(2.0, 1), r(3.0, 1), r(5.0, 1)],
    [r(1.0, 1), r(2.0, 1), r(4.0, 1), r(8.0, 1)],
];

pub const CONTROL_RATIOS: [[Ratio; 4]; 21] = [
    [r(1.0, 1), r(0.5, 2), r(0.25, 4), r(0.125, 8)],
    [r(1.0, 1), r(0.5, 2), r(0.33333333, 3), r(0.2, 5)],
    [r(1.0, 1), r(0.5, 2), r(0.33333333, 3), r(0.25, 4)],
    [r(1.0, 1), r(0.66666666, 3), r(0.5, 2), r(0.25, 4)],
    [r(1.0, 1), r(0.66666666, 3), r(0.5, 2), r(0.33333333, 3)],
    [r(1.0, 1), r(0.75, 4), r(0.66666666, 3), r(0.5, 2)],
    [r(1.0, 1), r(0.8, 5), r(0.66666666, 3), r(0.5, 2)],
    [r(1.0, 1), r(0.8, 5), r(0.75, 3), r(0.5, 2)],
    [r(1.0, 1), r(0.8, 5), r(0.75, 4), r(0.66666666, 3)],
    [r(1.0, 1), r(0.909090909091, 11), r(0.857142857143, 7), r(0.8, 5)],
    [r(1.0, 1), r(1.0, 1), r(1.0, 1), r(1.0, 1)],
    [r(1.0, 1), r(1.09090909091, 11), r(1.142857143, 7), r(1.2, 5)],
    [r(1.0, 1), r(1.25, 4), r(1.33333333, 3), r(1.5, 2)],
    [r(1.0, 1), r(1.25, 4), r(1.33333333, 3), r(2.0, 2)],
    [r(1.0, 1), r(1.25, 4), r(1.5, 3), r(2.0, 2)],
    [r(1.0, 1), r(1.33333333, 3), r(1.5, 2), r(2.0, 1)],
    [r(1.0, 1), r(1.5, 2), r(2.0, 1), r(3.0, 1)],
    [r(1.0, 1), r(1.5, 2), r(2.0, 1), r(4.0, 1)],
    [r(1.0, 1), r(2.0, 1), r(3.0, 1), r(4.0, 1)],
    [r(1.0, 1), r(2.0, 1), r(3.0, 1), r(5.0, 1)],
    [r(1.0, 1), r(2.0, 1), r(4.0, 1), r(8.0, 1)],
];

// ── ramp generator ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RampGenerator {
    next_ratio: [Ratio; NUM_CHANNELS],
    master_phase: f32,
    wrap_counter: [i32; NUM_CHANNELS],
    pub phase: [f32; NUM_CHANNELS],
    pub frequency: [f32; NUM_CHANNELS],
    ratio: [Ratio; NUM_CHANNELS],
}

impl Default for RampGenerator {
    fn default() -> Self {
        RampGenerator {
            next_ratio: [r(1.0, 1); NUM_CHANNELS],
            master_phase: 0.0,
            wrap_counter: [0; NUM_CHANNELS],
            phase: [0.0; NUM_CHANNELS],
            frequency: [0.0; NUM_CHANNELS],
            ratio: [r(1.0, 1); NUM_CHANNELS],
        }
    }
}

impl RampGenerator {
    pub fn set_next_ratio(&mut self, ratios: &[Ratio; NUM_CHANNELS]) {
        self.next_ratio = *ratios;
    }

    /// One per-sample step (ramp_generator.h Step), `external_ramp` =
    /// Some(phase) locks the loop to an external 0..1 ramp.
    #[allow(clippy::too_many_arguments)]
    pub fn step(
        &mut self,
        ramp_mode: RampMode,
        output_mode: OutputMode,
        range: Range,
        f0: f32,
        pw: &[f32],
        gate_flags: u8,
        external_ramp: Option<f32>,
    ) {
        let n = if output_mode == OutputMode::Frequency
            || (output_mode == OutputMode::SlopePhase && ramp_mode == RampMode::Ar)
        {
            NUM_CHANNELS
        } else {
            1
        };
        match ramp_mode {
            RampMode::Ad => {
                if gate_flags & GATE_FLAG_RISING != 0 {
                    self.phase[..n].fill(0.0);
                }
                for i in 0..n {
                    self.frequency[i] = (f0 * self.next_ratio[i].ratio).min(0.25);
                    if let Some(ramp) = external_ramp {
                        self.phase[i] = ramp * self.next_ratio[i].ratio;
                    } else {
                        self.phase[i] += self.frequency[i];
                    }
                    self.phase[i] = self.phase[i].min(1.0);
                }
            }
            RampMode::Ar => {
                if output_mode == OutputMode::SlopePhase {
                    self.frequency[..n].fill(f0);
                } else {
                    for i in 0..n {
                        self.frequency[i] = (f0 * self.next_ratio[i].ratio).min(0.25);
                    }
                }
                let should_ramp_up = match external_ramp {
                    Some(ramp) => ramp < 0.5,
                    None => gate_flags & GATE_FLAG_HIGH != 0,
                };
                let clip_at = if should_ramp_up { 0.5 } else { 1.0 };
                for i in 0..n {
                    if self.phase[i] < 0.5 && !should_ramp_up {
                        self.phase[i] = 0.5;
                    } else if self.phase[i] > 0.5 && should_ramp_up {
                        self.phase[i] = 0.0;
                    }
                    let this_pw = if output_mode == OutputMode::Frequency {
                        pw[0]
                    } else {
                        pw[i.min(pw.len() - 1)]
                    };
                    let slope = if self.phase[i] < 0.5 {
                        0.5 / (1.0e-6 + this_pw)
                    } else {
                        0.5 / (1.0 + 1.0e-6 - this_pw)
                    };
                    self.phase[i] += self.frequency[i] * slope;
                    self.phase[i] = self.phase[i].min(clip_at);
                }
            }
            RampMode::Looping => {
                if range == Range::Audio && output_mode == OutputMode::Frequency {
                    let mut reset = false;
                    if gate_flags & GATE_FLAG_RISING != 0 {
                        self.phase[..n].fill(0.0);
                        reset = true;
                    }
                    for i in 0..n {
                        self.frequency[i] = (f0 * self.next_ratio[i].ratio).min(0.25);
                    }
                    if !reset {
                        for i in 0..n {
                            self.phase[i] += self.frequency[i];
                            if self.phase[i] >= 1.0 {
                                self.phase[i] -= 1.0;
                            }
                        }
                    }
                } else {
                    if let Some(ramp) = external_ramp {
                        for i in 0..n {
                            self.frequency[i] = (f0 * self.ratio[i].ratio).min(0.25);
                        }
                        if ramp < self.master_phase {
                            for i in 0..n {
                                self.wrap_counter[i] += 1;
                                if self.wrap_counter[i] >= self.ratio[i].q {
                                    self.ratio[i] = self.next_ratio[i];
                                    self.wrap_counter[i] = 0;
                                }
                            }
                        }
                        self.master_phase = ramp;
                    } else {
                        let mut reset = false;
                        if gate_flags & GATE_FLAG_RISING != 0 {
                            self.master_phase = 0.0;
                            self.ratio[..n].copy_from_slice(&self.next_ratio[..n]);
                            self.wrap_counter[..n].fill(0);
                            reset = true;
                        }
                        for i in 0..n {
                            self.frequency[i] = (f0 * self.ratio[i].ratio).min(0.25);
                        }
                        if !reset {
                            self.master_phase += f0;
                        }
                        if self.master_phase >= 1.0 {
                            self.master_phase -= 1.0;
                            for i in 0..n {
                                self.wrap_counter[i] += 1;
                                if self.wrap_counter[i] >= self.ratio[i].q {
                                    self.ratio[i] = self.next_ratio[i];
                                    self.wrap_counter[i] = 0;
                                }
                            }
                        }
                    }
                    for i in 0..n {
                        let mult_phase =
                            (self.master_phase + self.wrap_counter[i] as f32) * self.ratio[i].ratio;
                        self.phase[i] = mult_phase - mult_phase.floor();
                    }
                }
            }
        }
    }
}

// ── ramp shaper ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct RampShaper {
    next_sample: f32,
    previous_phase_shift: f32,
}

impl RampShaper {
    pub fn band_limited_pulse(&mut self, phase: f32, frequency: f32, pw: f32) -> f32 {
        let pw = pw.clamp(frequency * 2.0, 1.0 - 2.0 * frequency);
        let this_sample = self.next_sample;
        let mut next_sample = 0.0;
        let mut wrap_point = pw;
        if phase < pw * 0.5 {
            wrap_point = 0.0;
        } else if phase > 0.5 + pw * 0.5 {
            wrap_point = 1.0;
        }
        let d = phase - wrap_point;
        let mut this_sample = this_sample;
        if d >= 0.0 && d < frequency {
            let t = d / frequency;
            let mut discontinuity = 1.0;
            if wrap_point != pw {
                discontinuity = -discontinuity;
            }
            if frequency < 0.0 {
                discontinuity = -discontinuity;
            }
            this_sample += this_blep_sample(t) * discontinuity;
            next_sample += next_blep_sample(t) * discontinuity;
        }
        next_sample += if phase < pw { 0.0 } else { 1.0 };
        self.next_sample = next_sample;
        this_sample
    }

    pub fn slope(
        &mut self,
        ramp_mode: RampMode,
        range: Range,
        phase: f32,
        phase_shift: f32,
        frequency: f32,
        pw: f32,
    ) -> f32 {
        match ramp_mode {
            RampMode::Ad => self.skewed_ramp(phase, 0.0, frequency, pw),
            RampMode::Ar => phase,
            RampMode::Looping => {
                if range == Range::Control {
                    self.skewed_ramp(phase, phase_shift, frequency, pw)
                } else {
                    self.band_limited_slope(phase, phase_shift, frequency, pw)
                }
            }
        }
    }

    pub fn eoa(&mut self, ramp_mode: RampMode, range: Range, phase: f32, frequency: f32, pw: f32) -> f32 {
        match ramp_mode {
            RampMode::Looping if range == Range::Audio => {
                self.band_limited_pulse(phase, frequency, pw)
            }
            RampMode::Ar => {
                if phase >= 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
            _ => {
                if phase >= pw {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    pub fn eor(&mut self, ramp_mode: RampMode, range: Range, phase: f32, frequency: f32) -> f32 {
        match ramp_mode {
            RampMode::Looping => {
                let pw = (96.0 * frequency).min(0.5);
                if range == Range::Audio {
                    1.0 - self.band_limited_pulse(phase, frequency, pw)
                } else if phase < pw {
                    1.0
                } else {
                    0.0
                }
            }
            _ => {
                if phase >= 1.0 {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    fn band_limited_slope(
        &mut self,
        mut phase: f32,
        phase_shift: f32,
        mut frequency: f32,
        pw: f32,
    ) -> f32 {
        if phase_shift != 0.0 {
            phase += phase_shift;
            frequency += phase_shift - self.previous_phase_shift;
            self.previous_phase_shift = phase_shift;
            if phase >= 1.0 {
                phase -= 1.0;
            } else if phase < 0.0 {
                phase += 1.0;
            }
        }
        let pw = pw.clamp(frequency.abs() * 2.0, 1.0 - 2.0 * frequency.abs());
        let this_sample = self.next_sample;
        let mut next_sample = 0.0;
        let mut wrap_point = pw;
        if phase < pw * 0.5 {
            wrap_point = 0.0;
        } else if phase > 0.5 + pw * 0.5 {
            wrap_point = 1.0;
        }
        let slope_up = 1.0 / pw;
        let slope_down = 1.0 / (1.0 - pw);
        let d = phase - wrap_point;
        let mut this_sample = this_sample;
        if d >= 0.0 && d < frequency {
            let t = d / frequency;
            let mut discontinuity = -(slope_up + slope_down) * frequency;
            if wrap_point != pw {
                discontinuity = -discontinuity;
            }
            if frequency < 0.0 {
                discontinuity = -discontinuity;
            }
            this_sample += this_integrated_blep_sample(t) * discontinuity;
            next_sample += next_integrated_blep_sample(t) * discontinuity;
        }
        next_sample += if phase < pw {
            phase * slope_up
        } else {
            1.0 - (phase - pw) * slope_down
        };
        self.next_sample = next_sample;
        this_sample
    }

    fn skewed_ramp(
        &mut self,
        mut phase: f32,
        phase_shift: f32,
        mut frequency: f32,
        pw: f32,
    ) -> f32 {
        if phase_shift != 0.0 {
            phase += phase_shift;
            frequency += phase_shift - self.previous_phase_shift;
            self.previous_phase_shift = phase_shift;
            if phase >= 1.0 {
                phase -= 1.0;
            } else if phase < 0.0 {
                phase += 1.0;
            }
        }
        let pw = pw.clamp(frequency.abs() * 2.0, 1.0 - 2.0 * frequency.abs());
        let slope_up = 0.5 / pw;
        let slope_down = 0.5 / (1.0 - pw);
        if phase < pw {
            phase * slope_up
        } else {
            (phase - pw) * slope_down + 0.5
        }
    }
}

// ── ramp waveshaper ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct RampWaveshaper {
    previous_input: f32,
    previous_output: f32,
    breakpoint: f32,
}

impl RampWaveshaper {
    /// `shape` points at one shape's 1025 entries inside the bank
    /// (the next shape's table follows at +SHAPE_STRIDE for the
    /// crossfade).
    pub fn shape(
        &mut self,
        ramp_mode: RampMode,
        input: f32,
        bank: &[f32],
        shape_offset: usize,
        shape_fractional: f32,
    ) -> f32 {
        let ws_index = 1024.0 * input;
        let i = (ws_index as usize) & 1023;
        let f = ws_index - ws_index.floor();
        let x0 = bank[shape_offset + i];
        let x1 = bank[shape_offset + i + 1];
        let next = (shape_offset + SHAPE_STRIDE).min(bank.len() - SHAPE_STRIDE);
        let y0 = bank[next + i];
        let y1 = bank[next + i + 1];
        let x = x0 + (x1 - x0) * f;
        let y = y0 + (y1 - y0) * f;
        let mut output = x + (y - x) * shape_fractional;
        if ramp_mode != RampMode::Ar {
            return output;
        }
        let crossed_up = self.previous_input <= 0.5 && input > 0.5;
        let crossed_down = self.previous_input > 0.5 && input < 0.5;
        if crossed_up || crossed_down {
            self.breakpoint = self.previous_output;
        } else if input == 1.0 {
            self.breakpoint = 1.0;
        } else if input == 0.5 {
            self.breakpoint = 0.0;
        }
        if input <= 0.5 {
            output = self.breakpoint + (1.0 - self.breakpoint) * output;
        } else {
            output *= self.breakpoint;
        }
        self.previous_input = input;
        self.previous_output = output;
        output
    }
}

// ── the poly slope generator ───────────────────────────────────────────────

pub struct PolySlopeGenerator {
    frequency: f32,
    pw: f32,
    shift: f32,
    shape: f32,
    fold: f32,
    ratio_quantizer: HysteresisQuantizer,
    pub ramp_generator: RampGenerator,
    ramp_shaper: [RampShaper; NUM_CHANNELS],
    ramp_waveshaper: [RampWaveshaper; NUM_CHANNELS],
    lp1: [f32; NUM_CHANNELS],
    lp2: [f32; NUM_CHANNELS],
    wavetable: Vec<f32>,
    bipolar_fold: Vec<f32>,
    unipolar_fold: Vec<f32>,
}

impl PolySlopeGenerator {
    pub fn new() -> Self {
        let (bipolar_fold, unipolar_fold) = build_folds();
        PolySlopeGenerator {
            frequency: 0.01,
            pw: 0.0,
            shift: 0.0,
            shape: 0.0,
            fold: 0.0,
            ratio_quantizer: HysteresisQuantizer::new(21, 0.05),
            ramp_generator: RampGenerator::default(),
            ramp_shaper: Default::default(),
            ramp_waveshaper: Default::default(),
            lp1: [0.0; NUM_CHANNELS],
            lp2: [0.0; NUM_CHANNELS],
            wavetable: build_wavetable(),
            bipolar_fold,
            unipolar_fold,
        }
    }

    fn tame(f0: f32, harmonics: f32, order: f32) -> f32 {
        let f0 = f0 * harmonics;
        let max_f = 0.5 * (1.0 / order);
        let max_amount = (1.0 - (f0 - max_f) / (0.5 - max_f)).clamp(0.0, 1.0);
        max_amount * max_amount * max_amount
    }

    fn fold(&self, ramp_mode: RampMode, unipolar: f32, fold_amount: f32) -> f32 {
        if ramp_mode == RampMode::Looping {
            let bipolar = 2.0 * unipolar - 1.0;
            let folded = if fold_amount > 0.0 {
                interpolate(
                    &self.bipolar_fold,
                    0.5 + bipolar * (0.03 + 0.46 * fold_amount),
                    1024.0,
                )
            } else {
                0.0
            };
            5.0 * (bipolar + (folded - bipolar) * fold_amount)
        } else {
            let folded = if fold_amount > 0.0 {
                interpolate(&self.unipolar_fold, unipolar * fold_amount, 1024.0)
            } else {
                0.0
            };
            8.0 * (unipolar + (folded - unipolar) * fold_amount)
        }
    }

    fn scale(ramp_mode: RampMode, unipolar: f32) -> f32 {
        if ramp_mode == RampMode::Looping {
            10.0 * unipolar - 5.0
        } else {
            8.0 * unipolar
        }
    }

    /// Render one block. `out` is `size` frames × 4 channels.
    /// Outputs are in the hardware's volt scale (±5 looping, 0–8
    /// otherwise) — callers normalize for their bus.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        ramp_mode: RampMode,
        output_mode: OutputMode,
        range: Range,
        mut frequency: f32,
        mut pw: f32,
        mut shape: f32,
        mut smoothness: f32,
        shift: f32,
        gate_flags: &[u8],
        external_ramp: Option<&[f32]>,
        out: &mut [[f32; NUM_CHANNELS]],
    ) {
        let size = out.len();
        frequency = frequency.min(0.25);
        if range == Range::Control && pw < 0.5 {
            pw = 0.5 + 0.6 * (pw - 0.5) / ((pw - 0.5).abs() + 0.1);
        }
        if external_ramp.is_some() && ramp_mode == RampMode::Ar {
            frequency *= 1.0 + 2.0 * (pw - 0.5).abs();
        }
        let slope = 3.0 + (pw - 0.5).abs() * 5.0;
        let shape_amount = (shape - 0.5).abs() * 2.0;
        let shape_amount_attenuation = Self::tame(frequency, slope, 16.0);
        shape = 0.5 + (shape - 0.5) * shape_amount_attenuation;
        if smoothness > 0.5 {
            smoothness = 0.5
                + (smoothness - 0.5)
                    * Self::tame(
                        frequency,
                        slope * (3.0 + shape_amount * shape_amount_attenuation * 5.0),
                        12.0,
                    );
        }

        self.render_internal(
            ramp_mode,
            output_mode,
            range,
            frequency,
            pw,
            shape,
            smoothness,
            shift,
            gate_flags,
            external_ramp,
            out,
        );

        // smoothness < 0.5: the 2-pole low-pass side
        if smoothness < 0.5 {
            let mut ratio = smoothness * 2.0;
            ratio *= ratio;
            ratio *= ratio;
            let last_channel = if output_mode == OutputMode::Gates {
                1
            } else {
                NUM_CHANNELS
            };
            let mut f = [0.0_f32; NUM_CHANNELS];
            for (i, fi) in f.iter_mut().enumerate().take(last_channel) {
                let source = if output_mode == OutputMode::Frequency {
                    i
                } else {
                    0
                };
                *fi = self.ramp_generator.frequency[source] * 0.5;
                *fi += (1.0 - *fi) * ratio;
            }
            #[allow(clippy::manual_memcpy)] // a 2-pole filter per
            // channel, not a copy — clippy misreads the recurrences
            for frame in out.iter_mut().take(size) {
                for i in 0..last_channel {
                    self.lp1[i] += f[i] * (frame[i] - self.lp1[i]);
                    self.lp2[i] += f[i] * (self.lp1[i] - self.lp2[i]);
                    frame[i] = self.lp2[i];
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_internal(
        &mut self,
        ramp_mode: RampMode,
        output_mode: OutputMode,
        range: Range,
        frequency: f32,
        pw: f32,
        shape: f32,
        smoothness: f32,
        shift: f32,
        gate_flags: &[u8],
        external_ramp: Option<&[f32]>,
        out: &mut [[f32; NUM_CHANNELS]],
    ) {
        let size = out.len();
        let is_phasor = !(range == Range::Audio && ramp_mode == RampMode::Looping);

        let mut fm = self.frequency;
        let fm_inc = (frequency - fm) / size as f32;
        let mut pwm = self.pw;
        let pwm_inc = (pw - pwm) / size as f32;
        let mut shift_m = self.shift;
        let shift_inc = (2.0 * shift - 1.0 - shift_m) / size as f32;
        let shape_target = if is_phasor {
            shape * 5.9999 + 5.0
        } else {
            shape * 3.9999
        };
        let mut shape_m = self.shape;
        let shape_inc = (shape_target - shape_m) / size as f32;
        let fold_target = (2.0 * (smoothness - 0.5)).max(0.0);
        let mut fold_m = self.fold;
        let fold_inc = (fold_target - fold_m) / size as f32;
        self.frequency = frequency;
        self.pw = pw;
        self.shift = 2.0 * shift - 1.0;
        self.shape = shape_target;
        self.fold = fold_target;

        if output_mode == OutputMode::Frequency {
            let ratio_index = self.ratio_quantizer.process(shift);
            let table = if range == Range::Control {
                &CONTROL_RATIOS[ratio_index]
            } else {
                &AUDIO_RATIOS[ratio_index]
            };
            self.ramp_generator.set_next_ratio(table);
        }

        for i in 0..size {
            fm += fm_inc;
            pwm += pwm_inc;
            shift_m += shift_inc;
            shape_m += shape_inc;
            fold_m += fold_inc;

            let f0 = fm;
            let pw = pwm;
            let shift = shift_m;
            let step = shift * (1.0 / (NUM_CHANNELS - 1) as f32);
            let partial_step = shift * (1.0 / NUM_CHANNELS as f32);
            let fold = fold_m;

            let mut per_channel_pw = [0.0_f32; NUM_CHANNELS];
            let pw_increment = (if shift > 0.0 { 1.0 - pw } else { pw }) * step;
            for (j, p) in per_channel_pw.iter_mut().enumerate() {
                *p = pw + pw_increment * j as f32;
            }

            let ramp_i = external_ramp.map(|r| r[i]);
            if output_mode == OutputMode::SlopePhase && ramp_mode == RampMode::Ar {
                self.ramp_generator.step(
                    ramp_mode,
                    output_mode,
                    range,
                    f0,
                    &per_channel_pw,
                    if ramp_i.is_some() {
                        GATE_FLAG_LOW
                    } else {
                        gate_flags[i]
                    },
                    ramp_i,
                );
            } else {
                self.ramp_generator.step(
                    ramp_mode,
                    output_mode,
                    range,
                    f0,
                    &[pw],
                    if ramp_i.is_some() {
                        GATE_FLAG_LOW
                    } else {
                        gate_flags[i]
                    },
                    ramp_i,
                );
            }

            let shape = shape_m;
            let shape_integral = shape as usize;
            let shape_fractional = shape - shape_integral as f32;
            let shape_offset = shape_integral.min(NUM_SHAPES - 1) * SHAPE_STRIDE;

            match output_mode {
                OutputMode::Gates => {
                    let phase = self.ramp_generator.phase[0];
                    let frequency = self.ramp_generator.frequency[0];
                    let raw = self.ramp_shaper[0].slope(ramp_mode, range, phase, 0.0, frequency, pw);
                    let slope = self.ramp_waveshaper[0].shape(
                        ramp_mode,
                        raw,
                        &self.wavetable,
                        shape_offset,
                        shape_fractional,
                    );
                    out[i][0] = self.fold(ramp_mode, slope, fold) * shift;
                    out[i][1] = Self::scale(
                        ramp_mode,
                        if is_phasor {
                            // shape 8 = the linear control shape
                            self.ramp_waveshaper[1].shape(
                                ramp_mode,
                                raw,
                                &self.wavetable,
                                8 * SHAPE_STRIDE,
                                0.0,
                            )
                        } else {
                            raw
                        },
                    );
                    out[i][2] =
                        self.ramp_shaper[2].eoa(ramp_mode, range, phase, frequency, pw) * 8.0;
                    out[i][3] = self.ramp_shaper[3].eor(ramp_mode, range, phase, frequency) * 8.0;
                }
                OutputMode::Amplitude => {
                    let phase = self.ramp_generator.phase[0];
                    let frequency = self.ramp_generator.frequency[0];
                    let raw = self.ramp_shaper[0].slope(ramp_mode, range, phase, 0.0, frequency, pw);
                    let shaped = self.ramp_waveshaper[0].shape(
                        ramp_mode,
                        raw,
                        &self.wavetable,
                        shape_offset,
                        shape_fractional,
                    );
                    let slope =
                        self.fold(ramp_mode, shaped, fold) * if shift < 0.0 { -1.0 } else { 1.0 };
                    let channel_index = (shift * 5.1).abs();
                    for (j, o) in out[i].iter_mut().enumerate() {
                        let channel = (j + 1) as f32;
                        let gain = (1.0 - (channel - channel_index).abs()).max(0.0);
                        let equal_pow = range == Range::Audio;
                        *o = slope * gain * if equal_pow { 2.0 - gain } else { 1.0 };
                    }
                }
                OutputMode::SlopePhase => {
                    let mut phase_shift = 0.0;
                    for j in 0..NUM_CHANNELS {
                        let source = if ramp_mode == RampMode::Ar { j } else { 0 };
                        let raw = self.ramp_shaper[j].slope(
                            ramp_mode,
                            range,
                            self.ramp_generator.phase[source],
                            phase_shift,
                            self.ramp_generator.frequency[source],
                            if ramp_mode == RampMode::Ad {
                                per_channel_pw[j]
                            } else {
                                pw
                            },
                        );
                        let shaped = self.ramp_waveshaper[j].shape(
                            ramp_mode,
                            raw,
                            &self.wavetable,
                            shape_offset,
                            shape_fractional,
                        );
                        out[i][j] = self.fold(ramp_mode, shaped, fold);
                        phase_shift -= if range == Range::Audio {
                            step
                        } else {
                            partial_step
                        };
                    }
                }
                OutputMode::Frequency => {
                    #[allow(clippy::needless_range_loop)] // j strides
                    // four parallel state arrays — a zip would obscure it
                    for j in 0..NUM_CHANNELS {
                        let raw = self.ramp_shaper[j].slope(
                            ramp_mode,
                            range,
                            self.ramp_generator.phase[j],
                            0.0,
                            self.ramp_generator.frequency[j],
                            pw,
                        );
                        let shaped = self.ramp_waveshaper[j].shape(
                            ramp_mode,
                            raw,
                            &self.wavetable,
                            shape_offset,
                            shape_fractional,
                        );
                        out[i][j] = self.fold(ramp_mode, shaped, fold);
                    }
                }
            }
        }
    }
}

impl Default for PolySlopeGenerator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_blocks(
        psg: &mut PolySlopeGenerator,
        ramp_mode: RampMode,
        output_mode: OutputMode,
        range: Range,
        frequency: f32,
        blocks: usize,
        gate_pattern: impl Fn(usize) -> bool,
    ) -> Vec<[f32; NUM_CHANNELS]> {
        let mut rendered = Vec::new();
        let mut prev = GATE_FLAG_LOW;
        for b in 0..blocks {
            let mut gates = [GATE_FLAG_LOW; 64];
            for (i, g) in gates.iter_mut().enumerate() {
                prev = extract_gate_flags(prev, gate_pattern(b * 64 + i));
                *g = prev;
            }
            let mut out = [[0.0_f32; NUM_CHANNELS]; 64];
            psg.render(
                ramp_mode, output_mode, range, frequency, 0.5, 0.5, 0.5, 0.5, &gates, None,
                &mut out,
            );
            rendered.extend_from_slice(&out);
        }
        rendered
    }

    #[test]
    fn wavetable_has_twelve_shapes_with_sane_ranges() {
        let t = build_wavetable();
        assert_eq!(t.len(), NUM_SHAPES * SHAPE_STRIDE);
        for s in 0..NUM_SHAPES {
            let shape = &t[s * SHAPE_STRIDE..(s + 1) * SHAPE_STRIDE];
            assert!(
                shape.iter().all(|v| (-0.01..=1.01).contains(v)),
                "shape {s} in range"
            );
            // linear (audio shape 2) is identity
            if s == 2 {
                assert!((shape[512] - 0.5).abs() < 1e-3, "linear midpoint");
            }
        }
    }

    #[test]
    fn fold_tables_are_normalized() {
        let (b, u) = build_folds();
        let bmax = b.iter().map(|v| v.abs()).fold(f32::MIN, f32::max);
        let umax = u.iter().map(|v| v.abs()).fold(f32::MIN, f32::max);
        assert!((bmax - 1.0).abs() < 1e-5);
        assert!((umax - 1.0).abs() < 1e-5);
    }

    #[test]
    fn looping_mode_oscillates_at_the_requested_rate() {
        let mut psg = PolySlopeGenerator::new();
        // 100 Hz at 48k = f0 ~0.0020833; count slope zero crossings
        let out = render_blocks(
            &mut psg,
            RampMode::Looping,
            OutputMode::SlopePhase,
            Range::Audio,
            100.0 / 48_000.0,
            100,
            |_| false,
        );
        let crossings = out
            .windows(2)
            .filter(|w| (w[0][0] >= 0.0) != (w[1][0] >= 0.0))
            .count();
        let seconds = out.len() as f32 / 48_000.0;
        let hz = crossings as f32 / 2.0 / seconds;
        assert!((hz / 100.0 - 1.0).abs() < 0.05, "loops at ~100 Hz: {hz}");
        assert!(out.iter().flatten().all(|v| v.is_finite()));
    }

    #[test]
    fn ad_mode_fires_once_per_gate_and_returns_to_zero() {
        let mut psg = PolySlopeGenerator::new();
        let out = render_blocks(
            &mut psg,
            RampMode::Ad,
            OutputMode::SlopePhase,
            Range::Control,
            0.001,
            40,
            |i| i == 0, // one trigger at the start
        );
        let peak = out.iter().map(|f| f[0]).fold(f32::MIN, f32::max);
        assert!(peak > 1.0, "the AD envelope rises: {peak}");
        let tail = out[out.len() - 64..]
            .iter()
            .map(|f| f[0].abs())
            .fold(f32::MIN, f32::max);
        assert!(tail < 0.05, "and falls back to zero: {tail}");
    }

    #[test]
    fn ar_mode_sustains_while_high() {
        let mut psg = PolySlopeGenerator::new();
        let out = render_blocks(
            &mut psg,
            RampMode::Ar,
            OutputMode::SlopePhase,
            Range::Control,
            0.002,
            40,
            |i| i < 40 * 32, // high for the first half
        );
        let mid = out[out.len() / 2 - 32][0];
        assert!(mid > 4.0, "holds near the top while gated: {mid}");
        let tail = out[out.len() - 1][0];
        assert!(tail < 0.5, "releases after the gate falls: {tail}");
    }

    #[test]
    fn frequency_mode_runs_four_related_ramps() {
        let mut psg = PolySlopeGenerator::new();
        let mut gates = [GATE_FLAG_LOW; 64];
        let mut prev = GATE_FLAG_LOW;
        for g in gates.iter_mut() {
            prev = extract_gate_flags(prev, false);
            *g = prev;
        }
        let mut out = [[0.0_f32; NUM_CHANNELS]; 64];
        for _ in 0..200 {
            psg.render(
                RampMode::Looping,
                OutputMode::Frequency,
                Range::Audio,
                0.01,
                0.5,
                0.5,
                0.5,
                1.0, // shift = max → the CCCC octaves table
                &gates,
                None,
                &mut out,
            );
        }
        // all four channels alive and finite
        for j in 0..NUM_CHANNELS {
            let energy: f32 = out.iter().map(|f| f[j] * f[j]).sum();
            assert!(energy > 0.0, "channel {j} sounds");
        }
        assert!(out.iter().flatten().all(|v| v.is_finite()));
    }

    #[test]
    fn external_ramp_locks_the_loop() {
        let mut psg = PolySlopeGenerator::new();
        let gates = [GATE_FLAG_LOW; 64];
        // external ramp at exactly 1 cycle per block
        let ramp: Vec<f32> = (0..64).map(|i| i as f32 / 64.0).collect();
        let mut out = [[0.0_f32; NUM_CHANNELS]; 64];
        for _ in 0..50 {
            psg.render(
                RampMode::Looping,
                OutputMode::SlopePhase,
                Range::Control,
                0.001, // knob frequency ignored when locked
                0.5,
                0.5,
                0.5,
                0.5,
                &gates,
                Some(&ramp),
                &mut out,
            );
        }
        // the slope must complete exactly one cycle per block: the
        // first and last samples bracket a full sweep
        let lo = out.iter().map(|f| f[0]).fold(f32::MAX, f32::min);
        let hi = out.iter().map(|f| f[0]).fold(f32::MIN, f32::max);
        assert!(hi - lo > 5.0, "locked loop sweeps the range: {lo}..{hi}");
    }

    #[test]
    fn every_mode_combination_survives_a_soak() {
        for ramp_mode in [RampMode::Ad, RampMode::Looping, RampMode::Ar] {
            for output_mode in [
                OutputMode::Gates,
                OutputMode::Amplitude,
                OutputMode::SlopePhase,
                OutputMode::Frequency,
            ] {
                for range in [Range::Control, Range::Audio] {
                    let mut psg = PolySlopeGenerator::new();
                    let out = render_blocks(
                        &mut psg,
                        ramp_mode,
                        output_mode,
                        range,
                        0.01,
                        50,
                        |i| (i / 700) % 2 == 0,
                    );
                    assert!(
                        out.iter().flatten().all(|v| v.is_finite()),
                        "{ramp_mode:?}/{output_mode:?}/{range:?} stays finite"
                    );
                }
            }
        }
    }

    #[test]
    fn hysteresis_quantizer_resists_jitter() {
        let mut q = HysteresisQuantizer::new(21, 0.05);
        let a = q.process(0.5);
        // tiny jitter must not flip the step
        let b = q.process(0.5 + 0.001);
        let c = q.process(0.5 - 0.001);
        assert_eq!(a, b);
        assert_eq!(a, c);
        // a real move does
        let d = q.process(0.9);
        assert!(d > a);
    }
}
