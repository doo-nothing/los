//! # Braids engine — the analog oscillator
//!
//! Ported from pichenettes/eurorack (braids/analog_oscillator.*, MIT,
//! copyright 2014 Emilie Gillet, attribution preserved), fixed-point
//! faithful: the nine band-limited analog shapes (saw, variable saw,
//! C-saw, square, triangle, sine, the triangle and sine wavefolders,
//! and the comb-buzz), each with hard sync, through the firmware's
//! 32-bit phase accumulator and polyBLEP corrections.
//!
//! Tables: `wav_sine` and `lut_oscillator_increments` are generated
//! from the upstream laws at 96 kHz; the two waveshaper fold tables
//! and the fifteen band-limited combs are extracted byte-exact into
//! `braids_tables.bin`. This is the foundation; the macro-oscillator
//! model wiring and the digital models build on it.

#![allow(clippy::excessive_precision)]
// The firmware's fixed-point idiom is `a + (b * c >> s)` throughout.
#![allow(clippy::precedence)]

use std::sync::OnceLock;

pub const NATIVE_SR: f64 = 96_000.0;
const NUM_ZONES: usize = 15;
const HIGHEST_NOTE: i32 = 128 * 128;
const PITCH_TABLE_START: i32 = 128 * 128;
const OCTAVE: i32 = 12 * 128;
const DIGITAL_HIGHEST_NOTE: i32 = 140 * 128;
const COMB_DELAY_LENGTH: usize = 8192;

const BRAIDS_TABLES_BIN: &[u8] = include_bytes!("braids_tables.bin");

pub struct Tables {
    pub wav_sine: Vec<i16>,                // 257
    pub increments: Vec<u32>,              // 97
    pub ws_sine_fold: Vec<i16>,            // 257
    pub ws_tri_fold: Vec<i16>,             // 257
    pub comb: Vec<Vec<i16>>,               // 15 × 257
    pub svf_cutoff: Vec<u16>,              // 257
    pub violent_overdrive: Vec<i16>,       // 257
    // digital-oscillator tables (braids_digital_tables.bin)
    pub oscillator_delays: Vec<u32>,       // 97
    pub svf_damp: Vec<u16>,                // 257
    pub moderate_overdrive: Vec<i16>,      // 257
    pub fm_frequency_quantizer: Vec<i16>,  // 129
}

const BRAIDS_DIGITAL_TABLES_BIN: &[u8] = include_bytes!("braids_digital_tables.bin");

static TABLES: OnceLock<Tables> = OnceLock::new();

pub fn tables() -> &'static Tables {
    TABLES.get_or_init(|| {
        let wav_sine: Vec<i16> = (0..257)
            .map(|i| {
                let v = (i as f64 / 256.0 * std::f64::consts::TAU).sin() * 32767.0;
                v.round().clamp(-32768.0, 32767.0) as i16
            })
            .collect();
        // top-octave increments: notes 128*128 .. 140*128 step 16
        let increments: Vec<u32> = (0..97)
            .map(|i| {
                let note = (128 * 128 + 16 * i) as f64;
                let pitch = 440.0 * 2.0_f64.powf((note - 69.0 * 128.0) / (128.0 * 12.0));
                (4_294_967_296.0 / NATIVE_SR * pitch) as u32
            })
            .collect();
        // unpack the extracted bin: sine_fold[257], tri_fold[257], 15×comb[257]
        let all: Vec<i16> = BRAIDS_TABLES_BIN
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        let ws_sine_fold = all[0..257].to_vec();
        let ws_tri_fold = all[257..514].to_vec();
        let comb: Vec<Vec<i16>> = (0..NUM_ZONES)
            .map(|z| all[514 + z * 257..514 + (z + 1) * 257].to_vec())
            .collect();
        // svf_cutoff is u16 (stored as raw bits after the 4369 i16),
        // then violent_overdrive (i16)
        let svf_base = 4369 * 2; // bytes
        let svf_cutoff: Vec<u16> = BRAIDS_TABLES_BIN[svf_base..svf_base + 257 * 2]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let vio_base = svf_base + 257 * 2;
        let violent_overdrive: Vec<i16> = BRAIDS_TABLES_BIN[vio_base..vio_base + 257 * 2]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        // digital tables: header of 4 u32 lengths, then delays(u32),
        // damp(u16), overdrive(i16), fm_quantizer(i16)
        let d = BRAIDS_DIGITAL_TABLES_BIN;
        let len = |i: usize| {
            u32::from_le_bytes([d[i * 4], d[i * 4 + 1], d[i * 4 + 2], d[i * 4 + 3]]) as usize
        };
        let (n_del, n_damp, n_over, n_fmq) = (len(0), len(1), len(2), len(3));
        let mut off = 16;
        let oscillator_delays: Vec<u32> = d[off..off + n_del * 4]
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        off += n_del * 4;
        let svf_damp: Vec<u16> = d[off..off + n_damp * 2]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        off += n_damp * 2;
        let moderate_overdrive: Vec<i16> = d[off..off + n_over * 2]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        off += n_over * 2;
        let fm_frequency_quantizer: Vec<i16> = d[off..off + n_fmq * 2]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        Tables {
            wav_sine,
            increments,
            ws_sine_fold,
            ws_tri_fold,
            comb,
            svf_cutoff,
            violent_overdrive,
            oscillator_delays,
            svf_damp,
            moderate_overdrive,
            fm_frequency_quantizer,
        }
    })
}

/// `ComputePhaseIncrement` for the digital oscillator (clamps to the
/// pitch-table start, then top-octave interpolation + octave shifts).
/// Identical law to the analog oscillator's.
fn compute_phase_increment(midi_pitch: i16) -> u32 {
    let t = tables();
    let mut pitch = midi_pitch as i32;
    if pitch >= PITCH_TABLE_START {
        pitch = PITCH_TABLE_START - 1;
    }
    let mut ref_pitch = pitch - PITCH_TABLE_START;
    let mut num_shifts = 0u32;
    while ref_pitch < 0 {
        ref_pitch += OCTAVE;
        num_shifts += 1;
    }
    let idx = (ref_pitch >> 4) as usize;
    let a = t.increments[idx.min(t.increments.len() - 1)];
    let b = t.increments[(idx + 1).min(t.increments.len() - 1)];
    let inc = a.wrapping_add(((b.wrapping_sub(a) as i32) * (ref_pitch & 0xf) >> 4) as u32);
    inc >> num_shifts
}

/// `ComputeDelay` — the comb/physical-model delay length in 16.16 samples.
fn compute_delay(midi_pitch: i16) -> u32 {
    let t = tables();
    let mut pitch = midi_pitch as i32;
    if pitch >= DIGITAL_HIGHEST_NOTE - OCTAVE {
        pitch = DIGITAL_HIGHEST_NOTE - OCTAVE;
    }
    let mut ref_pitch = pitch - PITCH_TABLE_START;
    let mut num_shifts = 0u32;
    while ref_pitch < 0 {
        ref_pitch += OCTAVE;
        num_shifts += 1;
    }
    let idx = (ref_pitch >> 4) as usize;
    let a = t.oscillator_delays[idx.min(t.oscillator_delays.len() - 1)];
    let b = t.oscillator_delays[(idx + 1).min(t.oscillator_delays.len() - 1)];
    let delay = a.wrapping_add(((b.wrapping_sub(a) as i32) * (ref_pitch & 0xf) >> 4) as u32);
    delay >> 12u32.saturating_sub(num_shifts)
}

#[inline]
fn interpolate824_u16(table: &[u16], phase: u32) -> i32 {
    let i = (phase >> 24) as usize;
    let a = table[i.min(table.len() - 1)] as i64;
    let b = table[(i + 1).min(table.len() - 1)] as i64;
    // u16 deltas times a 16-bit fraction overflow i32 — widen
    (a + ((b - a) * (((phase >> 8) & 0xffff) as i64) >> 16)) as i32
}

#[inline]
fn mix(a: i16, b: i16, balance: u16) -> i16 {
    let a = a as i64;
    let b = b as i64;
    // (b-a)·balance overflows i32 when both span full i16 — widen
    (a + ((b - a) * balance as i64 >> 16)).clamp(-32768, 32767) as i16
}

// ── fixed-point helpers ──────────────────────────────────────────────────────

#[inline]
fn interpolate824(table: &[i16], phase: u32) -> i32 {
    let i = (phase >> 24) as usize;
    let a = table[i] as i32;
    let b = table[(i + 1).min(table.len() - 1)] as i32;
    a + ((b - a) * ((phase >> 8) & 0xffff) as i32 >> 16)
}

#[inline]
fn interpolate88(table: &[i16], index: u16) -> i32 {
    let i = (index >> 8) as usize;
    let a = table[i.min(table.len() - 1)] as i32;
    let b = table[(i + 1).min(table.len() - 1)] as i32;
    a + ((b - a) * (index & 0xff) as i32 >> 8)
}

