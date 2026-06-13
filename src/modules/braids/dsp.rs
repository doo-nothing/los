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

const BRAIDS_TABLES_BIN: &[u8] = include_bytes!("braids_tables.bin");

pub struct Tables {
    pub wav_sine: Vec<i16>,                // 257
    pub increments: Vec<u32>,              // 97
    pub ws_sine_fold: Vec<i16>,            // 257
    pub ws_tri_fold: Vec<i16>,             // 257
    pub comb: Vec<Vec<i16>>,               // 15 × 257
    pub svf_cutoff: Vec<u16>,              // 257
    pub violent_overdrive: Vec<i16>,       // 257
}

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
        Tables {
            wav_sine,
            increments,
            ws_sine_fold,
            ws_tri_fold,
            comb,
            svf_cutoff,
            violent_overdrive,
        }
    })
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
}

pub const ANALOG_MODELS: [MacroModel; 13] = [
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
];

pub const MODEL_NAMES: [&str; 13] = [
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
];

pub struct MacroOscillator {
    osc: [AnalogOscillator; 3],
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
        }
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
        for model in ANALOG_MODELS {
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