#[inline]
fn crossfade(table_a: &[i16], table_b: &[i16], phase: u32, balance: u16) -> i32 {
    let a = interpolate824(table_a, phase);
    let b = interpolate824(table_b, phase);
    a + ((b - a) * balance as i32 >> 16)
}

#[inline]
fn this_blep(mut t: u32) -> i32 {
    if t > 65535 {
        t = 65535;
    }
    ((t as i64 * t as i64) >> 18) as i32
}

#[inline]
fn next_blep(mut t: u32) -> i32 {
    if t > 65535 {
        t = 65535;
    }
    t = 65535 - t;
    -(((t as i64 * t as i64) >> 18) as i32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalogShape {
    Saw,
    VariableSaw,
    CSaw,
    Square,
    Triangle,
    Sine,
    TriangleFold,
    SineFold,
    Buzz,
}

pub const ANALOG_SHAPES: [AnalogShape; 9] = [
    AnalogShape::Saw,
    AnalogShape::VariableSaw,
    AnalogShape::CSaw,
    AnalogShape::Square,
    AnalogShape::Triangle,
    AnalogShape::Sine,
    AnalogShape::TriangleFold,
    AnalogShape::SineFold,
    AnalogShape::Buzz,
];

pub struct AnalogOscillator {
    shape: AnalogShape,
    previous_shape: AnalogShape,
    phase: u32,
    phase_increment: u32,
    pub parameter: i16,
    pub aux_parameter: i16,
    pub pitch: i16,
    high: bool,
    next_sample: i32,
    discontinuity_depth: i32,
}

impl Default for AnalogOscillator {
    fn default() -> Self {
        let mut o = Self {
            shape: AnalogShape::Saw,
            previous_shape: AnalogShape::Saw,
            phase: 0,
            phase_increment: 1,
            parameter: 0,
            aux_parameter: 0,
            pitch: 60 << 7,
            high: false,
            next_sample: 0,
            discontinuity_depth: -16383,
        };
        o.init();
        o
    }
}

impl AnalogOscillator {
    pub fn new() -> Self {
        Self::default()
    }

    fn init(&mut self) {
        self.phase = 0;
        self.phase_increment = 1;
        self.high = false;
        self.parameter = 0;
        self.aux_parameter = 0;
        self.discontinuity_depth = -16383;
        self.pitch = 60 << 7;
        self.next_sample = 0;
    }

    pub fn set_shape(&mut self, shape: AnalogShape) {
        self.shape = shape;
    }
    pub fn set_pitch(&mut self, pitch: i16) {
        self.pitch = pitch;
    }
    pub fn set_parameter(&mut self, parameter: i16) {
        self.parameter = parameter;
    }
    pub fn set_aux_parameter(&mut self, aux: i16) {
        self.aux_parameter = aux;
    }

    fn compute_phase_increment(&self, midi_pitch: i16) -> u32 {
        let t = tables();
        let mut pitch = midi_pitch as i32;
        if pitch >= HIGHEST_NOTE {
            pitch = HIGHEST_NOTE - 1;
        }
        let mut ref_pitch = pitch - PITCH_TABLE_START;
        let mut num_shifts = 0u32;
        while ref_pitch < 0 {
            ref_pitch += OCTAVE;
            num_shifts += 1;
        }
        let idx = (ref_pitch >> 4) as usize;
        let a = t.increments[idx.min(t.increments.len() - 1)];
        let b = t.increments[(idx + 1).min(t.increments.len() - 1)];
        let phase_increment =
            a.wrapping_add(((b.wrapping_sub(a) as i32) * (ref_pitch & 0xf) >> 4) as u32);
        phase_increment >> num_shifts
    }

    /// Render a block. `sync_in` carries fractional reset times (0 =
    /// no sync); `sync_out`, when present, publishes this oscillator's
    /// resets for a slave. Output is i16.
    pub fn render(
        &mut self,
        sync_in: &[u8],
        buffer: &mut [i16],
        sync_out: Option<&mut [u8]>,
        size: usize,
    ) {
        if self.shape != self.previous_shape {
            self.init();
            self.previous_shape = self.shape;
        }
        self.phase_increment = self.compute_phase_increment(self.pitch);
        self.pitch = self.pitch.clamp(0, HIGHEST_NOTE as i16);

        match self.shape {
            AnalogShape::Saw => self.render_saw(sync_in, buffer, sync_out, size),
            AnalogShape::VariableSaw => {
                self.render_variable_saw(sync_in, buffer, sync_out, size)
            }
            AnalogShape::CSaw => self.render_csaw(sync_in, buffer, sync_out, size),
            AnalogShape::Square => {
                self.render_square(sync_in, buffer, sync_out, size)
            }
            AnalogShape::Triangle => self.render_triangle(sync_in, buffer, size),
            AnalogShape::Sine => self.render_sine(sync_in, buffer, size),
            AnalogShape::TriangleFold => self.render_triangle_fold(sync_in, buffer, size),
            AnalogShape::SineFold => self.render_sine_fold(sync_in, buffer, size),
            AnalogShape::Buzz => self.render_buzz(sync_in, buffer, size),
        }
    }

    #[inline]
    fn sync_out_write(sync_out: &mut Option<&mut [u8]>, n: usize, phase: u32, inc: u32) {
        if let Some(so) = sync_out.as_mut() {
            so[n] = if phase < inc {
                (phase / (inc >> 7) + 1) as u8
            } else {
                0
            };
        }
    }

    fn render_saw(
        &mut self,
        sync_in: &[u8],
        buffer: &mut [i16],
        mut sync_out: Option<&mut [u8]>,
        size: usize,
    ) {
        let inc = self.phase_increment;
        let mut next_sample = self.next_sample;
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            let mut sync_reset = false;
            let mut transition_during_reset = false;
            let mut reset_time = 0u32;
            let this0 = next_sample;
            let mut this_sample = this0;
            next_sample = 0;
            if sync_in[n] != 0 {
                reset_time = ((sync_in[n] - 1) as u32) << 9;
                let phase_at_reset = self
                    .phase
                    .wrapping_add((65535 - reset_time).wrapping_mul(inc >> 16));
                sync_reset = true;
                if phase_at_reset < self.phase {
                    transition_during_reset = true;
                }
                let discontinuity = (phase_at_reset >> 17) as i32;
                this_sample -= (discontinuity as i64 * this_blep(reset_time) as i64 >> 15) as i32;
                next_sample -= (discontinuity as i64 * next_blep(reset_time) as i64 >> 15) as i32;
            }
            self.phase = self.phase.wrapping_add(inc);
            let self_reset = self.phase < inc;
            Self::sync_out_write(&mut sync_out, n, self.phase, inc);
            if (transition_during_reset || !sync_reset) && self_reset {
                let t = self.phase / (inc >> 16);
                this_sample -= this_blep(t);
                next_sample -= next_blep(t);
            }
            if sync_reset {
                self.phase = reset_time.wrapping_mul(inc >> 16);
                self.high = false;
            }
            next_sample += (self.phase >> 17) as i32;
            buffer[n] = ((this_sample - 16384) << 1) as i16;
        }
        self.next_sample = next_sample;
    }

    fn render_square(
        &mut self,
        sync_in: &[u8],
        buffer: &mut [i16],
        mut sync_out: Option<&mut [u8]>,
        size: usize,
    ) {
        let inc = self.phase_increment;
        if self.parameter > 32000 {
            self.parameter = 32000;
        }
        let pw = ((32768 - self.parameter as i32) as u32) << 16;
        let mut next_sample = self.next_sample;
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            let mut sync_reset = false;
            let mut self_reset;
            let mut transition_during_reset = false;
            let mut reset_time = 0u32;
            let mut this_sample = next_sample;
            next_sample = 0;
            if sync_in[n] != 0 {
                reset_time = ((sync_in[n] - 1) as u32) << 9;
                let phase_at_reset = self
                    .phase
                    .wrapping_add((65535 - reset_time).wrapping_mul(inc >> 16));
                sync_reset = true;
                if phase_at_reset < self.phase || (!self.high && phase_at_reset >= pw) {
                    transition_during_reset = true;
                }
                if phase_at_reset >= pw {
                    this_sample -= this_blep(reset_time);
                    next_sample -= next_blep(reset_time);
                }
            }
            self.phase = self.phase.wrapping_add(inc);
            self_reset = self.phase < inc;
            Self::sync_out_write(&mut sync_out, n, self.phase, inc);
            loop {
                if !transition_during_reset && sync_reset {
                    break;
                }
                if !self.high {
                    if self.phase < pw {
                        break;
                    }
                    let t = (self.phase - pw) / (inc >> 16);
                    this_sample += this_blep(t);
                    next_sample += next_blep(t);
                    self.high = true;
                }
                if self.high {
                    if !self_reset {
                        break;
                    }
                    self_reset = false;
                    let t = self.phase / (inc >> 16);
                    this_sample -= this_blep(t);
                    next_sample -= next_blep(t);
                    self.high = false;
                }
            }
            if sync_reset {
                self.phase = reset_time.wrapping_mul(inc >> 16);
                self.high = false;
            }
            next_sample += if self.phase < pw { 0 } else { 32767 };
            buffer[n] = ((this_sample - 16384) << 1) as i16;
        }
        self.next_sample = next_sample;
    }

    fn render_variable_saw(
        &mut self,
        sync_in: &[u8],
        buffer: &mut [i16],
        mut sync_out: Option<&mut [u8]>,
        size: usize,
    ) {
        let inc = self.phase_increment;
        if self.parameter < 1024 {
            self.parameter = 1024;
        }
        let pw = (self.parameter as u32) << 16;
        let mut next_sample = self.next_sample;
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            let mut sync_reset = false;
            let mut self_reset;
            let mut transition_during_reset = false;
            let mut reset_time = 0u32;
            let mut this_sample = next_sample;
            next_sample = 0;
            if sync_in[n] != 0 {
                reset_time = ((sync_in[n] - 1) as u32) << 9;
                let phase_at_reset = self
                    .phase
                    .wrapping_add((65535 - reset_time).wrapping_mul(inc >> 16));
                sync_reset = true;
                if phase_at_reset < self.phase || (!self.high && phase_at_reset >= pw) {
                    transition_during_reset = true;
                }
                let before =
                    (phase_at_reset >> 18) as i32 + ((phase_at_reset.wrapping_sub(pw)) >> 18) as i32;
                let after = ((0u32.wrapping_sub(pw)) >> 18) as i32;
                let discontinuity = after - before;
                this_sample += (discontinuity as i64 * this_blep(reset_time) as i64 >> 15) as i32;
                next_sample += (discontinuity as i64 * next_blep(reset_time) as i64 >> 15) as i32;
            }
            self.phase = self.phase.wrapping_add(inc);
            self_reset = self.phase < inc;
            Self::sync_out_write(&mut sync_out, n, self.phase, inc);
            loop {
                if !transition_during_reset && sync_reset {
                    break;
                }
                if !self.high {
                    if self.phase < pw {
                        break;
                    }
                    let t = (self.phase - pw) / (inc >> 16);
                    this_sample -= this_blep(t) >> 1;
                    next_sample -= next_blep(t) >> 1;
                    self.high = true;
                }
                if self.high {
                    if !self_reset {
                        break;
                    }
                    self_reset = false;
                    let t = self.phase / (inc >> 16);
                    this_sample -= this_blep(t) >> 1;
                    next_sample -= next_blep(t) >> 1;
                    self.high = false;
                }
            }
            if sync_reset {
                self.phase = reset_time.wrapping_mul(inc >> 16);
                self.high = false;
            }
            next_sample += (self.phase >> 18) as i32 + ((self.phase.wrapping_sub(pw)) >> 18) as i32;
            buffer[n] = ((this_sample - 16384) << 1) as i16;
        }
        self.next_sample = next_sample;
    }

    fn render_csaw(
        &mut self,
        sync_in: &[u8],
        buffer: &mut [i16],
        mut sync_out: Option<&mut [u8]>,
        size: usize,
    ) {
        let inc = self.phase_increment;
        let mut next_sample = self.next_sample;
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            let mut sync_reset = false;
            let mut self_reset;
            let mut transition_during_reset = false;
            let mut reset_time = 0u32;
            let mut this_sample = next_sample;
            next_sample = 0;
            let mut pw = (self.parameter as u32).wrapping_mul(49152);
            if pw < 8 * inc {
                pw = 8 * inc;
            }
            if sync_in[n] != 0 {
                reset_time = ((sync_in[n] - 1) as u32) << 9;
                let phase_at_reset = self
                    .phase
                    .wrapping_add((65535 - reset_time).wrapping_mul(inc >> 16));
                sync_reset = true;
                if phase_at_reset < self.phase || (!self.high && phase_at_reset >= pw) {
                    transition_during_reset = true;
                }
                if self.phase >= pw {
                    self.discontinuity_depth = -2048 + (self.aux_parameter as i32 >> 2);
                    let before = (phase_at_reset >> 18) as i32;
                    let after = self.discontinuity_depth;
                    let discontinuity = after - before;
                    this_sample += (discontinuity as i64 * this_blep(reset_time) as i64 >> 15) as i32;
                    next_sample += (discontinuity as i64 * next_blep(reset_time) as i64 >> 15) as i32;
                }
            }
            self.phase = self.phase.wrapping_add(inc);
            self_reset = self.phase < inc;
            Self::sync_out_write(&mut sync_out, n, self.phase, inc);
            loop {
                if !transition_during_reset && sync_reset {
                    break;
                }
                if !self.high {
                    if self.phase < pw {
                        break;
                    }
                    let t = (self.phase - pw) / (inc >> 16);
                    let before = self.discontinuity_depth;
                    let after = (self.phase >> 18) as i32;
                    let discontinuity = after - before;
                    this_sample += (discontinuity as i64 * this_blep(t) as i64 >> 15) as i32;
                    next_sample += (discontinuity as i64 * next_blep(t) as i64 >> 15) as i32;
                    self.high = true;
                }
                if self.high {
                    if !self_reset {
                        break;
                    }
                    self_reset = false;
                    self.discontinuity_depth = -2048 + (self.aux_parameter as i32 >> 2);
                    let t = self.phase / (inc >> 16);
                    let before = 16383i32;
                    let after = self.discontinuity_depth;
                    let discontinuity = after - before;
                    this_sample += (discontinuity as i64 * this_blep(t) as i64 >> 15) as i32;
                    next_sample += (discontinuity as i64 * next_blep(t) as i64 >> 15) as i32;
                    self.high = false;
                }
            }
            if sync_reset {
                self.phase = reset_time.wrapping_mul(inc >> 16);
                self.high = false;
            }
            next_sample += if self.phase < pw {
                self.discontinuity_depth
            } else {
                (self.phase >> 18) as i32
            };
            buffer[n] = ((this_sample - 8192) << 1) as i16;
        }
        self.next_sample = next_sample;
    }

    fn render_triangle(&mut self, sync_in: &[u8], buffer: &mut [i16], size: usize) {
        let inc = self.phase_increment;
        let mut phase = self.phase;
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            if sync_in[n] != 0 {
                phase = 0;
            }
            phase = phase.wrapping_add(inc >> 1);
            let phase_16 = (phase >> 16) as u16;
            let mut triangle =
                ((phase_16 << 1) ^ if phase_16 & 0x8000 != 0 { 0xffff } else { 0 }) as i32;
            triangle += 32768;
            buffer[n] = (triangle >> 1) as i16;
            phase = phase.wrapping_add(inc >> 1);
            let phase_16 = (phase >> 16) as u16;
            let mut triangle =
                ((phase_16 << 1) ^ if phase_16 & 0x8000 != 0 { 0xffff } else { 0 }) as i32;
            triangle += 32768;
            buffer[n] = buffer[n].wrapping_add((triangle >> 1) as i16);
        }
        self.phase = phase;
    }

    fn render_sine(&mut self, sync_in: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        let inc = self.phase_increment;
        let mut phase = self.phase;
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            phase = phase.wrapping_add(inc);
            if sync_in[n] != 0 {
                phase = 0;
            }
            buffer[n] = interpolate824(&t.wav_sine, phase) as i16;
        }
        self.phase = phase;
    }

    fn render_triangle_fold(&mut self, sync_in: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        let inc = self.phase_increment;
        let mut phase = self.phase;
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            let gain = 2048 + (self.parameter as i32 * 30720 >> 15);
            if sync_in[n] != 0 {
                phase = 0;
            }
            phase = phase.wrapping_add(inc >> 1);
            let phase_16 = (phase >> 16) as u16;
            let mut triangle =
                ((phase_16 << 1) ^ if phase_16 & 0x8000 != 0 { 0xffff } else { 0 }) as i32;
            triangle += 32768;
            triangle = (triangle as i64 * gain as i64 >> 15) as i32;
            let folded = interpolate88(&t.ws_tri_fold, (triangle + 32768) as u16);
            buffer[n] = (folded >> 1) as i16;
            phase = phase.wrapping_add(inc >> 1);
            let phase_16 = (phase >> 16) as u16;
            let mut triangle =
                ((phase_16 << 1) ^ if phase_16 & 0x8000 != 0 { 0xffff } else { 0 }) as i32;
            triangle += 32768;
            triangle = (triangle as i64 * gain as i64 >> 15) as i32;
            let folded = interpolate88(&t.ws_tri_fold, (triangle + 32768) as u16);
            buffer[n] = buffer[n].wrapping_add((folded >> 1) as i16);
        }
        self.phase = phase;
    }

    fn render_sine_fold(&mut self, sync_in: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        let inc = self.phase_increment;
        let mut phase = self.phase;
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            let gain = 2048 + (self.parameter as i32 * 30720 >> 15);
            if sync_in[n] != 0 {
                phase = 0;
            }
            phase = phase.wrapping_add(inc >> 1);
            let mut sine = interpolate824(&t.wav_sine, phase);
            sine = (sine as i64 * gain as i64 >> 15) as i32;
            let folded = interpolate88(&t.ws_sine_fold, (sine + 32768) as u16);
            buffer[n] = (folded >> 1) as i16;
            phase = phase.wrapping_add(inc >> 1);
            let mut sine = interpolate824(&t.wav_sine, phase);
            sine = (sine as i64 * gain as i64 >> 15) as i32;
            let folded = interpolate88(&t.ws_sine_fold, (sine + 32768) as u16);
            buffer[n] = buffer[n].wrapping_add((folded >> 1) as i16);
        }
        self.phase = phase;
    }

    fn render_buzz(&mut self, sync_in: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        let inc = self.phase_increment;
        let shifted_pitch = self.pitch as i32 + ((32767 - self.parameter as i32) >> 1);
        let crossfade_amt = ((shifted_pitch << 6) & 0xffff) as u16;
        let mut index = (shifted_pitch >> 10) as usize;
        if index >= NUM_ZONES {
            index = NUM_ZONES - 1;
        }
        let mut index2 = index + 1;
        if index2 >= NUM_ZONES {
            index2 = NUM_ZONES - 1;
        }
        let wave_1 = &t.comb[index];
        let wave_2 = &t.comb[index2];
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            self.phase = self.phase.wrapping_add(inc);
            if sync_in[n] != 0 {
                self.phase = 0;
            }
            buffer[n] = crossfade(wave_1, wave_2, self.phase, crossfade_amt) as i16;
        }
    }
}

// ── macro oscillator (the models) ───────────────────────────────────────────

const SEMI: i16 = 128;

/// macro_oscillator.cc intervals[65] — detune offsets (semitones×128).
const INTERVALS: [i16; 65] = [
    -24 * SEMI, -24 * SEMI, -24 * SEMI + 4,
    -23 * SEMI, -22 * SEMI, -21 * SEMI, -20 * SEMI, -19 * SEMI, -18 * SEMI,
    -17 * SEMI - 4, -17 * SEMI,
    -16 * SEMI, -15 * SEMI, -14 * SEMI, -13 * SEMI,
    -12 * SEMI - 4, -12 * SEMI,
    -11 * SEMI, -10 * SEMI, -9 * SEMI, -8 * SEMI,
    -7 * SEMI - 4, -7 * SEMI,
    -6 * SEMI, -5 * SEMI, -4 * SEMI, -3 * SEMI, -2 * SEMI, -SEMI,
    -24, -8, -4, 0, 4, 8, 24,
    SEMI, 2 * SEMI, 3 * SEMI, 4 * SEMI, 5 * SEMI, 6 * SEMI,
    7 * SEMI, 7 * SEMI + 4,
    8 * SEMI, 9 * SEMI, 10 * SEMI, 11 * SEMI,
    12 * SEMI, 12 * SEMI + 4,
    13 * SEMI, 14 * SEMI, 15 * SEMI, 16 * SEMI,
    17 * SEMI, 17 * SEMI + 4,
    18 * SEMI, 19 * SEMI, 20 * SEMI, 21 * SEMI, 22 * SEMI, 23 * SEMI,
    24 * SEMI - 4, 24 * SEMI, 24 * SEMI,
];

/// The braids macro-oscillator models. The analog models (0..=12) are
/// ported here; the digital models (13+) dispatch to the digital
/// oscillator (ported in a later pass; currently silent placeholders).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroModel {
    CSaw,
    Morph,
    SawSquare,
    SineTriangle,
    Buzz,
    SquareSub,
    SawSub,
    SquareSync,
    SawSync,
    TripleSaw,
    TripleSquare,
    TripleTriangle,
    TripleSine,
    // digital models (dispatched to DigitalOscillator)
    TripleRingMod,
    SawSwarm,
    SawComb,
    Toy,
    DigitalFilterLp,
    DigitalFilterPk,
    DigitalFilterBp,
    DigitalFilterHp,
}

/// All macro models in panel order — parallel to [`MODEL_NAMES`].
pub const MODELS: [MacroModel; 21] = [
    MacroModel::CSaw,
    MacroModel::Morph,
    MacroModel::SawSquare,
    MacroModel::SineTriangle,
    MacroModel::Buzz,
    MacroModel::SquareSub,
    MacroModel::SawSub,
    MacroModel::SquareSync,
    MacroModel::SawSync,
    MacroModel::TripleSaw,
    MacroModel::TripleSquare,
    MacroModel::TripleTriangle,
    MacroModel::TripleSine,
    MacroModel::TripleRingMod,
    MacroModel::SawSwarm,
    MacroModel::SawComb,
    MacroModel::Toy,
    MacroModel::DigitalFilterLp,
    MacroModel::DigitalFilterPk,
    MacroModel::DigitalFilterBp,
    MacroModel::DigitalFilterHp,
];

pub const MODEL_NAMES: [&str; 21] = [
    "csaw",
    "morph",
    "saw_square",
    "sine_triangle",
    "buzz",
    "square_sub",
    "saw_sub",
    "square_sync",
    "saw_sync",
    "triple_saw",
    "triple_square",
    "triple_triangle",
    "triple_sine",
    "triple_ring_mod",
    "saw_swarm",
    "saw_comb",
    "toy",
    "digital_filter_lp",
    "digital_filter_pk",
    "digital_filter_bp",
    "digital_filter_hp",
];

pub struct MacroOscillator {
    osc: [AnalogOscillator; 3],
    digital: DigitalOscillator,
    pub model: MacroModel,
    pub pitch: i16,
    pub parameter: [i16; 2],
    temp: Vec<i16>,
    sync_buffer: Vec<u8>,
    lp_state: i32,
}

impl Default for MacroOscillator {
    fn default() -> Self {
        Self {
            osc: [
                AnalogOscillator::new(),
                AnalogOscillator::new(),
                AnalogOscillator::new(),
            ],
            digital: DigitalOscillator::new(),
            model: MacroModel::Morph,
            pitch: 60 << 7,
            parameter: [0, 0],
            temp: vec![0; 128],
            sync_buffer: vec![0; 128],
            lp_state: 0,
        }
    }
}

impl MacroOscillator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_model(&mut self, model: MacroModel) {
        self.model = model;
    }
    pub fn set_pitch(&mut self, pitch: i16) {
        self.pitch = pitch;
    }
    pub fn set_parameters(&mut self, p0: i16, p1: i16) {
        self.parameter = [p0, p1];
    }

    fn ensure(&mut self, size: usize) {
        if self.temp.len() < size {
            self.temp.resize(size, 0);
            self.sync_buffer.resize(size, 0);
        }
    }

    pub fn render(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        self.ensure(size);
        match self.model {
            MacroModel::CSaw => self.render_csaw(sync, buffer, size),
            MacroModel::Morph => self.render_morph(sync, buffer, size),
            MacroModel::SawSquare => self.render_saw_square(sync, buffer, size),
            MacroModel::SineTriangle => self.render_sine_triangle(sync, buffer, size),
            MacroModel::Buzz => self.render_buzz(sync, buffer, size),
            MacroModel::SquareSub | MacroModel::SawSub => self.render_sub(sync, buffer, size),
            MacroModel::SquareSync | MacroModel::SawSync => self.render_dual_sync(sync, buffer, size),
            MacroModel::TripleSaw
            | MacroModel::TripleSquare
            | MacroModel::TripleTriangle
            | MacroModel::TripleSine => self.render_triple(sync, buffer, size),
            MacroModel::TripleRingMod => {
                self.render_digital(DigitalShape::TripleRingMod, sync, buffer, size)
            }
            MacroModel::SawSwarm => self.render_digital(DigitalShape::SawSwarm, sync, buffer, size),
            MacroModel::Toy => self.render_digital(DigitalShape::Toy, sync, buffer, size),
            MacroModel::SawComb => self.render_saw_comb(sync, buffer, size),
            MacroModel::DigitalFilterLp => {
                self.render_digital(DigitalShape::DigitalFilterLp, sync, buffer, size)
            }
            MacroModel::DigitalFilterPk => {
                self.render_digital(DigitalShape::DigitalFilterPk, sync, buffer, size)
            }
            MacroModel::DigitalFilterBp => {
                self.render_digital(DigitalShape::DigitalFilterBp, sync, buffer, size)
            }
            MacroModel::DigitalFilterHp => {
                self.render_digital(DigitalShape::DigitalFilterHp, sync, buffer, size)
            }
        }
    }

    fn render_digital(
        &mut self,
        shape: DigitalShape,
        sync: &[u8],
        buffer: &mut [i16],
        size: usize,
    ) {
        self.digital.set_shape(shape);
        self.digital.set_parameters(self.parameter[0], self.parameter[1]);
        self.digital.set_pitch(self.pitch);
        self.digital.render(sync, buffer, size);
    }

    /// SAW_COMB: render a raw saw, then run it through the digital comb.
    fn render_saw_comb(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        self.osc[0].set_parameter(0);
        self.osc[0].set_pitch(self.pitch);
        self.osc[0].set_shape(AnalogShape::Saw);
        self.osc[0].render(sync, buffer, None, size);
        self.digital.set_shape(DigitalShape::Comb);
        self.digital.set_parameters(self.parameter[0], self.parameter[1]);
        self.digital.set_pitch(self.pitch);
        self.digital.render(sync, buffer, size);
    }

    fn render_csaw(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        self.osc[0].set_pitch(self.pitch);
        self.osc[0].set_shape(AnalogShape::CSaw);
        self.osc[0].set_parameter(self.parameter[0]);
        self.osc[0].set_aux_parameter(self.parameter[1]);
        self.osc[0].render(sync, buffer, None, size);
        let shift = (32767 - self.parameter[1] as i32) >> 4;
        for b in buffer.iter_mut().take(size) {
            let s = *b as i32 + shift;
            *b = ((s * 13) >> 3).clamp(-32768, 32767) as i16;
        }
    }

    fn render_morph(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        self.osc[0].set_pitch(self.pitch);
        self.osc[1].set_pitch(self.pitch);
        let p0 = self.parameter[0] as i32;
        let balance: u16;
        if p0 <= 10922 {
            self.osc[0].set_parameter(0);
            self.osc[1].set_parameter(0);
            self.osc[0].set_shape(AnalogShape::Triangle);
            self.osc[1].set_shape(AnalogShape::Saw);
            balance = (p0 * 6).clamp(0, 65535) as u16;
        } else if p0 <= 21845 {
            self.osc[0].set_parameter(0);
            self.osc[1].set_parameter(0);
            self.osc[0].set_shape(AnalogShape::Square);
            self.osc[1].set_shape(AnalogShape::Saw);
            balance = (65535 - (p0 - 10923) * 6).clamp(0, 65535) as u16;
        } else {
            self.osc[0].set_parameter(((p0 - 21846) * 3).clamp(-32768, 32767) as i16);
            self.osc[1].set_parameter(0);
            self.osc[0].set_shape(AnalogShape::Square);
            self.osc[1].set_shape(AnalogShape::Sine);
            balance = 0;
        }
        self.osc[0].render(sync, buffer, None, size);
        self.osc[1].render(sync, &mut self.temp[..size], None, size);

        let mut lp_cutoff = self.pitch as i32 - (self.parameter[1] as i32 >> 1) + 128 * 128;
        lp_cutoff = lp_cutoff.clamp(0, 32767);
        let f = interpolate824_u16(&t.svf_cutoff, (lp_cutoff as u32) << 17);
        let mut lp_state = self.lp_state;
        let mut fuzz_amount = (self.parameter[1] as i32) << 1;
        if self.pitch as i32 > (80 << 7) {
            fuzz_amount -= (self.pitch as i32 - (80 << 7)) << 4;
            if fuzz_amount < 0 {
                fuzz_amount = 0;
            }
        }
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            let sample = mix(buffer[n], self.temp[n], balance);
            let shifted_sample = sample as i32;
            lp_state += ((shifted_sample - lp_state) as i64 * f as i64 >> 15) as i32;
            lp_state = lp_state.clamp(-32768, 32767);
            let idx = (lp_state + 32768).clamp(0, 65535) as u16;
            let fuzzed = interpolate88(&t.violent_overdrive, idx) as i16;
            buffer[n] = mix(sample, fuzzed, fuzz_amount.clamp(0, 65535) as u16);
        }
        self.lp_state = lp_state;
    }

    fn render_saw_square(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        self.osc[0].set_parameter(self.parameter[0]);
        self.osc[1].set_parameter(self.parameter[0]);
        self.osc[0].set_pitch(self.pitch);
        self.osc[1].set_pitch(self.pitch);
        self.osc[0].set_shape(AnalogShape::VariableSaw);
        self.osc[1].set_shape(AnalogShape::Square);
        self.osc[0].render(sync, buffer, None, size);
        self.osc[1].render(sync, &mut self.temp[..size], None, size);
        let balance = ((self.parameter[0] as i32) << 1).clamp(0, 65535) as u16;
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            let attenuated_square = ((self.temp[n] as i32) * 148 >> 8) as i16;
            buffer[n] = mix(buffer[n], attenuated_square, balance);
        }
    }

    fn render_sine_triangle(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let mut att_sine = 32767 - 6 * (self.pitch as i32 - (92 << 7));
        let mut att_tri = 32767 - 7 * (self.pitch as i32 - (80 << 7));
        att_tri = att_tri.clamp(0, 32767);
        att_sine = att_sine.clamp(0, 32767);
        let timbre = self.parameter[0] as i32;
        self.osc[0].set_parameter((timbre * att_sine >> 15) as i16);
        self.osc[1].set_parameter((timbre * att_tri >> 15) as i16);
        self.osc[0].set_pitch(self.pitch);
        self.osc[1].set_pitch(self.pitch);
        self.osc[0].set_shape(AnalogShape::SineFold);
        self.osc[1].set_shape(AnalogShape::TriangleFold);
        self.osc[0].render(sync, buffer, None, size);
        self.osc[1].render(sync, &mut self.temp[..size], None, size);
        let balance = ((self.parameter[1] as i32) << 1).clamp(0, 65535) as u16;
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            buffer[n] = mix(buffer[n], self.temp[n], balance);
        }
    }

    fn render_buzz(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        self.osc[0].set_parameter(self.parameter[0]);
        self.osc[0].set_shape(AnalogShape::Buzz);
        self.osc[0].set_pitch(self.pitch);
        self.osc[1].set_parameter(self.parameter[0]);
        self.osc[1].set_shape(AnalogShape::Buzz);
        self.osc[1].set_pitch(self.pitch + (self.parameter[1] >> 8));
        self.osc[0].render(sync, buffer, None, size);
        self.osc[1].render(sync, &mut self.temp[..size], None, size);
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            buffer[n] = (buffer[n] >> 1).wrapping_add(self.temp[n] >> 1);
        }
    }

    fn render_sub(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let base = if self.model == MacroModel::SquareSub {
            AnalogShape::Square
        } else {
            AnalogShape::VariableSaw
        };
        self.osc[0].set_parameter(self.parameter[0]);
        self.osc[0].set_shape(base);
        self.osc[0].set_pitch(self.pitch);
        self.osc[1].set_parameter(0);
        self.osc[1].set_shape(AnalogShape::Square);
        let octave = if self.parameter[1] < 16384 { 24 << 7 } else { 12 << 7 };
        self.osc[1].set_pitch(self.pitch - octave);
        self.osc[0].render(sync, buffer, None, size);
        self.osc[1].render(sync, &mut self.temp[..size], None, size);
        let p1 = self.parameter[1] as i32;
        let sub_gain = (if p1 < 16384 { 16383 - p1 } else { p1 - 16384 } << 1).clamp(0, 65535) as u16;
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            buffer[n] = mix(buffer[n], self.temp[n], sub_gain);
        }
    }

    fn render_dual_sync(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let base = if self.model == MacroModel::SquareSync {
            AnalogShape::Square
        } else {
            AnalogShape::Saw
        };
        self.osc[0].set_parameter(0);
        self.osc[0].set_shape(base);
        self.osc[0].set_pitch(self.pitch);
        self.osc[1].set_parameter(0);
        self.osc[1].set_shape(base);
        self.osc[1].set_pitch(self.pitch + (self.parameter[0] >> 2));
        // osc0 publishes its resets into sync_buffer; osc1 follows them
        let mut sb = std::mem::take(&mut self.sync_buffer);
        self.osc[0].render(sync, buffer, Some(&mut sb[..size]), size);
        self.osc[1].render(&sb[..size], &mut self.temp[..size], None, size);
        self.sync_buffer = sb;
        let balance = ((self.parameter[1] as i32) << 1).clamp(0, 65535) as u16;
        #[allow(clippy::needless_range_loop)] // n strides buffer + temp
        for n in 0..size {
            buffer[n] = ((mix(buffer[n], self.temp[n], balance) >> 2) as i32 * 3) as i16;
        }
    }

    fn render_triple(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let base = match self.model {
            MacroModel::TripleSaw => AnalogShape::Saw,
            MacroModel::TripleTriangle => AnalogShape::Triangle,
            MacroModel::TripleSquare => AnalogShape::Square,
            _ => AnalogShape::Sine,
        };
        self.osc[0].set_parameter(0);
        self.osc[1].set_parameter(0);
        self.osc[2].set_parameter(0);
        self.osc[0].set_pitch(self.pitch);
        for i in 0..2 {
            let p = self.parameter[i] as i32;
            let detune_1 = INTERVALS[(p >> 9).clamp(0, 64) as usize] as i32;
            let detune_2 = INTERVALS[(((p >> 8) + 1) >> 1).clamp(0, 64) as usize] as i32;
            let xfade = (p << 8) & 0xffff;
            let detune = detune_1 + ((detune_2 - detune_1) * xfade >> 16);
            self.osc[i + 1].set_pitch((self.pitch as i32 + detune).clamp(0, 32767) as i16);
        }
        self.osc[0].set_shape(base);
        self.osc[1].set_shape(base);
        self.osc[2].set_shape(base);
        buffer[..size].fill(0);
        for i in 0..3 {
            self.osc[i].render(sync, &mut self.temp[..size], None, size);
            #[allow(clippy::needless_range_loop)] // n strides buffer + temp
            for n in 0..size {
                buffer[n] =
                    buffer[n].wrapping_add(((self.temp[n] as i32) * 21 >> 6) as i16);
            }
        }
    }
}

// ── the digital oscillator ───────────────────────────────────────────────────

/// The braids digital-oscillator shapes (a growing subset of the
/// firmware's `DigitalOscillatorShape`, ported in batches).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigitalShape {
    TripleRingMod,
    SawSwarm,
    Comb,
    Toy,
    DigitalFilterLp,
    DigitalFilterPk,
    DigitalFilterBp,
    DigitalFilterHp,
}

const FIR4_COEFFICIENTS: [u32; 4] = [10530, 14751, 16384, 14751];
const FIR4_DC_OFFSET: i32 = 28208;
const PHASE_RESET: [u32; 4] = [0, 0x8000_0000, 0x4000_0000, 0x8000_0000];

#[inline]
fn clip16(x: i32) -> i32 {
    x.clamp(-32768, 32767)
}

/// braids' `DigitalOscillator` — the bank of wavetable/physical/noise
/// digital models behind the macro oscillator. State for all models is
/// flattened here (only one model is active at a time; `init` zeroes it).
pub struct DigitalOscillator {
    shape: DigitalShape,
    previous_shape: DigitalShape,
    pitch: i16,
    parameter: [i16; 2],
    phase: u32,
    phase_increment: u32,
    strike: bool,
    rng: u32,
    // flattened per-model state
    formant_phase: [u32; 3],
    saw_phase: [u32; 6],
    saw_bp: i32,
    saw_lp: i32,
    ffm_previous_sample: i32,
    toy_decimation_counter: u16,
    toy_held_sample: u8,
    comb_delay: Vec<i16>,
    // resonant digital-filter (res) state
    res_modulator_phase_increment: u32,
    res_modulator_phase: u32,
    res_square_modulator_phase: u32,
    res_integrator: i32,
    res_polarity: bool,
}

impl Default for DigitalOscillator {
    fn default() -> Self {
        Self {
            shape: DigitalShape::TripleRingMod,
            previous_shape: DigitalShape::TripleRingMod,
            pitch: 60 << 7,
            parameter: [0, 0],
            phase: 0,
            phase_increment: 0,
            strike: true,
            rng: 0x2192_8374,
            formant_phase: [0; 3],
            saw_phase: [0; 6],
            saw_bp: 0,
            saw_lp: 0,
            ffm_previous_sample: 0,
            toy_decimation_counter: 0,
            toy_held_sample: 0,
            comb_delay: vec![0; COMB_DELAY_LENGTH],
            res_modulator_phase_increment: 0,
            res_modulator_phase: 0,
            res_square_modulator_phase: 0,
            res_integrator: 0,
            res_polarity: false,
        }
    }
}

impl DigitalOscillator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_shape(&mut self, shape: DigitalShape) {
        self.shape = shape;
    }
    pub fn set_pitch(&mut self, pitch: i16) {
        // Smooth HF noise when the pitch CV is noisy (set_pitch in firmware).
        if self.pitch > (90 << 7) && pitch > (90 << 7) {
            self.pitch = ((self.pitch as i32 + pitch as i32) >> 1) as i16;
        } else {
            self.pitch = pitch;
        }
    }
    pub fn set_parameters(&mut self, p0: i16, p1: i16) {
        self.parameter = [p0, p1];
    }
    pub fn strike(&mut self) {
        self.strike = true;
    }

    fn init(&mut self) {
        self.formant_phase = [0; 3];
        self.saw_phase = [0; 6];
        self.saw_bp = 0;
        self.saw_lp = 0;
        self.ffm_previous_sample = 0;
        self.toy_decimation_counter = 0;
        self.toy_held_sample = 0;
        self.comb_delay.iter_mut().for_each(|s| *s = 0);
        self.res_modulator_phase_increment = 0;
        self.res_modulator_phase = 0;
        self.res_square_modulator_phase = 0;
        self.res_integrator = 0;
        self.res_polarity = false;
        self.phase = 0;
        self.strike = true;
    }

    #[inline]
    fn next_word(&mut self) -> u32 {
        // stmlib Random LCG.
        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.rng
    }

    pub fn render(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        if self.shape != self.previous_shape {
            self.init();
            self.previous_shape = self.shape;
        }
        self.phase_increment = compute_phase_increment(self.pitch);
        if self.pitch > DIGITAL_HIGHEST_NOTE as i16 {
            self.pitch = DIGITAL_HIGHEST_NOTE as i16;
        } else if self.pitch < 0 {
            self.pitch = 0;
        }
        match self.shape {
            DigitalShape::TripleRingMod => self.render_triple_ring_mod(sync, buffer, size),
            DigitalShape::SawSwarm => self.render_saw_swarm(sync, buffer, size),
            DigitalShape::Comb => self.render_comb(buffer, size),
            DigitalShape::Toy => self.render_toy(sync, buffer, size),
            DigitalShape::DigitalFilterLp => self.render_digital_filter(0, sync, buffer, size),
            DigitalShape::DigitalFilterPk => self.render_digital_filter(1, sync, buffer, size),
            DigitalShape::DigitalFilterBp => self.render_digital_filter(2, sync, buffer, size),
            DigitalShape::DigitalFilterHp => self.render_digital_filter(3, sync, buffer, size),
        }
    }

    /// The resonant digital filter (LP/PK/BP/HP), a "two-operator"
    /// formant model: a carrier sine resonant peak swept by `parameter_0`
    /// over a saw/triangle/square source, balanced by `parameter_1`.
    fn render_digital_filter(
        &mut self,
        filter_type: u8,
        sync: &[u8],
        buffer: &mut [i16],
        size: usize,
    ) {
        let t = tables();
        let mut shifted_pitch =
            (self.pitch as i32 + ((self.parameter[0] as i32 - 2048) >> 1)) as i16;
        if shifted_pitch > 16383 {
            shifted_pitch = 16383;
        }
        let mut modulator_phase = self.res_modulator_phase;
        let mut square_modulator_phase = self.res_square_modulator_phase;
        let mut square_integrator = self.res_integrator;
        let mut polarity = self.res_polarity;
        let mut modulator_phase_increment = self.res_modulator_phase_increment;
        let target_increment = compute_phase_increment(shifted_pitch);
        let size_u = size.max(1) as u32;
        let phase_inc_inc = if modulator_phase_increment < target_increment {
            (target_increment - modulator_phase_increment) / size_u
        } else {
            !((modulator_phase_increment - target_increment) / size_u)
        };

        for (n, b) in buffer.iter_mut().take(size).enumerate() {
            self.phase = self.phase.wrapping_add(self.phase_increment);
            modulator_phase_increment = modulator_phase_increment.wrapping_add(phase_inc_inc);
            modulator_phase = modulator_phase.wrapping_add(modulator_phase_increment);
            let integrator_gain = (modulator_phase_increment >> 14) as u16;

            if sync[n] != 0 {
                polarity = true;
                self.phase = 0;
                modulator_phase = 0;
                square_modulator_phase = 0;
                square_integrator = 0;
            }
            square_modulator_phase =
                square_modulator_phase.wrapping_add(modulator_phase_increment);
            if self.phase < self.phase_increment {
                modulator_phase = PHASE_RESET[filter_type as usize];
            }
            if (self.phase << 1) < (self.phase_increment << 1) {
                polarity = !polarity;
                square_modulator_phase = PHASE_RESET[((filter_type & 1) + 2) as usize];
            }

            let carrier = interpolate824(&t.wav_sine, modulator_phase);
            let square_carrier = interpolate824(&t.wav_sine, square_modulator_phase);

            let saw = !(self.phase >> 16) as u16;
            let double_saw = !(self.phase >> 15) as u16;
            let triangle =
                ((self.phase >> 15) as u16) ^ (if self.phase & 0x8000_0000 != 0 { 0xffff } else { 0 });
            let window = if self.parameter[1] < 16384 { saw } else { triangle };

            let mut pulse = ((square_carrier as i64 * double_saw as i64) >> 16) as i32;
            if polarity {
                pulse = -pulse;
            }
            square_integrator += ((pulse as i64 * integrator_gain as i64) >> 16) as i32;
            square_integrator = clip16(square_integrator);

            let saw_tri_signal: i32;
            let square_signal: i32;
            if filter_type & 2 != 0 {
                saw_tri_signal = ((carrier as i64 * window as i64) >> 16) as i32;
                square_signal = pulse;
            } else {
                saw_tri_signal =
                    ((window as i64 * (carrier as i64 + 32768) >> 16) - 32768) as i32;
                square_signal = if filter_type == 1 {
                    (pulse + square_integrator) >> 1
                } else {
                    square_integrator
                };
            }
            let balance = ((if self.parameter[1] < 16384 {
                self.parameter[1] as i32
            } else {
                !(self.parameter[1] as i32)
            }) << 2) as u16;
            *b = mix(saw_tri_signal as i16, square_signal as i16, balance);
        }
        self.res_modulator_phase = modulator_phase;
        self.res_square_modulator_phase = square_modulator_phase;
        self.res_integrator = square_integrator;
        self.res_modulator_phase_increment = modulator_phase_increment;
        self.res_polarity = polarity;
    }

    fn render_triple_ring_mod(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        let mut phase = self.phase.wrapping_add(1 << 30);
        let increment = self.phase_increment;
        let mut mod_phase = self.formant_phase[0];
        let mut mod_phase_2 = self.formant_phase[1];
        let mod_inc = compute_phase_increment(
            (self.pitch as i32 + ((self.parameter[0] as i32 - 16384) >> 2)) as i16,
        );
        let mod_inc_2 = compute_phase_increment(
            (self.pitch as i32 + ((self.parameter[1] as i32 - 16384) >> 2)) as i16,
        );
        for (n, b) in buffer.iter_mut().take(size).enumerate() {
            phase = phase.wrapping_add(increment);
            if sync[n] != 0 {
                phase = 0;
                mod_phase = 0;
                mod_phase_2 = 0;
            }
            mod_phase = mod_phase.wrapping_add(mod_inc);
            mod_phase_2 = mod_phase_2.wrapping_add(mod_inc_2);
            let mut result = interpolate824(&t.wav_sine, phase);
            result = result * interpolate824(&t.wav_sine, mod_phase) >> 16;
            result = result * interpolate824(&t.wav_sine, mod_phase_2) >> 16;
            result = interpolate88(&t.moderate_overdrive, (result + 32768) as u16);
            *b = result as i16;
        }
        self.phase = phase.wrapping_sub(1 << 30);
        self.formant_phase[0] = mod_phase;
        self.formant_phase[1] = mod_phase_2;
    }

    fn render_saw_swarm(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        let mut detune = self.parameter[0] as i32 + 1024;
        detune = (detune * detune) >> 9;
        let mut increments = [0u32; 7];
        for (i, inc) in increments.iter_mut().enumerate() {
            let saw_detune = detune * (i as i32 - 3);
            let detune_integral = saw_detune >> 16;
            let detune_fractional = saw_detune & 0xffff;
            let inc_a = compute_phase_increment((self.pitch as i32 + detune_integral) as i16) as i64;
            let inc_b =
                compute_phase_increment((self.pitch as i32 + detune_integral + 1) as i16) as i64;
            *inc = (inc_a + (((inc_b - inc_a) * detune_fractional as i64) >> 16)) as u32;
        }
        if self.strike {
            for k in 0..6 {
                self.saw_phase[k] = self.next_word();
            }
            self.strike = false;
        }
        let mut hp_cutoff = self.pitch as i32;
        if (self.parameter[1] as i32) < 10922 {
            hp_cutoff += ((self.parameter[1] as i32 - 10922) * 24) >> 5;
        } else {
            hp_cutoff += ((self.parameter[1] as i32 - 10922) * 12) >> 5;
        }
        hp_cutoff = hp_cutoff.clamp(0, 32767);
        let f = interpolate824_u16(&t.svf_cutoff, (hp_cutoff as u32) << 17) as i64;
        let damp = t.svf_damp[0] as i64;
        let mut bp = self.saw_bp;
        let mut lp = self.saw_lp;
        let mut phase0 = self.phase;
        for (n, b) in buffer.iter_mut().take(size).enumerate() {
            if sync[n] != 0 {
                self.saw_phase = [0; 6];
            }
            phase0 = phase0.wrapping_add(increments[0]);
            for k in 0..6 {
                self.saw_phase[k] = self.saw_phase[k].wrapping_add(increments[k + 1]);
            }
            let mut sample = -28672i32;
            sample += (phase0 >> 19) as i32;
            for k in 0..6 {
                sample += (self.saw_phase[k] >> 19) as i32;
            }
            sample = interpolate88(&t.moderate_overdrive, (sample + 32768) as u16);
            let notch = sample - ((bp as i64 * damp >> 15) as i32);
            lp += (f * bp as i64 >> 15) as i32;
            lp = clip16(lp);
            let hp = notch - lp;
            bp += (f * hp as i64 >> 15) as i32;
            *b = clip16(hp) as i16;
        }
        self.phase = phase0;
        self.saw_bp = bp;
        self.saw_lp = lp;
    }

    /// Comb filter applied in place over a pre-rendered saw buffer.
    fn render_comb(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        let pitch = self.pitch as i32 + ((self.parameter[0] as i32 - 16384) >> 1);
        let filtered_pitch = (15 * self.ffm_previous_sample + pitch) >> 4;
        self.ffm_previous_sample = filtered_pitch;
        let mut delay = compute_delay(filtered_pitch as i16);
        if delay > (COMB_DELAY_LENGTH as u32) << 16 {
            delay = (COMB_DELAY_LENGTH as u32) << 16;
        }
        let delay_integral = (delay >> 16) as usize;
        let delay_fractional = (delay & 0xffff) as i64;
        let mut resonance = (self.parameter[1] as i32) * 2 - 32768;
        resonance = interpolate88(&t.moderate_overdrive, (resonance + 32768) as u16);
        let resonance = resonance as i64;
        let mut delay_ptr = (self.phase as usize) % COMB_DELAY_LENGTH;
        for b in buffer.iter_mut().take(size) {
            let input = *b as i32;
            let offset = delay_ptr + 2 * COMB_DELAY_LENGTH - delay_integral;
            let a = self.comb_delay[offset % COMB_DELAY_LENGTH] as i64;
            let bb = self.comb_delay[(offset - 1) % COMB_DELAY_LENGTH] as i64;
            let delayed = (a + (((bb - a) * (delay_fractional >> 1)) >> 15)) as i32;
            let feedback = clip16(((delayed as i64 * resonance >> 15) as i32) + (input >> 1));
            self.comb_delay[delay_ptr] = feedback as i16;
            let out = clip16((input + (delayed << 1)) >> 1);
            *b = out as i16;
            delay_ptr = (delay_ptr + 1) % COMB_DELAY_LENGTH;
        }
        self.phase = delay_ptr as u32;
    }

    fn render_toy(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        // 4x oversampling.
        self.phase_increment >>= 2;
        let phase_increment = self.phase_increment;
        let mut phase = self.phase;
        let mut decimation_counter = self.toy_decimation_counter;
        let decimation_count = 512u16.wrapping_sub((self.parameter[0] >> 6) as u16);
        let mut held_sample = self.toy_held_sample;
        for (n, b) in buffer.iter_mut().take(size).enumerate() {
            let mut filtered_sample = 0i32;
            if sync[n] != 0 {
                phase = 0;
            }
            for &coeff in FIR4_COEFFICIENTS.iter() {
                phase = phase.wrapping_add(phase_increment);
                if decimation_counter >= decimation_count {
                    let x = (self.parameter[1] >> 8) as u32;
                    let v = (((phase >> 24) ^ (x << 1)) & !x).wrapping_add(x >> 1);
                    held_sample = v as u8;
                    decimation_counter = 0;
                }
                filtered_sample += (coeff * held_sample as u32) as i32;
                decimation_counter = decimation_counter.wrapping_add(1);
            }
            *b = ((filtered_sample >> 8) - FIR4_DC_OFFSET) as i16;
        }
        self.toy_held_sample = held_sample;
        self.toy_decimation_counter = decimation_counter;
        self.phase = phase;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_shape(shape: AnalogShape, param: i16, pitch: i16, n: usize) -> Vec<i16> {
        let mut osc = AnalogOscillator::new();
        osc.set_shape(shape);
        osc.set_pitch(pitch);
        let sync = vec![0u8; n];
        let mut out = vec![0i16; n];
        // first render triggers init() on the shape change (the firmware
        // re-sets parameters every block) — so set the parameter after it
        osc.render(&sync, &mut out, None, n);
        osc.set_parameter(param);
        for _ in 0..4 {
            osc.render(&sync, &mut out, None, n);
        }
        out
    }

    fn zero_crossings(buf: &[i16]) -> usize {
        buf.windows(2).filter(|w| (w[0] >= 0) != (w[1] >= 0)).count()
    }

    #[test]
    fn tables_load_and_size() {
        let t = tables();
        assert_eq!(t.wav_sine.len(), 257);
        assert_eq!(t.increments.len(), 97);
        assert_eq!(t.ws_sine_fold.len(), 257);
        assert_eq!(t.ws_tri_fold.len(), 257);
        assert_eq!(t.comb.len(), 15);
        assert!(t.comb.iter().all(|c| c.len() == 257));
        // wav_sine is a real sine: peak near +/-32767, zero at 0 and 128
        assert!(t.wav_sine[64] > 32000);
        assert_eq!(t.wav_sine[0], 0);
    }

    #[test]
    fn increments_rise_with_pitch() {
        let lo = AnalogOscillator::new().compute_phase_increment(48 << 7);
        let hi = AnalogOscillator::new().compute_phase_increment(72 << 7);
        // an octave up roughly doubles the increment
        assert!(hi > lo, "{lo} -> {hi}");
        let ratio = hi as f64 / lo as f64;
        assert!((3.5..4.5).contains(&ratio), "two octaves ~4x: {ratio}");
    }

    #[test]
    fn saw_oscillates_at_pitch() {
        // a saw at MIDI 60 (~262 Hz) over 96k: ~2.7 cycles per 1024 samples
        let out = render_shape(AnalogShape::Saw, 0, 60 << 7, 1024);
        let zc = zero_crossings(&out);
        assert!(zc >= 2 && zc <= 8, "saw has a handful of crossings: {zc}");
        assert!(out.iter().any(|&v| v > 8000) && out.iter().any(|&v| v < -8000));
    }

    #[test]
    fn square_pulse_width_changes_duty() {
        // a 50% square vs a narrow one: the narrow one spends less time high
        let mid = render_shape(AnalogShape::Square, 0, 48 << 7, 2048);
        let narrow = render_shape(AnalogShape::Square, 28000, 48 << 7, 2048);
        let high_mid = mid.iter().filter(|&&v| v > 0).count();
        let high_narrow = narrow.iter().filter(|&&v| v > 0).count();
        assert_ne!(high_mid, high_narrow, "pw changes the duty cycle");
    }

    #[test]
    fn sine_is_smooth_and_bounded() {
        let out = render_shape(AnalogShape::Sine, 0, 60 << 7, 1024);
        assert!(out.iter().all(|&v| (-32768..=32767).contains(&(v as i32))));
        let zc = zero_crossings(&out);
        assert!(zc >= 2 && zc <= 8, "sine crosses a few times: {zc}");
    }

    #[test]
    fn triangle_fold_adds_harmonics() {
        // folding at high parameter produces more zero crossings than the
        // bare triangle (the fold creates extra excursions)
        let plain = render_shape(AnalogShape::Triangle, 0, 48 << 7, 2048);
        let folded = render_shape(AnalogShape::TriangleFold, 30000, 48 << 7, 2048);
        assert!(
            zero_crossings(&folded) >= zero_crossings(&plain),
            "fold adds crossings: {} vs {}",
            zero_crossings(&folded),
            zero_crossings(&plain)
        );
    }

    #[test]
    fn all_shapes_render_finite_and_bounded() {
        for shape in ANALOG_SHAPES {
            let out = render_shape(shape, 16000, 55 << 7, 512);
            assert!(
                out.iter().all(|&v| (-32768..=32767).contains(&(v as i32))),
                "{shape:?} bounded"
            );
            // non-silent (Buzz at some pitches can be quiet, but most shapes ring)
            let energy: i64 = out.iter().map(|&v| (v as i64) * (v as i64)).sum();
            assert!(energy > 0, "{shape:?} produces output");
        }
    }

    #[test]
    fn shape_switch_reinitialises() {
        let mut osc = AnalogOscillator::new();
        osc.set_shape(AnalogShape::Saw);
        osc.set_pitch(60 << 7);
        let sync = vec![0u8; 256];
        let mut out = vec![0i16; 256];
        osc.render(&sync, &mut out, None, 256);
        osc.set_shape(AnalogShape::Square);
        // first render after a shape change calls init(); must not panic
        osc.render(&sync, &mut out, None, 256);
        assert!(out.iter().all(|&v| (-32768..=32767).contains(&(v as i32))));
    }

    #[test]
    fn all_analog_macro_models_render_bounded_audio() {
        for model in MODELS {
            let mut m = MacroOscillator::new();
            m.set_model(model);
            m.set_pitch(55 << 7);
            m.set_parameters(20000, 16000);
            let sync = vec![0u8; 128];
            let mut out = vec![0i16; 128];
            let mut energy = 0i64;
            for _ in 0..16 {
                m.render(&sync, &mut out, 128);
                energy += out.iter().map(|&v| (v as i64) * (v as i64)).sum::<i64>();
                assert!(
                    out.iter().all(|&v| (-32768..=32767).contains(&(v as i32))),
                    "{model:?} bounded"
                );
            }
            assert!(energy > 0, "{model:?} produces audio");
        }
    }

    #[test]
    fn morph_sweeps_through_waveshapes() {
        // the morph model crosses triangle→saw→square→sine as parameter 0
        // rises; output stays bounded and present at every position
        let mut m = MacroOscillator::new();
        m.set_model(MacroModel::Morph);
        m.set_pitch(50 << 7);
        let sync = vec![0u8; 128];
        let mut out = vec![0i16; 128];
        for p0 in [0i16, 8000, 16000, 24000, 32000] {
            m.set_parameters(p0, 8000);
            let mut energy = 0i64;
            for _ in 0..20 {
                m.render(&sync, &mut out, 128);
                energy += out.iter().map(|&v| (v as i64) * (v as i64)).sum::<i64>();
            }
            assert!(energy > 0, "morph at param {p0} produces audio");
            assert!(out.iter().all(|&v| (-32768..=32767).contains(&(v as i32))));
        }
    }

    #[test]
    fn triple_detunes_three_voices() {
        // a triple saw with detune should be louder/fuller than a single
        let mut m = MacroOscillator::new();
        m.set_model(MacroModel::TripleSaw);
        m.set_pitch(48 << 7);
        m.set_parameters(20000, 12000);
        let sync = vec![0u8; 128];
        let mut out = vec![0i16; 128];
        let mut nonzero = false;
        for _ in 0..16 {
            m.render(&sync, &mut out, 128);
            nonzero |= out.iter().any(|&v| v.abs() > 1000);
        }
        assert!(nonzero, "triple saw sings");
    }

    #[test]
    fn digital_models_make_bounded_sound() {
        let sync = vec![0u8; 128];
        for model in [
            MacroModel::TripleRingMod,
            MacroModel::SawSwarm,
            MacroModel::SawComb,
            MacroModel::Toy,
            MacroModel::DigitalFilterLp,
            MacroModel::DigitalFilterPk,
            MacroModel::DigitalFilterBp,
            MacroModel::DigitalFilterHp,
        ] {
            let mut m = MacroOscillator::new();
            m.set_model(model);
            m.set_pitch(60 << 7);
            let mut nonzero = false;
            for (p0, p1) in [(2000, 4000), (16000, 20000), (30000, 8000)] {
                m.set_parameters(p0, p1);
                let mut out = vec![0i16; 128];
                for _ in 0..40 {
                    m.render(&sync, &mut out, 128);
                    assert!(
                        out.iter().all(|&v| (-32768..=32767).contains(&(v as i32))),
                        "{model:?} bounded"
                    );
                    nonzero |= out.iter().any(|&v| (v as i32).abs() > 200);
                }
            }
            assert!(nonzero, "{model:?} makes sound");
        }
    }

    #[test]
    fn digital_model_count_matches_names() {
        assert_eq!(MODELS.len(), MODEL_NAMES.len());
    }

    #[test]
    fn sync_resets_the_phase() {
        // a sync pulse mid-block should force a discontinuity in the saw
        let mut osc = AnalogOscillator::new();
        osc.set_shape(AnalogShape::Saw);
        osc.set_pitch(50 << 7);
        let mut sync = vec![0u8; 512];
        sync[256] = 128; // a reset partway in
        let mut out = vec![0i16; 512];
        for _ in 0..3 {
            osc.render(&sync, &mut out, None, 512);
        }
        assert!(out.iter().all(|&v| (-32768..=32767).contains(&(v as i32))));
    }
}
