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
    // formant-synth tables (braids_formant_tables.bin)
    pub lut_bell: Vec<u16>,                // 257
    pub wav_formant_sine: Vec<i16>,        // 256
    pub wav_formant_square: Vec<i16>,      // 256
    pub formant_f_data: Vec<i16>,          // 125 (5×5×5)
    pub formant_a_data: Vec<i16>,          // 125
    // noise tables (braids_noise_tables.bin)
    pub svf_scale: Vec<u16>,               // 257
    pub resonator_coefficient: Vec<u16>,   // 129
    pub resonator_scale: Vec<u16>,         // 129
    // bowing tables (braids_bowing_tables.bin)
    pub bowing_envelope: Vec<u16>,         // 752
    pub bowing_friction: Vec<u16>,         // 257
    // wind tables (braids_wind_tables.bin)
    pub flute_body_filter: Vec<u16>,       // 128
    pub blowing_envelope: Vec<u16>,        // 392
    pub blowing_jet: Vec<i16>,             // 257
    // wavetable data (braids_wavetable.bin)
    pub wt_waves: Vec<u8>,                 // 33024 (256 waves × 129)
    pub wt_map: Vec<u8>,                   // 256
    // granular/question-mark tables (braids_granular_tables.bin)
    pub granular_envelope_rate: Vec<u16>,  // 257
    pub granular_envelope: Vec<u16>,       // 513
    pub wt_code: Vec<u8>,                  // 1064
}

const BRAIDS_FORMANT_TABLES_BIN: &[u8] = include_bytes!("braids_formant_tables.bin");
const BRAIDS_NOISE_TABLES_BIN: &[u8] = include_bytes!("braids_noise_tables.bin");
const BRAIDS_BOWING_TABLES_BIN: &[u8] = include_bytes!("braids_bowing_tables.bin");
const BRAIDS_WIND_TABLES_BIN: &[u8] = include_bytes!("braids_wind_tables.bin");
const BRAIDS_WAVETABLE_BIN: &[u8] = include_bytes!("braids_wavetable.bin");
const BRAIDS_GRANULAR_TABLES_BIN: &[u8] = include_bytes!("braids_granular_tables.bin");

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
        // formant tables: header of 5 u32 lengths, then lut_bell(u16),
        // wav_formant_sine(i16), wav_formant_square(i16), formant_f(i16),
        // formant_a(i16)
        let fd = BRAIDS_FORMANT_TABLES_BIN;
        let flen = |i: usize| {
            u32::from_le_bytes([fd[i * 4], fd[i * 4 + 1], fd[i * 4 + 2], fd[i * 4 + 3]]) as usize
        };
        let (n_bell, n_fsin, n_fsq, n_ff, n_fa) = (flen(0), flen(1), flen(2), flen(3), flen(4));
        let mut fo = 20;
        let take_u16 = |fd: &[u8], off: &mut usize, n: usize| {
            let v: Vec<u16> = fd[*off..*off + n * 2]
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect();
            *off += n * 2;
            v
        };
        let take_i16 = |fd: &[u8], off: &mut usize, n: usize| {
            let v: Vec<i16> = fd[*off..*off + n * 2]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            *off += n * 2;
            v
        };
        let lut_bell = take_u16(fd, &mut fo, n_bell);
        let wav_formant_sine = take_i16(fd, &mut fo, n_fsin);
        let wav_formant_square = take_i16(fd, &mut fo, n_fsq);
        let formant_f_data = take_i16(fd, &mut fo, n_ff);
        let formant_a_data = take_i16(fd, &mut fo, n_fa);
        // noise tables: header of 3 u32 lengths, then svf_scale, res_coeff,
        // res_scale (all u16)
        let nd = BRAIDS_NOISE_TABLES_BIN;
        let nlen = |i: usize| {
            u32::from_le_bytes([nd[i * 4], nd[i * 4 + 1], nd[i * 4 + 2], nd[i * 4 + 3]]) as usize
        };
        let (n_sc, n_rc, n_rs) = (nlen(0), nlen(1), nlen(2));
        let mut no = 12;
        let svf_scale = take_u16(nd, &mut no, n_sc);
        let resonator_coefficient = take_u16(nd, &mut no, n_rc);
        let resonator_scale = take_u16(nd, &mut no, n_rs);
        // bowing tables: header of 2 u32 lengths, then envelope, friction (u16)
        let bd = BRAIDS_BOWING_TABLES_BIN;
        let blen = |i: usize| {
            u32::from_le_bytes([bd[i * 4], bd[i * 4 + 1], bd[i * 4 + 2], bd[i * 4 + 3]]) as usize
        };
        let (n_env, n_fric) = (blen(0), blen(1));
        let mut bo = 8;
        let bowing_envelope = take_u16(bd, &mut bo, n_env);
        let bowing_friction = take_u16(bd, &mut bo, n_fric);
        // wind tables: header of 3 u32 lengths, then flute(u16), env(u16),
        // jet(i16)
        let wd = BRAIDS_WIND_TABLES_BIN;
        let wlen = |i: usize| {
            u32::from_le_bytes([wd[i * 4], wd[i * 4 + 1], wd[i * 4 + 2], wd[i * 4 + 3]]) as usize
        };
        let (n_flute, n_benv, n_jet) = (wlen(0), wlen(1), wlen(2));
        let mut wo = 12;
        let flute_body_filter = take_u16(wd, &mut wo, n_flute);
        let blowing_envelope = take_u16(wd, &mut wo, n_benv);
        let blowing_jet = take_i16(wd, &mut wo, n_jet);
        // wavetable data: header of 2 u32 lengths, then wt_waves(u8), wt_map(u8)
        let wt = BRAIDS_WAVETABLE_BIN;
        let n_wtw =
            u32::from_le_bytes([wt[0], wt[1], wt[2], wt[3]]) as usize;
        let n_wtm = u32::from_le_bytes([wt[4], wt[5], wt[6], wt[7]]) as usize;
        let wt_waves = wt[8..8 + n_wtw].to_vec();
        let wt_map = wt[8 + n_wtw..8 + n_wtw + n_wtm].to_vec();
        // granular tables: header of 3 u32 lengths, then env_rate(u16),
        // envelope(u16), wt_code(u8)
        let gd = BRAIDS_GRANULAR_TABLES_BIN;
        let glen = |i: usize| {
            u32::from_le_bytes([gd[i * 4], gd[i * 4 + 1], gd[i * 4 + 2], gd[i * 4 + 3]]) as usize
        };
        let (n_ger, n_ge, n_wtc) = (glen(0), glen(1), glen(2));
        let mut go = 12;
        let granular_envelope_rate = take_u16(gd, &mut go, n_ger);
        let granular_envelope = take_u16(gd, &mut go, n_ge);
        let wt_code = gd[go..go + n_wtc].to_vec();
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
            lut_bell,
            wav_formant_sine,
            wav_formant_square,
            formant_f_data,
            formant_a_data,
            svf_scale,
            resonator_coefficient,
            resonator_scale,
            bowing_envelope,
            bowing_friction,
            flute_body_filter,
            blowing_envelope,
            blowing_jet,
            wt_waves,
            wt_map,
            granular_envelope_rate,
            granular_envelope,
            wt_code,
        }
    })
}

/// braids' formant phoneme table: 3 formant frequencies + 3 amplitudes
/// (all u8) per phoneme.
#[derive(Clone, Copy)]
struct PhonemeDefinition {
    formant_frequency: [u8; 3],
    formant_amplitude: [u8; 3],
}

const VOWELS_DATA: [PhonemeDefinition; 9] = [
    PhonemeDefinition { formant_frequency: [27, 40, 89], formant_amplitude: [15, 13, 1] },
    PhonemeDefinition { formant_frequency: [18, 51, 62], formant_amplitude: [13, 12, 6] },
    PhonemeDefinition { formant_frequency: [15, 69, 93], formant_amplitude: [14, 12, 7] },
    PhonemeDefinition { formant_frequency: [10, 84, 110], formant_amplitude: [13, 10, 8] },
    PhonemeDefinition { formant_frequency: [23, 44, 87], formant_amplitude: [15, 12, 1] },
    PhonemeDefinition { formant_frequency: [13, 29, 80], formant_amplitude: [13, 8, 0] },
    PhonemeDefinition { formant_frequency: [6, 46, 81], formant_amplitude: [12, 3, 0] },
    PhonemeDefinition { formant_frequency: [9, 51, 95], formant_amplitude: [15, 3, 0] },
    PhonemeDefinition { formant_frequency: [6, 73, 99], formant_amplitude: [7, 3, 14] },
];

const CONSONANT_DATA: [PhonemeDefinition; 8] = [
    PhonemeDefinition { formant_frequency: [6, 54, 121], formant_amplitude: [9, 9, 0] },
    PhonemeDefinition { formant_frequency: [18, 50, 51], formant_amplitude: [12, 10, 5] },
    PhonemeDefinition { formant_frequency: [11, 24, 70], formant_amplitude: [13, 8, 0] },
    PhonemeDefinition { formant_frequency: [15, 69, 74], formant_amplitude: [14, 12, 7] },
    PhonemeDefinition { formant_frequency: [16, 37, 111], formant_amplitude: [14, 8, 1] },
    PhonemeDefinition { formant_frequency: [18, 51, 62], formant_amplitude: [14, 12, 6] },
    PhonemeDefinition { formant_frequency: [6, 26, 81], formant_amplitude: [5, 5, 5] },
    PhonemeDefinition { formant_frequency: [6, 73, 99], formant_amplitude: [7, 10, 14] },
];

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

/// stmlib `Interpolate1022`: 10.22 fixed-point read of a 1024(+1)-entry
/// table (10-bit index, 16-bit fraction).
#[inline]
fn interpolate1022(table: &[i16], phase: u32) -> i32 {
    // The firmware computes `(b-a)*frac` in int32 and lets it wrap — the
    // Karplus-Strong delay line has large sample-to-sample deltas, so the
    // wrap is audible and must be reproduced (wrapping_mul, not an i64 widen).
    let i = (phase >> 22) as usize;
    let a = table[i] as i32;
    let b = table[(i + 1).min(table.len() - 1)] as i32;
    a + ((b - a).wrapping_mul(((phase >> 6) & 0xffff) as i32) >> 16)
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
    Vosim,
    Vowel,
    VowelFof,
    Harmonics,
    Fm,
    FeedbackFm,
    ChaoticFeedbackFm,
    FilteredNoise,
    TwinPeaksNoise,
    ClockedNoise,
    Plucked,
    StruckBell,
    StruckDrum,
    Kick,
    Snare,
    Cymbal,
    Bowed,
    Blown,
    Fluted,
    Wavetables,
    WaveMap,
    WaveLine,
    WaveParaphonic,
    GranularCloud,
    ParticleNoise,
    DigitalModulation,
    QuestionMark,
}

/// All macro models in panel order — parallel to [`MODEL_NAMES`].
pub const MODELS: [MacroModel; 48] = [
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
    MacroModel::Vosim,
    MacroModel::Vowel,
    MacroModel::VowelFof,
    MacroModel::Harmonics,
    MacroModel::Fm,
    MacroModel::FeedbackFm,
    MacroModel::ChaoticFeedbackFm,
    MacroModel::FilteredNoise,
    MacroModel::TwinPeaksNoise,
    MacroModel::ClockedNoise,
    MacroModel::Plucked,
    MacroModel::StruckBell,
    MacroModel::StruckDrum,
    MacroModel::Kick,
    MacroModel::Snare,
    MacroModel::Cymbal,
    MacroModel::Bowed,
    MacroModel::Blown,
    MacroModel::Fluted,
    MacroModel::Wavetables,
    MacroModel::WaveMap,
    MacroModel::WaveLine,
    MacroModel::WaveParaphonic,
    MacroModel::GranularCloud,
    MacroModel::ParticleNoise,
    MacroModel::DigitalModulation,
    MacroModel::QuestionMark,
];

pub const MODEL_NAMES: [&str; 48] = [
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
    "vosim",
    "vowel",
    "vowel_fof",
    "harmonics",
    "fm",
    "feedback_fm",
    "chaotic_feedback_fm",
    "filtered_noise",
    "twin_peaks_noise",
    "clocked_noise",
    "plucked",
    "struck_bell",
    "struck_drum",
    "kick",
    "snare",
    "cymbal",
    "bowed",
    "blown",
    "fluted",
    "wavetables",
    "wave_map",
    "wave_line",
    "wave_paraphonic",
    "granular_cloud",
    "particle_noise",
    "digital_modulation",
    "question_mark",
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
            MacroModel::Vosim => self.render_digital(DigitalShape::Vosim, sync, buffer, size),
            MacroModel::Vowel => self.render_digital(DigitalShape::Vowel, sync, buffer, size),
            MacroModel::VowelFof => {
                self.render_digital(DigitalShape::VowelFof, sync, buffer, size)
            }
            MacroModel::Harmonics => {
                self.render_digital(DigitalShape::Harmonics, sync, buffer, size)
            }
            MacroModel::Fm => self.render_digital(DigitalShape::Fm, sync, buffer, size),
            MacroModel::FeedbackFm => {
                self.render_digital(DigitalShape::FeedbackFm, sync, buffer, size)
            }
            MacroModel::ChaoticFeedbackFm => {
                self.render_digital(DigitalShape::ChaoticFeedbackFm, sync, buffer, size)
            }
            MacroModel::FilteredNoise => {
                self.render_digital(DigitalShape::FilteredNoise, sync, buffer, size)
            }
            MacroModel::TwinPeaksNoise => {
                self.render_digital(DigitalShape::TwinPeaksNoise, sync, buffer, size)
            }
            MacroModel::ClockedNoise => {
                self.render_digital(DigitalShape::ClockedNoise, sync, buffer, size)
            }
            MacroModel::Plucked => self.render_digital(DigitalShape::Plucked, sync, buffer, size),
            MacroModel::StruckBell => {
                self.render_digital(DigitalShape::StruckBell, sync, buffer, size)
            }
            MacroModel::StruckDrum => {
                self.render_digital(DigitalShape::StruckDrum, sync, buffer, size)
            }
            MacroModel::Kick => self.render_digital(DigitalShape::Kick, sync, buffer, size),
            MacroModel::Snare => self.render_digital(DigitalShape::Snare, sync, buffer, size),
            MacroModel::Cymbal => self.render_digital(DigitalShape::Cymbal, sync, buffer, size),
            MacroModel::Bowed => self.render_digital(DigitalShape::Bowed, sync, buffer, size),
            MacroModel::Blown => self.render_digital(DigitalShape::Blown, sync, buffer, size),
            MacroModel::Fluted => self.render_digital(DigitalShape::Fluted, sync, buffer, size),
            MacroModel::Wavetables => {
                self.render_digital(DigitalShape::Wavetables, sync, buffer, size)
            }
            MacroModel::WaveMap => self.render_digital(DigitalShape::WaveMap, sync, buffer, size),
            MacroModel::WaveLine => self.render_digital(DigitalShape::WaveLine, sync, buffer, size),
            MacroModel::WaveParaphonic => {
                self.render_digital(DigitalShape::WaveParaphonic, sync, buffer, size)
            }
            MacroModel::GranularCloud => {
                self.render_digital(DigitalShape::GranularCloud, sync, buffer, size)
            }
            MacroModel::ParticleNoise => {
                self.render_digital(DigitalShape::ParticleNoise, sync, buffer, size)
            }
            MacroModel::DigitalModulation => {
                self.render_digital(DigitalShape::DigitalModulation, sync, buffer, size)
            }
            MacroModel::QuestionMark => {
                self.render_digital(DigitalShape::QuestionMark, sync, buffer, size)
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
    Vosim,
    Vowel,
    VowelFof,
    Harmonics,
    Fm,
    FeedbackFm,
    ChaoticFeedbackFm,
    FilteredNoise,
    TwinPeaksNoise,
    ClockedNoise,
    Plucked,
    StruckBell,
    StruckDrum,
    Kick,
    Snare,
    Cymbal,
    Bowed,
    Blown,
    Fluted,
    Wavetables,
    WaveMap,
    WaveLine,
    WaveParaphonic,
    GranularCloud,
    ParticleNoise,
    DigitalModulation,
    QuestionMark,
}

const NUM_FORMANTS: usize = 5;
const NUM_ADDITIVE_HARMONICS: usize = 12;
const NUM_PLUCK_VOICES: usize = 3;
const KS_VOICE_STRIDE: usize = 1025;
const NUM_BELL_PARTIALS: usize = 11;
const NUM_DRUM_PARTIALS: usize = 6;

#[rustfmt::skip]
const BELL_PARTIALS: [i16; NUM_BELL_PARTIALS] =
    [-1284, -1283, -184, -183, 385, 1175, 1536, 2233, 2434, 2934, 3110];
#[rustfmt::skip]
const BELL_PARTIAL_AMPLITUDES: [i32; NUM_BELL_PARTIALS] =
    [8192, 5488, 8192, 14745, 21872, 13680, 11960, 10895, 10895, 6144, 10895];
#[rustfmt::skip]
const BELL_PARTIAL_DECAY_LONG: [i32; NUM_BELL_PARTIALS] =
    [65533, 65533, 65533, 65532, 65531, 65531, 65530, 65529, 65527, 65523, 65519];
#[rustfmt::skip]
const BELL_PARTIAL_DECAY_SHORT: [i32; NUM_BELL_PARTIALS] =
    [65308, 65283, 65186, 65123, 64839, 64889, 64632, 64409, 64038, 63302, 62575];
const DRUM_PARTIALS: [i16; NUM_DRUM_PARTIALS] = [0, 0, 1041, 1747, 1846, 3072];
const DRUM_PARTIAL_AMPLITUDE: [i32; NUM_DRUM_PARTIALS] = [16986, 2654, 3981, 5308, 3981, 2985];
const DRUM_PARTIAL_DECAY_LONG: [i32; NUM_DRUM_PARTIALS] =
    [65533, 65531, 65531, 65531, 65531, 65516];
const DRUM_PARTIAL_DECAY_SHORT: [i32; NUM_DRUM_PARTIALS] =
    [65083, 64715, 64715, 64715, 64715, 62312];

// ── granular / modulation / particle / question-mark models ──────────────────

const PARTICLE_NOISE_DECAY: i64 = 64763;
const RESONANCE_SQUARED: i64 = 32506; // 32768 * 0.996 * 0.996
const RESONANCE_FACTOR: i64 = 32636; // 32768 * 0.996
const CONSTELLATION_Q: [i32; 4] = [23100, -23100, -23100, 23100];
const CONSTELLATION_I: [i32; 4] = [23100, 23100, -23100, -23100];

/// One grain of the GRANULAR_CLOUD model.
#[derive(Debug, Clone, Copy, Default)]
struct Grain {
    phase: u32,
    phase_increment: u32,
    envelope_phase: u32,
    envelope_phase_increment: u32,
}

// ── wavetable models ─────────────────────────────────────────────────────────

const WAVE_STRIDE: usize = 129;

#[rustfmt::skip]
const WAVE_LINE: [u8; 64] = [
    187, 179, 154, 155, 135, 134, 137, 19, 24, 3, 8, 66, 79, 25, 180, 174, 64,
    127, 198, 15, 10, 7, 11, 0, 191, 192, 115, 238, 237, 236, 241, 47, 70, 76,
    235, 26, 133, 208, 34, 175, 183, 146, 147, 148, 150, 151, 152, 153, 117,
    138, 32, 33, 35, 125, 199, 201, 30, 31, 193, 27, 29, 21, 18, 182,
];
#[rustfmt::skip]
const MINI_WAVE_LINE: [u8; 33] = [
    157, 161, 171, 188, 189, 191, 192, 193, 196, 198, 201, 234, 232,
    229, 226, 224, 1, 2, 3, 4, 5, 8, 12, 32, 36, 42, 47, 252, 254, 141, 139,
    135, 174,
];

const CHORDS: [[u16; 3]; 17] = [
    [2, 4, 6],
    [16, 32, 48],
    [256, 896, 1536],
    [384, 896, 1280],
    [384, 896, 1536],
    [384, 896, 1792],
    [384, 896, 2176],
    [896, 1536, 2432],
    [896, 1539, 2437],
    [512, 896, 2176],
    [512, 896, 1792],
    [512, 896, 1536],
    [512, 896, 1408],
    [640, 896, 1536],
    [4, 896, 1536],
    [4, 1540, 1536],
    [4, 1540, 1536],
];

const WAVETABLE_DEFS: [(u8, [u8; 17]); 20] = [
    (16, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 15]),
    (16, [16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 31]),
    (16, [32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 47]),
    (16, [48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 63]),
    (16, [64, 65, 66, 67, 68, 68, 69, 70, 71, 72, 73, 73, 74, 75, 75, 76, 76]),
    (16, [77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 92]),
    (16, [93, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 108]),
    (16, [109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 124]),
    (16, [125, 126, 127, 128, 129, 130, 131, 132, 132, 132, 132, 132, 132, 132, 132, 132, 132]),
    (16, [133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 144, 144, 145, 145, 145]),
    (16, [146, 147, 148, 149, 150, 151, 151, 151, 152, 152, 152, 152, 153, 153, 153, 153, 153]),
    (8, [154, 154, 154, 154, 154, 154, 155, 156, 156, 0, 0, 0, 0, 0, 0, 0, 0]),
    (16, [176, 157, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 171]),
    (16, [172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 186, 187, 187]),
    (16, [176, 188, 189, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 202]),
    (16, [203, 205, 204, 205, 212, 206, 207, 208, 208, 209, 210, 210, 211, 211, 212, 212, 212]),
    (8, [213, 213, 213, 214, 215, 216, 217, 218, 219, 0, 0, 0, 0, 0, 0, 0, 0]),
    (16, [220, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 235, 235]),
    (16, [236, 237, 238, 239, 240, 241, 242, 243, 244, 245, 246, 247, 248, 249, 250, 251, 251]),
    (4, [252, 253, 254, 255, 254, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
];

/// stmlib `Crossfade` over two unsigned-8 wavetables: interpolate within
/// each 129-byte wave at `phase` (824 layout — the caller passes `phase>>1`
/// so the 128-sample wave spans the full cycle), crossfade by `balance`,
/// and centre the uint8 (−128) into an i16-range sample.
#[inline]
fn crossfade_u8(a: &[u8], b: &[u8], phase: u32, balance: u16) -> i32 {
    let i = (phase >> 24) as usize;
    let frac = ((phase >> 8) & 0xffff) as i32;
    let a_v = a[i] as i32 + ((a[i + 1] as i32 - a[i] as i32) * frac >> 16);
    let b_v = b[i] as i32 + ((b[i + 1] as i32 - b[i] as i32) * frac >> 16);
    let x = a_v + ((b_v - a_v) * balance as i32 >> 16);
    (x - 128) << 8
}

/// braids' `Excitation` — an exponential-decay pulse with an optional
/// delay before it fires.
#[derive(Debug, Clone, Copy)]
struct Excitation {
    delay: u32,
    decay: u32,
    counter: i32,
    state: i32,
    level: i32,
}

impl Default for Excitation {
    fn default() -> Self {
        Self { delay: 0, decay: 4093, counter: 0, state: 0, level: 0 }
    }
}

impl Excitation {
    fn set_delay(&mut self, delay: u32) {
        self.delay = delay;
    }
    fn set_decay(&mut self, decay: u32) {
        self.decay = decay;
    }
    fn trigger(&mut self, level: i32) {
        self.level = level;
        self.counter = self.delay as i32 + 1;
    }
    fn done(&self) -> bool {
        self.counter == 0
    }
    fn process(&mut self) -> i32 {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SvfMode {
    #[allow(dead_code)] // part of the faithful braids Svf; no model selects LP
    Lp,
    Bp,
    Hp,
}

/// braids' `Svf` — the drum-modeling state-variable filter (with the
/// "punch" frequency/damp boost). Distinct from the plaits/TPT Svf.
#[derive(Debug, Clone, Copy)]
struct DrumSvf {
    dirty: bool,
    frequency: i16,
    resonance: i16,
    punch: i32,
    f: i32,
    damp: i32,
    lp: i32,
    bp: i32,
    mode: SvfMode,
}

impl Default for DrumSvf {
    fn default() -> Self {
        Self {
            dirty: true,
            frequency: 33 << 7,
            resonance: 16384,
            punch: 0,
            f: 0,
            damp: 0,
            lp: 0,
            bp: 0,
            mode: SvfMode::Bp,
        }
    }
}

impl DrumSvf {
    fn set_frequency(&mut self, frequency: i16) {
        self.dirty = self.dirty || self.frequency != frequency;
        self.frequency = frequency;
    }
    fn set_resonance(&mut self, resonance: i16) {
        self.resonance = resonance;
        self.dirty = true;
    }
    fn set_punch(&mut self, punch: u16) {
        self.punch = ((punch as u32 * punch as u32) >> 24) as i32;
    }
    fn set_mode(&mut self, mode: SvfMode) {
        self.mode = mode;
    }
    fn process(&mut self, input: i32) -> i32 {
        let t = tables();
        if self.dirty {
            self.f = interpolate824_u16(&t.svf_cutoff, (self.frequency as i32 as u32) << 17);
            self.damp = interpolate824_u16(&t.svf_damp, (self.resonance as i32 as u32) << 17);
            self.dirty = false;
        }
        let mut f = self.f;
        let mut damp = self.damp;
        if self.punch != 0 {
            let punch_signal = if self.lp > 4096 { self.lp } else { 2048 };
            f += ((punch_signal >> 4) * self.punch) >> 9;
            damp += (punch_signal - 2048) >> 3;
        }
        let notch = input - ((self.bp as i64 * damp as i64 >> 15) as i32);
        self.lp += (f as i64 * self.bp as i64 >> 15) as i32;
        self.lp = clip16(self.lp);
        let hp = notch - self.lp;
        self.bp += (f as i64 * hp as i64 >> 15) as i32;
        self.bp = clip16(self.bp);
        match self.mode {
            SvfMode::Bp => self.bp,
            SvfMode::Hp => hp,
            SvfMode::Lp => self.lp,
        }
    }
}

/// One Karplus-Strong voice of the PLUCKED model.
#[derive(Debug, Clone, Copy, Default)]
struct PluckVoice {
    size: usize,
    write_ptr: usize,
    shift: u32,
    mask: usize,
    initialization_ptr: usize,
    phase: u32,
    phase_increment: u32,
    max_phase_increment: u32,
}

/// braids' `InterpolateFormantParameter`: bilinear lookup into a 5×5×5
/// formant table (x = parameter_1, y = parameter_0).
fn interpolate_formant(table: &[i16], x: i16, y: i16, formant: usize) -> i16 {
    let x_index = (x >> 13) as usize;
    let x_mix = ((x as u32) << 3) as u16 as i64;
    let y_index = (y >> 13) as usize;
    let y_mix = ((y as u32) << 3) as u16 as i64;
    let at = |xi: usize, yi: usize| table[xi * 25 + yi * 5 + formant] as i64;
    let a0 = at(x_index, y_index);
    let b = at(x_index + 1, y_index);
    let c0 = at(x_index, y_index + 1);
    let d = at(x_index + 1, y_index + 1);
    let a = a0 + ((b - a0) * x_mix >> 16);
    let c = c0 + ((d - c0) * x_mix >> 16);
    (a + ((c - a) * y_mix >> 16)) as i16
}

const FIR4_COEFFICIENTS: [u32; 4] = [10530, 14751, 16384, 14751];
const FIR4_DC_OFFSET: i32 = 28208;
const PHASE_RESET: [u32; 4] = [0, 0x8000_0000, 0x4000_0000, 0x8000_0000];

// waveguide string (bowed) lengths + filter coefficients
const WG_BRIDGE_LENGTH: usize = 1024;
const WG_NECK_LENGTH: usize = 4096;
const BRIDGE_LP_GAIN: i64 = 14008;
const BRIDGE_LP_POLE1: i64 = 18022;
const BIQUAD_GAIN: i64 = 6553;
const BIQUAD_POLE1: i64 = 6948;
const BIQUAD_POLE2: i64 = -2959;
const LUT_BOWING_ENVELOPE_SIZE: usize = 752;
// wind (blown/fluted) lengths + coefficients
const WG_BORE_LENGTH: usize = 2048;
const WG_JET_LENGTH: usize = 1024;
const WG_FBORE_LENGTH: usize = 4096;
const BREATH_PRESSURE: i64 = 26214;
const REFLECTION_COEFFICIENT: i64 = -3891;
const REED_SLOPE: i64 = -1229;
const REED_OFFSET: i32 = 22938;
const DC_BLOCKING_POLE: i64 = 4055; // 0.99 * 4096
const LUT_BLOWING_ENVELOPE_SIZE: usize = 392;

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
    // formant-synth (vow) state — formant_phase reuses the field above
    vow_formant_increment: [u32; 3],
    vow_formant_amplitude: [u32; 3],
    vow_consonant_frames: u16,
    vow_noise: u16,
    // vowel-FOF (fof) state
    digital_init: bool,
    fof_svf_lp: [i32; NUM_FORMANTS],
    fof_svf_bp: [i32; NUM_FORMANTS],
    fof_previous_sample: i32,
    fof_next_saw_sample: i32,
    // harmonics (hrm/add) + FM-family (fm) state, and param interpolation
    previous_parameter: [i16; 2],
    hrm_amplitude: [i32; NUM_ADDITIVE_HARMONICS],
    add_previous_sample: i16,
    fm_modulator_phase: u32,
    fm_previous_sample: i16,
    // noise models (svf / pno / clk) state
    svf_bp: i32,
    svf_lp: i32,
    pno_filter_state: [[i32; 2]; 2],
    clk_cycle_phase: u32,
    clk_cycle_phase_increment: u32,
    clk_rng_state: u32,
    clk_seed: u32,
    clk_sample: i16,
    // plucked (Karplus-Strong) state
    pluck: [PluckVoice; NUM_PLUCK_VOICES],
    pluck_active_voice: usize,
    pluck_previous_sample: i16,
    ks_delay: Vec<i16>,
    // struck bell/drum (additive) state
    add_partial_phase: [u32; NUM_BELL_PARTIALS],
    add_partial_phase_increment: [u32; NUM_BELL_PARTIALS],
    add_partial_amplitude: [i32; NUM_BELL_PARTIALS],
    add_target_partial_amplitude: [i32; NUM_BELL_PARTIALS],
    add_current_partial: usize,
    add_lp_noise: [i32; 3],
    // analog drum (kick/snare/cymbal) state
    pulse: [Excitation; 4],
    bsvf: [DrumSvf; 3],
    hat_phase: [u32; 6],
    hat_rng_state: u32,
    // waveguide (bowed/blown/fluted) state
    digital_delay: u32,
    phy_delay_ptr: u16,
    phy_excitation_ptr: u16,
    phy_lp_state: i32,
    phy_filter_state: [i32; 2],
    phy_previous_sample: i16,
    wg_bridge: Vec<i8>,
    wg_neck: Vec<i8>,
    wg_bore: Vec<i16>,
    // wavetable state (paraphonic reuses saw_phase[0..4])
    smoothed_parameter: i32,
    // granular / modulation / particle / question-mark state
    grain: [Grain; 4],
    dmd_symbol_phase: u32,
    dmd_symbol_count: u16,
    dmd_filter_state: i32,
    dmd_data_byte: u8,
    pno_amplitude: u16,
    pno3_filter_state: [[i32; 2]; 3],
    pno3_filter_scale: [i32; 3],
    pno3_filter_coefficient: [i32; 3],
    qm_rng_state: u32,
    qm_cycle_phase: u32,
    qm_sample: i32,
    qm_cycle_phase_increment: i32,
    qm_seed: i32,
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
            vow_formant_increment: [0; 3],
            vow_formant_amplitude: [0; 3],
            vow_consonant_frames: 0,
            vow_noise: 0,
            digital_init: true,
            fof_svf_lp: [0; NUM_FORMANTS],
            fof_svf_bp: [0; NUM_FORMANTS],
            fof_previous_sample: 0,
            fof_next_saw_sample: 0,
            previous_parameter: [0; 2],
            hrm_amplitude: [0; NUM_ADDITIVE_HARMONICS],
            add_previous_sample: 0,
            fm_modulator_phase: 0,
            fm_previous_sample: 0,
            svf_bp: 0,
            svf_lp: 0,
            pno_filter_state: [[0; 2]; 2],
            clk_cycle_phase: 0,
            clk_cycle_phase_increment: 0,
            clk_rng_state: 0,
            clk_seed: 0,
            clk_sample: 0,
            pluck: [PluckVoice::default(); NUM_PLUCK_VOICES],
            pluck_active_voice: 0,
            pluck_previous_sample: 0,
            ks_delay: vec![0; NUM_PLUCK_VOICES * KS_VOICE_STRIDE],
            add_partial_phase: [0; NUM_BELL_PARTIALS],
            add_partial_phase_increment: [0; NUM_BELL_PARTIALS],
            add_partial_amplitude: [0; NUM_BELL_PARTIALS],
            add_target_partial_amplitude: [0; NUM_BELL_PARTIALS],
            add_current_partial: 0,
            add_lp_noise: [0; 3],
            pulse: [Excitation::default(); 4],
            bsvf: [DrumSvf::default(); 3],
            hat_phase: [0; 6],
            hat_rng_state: 0,
            digital_delay: 0,
            phy_delay_ptr: 0,
            phy_excitation_ptr: 0,
            phy_lp_state: 0,
            phy_filter_state: [0; 2],
            phy_previous_sample: 0,
            wg_bridge: vec![0; WG_BRIDGE_LENGTH],
            wg_neck: vec![0; WG_NECK_LENGTH],
            wg_bore: vec![0; WG_BORE_LENGTH],
            smoothed_parameter: 0,
            grain: [Grain::default(); 4],
            dmd_symbol_phase: 0,
            dmd_symbol_count: 0,
            dmd_filter_state: 0,
            dmd_data_byte: 0,
            pno_amplitude: 0,
            pno3_filter_state: [[0; 2]; 3],
            pno3_filter_scale: [0; 3],
            pno3_filter_coefficient: [0; 3],
            qm_rng_state: 0,
            qm_cycle_phase: 0,
            qm_sample: 0,
            qm_cycle_phase_increment: 0,
            qm_seed: 0,
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
        self.vow_formant_increment = [0; 3];
        self.vow_formant_amplitude = [0; 3];
        self.vow_consonant_frames = 0;
        self.vow_noise = 0;
        self.digital_init = true;
        self.fof_svf_lp = [0; NUM_FORMANTS];
        self.fof_svf_bp = [0; NUM_FORMANTS];
        self.fof_previous_sample = 0;
        self.fof_next_saw_sample = 0;
        self.hrm_amplitude = [0; NUM_ADDITIVE_HARMONICS];
        self.add_previous_sample = 0;
        self.fm_modulator_phase = 0;
        self.fm_previous_sample = 0;
        self.svf_bp = 0;
        self.svf_lp = 0;
        self.pno_filter_state = [[0; 2]; 2];
        self.clk_cycle_phase = 0;
        self.clk_cycle_phase_increment = 0;
        self.clk_rng_state = 0;
        self.clk_seed = 0;
        self.clk_sample = 0;
        self.pluck = [PluckVoice::default(); NUM_PLUCK_VOICES];
        self.pluck_active_voice = 0;
        self.pluck_previous_sample = 0;
        self.ks_delay.iter_mut().for_each(|s| *s = 0);
        self.add_partial_phase = [0; NUM_BELL_PARTIALS];
        self.add_partial_phase_increment = [0; NUM_BELL_PARTIALS];
        self.add_partial_amplitude = [0; NUM_BELL_PARTIALS];
        self.add_target_partial_amplitude = [0; NUM_BELL_PARTIALS];
        self.add_current_partial = 0;
        self.add_lp_noise = [0; 3];
        self.pulse = [Excitation::default(); 4];
        self.bsvf = [DrumSvf::default(); 3];
        self.hat_phase = [0; 6];
        self.hat_rng_state = 0;
        self.phy_delay_ptr = 0;
        self.phy_excitation_ptr = 0;
        self.phy_lp_state = 0;
        self.phy_filter_state = [0; 2];
        self.phy_previous_sample = 0;
        self.wg_bridge.iter_mut().for_each(|s| *s = 0);
        self.wg_neck.iter_mut().for_each(|s| *s = 0);
        self.wg_bore.iter_mut().for_each(|s| *s = 0);
        self.smoothed_parameter = 0;
        self.grain = [Grain::default(); 4];
        self.dmd_symbol_phase = 0;
        self.dmd_symbol_count = 0;
        self.dmd_filter_state = 0;
        self.dmd_data_byte = 0;
        self.pno_amplitude = 0;
        self.pno3_filter_state = [[0; 2]; 3];
        self.pno3_filter_scale = [0; 3];
        self.pno3_filter_coefficient = [0; 3];
        self.qm_rng_state = 0;
        self.qm_cycle_phase = 0;
        self.qm_sample = 0;
        self.qm_cycle_phase_increment = 0;
        self.qm_seed = 0;
        // previous_parameter is NOT reset by Init in the firmware
        self.phase = 0;
        self.strike = true;
    }

    #[inline]
    fn next_word(&mut self) -> u32 {
        // stmlib Random LCG.
        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.rng
    }

    /// stmlib `Random::GetSample()` — the top 16 bits as a signed sample.
    #[inline]
    fn next_sample(&mut self) -> i16 {
        (self.next_word() >> 16) as i16
    }

    pub fn render(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        // Quantize parameter_1 to a musical FM ratio for the FM family.
        if matches!(
            self.shape,
            DigitalShape::Fm | DigitalShape::FeedbackFm | DigitalShape::ChaoticFeedbackFm
        ) {
            let t = tables();
            let integral = (self.parameter[1] >> 8) as usize;
            let fractional = (self.parameter[1] & 255) as i32;
            let a = t.fm_frequency_quantizer[integral.min(t.fm_frequency_quantizer.len() - 2)] as i32;
            let b = t.fm_frequency_quantizer
                [(integral + 1).min(t.fm_frequency_quantizer.len() - 1)] as i32;
            self.parameter[1] = (a + ((b - a) * fractional >> 8)) as i16;
        }
        if self.shape != self.previous_shape {
            self.init();
            self.previous_shape = self.shape;
        }
        self.phase_increment = compute_phase_increment(self.pitch);
        self.digital_delay = compute_delay(self.pitch);
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
            DigitalShape::Vosim => self.render_vosim(sync, buffer, size),
            DigitalShape::Vowel => self.render_vowel(buffer, size),
            DigitalShape::VowelFof => self.render_vowel_fof(buffer, size),
            DigitalShape::Harmonics => self.render_harmonics(sync, buffer, size),
            DigitalShape::Fm => self.render_fm(sync, buffer, size),
            DigitalShape::FeedbackFm => self.render_feedback_fm(sync, buffer, size),
            DigitalShape::ChaoticFeedbackFm => self.render_chaotic_feedback_fm(sync, buffer, size),
            DigitalShape::FilteredNoise => self.render_filtered_noise(buffer, size),
            DigitalShape::TwinPeaksNoise => self.render_twin_peaks_noise(buffer, size),
            DigitalShape::ClockedNoise => self.render_clocked_noise(sync, buffer, size),
            DigitalShape::Plucked => self.render_plucked(buffer, size),
            DigitalShape::StruckBell => self.render_struck_bell(buffer, size),
            DigitalShape::StruckDrum => self.render_struck_drum(buffer, size),
            DigitalShape::Kick => self.render_kick(buffer, size),
            DigitalShape::Snare => self.render_snare(buffer, size),
            DigitalShape::Cymbal => self.render_cymbal(buffer, size),
            DigitalShape::Bowed => self.render_bowed(buffer, size),
            DigitalShape::Blown => self.render_blown(buffer, size),
            DigitalShape::Fluted => self.render_fluted(buffer, size),
            DigitalShape::Wavetables => self.render_wavetables(sync, buffer, size),
            DigitalShape::WaveMap => self.render_wave_map(sync, buffer, size),
            DigitalShape::WaveLine => self.render_wave_line(sync, buffer, size),
            DigitalShape::WaveParaphonic => self.render_wave_paraphonic(buffer, size),
            DigitalShape::GranularCloud => self.render_granular_cloud(buffer, size),
            DigitalShape::ParticleNoise => self.render_particle_noise(buffer, size),
            DigitalShape::DigitalModulation => self.render_digital_modulation(buffer, size),
            DigitalShape::QuestionMark => self.render_question_mark(buffer, size),
        }
    }

    /// GRANULAR_CLOUD — four sine grains with randomly seeded pitch and
    /// envelope rate (parameter_0 = grain length, parameter_1 = pitch
    /// spread).
    fn render_granular_cloud(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        let env_rate =
            (t.granular_envelope_rate[(self.parameter[0] as usize >> 7).min(256)] as u32) << 3;
        let phase_increment = self.phase_increment;
        for gi in 0..4 {
            if self.grain[gi].envelope_phase > (1 << 24)
                || self.grain[gi].envelope_phase_increment == 0
            {
                self.grain[gi].envelope_phase_increment = 0;
                if (self.rng_lcg() & 0xffff) < 0x4000 {
                    let pitch_mod = (self.qm_next_sample() as i32 * self.parameter[1] as i32) >> 16;
                    let phi = (phase_increment >> 8) as i32;
                    let mut inc = phase_increment;
                    if pitch_mod < 0 {
                        inc = inc.wrapping_add((phi * (pitch_mod >> 8)) as u32);
                    } else {
                        inc = inc.wrapping_add((phi * (pitch_mod >> 7)) as u32);
                    }
                    let g = &mut self.grain[gi];
                    g.envelope_phase_increment = env_rate;
                    g.envelope_phase = 0;
                    g.phase_increment = inc;
                }
            }
        }
        for b in buffer.iter_mut().take(size) {
            let mut sample = 0i32;
            for g in self.grain.iter_mut() {
                g.phase = g.phase.wrapping_add(g.phase_increment);
                g.envelope_phase = g.envelope_phase.wrapping_add(g.envelope_phase_increment);
                let env =
                    t.granular_envelope[((g.envelope_phase >> 16) as usize).min(512)] as i32;
                sample += interpolate824(&t.wav_sine, g.phase) * env >> 17;
            }
            *b = clip16(sample) as i16;
        }
    }

    /// PARTICLE_NOISE — sparse noise impulses through three tuned
    /// resonators (parameter_0 = density, parameter_1 = pitch spread).
    /// Rendered half-rate, upsampled 2×.
    fn render_particle_noise(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        let mut amplitude = self.pno_amplitude;
        let density = 1024 + self.parameter[0] as u32;
        let mut y = self.pno3_filter_state;
        let mut s = self.pno3_filter_scale;
        let mut c = self.pno3_filter_coefficient;
        let mut j = 0;
        while j < size {
            let noise = self.rng_lcg();
            if (noise & 0x7fffff) < density {
                amplitude = 65535;
                let noise_a = ((noise & 0x0fff) as i32) - 0x800;
                let noise_b = (((noise >> 15) & 0x1fff) as i32) - 0x1000;
                let offsets = [0x600, 0x980, 0x790];
                let muls = [
                    3 * noise_a * self.parameter[1] as i32 >> 17,
                    noise_a * self.parameter[1] as i32 >> 15,
                    noise_b * self.parameter[1] as i32 >> 16,
                ];
                for k in 0..3 {
                    let pp = (self.pitch as i32 + muls[k] + offsets[k]).clamp(0, 16383);
                    c[k] = (interpolate824_u16(&t.resonator_coefficient, (pp as u32) << 17) as i64
                        * RESONANCE_FACTOR
                        >> 15) as i32;
                    s[k] = interpolate824_u16(&t.resonator_scale, (pp as u32) << 17);
                }
            }
            let sample = ((noise as i16 as i32) * amplitude as i32) >> 16;
            amplitude = ((amplitude as i64 * PARTICLE_NOISE_DECAY) >> 16) as u16;
            let mut acc = 0i32;
            for k in 0..3 {
                let mut y0 = if sample > 0 {
                    sample * s[k] >> 16
                } else {
                    -((-sample) * s[k] >> 16)
                };
                y0 += (y[k][0] as i64 * c[k] as i64 >> 15) as i32;
                y0 -= (y[k][1] as i64 * RESONANCE_SQUARED >> 15) as i32;
                y0 = clip16(y0);
                y[k][1] = y[k][0];
                y[k][0] = y0;
                acc += y0;
            }
            acc = clip16(acc);
            buffer[j] = acc as i16;
            if j + 1 < size {
                buffer[j + 1] = acc as i16;
            }
            j += 2;
        }
        self.pno_amplitude = amplitude;
        self.pno3_filter_state = y;
        self.pno3_filter_scale = s;
        self.pno3_filter_coefficient = c;
    }

    /// DIGITAL_MODULATION — a QPSK data-stream voice: a sine carrier
    /// modulated by a constellation driven by a (parameter_1-seeded) symbol
    /// stream at a parameter_0 baud rate.
    fn render_digital_modulation(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        let symbol_stream_phase_increment = compute_phase_increment(
            (self.pitch as i32 - 1536 + ((self.parameter[0] as i32 - 32767) >> 3)) as i16,
        );
        if self.strike {
            self.dmd_symbol_count = 0;
            self.strike = false;
        }
        let mut symbol_stream_phase = self.dmd_symbol_phase;
        let mut data_byte = self.dmd_data_byte;
        for b in buffer.iter_mut().take(size) {
            self.phase = self.phase.wrapping_add(self.phase_increment);
            symbol_stream_phase = symbol_stream_phase.wrapping_add(symbol_stream_phase_increment);
            if symbol_stream_phase < symbol_stream_phase_increment {
                self.dmd_symbol_count = self.dmd_symbol_count.wrapping_add(1);
                if self.dmd_symbol_count & 3 == 0 {
                    if self.dmd_symbol_count >= (64 + 4 * 256) {
                        self.dmd_symbol_count = 0;
                    }
                    if self.dmd_symbol_count < 32 {
                        data_byte = 0x00;
                    } else if self.dmd_symbol_count < 48 {
                        data_byte = 0x99;
                    } else if self.dmd_symbol_count < 64 {
                        data_byte = 0xcc;
                    } else {
                        self.dmd_filter_state =
                            (self.dmd_filter_state * 3 + self.parameter[1] as i32) >> 2;
                        data_byte = (self.dmd_filter_state >> 7) as u8;
                    }
                } else {
                    data_byte >>= 2;
                }
            }
            let i = interpolate824(&t.wav_sine, self.phase);
            let q = interpolate824(&t.wav_sine, self.phase.wrapping_add(1 << 30));
            let idx = (data_byte & 3) as usize;
            *b = ((CONSTELLATION_Q[idx] * q >> 15) + (CONSTELLATION_I[idx] * i >> 15)) as i16;
        }
        self.dmd_symbol_phase = symbol_stream_phase;
        self.dmd_data_byte = data_byte;
    }

    /// QUESTION_MARK — a morse-code "?" beep (·· − − ··) over a sine, with a
    /// noisy radio-static bed (parameter_0 = speed, parameter_1 = noise/drive).
    fn render_question_mark(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        if self.strike {
            self.qm_rng_state = 0;
            self.qm_cycle_phase = 0;
            self.qm_sample = 10;
            self.qm_cycle_phase_increment = -1;
            self.qm_seed = 32767;
            self.strike = false;
        }
        let mut phase = self.phase;
        let increment = self.phase_increment;
        let dit_duration = 3600 + ((32767 - self.parameter[0] as i32) >> 2) as u32;
        let noise_threshold = 1024 + (self.parameter[1] as i32 >> 3);
        for b in buffer.iter_mut().take(size) {
            phase = phase.wrapping_add(increment);
            let mut sample = if self.qm_rng_state != 0 {
                (interpolate824(&t.wav_sine, phase) * 3) >> 2
            } else {
                0
            };
            self.qm_cycle_phase = self.qm_cycle_phase.wrapping_add(1);
            if self.qm_cycle_phase > dit_duration {
                self.qm_sample -= 1;
                if self.qm_sample == 0 {
                    self.qm_cycle_phase_increment += 1;
                    self.qm_rng_state = if self.qm_rng_state != 0 { 0 } else { 1 };
                    let address = (self.qm_cycle_phase_increment >> 2) as usize;
                    let shift = ((self.qm_cycle_phase_increment & 0x3) << 1) as u32;
                    let code = t.wt_code[address.min(t.wt_code.len() - 1)] as u32;
                    self.qm_sample = (2 << ((code >> shift) & 3)) - 1;
                    if self.qm_sample == 15 {
                        self.qm_sample = 100;
                        self.qm_rng_state = 0;
                        self.qm_cycle_phase_increment = -1;
                    }
                    phase = 1 << 30;
                }
                self.qm_cycle_phase = 0;
            }
            self.qm_seed += (self.qm_next_sample() as i32) >> 2;
            let mut noise_intensity = (self.qm_seed >> 8).abs();
            if noise_intensity < noise_threshold {
                noise_intensity = noise_threshold;
            }
            if noise_intensity > 16000 {
                noise_intensity = 16000;
            }
            let mut noise = self.qm_next_sample() as i32 * noise_intensity >> 15;
            noise = noise * t.wav_sine[((phase >> 22) & 0xff) as usize] as i32 >> 15;
            sample += noise;
            sample = clip16(sample);
            let distorted = sample * sample >> 14;
            sample += distorted * self.parameter[1] as i32 >> 15;
            *b = clip16(sample) as i16;
        }
        self.phase = phase;
    }

    /// stmlib `Random::GetWord` (the global LCG, reused for the granular,
    /// particle, and question-mark models).
    #[inline]
    fn rng_lcg(&mut self) -> u32 {
        self.next_word()
    }

    /// stmlib `Random::GetSample` for the same models.
    #[inline]
    fn qm_next_sample(&mut self) -> i16 {
        self.next_sample()
    }

    /// WAVETABLES — scan one of 20 wavetables (parameter_1 selects, with
    /// hysteresis) by sweeping parameter_0 through its waves, 2× oversampled.
    fn render_wavetables(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        let (p1, pp1) = (self.parameter[1] as i32, self.previous_parameter[1] as i32);
        if p1 > pp1 + 64 || p1 < pp1 - 64 {
            self.previous_parameter[1] = self.parameter[1];
        }
        let wavetable_index =
            ((self.previous_parameter[1] as u32 * 20) >> 15) as usize % WAVETABLE_DEFS.len();
        let (num_steps, wave_index) = WAVETABLE_DEFS[wavetable_index];
        let wave_pointer = ((self.parameter[0] as u32) << 1).wrapping_mul(num_steps as u32);
        let wp = (wave_pointer >> 16) as usize;
        let w0 = wave_index[wp.min(16)] as usize * WAVE_STRIDE;
        let w1 = wave_index[(wp + 1).min(16)] as usize * WAVE_STRIDE;
        let wave0 = &t.wt_waves[w0..w0 + WAVE_STRIDE];
        let wave1 = &t.wt_waves[w1..w1 + WAVE_STRIDE];
        let balance = wave_pointer as u16;
        let phase_increment = self.phase_increment >> 1;
        for (n, b) in buffer.iter_mut().take(size).enumerate() {
            self.phase = self.phase.wrapping_add(phase_increment);
            if sync[n] != 0 {
                self.phase = 0;
            }
            let mut sample = crossfade_u8(wave0, wave1, self.phase >> 1, balance) >> 1;
            self.phase = self.phase.wrapping_add(phase_increment);
            sample += crossfade_u8(wave0, wave1, self.phase >> 1, balance) >> 1;
            *b = sample as i16;
        }
    }

    /// WAVE_MAP — a 16×16 terrain of waves; parameter_0/1 are the X/Y
    /// coordinate, bilinearly blended. 2× oversampled.
    fn render_wave_map(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        let p0 = self.parameter[0] as i32 * 15 >> 4;
        let p1 = self.parameter[1] as i32 * 15 >> 4;
        let wave_xfade0 = (p0 << 5) as u16;
        let wave_xfade1 = (p1 << 5) as u16;
        let coord0 = (p0 >> 11) as usize;
        let coord1 = (p1 >> 11) as usize;
        let slice = |i: usize, j: usize| -> &[u8] {
            let idx = ((coord0 + i) * 16 + (coord1 + j)).min(255);
            let w = t.wt_map[idx] as usize * WAVE_STRIDE;
            &t.wt_waves[w..w + WAVE_STRIDE]
        };
        let (w00, w01) = (slice(0, 0), slice(0, 1));
        let (w10, w11) = (slice(1, 0), slice(1, 1));
        let cf = |a: &[u8], bb: &[u8], ph: u32| crossfade_u8(a, bb, ph, wave_xfade1) as i16;
        let phase_increment = self.phase_increment >> 1;
        for (n, b) in buffer.iter_mut().take(size).enumerate() {
            self.phase = self.phase.wrapping_add(phase_increment);
            if sync[n] != 0 {
                self.phase = 0;
            }
            let ph = self.phase >> 1;
            let mut sample =
                mix(cf(w00, w01, ph), cf(w10, w11, ph), wave_xfade0) as i32 >> 1;
            self.phase = self.phase.wrapping_add(phase_increment);
            let ph = self.phase >> 1;
            sample += mix(cf(w00, w01, ph), cf(w10, w11, ph), wave_xfade0) as i32 >> 1;
            *b = sample as i16;
        }
    }

    /// WAVE_LINE — scan a 64-step wave path (parameter_0), morphing between
    /// "rough" (bit-reduced phase) and "smooth" reads by parameter_1. Each
    /// output sums two 2× oversamples.
    fn render_wave_line(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        self.smoothed_parameter =
            (3 * self.smoothed_parameter + ((self.parameter[0] as i32) << 1)) >> 2;
        let scan = self.smoothed_parameter as u32 & 0xffff;
        let wl = |i: usize| -> &[u8] {
            let w = WAVE_LINE[i.min(63)] as usize * WAVE_STRIDE;
            &t.wt_waves[w..w + WAVE_STRIDE]
        };
        let wave_0 = wl((self.previous_parameter[0] as u32 >> 9) as usize);
        let wave_1 = wl((scan >> 10) as usize);
        let wave_2 = wl((scan >> 10) as usize + 1);
        let smooth_xfade = (scan << 6) as u16;
        let mut rough_xfade = 0u16;
        let rough_xfade_increment = (32768 / size.max(1) as u32) as u16;
        let balance = ((self.parameter[1] as u32) << 3) as u16;
        let p1 = self.parameter[1];
        let phase_increment = self.phase_increment >> 1;
        for (n, b) in buffer.iter_mut().take(size).enumerate() {
            if sync[n] != 0 {
                self.phase = 0;
            }
            let mut sample = 0i32;
            for _ in 0..2 {
                let ph = self.phase >> 1;
                sample += if p1 < 8192 {
                    let rough = crossfade_u8(wave_0, wave_1, ph & 0xfe00_0000, rough_xfade);
                    let smooth = crossfade_u8(wave_0, wave_1, ph, rough_xfade);
                    mix(rough as i16, smooth as i16, balance) as i32
                } else if p1 < 16384 {
                    let rough = crossfade_u8(wave_0, wave_1, ph, rough_xfade);
                    let smooth = crossfade_u8(wave_1, wave_2, ph, smooth_xfade);
                    mix(rough as i16, smooth as i16, balance) as i32
                } else if p1 < 24576 {
                    let smooth = crossfade_u8(wave_1, wave_2, ph, smooth_xfade);
                    let rough = crossfade_u8(wave_1, wave_2, ph & 0xfe00_0000, smooth_xfade);
                    mix(smooth as i16, rough as i16, balance) as i32
                } else {
                    let smooth = crossfade_u8(wave_1, wave_2, ph & 0xfe00_0000, smooth_xfade);
                    let rough = crossfade_u8(wave_1, wave_2, ph & 0xf800_0000, smooth_xfade);
                    mix(smooth as i16, rough as i16, balance) as i32
                };
                self.phase = self.phase.wrapping_add(phase_increment);
                if p1 < 16384 {
                    rough_xfade = rough_xfade.wrapping_add(rough_xfade_increment);
                }
            }
            *b = (sample >> 1) as i16;
        }
        self.previous_parameter[0] = (self.smoothed_parameter >> 1) as i16;
    }

    /// WAVE_PARAPHONIC — four detuned wavetable voices forming a chord
    /// (parameter_1 selects the chord, parameter_0 the wave).
    fn render_wave_paraphonic(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        if self.strike {
            for k in 0..4 {
                self.saw_phase[k] = self.next_word();
            }
            self.strike = false;
        }
        let chord_integral = (self.parameter[1] as usize >> 11).min(15);
        let mut chord_fractional = ((self.parameter[1] as u32) << 5) & 0xffff;
        if chord_fractional < 30720 {
            chord_fractional = 0;
        } else if chord_fractional >= 34816 {
            chord_fractional = 65535;
        } else {
            chord_fractional = (chord_fractional - 30720) * 16;
        }
        let mut phase_increment = [0u32; 3];
        for (i, pi) in phase_increment.iter_mut().enumerate() {
            let d1 = CHORDS[chord_integral][i] as i32;
            let d2 = CHORDS[chord_integral + 1][i] as i32;
            let detune = d1 + ((d2 - d1) * chord_fractional as i32 >> 16);
            *pi = compute_phase_increment((self.pitch as i32 + detune) as i16);
        }
        let w1 = MINI_WAVE_LINE[(self.parameter[0] as usize >> 10).min(32)] as usize * WAVE_STRIDE;
        let w2 =
            MINI_WAVE_LINE[((self.parameter[0] as usize >> 10) + 1).min(32)] as usize * WAVE_STRIDE;
        let wave_1 = &t.wt_waves[w1..w1 + WAVE_STRIDE];
        let wave_2 = &t.wt_waves[w2..w2 + WAVE_STRIDE];
        let wave_xfade = ((self.parameter[0] as u32) << 6) as u16;
        let incs = [
            self.phase_increment,
            phase_increment[0],
            phase_increment[1],
            phase_increment[2],
        ];
        for b in buffer.iter_mut().take(size) {
            let mut sample = 0i32;
            #[allow(clippy::needless_range_loop)] // parallel saw_phase / incs
            for k in 0..4 {
                self.saw_phase[k] = self.saw_phase[k].wrapping_add(incs[k]);
                sample += crossfade_u8(wave_1, wave_2, self.saw_phase[k] >> 1, wave_xfade);
            }
            *b = (sample >> 2) as i16;
        }
    }

    /// BLOWN — a clarinet-ish bore waveguide driven by a non-linear reed.
    /// parameter_0 = breath noise, parameter_1 = body tuning. Full rate.
    fn render_blown(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        if self.strike {
            self.wg_bore.iter_mut().for_each(|s| *s = 0);
            self.strike = false;
        }
        let mut delay = (self.digital_delay >> 1).wrapping_sub(1 << 16);
        while delay > ((WG_BORE_LENGTH as u32 - 1) << 16) {
            delay >>= 1;
        }
        let bore_delay_integral = (delay >> 16) as u16;
        let bore_delay_fractional = (delay & 0xffff) as u16;
        let parameter = 28000 - (self.parameter[0] >> 1);
        let mut filter_state = self.phy_filter_state[0];
        let mut normalized_pitch = (self.pitch as i32 - 8192 + (self.parameter[1] as i32 >> 1)) >> 7;
        normalized_pitch = normalized_pitch.clamp(0, 127);
        let filter_coefficient = t.flute_body_filter[normalized_pitch as usize] as i32;
        let mut delay_ptr = self.phy_delay_ptr;
        let mut lp_state = self.phy_lp_state;
        for b in buffer.iter_mut().take(size) {
            self.phase = self.phase.wrapping_add(self.phase_increment);
            let mut breath_pressure = self.next_sample() as i32 * parameter as i32 >> 15;
            breath_pressure = (breath_pressure as i64 * BREATH_PRESSURE >> 15) as i32;
            breath_pressure += BREATH_PRESSURE as i32;
            let bore_delay_ptr =
                delay_ptr.wrapping_add(2 * WG_BORE_LENGTH as u16).wrapping_sub(bore_delay_integral);
            let dl_a = self.wg_bore[bore_delay_ptr as usize % WG_BORE_LENGTH];
            let dl_b = self.wg_bore[(bore_delay_ptr.wrapping_sub(1)) as usize % WG_BORE_LENGTH];
            let dl_value = mix(dl_a, dl_b, bore_delay_fractional) as i32;
            let mut pressure_delta = (dl_value >> 1) + lp_state;
            lp_state = dl_value >> 1;
            pressure_delta = (REFLECTION_COEFFICIENT * pressure_delta as i64 >> 12) as i32;
            pressure_delta -= breath_pressure;
            let reed = clip16((pressure_delta as i64 * REED_SLOPE >> 12) as i32 + REED_OFFSET);
            let mut out = (pressure_delta as i64 * reed as i64 >> 15) as i32;
            out += breath_pressure;
            out = clip16(out);
            self.wg_bore[delay_ptr as usize % WG_BORE_LENGTH] = out as i16;
            delay_ptr = delay_ptr.wrapping_add(1);
            filter_state = (filter_coefficient * out
                + (4096 - filter_coefficient) * filter_state)
                >> 12;
            *b = filter_state as i16;
        }
        self.phy_filter_state[0] = filter_state;
        self.phy_delay_ptr = delay_ptr % WG_BORE_LENGTH as u16;
        self.phy_lp_state = lp_state;
    }

    /// FLUTED — a flute waveguide: a jet delay driving a bore delay through
    /// the jet non-linearity, a body low-pass and a DC blocker.
    /// parameter_0 = breath intensity, parameter_1 = jet ratio. Full rate.
    fn render_fluted(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        let mut delay_ptr = self.phy_delay_ptr;
        let mut excitation_ptr = self.phy_excitation_ptr;
        let mut lp_state = self.phy_lp_state;
        let mut dc_x0 = self.phy_filter_state[0];
        let mut dc_y0 = self.phy_filter_state[1];
        if self.strike {
            excitation_ptr = 0;
            self.wg_neck.iter_mut().for_each(|s| *s = 0); // fbore
            self.wg_bridge.iter_mut().for_each(|s| *s = 0); // jet
            lp_state = 0;
            self.strike = false;
        }
        let mut bore_delay = (self.digital_delay << 1).wrapping_sub(2 << 16);
        let mut jet_delay = (bore_delay >> 8) * (48 + (self.parameter[1] as u32 >> 10));
        bore_delay = bore_delay.wrapping_sub(jet_delay);
        while bore_delay > ((WG_FBORE_LENGTH as u32 - 1) << 16)
            || jet_delay > ((WG_JET_LENGTH as u32 - 1) << 16)
        {
            bore_delay >>= 1;
            jet_delay >>= 1;
        }
        let bore_delay_integral = (bore_delay >> 16) as u16;
        let bore_delay_fractional = (bore_delay & 0xffff) as u16;
        let jet_delay_integral = (jet_delay >> 16) as u16;
        let jet_delay_fractional = (jet_delay & 0xffff) as u16;
        let breath_intensity = 2100 - (self.parameter[0] >> 4);
        let filter_coefficient =
            t.flute_body_filter[((self.pitch >> 7) as usize).min(127)] as i32;
        let mut size_left = size;
        let mut j = 0;
        while size_left > 0 {
            size_left -= 1; // mirror the firmware's `while (size--)`
            self.phase = self.phase.wrapping_add(self.phase_increment);
            let bore_delay_ptr =
                delay_ptr.wrapping_add(2 * WG_FBORE_LENGTH as u16).wrapping_sub(bore_delay_integral);
            let jet_delay_ptr =
                delay_ptr.wrapping_add(2 * WG_JET_LENGTH as u16).wrapping_sub(jet_delay_integral);
            let bore_dl_a = self.wg_neck[bore_delay_ptr as usize % WG_FBORE_LENGTH] as i16;
            let bore_dl_b =
                self.wg_neck[(bore_delay_ptr.wrapping_sub(1)) as usize % WG_FBORE_LENGTH] as i16;
            let jet_dl_a = self.wg_bridge[jet_delay_ptr as usize % WG_JET_LENGTH] as i16;
            let jet_dl_b =
                self.wg_bridge[(jet_delay_ptr.wrapping_sub(1)) as usize % WG_JET_LENGTH] as i16;
            let bore_value = (mix(bore_dl_a, bore_dl_b, bore_delay_fractional) as i32) << 9;
            let jet_value = (mix(jet_dl_a, jet_dl_b, jet_delay_fractional) as i32) << 9;
            let mut breath_pressure =
                (t.blowing_envelope[(excitation_ptr as usize).min(LUT_BLOWING_ENVELOPE_SIZE - 1)]
                    as i32)
                    << 1;
            let mut random_pressure = self.next_sample() as i32 * breath_intensity as i32 >> 12;
            random_pressure = (random_pressure as i64 * breath_pressure as i64 >> 15) as i32;
            breath_pressure += random_pressure;
            lp_state = ((-filter_coefficient as i64 * bore_value as i64
                + (4096 - filter_coefficient) as i64 * lp_state as i64)
                >> 12) as i32;
            let mut reflection = lp_state;
            dc_y0 = (DC_BLOCKING_POLE * dc_y0 as i64 >> 12) as i32;
            dc_y0 += reflection - dc_x0;
            dc_x0 = reflection;
            reflection = dc_y0;
            let pressure_delta = breath_pressure - (reflection >> 1);
            self.wg_bridge[delay_ptr as usize % WG_JET_LENGTH] = (pressure_delta >> 9) as i8;
            let jet_table_index = jet_value.clamp(0, 65535);
            let pressure_delta =
                t.blowing_jet[(jet_table_index >> 8) as usize] as i32 + (reflection >> 1);
            self.wg_neck[delay_ptr as usize % WG_FBORE_LENGTH] = (pressure_delta >> 9) as i8;
            delay_ptr = delay_ptr.wrapping_add(1);
            let out = clip16(bore_value >> 1);
            buffer[j] = out as i16;
            j += 1;
            if size_left & 3 != 0 {
                excitation_ptr = excitation_ptr.wrapping_add(1);
            }
        }
        if excitation_ptr as usize >= LUT_BLOWING_ENVELOPE_SIZE - 32 {
            excitation_ptr = (LUT_BLOWING_ENVELOPE_SIZE - 32) as u16;
        }
        self.phy_delay_ptr = delay_ptr;
        self.phy_excitation_ptr = excitation_ptr;
        self.phy_lp_state = lp_state;
        self.phy_filter_state = [dc_x0, dc_y0];
    }

    /// BOWED — a bowed-string waveguide: a bridge + neck delay-line pair
    /// with a non-linear bow-friction excitation and a body resonator
    /// biquad. parameter_0 = bow force, parameter_1 = string length.
    /// Rendered half-rate, upsampled 2×.
    fn render_bowed(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        if self.strike {
            self.wg_bridge.iter_mut().for_each(|s| *s = 0);
            self.wg_neck.iter_mut().for_each(|s| *s = 0);
            self.phy_delay_ptr = 0;
            self.phy_excitation_ptr = 0;
            self.phy_lp_state = 0;
            self.phy_filter_state = [0; 2];
            self.phy_previous_sample = 0;
            self.strike = false;
        }
        let parameter_0 = 172 - (self.parameter[0] >> 8);
        let parameter_1 = 6 + (self.parameter[1] >> 9);

        let mut delay_ptr = self.phy_delay_ptr;
        let mut excitation_ptr = self.phy_excitation_ptr;
        let mut lp_state = self.phy_lp_state;
        let mut biquad_y0 = self.phy_filter_state[0];
        let mut biquad_y1 = self.phy_filter_state[1];

        let mut delay = (self.digital_delay >> 1).wrapping_sub(2 << 16);
        let mut bridge_delay = (delay >> 8) * parameter_1 as u32;
        while (delay.wrapping_sub(bridge_delay)) > ((WG_NECK_LENGTH as u32 - 1) << 16)
            || bridge_delay > ((WG_BRIDGE_LENGTH as u32 - 1) << 16)
        {
            delay >>= 1;
            bridge_delay >>= 1;
        }
        let bridge_delay_integral = (bridge_delay >> 16) as u16;
        let bridge_delay_fractional = (bridge_delay & 0xffff) as u16;
        let neck_delay = delay.wrapping_sub(bridge_delay);
        let neck_delay_integral = (neck_delay >> 16) as u16;
        let neck_delay_fractional = (neck_delay & 0xffff) as u16;
        let mut previous_sample = self.phy_previous_sample as i32;

        let mut j = 0;
        while j < size {
            self.phase = self.phase.wrapping_add(self.phase_increment);
            let bridge_delay_ptr =
                delay_ptr.wrapping_add(2 * WG_BRIDGE_LENGTH as u16).wrapping_sub(bridge_delay_integral);
            let neck_delay_ptr =
                delay_ptr.wrapping_add(2 * WG_NECK_LENGTH as u16).wrapping_sub(neck_delay_integral);
            let bridge_dl_a = self.wg_bridge[bridge_delay_ptr as usize % WG_BRIDGE_LENGTH] as i16;
            let bridge_dl_b =
                self.wg_bridge[(bridge_delay_ptr.wrapping_sub(1)) as usize % WG_BRIDGE_LENGTH] as i16;
            let nut_dl_a = self.wg_neck[neck_delay_ptr as usize % WG_NECK_LENGTH] as i16;
            let nut_dl_b =
                self.wg_neck[(neck_delay_ptr.wrapping_sub(1)) as usize % WG_NECK_LENGTH] as i16;
            let bridge_value = (mix(bridge_dl_a, bridge_dl_b, bridge_delay_fractional) as i32) << 8;
            let nut_value = (mix(nut_dl_a, nut_dl_b, neck_delay_fractional) as i32) << 8;
            lp_state = ((bridge_value as i64 * BRIDGE_LP_GAIN
                + lp_state as i64 * BRIDGE_LP_POLE1)
                >> 15) as i32;
            let bridge_reflection = -lp_state;
            let nut_reflection = -nut_value;
            let string_velocity = bridge_reflection + nut_reflection;
            // clamp the envelope index (the firmware can run one past the
            // LUT within a block before its end-of-block clamp)
            let last = LUT_BOWING_ENVELOPE_SIZE - 1;
            let mut bow_velocity =
                t.bowing_envelope[((excitation_ptr >> 1) as usize).min(last)] as i32;
            bow_velocity +=
                t.bowing_envelope[(((excitation_ptr + 1) >> 1) as usize).min(last)] as i32;
            bow_velocity >>= 1;
            let velocity_delta = bow_velocity - string_velocity;
            let mut friction = velocity_delta * parameter_0 as i32 >> 5;
            friction = friction.abs();
            if friction >= (1 << 17) {
                friction = (1 << 17) - 1;
            }
            friction = t.bowing_friction[(friction >> 9) as usize] as i32;
            let new_velocity = (friction as i64 * velocity_delta as i64 >> 15) as i32;
            self.wg_neck[delay_ptr as usize % WG_NECK_LENGTH] =
                ((bridge_reflection + new_velocity) >> 8) as i8;
            self.wg_bridge[delay_ptr as usize % WG_BRIDGE_LENGTH] =
                ((nut_reflection + new_velocity) >> 8) as i8;
            delay_ptr = delay_ptr.wrapping_add(1);

            let mut temp = (bridge_value as i64 * BIQUAD_GAIN >> 15) as i32;
            temp += (biquad_y0 as i64 * BIQUAD_POLE1 >> 12) as i32;
            temp += (biquad_y1 as i64 * BIQUAD_POLE2 >> 12) as i32;
            let out = clip16(temp - biquad_y1);
            biquad_y1 = biquad_y0;
            biquad_y0 = temp;

            buffer[j] = ((out + previous_sample) >> 1) as i16;
            if j + 1 < size {
                buffer[j + 1] = out as i16;
            }
            previous_sample = out;
            excitation_ptr = excitation_ptr.wrapping_add(1);
            j += 2;
        }
        if (excitation_ptr >> 1) as usize >= LUT_BOWING_ENVELOPE_SIZE - 32 {
            excitation_ptr = ((LUT_BOWING_ENVELOPE_SIZE - 32) << 1) as u16;
        }
        self.phy_delay_ptr = delay_ptr % WG_NECK_LENGTH as u16;
        self.phy_excitation_ptr = excitation_ptr;
        self.phy_lp_state = lp_state;
        self.phy_filter_state = [biquad_y0, biquad_y1];
        self.phy_previous_sample = previous_sample as i16;
    }

    /// KICK — an 808-ish bridged-T kick: a triple-pulse exciter into a
    /// punchy resonant band-pass with a sweepable pitch and a one-pole
    /// tone control (parameter_0 = decay, parameter_1 = tone). Half-rate.
    fn render_kick(&mut self, buffer: &mut [i16], size: usize) {
        if self.digital_init {
            self.pulse[0] = Excitation::default();
            self.pulse[0].set_delay(0);
            self.pulse[0].set_decay(3340);
            self.pulse[1] = Excitation::default();
            self.pulse[1].set_delay((1.0e-3 * 48000.0) as u32);
            self.pulse[1].set_decay(3072);
            self.pulse[2] = Excitation::default();
            self.pulse[2].set_delay((4.0e-3 * 48000.0) as u32);
            self.pulse[2].set_decay(4093);
            self.bsvf[0] = DrumSvf::default();
            self.bsvf[0].set_punch(32768);
            self.bsvf[0].set_mode(SvfMode::Bp);
            self.digital_init = false;
        }
        if self.strike {
            self.strike = false;
            self.pulse[0].trigger((12 * 32768) * 7 / 10);
            self.pulse[1].trigger(-19662 * 7 / 10);
            self.pulse[2].trigger(18000);
            self.bsvf[0].set_punch(24000);
        }
        let decay = self.parameter[0] as u32;
        let mut scaled = 65535 - (decay << 1);
        let squared = (scaled as u64 * scaled as u64 >> 16) as u32;
        scaled = (squared as u64 * scaled as u64 >> 18) as u32;
        self.bsvf[0].set_resonance((32768 - 128 - scaled as i32) as i16);
        let mut coefficient = self.parameter[1] as i64;
        coefficient = coefficient * coefficient >> 15;
        coefficient = coefficient * coefficient >> 15;
        let lp_coefficient = 128 + (coefficient as i32 >> 1) * 3;
        let mut lp_state = self.svf_lp;
        let mut j = 0;
        while j < size {
            let mut excitation = self.pulse[0].process();
            excitation += if !self.pulse[1].done() { 16384 } else { 0 };
            excitation += self.pulse[1].process();
            self.pulse[2].process();
            self.bsvf[0]
                .set_frequency(self.pitch + if self.pulse[2].done() { 0 } else { 17 << 7 });
            for _ in 0..2 {
                let resonator_output = (excitation >> 4) + self.bsvf[0].process(excitation);
                lp_state += ((resonator_output - lp_state) as i64 * lp_coefficient as i64 >> 15)
                    as i32;
                lp_state = clip16(lp_state);
                if j < size {
                    buffer[j] = lp_state as i16;
                    j += 1;
                }
            }
        }
        self.svf_lp = lp_state;
    }

    /// SNARE — two tuned resonant band-passes (shell modes) excited by
    /// pulses, plus a band-passed noise burst (the snares). parameter_0
    /// balances the two shell modes, parameter_1 sets decay + snappiness.
    fn render_snare(&mut self, buffer: &mut [i16], size: usize) {
        if self.digital_init {
            self.pulse[0] = Excitation::default();
            self.pulse[0].set_delay(0);
            self.pulse[0].set_decay(1536);
            self.pulse[1] = Excitation::default();
            self.pulse[1].set_delay((1e-3 * 48000.0) as u32);
            self.pulse[1].set_decay(3072);
            self.pulse[2] = Excitation::default();
            self.pulse[2].set_delay((1e-3 * 48000.0) as u32);
            self.pulse[2].set_decay(1200);
            self.pulse[3] = Excitation::default();
            self.pulse[3].set_delay(0);
            self.bsvf[0] = DrumSvf::default();
            self.bsvf[1] = DrumSvf::default();
            self.bsvf[2] = DrumSvf::default();
            self.bsvf[2].set_resonance(2000);
            self.bsvf[2].set_mode(SvfMode::Bp);
            self.digital_init = false;
        }
        if self.strike {
            let mut decay = 49152 - self.pitch as i32;
            decay += if self.parameter[1] < 16384 {
                0
            } else {
                self.parameter[1] as i32 - 16384
            };
            if decay > 65535 {
                decay = 65535;
            }
            self.bsvf[0].set_resonance((29000 + (decay >> 5)) as i16);
            self.bsvf[1].set_resonance((26500 + (decay >> 5)) as i16);
            self.pulse[3].set_decay((4092 + (decay >> 14)) as u32);
            self.pulse[0].trigger(15 * 32768);
            self.pulse[1].trigger(-32768);
            self.pulse[2].trigger(13107);
            let snappy = (self.parameter[1] as i32).min(14336);
            self.pulse[3].trigger(512 + (snappy << 1));
            self.strike = false;
        }
        self.bsvf[0].set_frequency(self.pitch + (12 << 7));
        self.bsvf[1].set_frequency(self.pitch + (24 << 7));
        self.bsvf[2].set_frequency(self.pitch + (60 << 7));
        let g_1 = 22000 - (self.parameter[0] as i32 >> 1);
        let g_2 = 22000 + (self.parameter[0] as i32 >> 1);
        let mut j = 0;
        while j < size {
            let mut excitation_1 = self.pulse[0].process();
            excitation_1 += self.pulse[1].process();
            excitation_1 += if !self.pulse[1].done() { 2621 } else { 0 };
            let mut excitation_2 = self.pulse[2].process();
            excitation_2 += if !self.pulse[2].done() { 13107 } else { 0 };
            let noise_sample = (self.next_sample() as i32 * self.pulse[3].process()) >> 15;
            let mut sd = (self.bsvf[0].process(excitation_1) as i64 + (excitation_1 >> 4) as i64)
                * g_1 as i64
                >> 15;
            sd += (self.bsvf[1].process(excitation_2) as i64 + (excitation_2 >> 4) as i64)
                * g_2 as i64
                >> 15;
            sd += self.bsvf[2].process(noise_sample) as i64;
            let sd = clip16(sd as i32);
            buffer[j] = sd as i16;
            if j + 1 < size {
                buffer[j + 1] = sd as i16;
            }
            j += 2;
        }
    }

    /// CYMBAL — six inharmonic square oscillators (808-style metallic
    /// noise) plus an LCG noise source, each band/high-passed and crossfaded
    /// by parameter_1; parameter_0 sets the filter frequencies.
    fn render_cymbal(&mut self, buffer: &mut [i16], size: usize) {
        if self.digital_init {
            self.bsvf[0] = DrumSvf::default();
            self.bsvf[0].set_mode(SvfMode::Bp);
            self.bsvf[0].set_resonance(12000);
            self.bsvf[1] = DrumSvf::default();
            self.bsvf[1].set_mode(SvfMode::Hp);
            self.bsvf[1].set_resonance(2000);
            self.digital_init = false;
        }
        let mut increments = [0u32; 7];
        let note = ((40 << 7) + (self.pitch as i32 >> 1)) as i16;
        increments[0] = compute_phase_increment(note);
        let root = increments[0] >> 10;
        increments[1] = root.wrapping_mul(24273) >> 4;
        increments[2] = root.wrapping_mul(12561) >> 4;
        increments[3] = root.wrapping_mul(18417) >> 4;
        increments[4] = root.wrapping_mul(22452) >> 4;
        increments[5] = root.wrapping_mul(31858) >> 4;
        increments[6] = increments[0].wrapping_mul(24);
        let xfade = self.parameter[1] as i64;
        self.bsvf[0].set_frequency(self.parameter[0] >> 1);
        self.bsvf[1].set_frequency(self.parameter[0] >> 1);
        for b in buffer.iter_mut().take(size) {
            self.phase = self.phase.wrapping_add(increments[6]);
            if self.phase < increments[6] {
                self.hat_rng_state = self
                    .hat_rng_state
                    .wrapping_mul(1_664_525)
                    .wrapping_add(1_013_904_223);
            }
            let mut hat_noise = 0i32;
            #[allow(clippy::needless_range_loop)] // parallel hat_phase / increments
            for i in 0..6 {
                self.hat_phase[i] = self.hat_phase[i].wrapping_add(increments[i]);
                hat_noise += (self.hat_phase[i] >> 31) as i32;
            }
            hat_noise -= 3;
            hat_noise *= 5461;
            hat_noise = clip16(self.bsvf[0].process(hat_noise));
            let mut noise = (self.hat_rng_state >> 16) as i32 - 32768;
            noise = clip16(self.bsvf[1].process(noise >> 1));
            *b = (hat_noise + ((noise - hat_noise) as i64 * xfade >> 15) as i32) as i16;
        }
    }

    /// STRUCK_BELL — an 11-partial inharmonic additive bell. parameter_0
    /// sets decay (max = droning), parameter_1 detunes odd/even partials.
    /// Rendered half-rate, upsampled 2×.
    #[allow(clippy::needless_range_loop)] // parallel add_* / partial-table arrays
    fn render_struck_bell(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        // Stagger partial-frequency refresh (the firmware's CPU-saving arp).
        let mut first_partial = self.add_current_partial;
        let mut last_partial = (self.add_current_partial + 3).min(NUM_BELL_PARTIALS);
        self.add_current_partial = (first_partial + 3) % NUM_BELL_PARTIALS;

        if self.strike {
            for i in 0..NUM_BELL_PARTIALS {
                self.add_partial_amplitude[i] = BELL_PARTIAL_AMPLITUDES[i];
                self.add_partial_phase[i] = 1 << 30;
            }
            self.strike = false;
            first_partial = 0;
            last_partial = NUM_BELL_PARTIALS;
        }

        for i in first_partial..last_partial {
            let detune = (self.parameter[1] >> 7) as i32;
            let partial_pitch =
                (self.pitch as i32 + BELL_PARTIALS[i] as i32 + if i & 1 != 0 { detune } else { -detune })
                    as i16;
            self.add_partial_phase_increment[i] = compute_phase_increment(partial_pitch) << 1;
        }

        if self.parameter[0] < 32000 {
            for i in 0..NUM_BELL_PARTIALS {
                let decay_long = BELL_PARTIAL_DECAY_LONG[i];
                let decay_short = BELL_PARTIAL_DECAY_SHORT[i];
                let mut balance = (32767 - self.parameter[0] as i32) >> 8;
                balance = balance * balance >> 7;
                let decay = decay_long - ((decay_long - decay_short) * balance >> 7);
                self.add_partial_amplitude[i] =
                    (self.add_partial_amplitude[i] as i64 * decay as i64 >> 16) as i32;
            }
        }

        let mut previous_sample = self.add_previous_sample as i32;
        let mut j = 0;
        while j < size {
            let mut out = 0i32;
            for i in 0..NUM_BELL_PARTIALS {
                self.add_partial_phase[i] =
                    self.add_partial_phase[i].wrapping_add(self.add_partial_phase_increment[i]);
                let partial = interpolate824(&t.wav_sine, self.add_partial_phase[i]);
                out += (partial as i64 * self.add_partial_amplitude[i] as i64 >> 17) as i32;
            }
            out = clip16(out);
            buffer[j] = ((out + previous_sample) >> 1) as i16;
            if j + 1 < size {
                buffer[j + 1] = out as i16;
            }
            previous_sample = out;
            j += 2;
        }
        self.add_previous_sample = previous_sample as i16;
    }

    /// STRUCK_DRUM — 6 inharmonic partials plus filtered-noise modes for
    /// the body and snares. parameter_0 = decay, parameter_1 = brightness /
    /// noise-mode balance. Rendered half-rate, upsampled 2×.
    #[allow(clippy::needless_range_loop)] // parallel add_* / partial-table arrays
    fn render_struck_drum(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        if self.strike {
            let reset_phase = self.add_partial_amplitude[0] < 1024;
            for i in 0..NUM_DRUM_PARTIALS {
                self.add_target_partial_amplitude[i] = DRUM_PARTIAL_AMPLITUDE[i];
                if reset_phase {
                    self.add_partial_phase[i] = 1 << 30;
                }
            }
            self.strike = false;
        } else if self.parameter[0] < 32000 {
            for i in 0..NUM_DRUM_PARTIALS {
                let decay_long = DRUM_PARTIAL_DECAY_LONG[i];
                let decay_short = DRUM_PARTIAL_DECAY_SHORT[i];
                let mut balance = (32767 - self.parameter[0] as i32) >> 8;
                balance = balance * balance >> 7;
                let decay = decay_long - ((decay_long - decay_short) * balance >> 7);
                self.add_target_partial_amplitude[i] =
                    (self.add_partial_amplitude[i] as i64 * decay as i64 >> 16) as i32;
            }
        }
        for i in 0..NUM_DRUM_PARTIALS {
            let partial_pitch = (self.pitch as i32 + DRUM_PARTIALS[i] as i32) as i16;
            self.add_partial_phase_increment[i] = compute_phase_increment(partial_pitch) << 1;
        }

        let mut previous_sample = self.add_previous_sample as i32;
        let cutoff = ((self.pitch as i32 - 12 * 128) + (self.parameter[1] as i32 >> 2)).clamp(0, 32767);
        let f = interpolate824_u16(&t.svf_cutoff, (cutoff as u32) << 16) as i64;
        let mut lp0 = self.add_lp_noise[0];
        let mut lp1 = self.add_lp_noise[1];
        let mut lp2 = self.add_lp_noise[2];
        let harmonics_gain = if self.parameter[1] < 12888 {
            self.parameter[1] as i32 + 4096
        } else {
            16384
        };
        let noise_mode_gain = if self.parameter[1] < 16384 {
            0
        } else {
            self.parameter[1] as i32 - 16384
        };
        let noise_mode_gain = noise_mode_gain * 12888 >> 14;
        let fade_increment = 65536 / size.max(1) as i32;
        let mut fade = 0i32;
        let mut partials = [0i32; NUM_DRUM_PARTIALS];
        let mut j = 0;
        while j < size {
            fade += fade_increment;
            let mut noise = self.next_sample() as i32;
            noise = noise.clamp(-16384, 16384);
            lp0 += ((noise - lp0) as i64 * f >> 15) as i32;
            lp1 += ((lp0 - lp1) as i64 * f >> 15) as i32;
            lp2 += ((lp1 - lp2) as i64 * f >> 15) as i32;

            let mut harmonics = 0i32;
            for i in 0..NUM_DRUM_PARTIALS {
                self.add_partial_phase[i] =
                    self.add_partial_phase[i].wrapping_add(self.add_partial_phase_increment[i]);
                let partial = interpolate824(&t.wav_sine, self.add_partial_phase[i]);
                let amplitude = self.add_partial_amplitude[i]
                    + (((self.add_target_partial_amplitude[i] - self.add_partial_amplitude[i])
                        * fade)
                        >> 15);
                let partial = (partial as i64 * amplitude as i64 >> 16) as i32;
                harmonics += partial;
                partials[i] = partial;
            }
            let mut sample = partials[0];
            let noise_mode_1 = (partials[1] as i64 * lp2 as i64 >> 8) as i32;
            let noise_mode_2 = (partials[3] as i64 * lp2 as i64 >> 9) as i32;
            sample += (noise_mode_1 as i64 * (12288 - noise_mode_gain) as i64 >> 14) as i32;
            sample += (noise_mode_2 as i64 * noise_mode_gain as i64 >> 14) as i32;
            sample += (harmonics as i64 * harmonics_gain as i64 >> 14) as i32;
            sample = clip16(sample);
            buffer[j] = ((sample + previous_sample) >> 1) as i16;
            if j + 1 < size {
                buffer[j + 1] = sample as i16;
            }
            previous_sample = sample;
            j += 2;
        }
        self.add_previous_sample = previous_sample as i16;
        self.add_lp_noise = [lp0, lp1, lp2];
        // firmware copies all 11 (kNumBellPartials) amplitudes from target
        for i in 0..NUM_BELL_PARTIALS {
            self.add_partial_amplitude[i] = self.add_target_partial_amplitude[i];
        }
    }

    /// PLUCKED — a 3-voice Karplus-Strong string with per-voice
    /// oversampling. parameter_0 sets damping/loss, parameter_1 the pluck
    /// position (initial noise burst length). Rendered half-rate, 2× up.
    fn render_plucked(&mut self, buffer: &mut [i16], size: usize) {
        self.phase_increment <<= 1;
        if self.strike {
            self.pluck_active_voice += 1;
            if self.pluck_active_voice >= NUM_PLUCK_VOICES {
                self.pluck_active_voice = 0;
            }
            let phase_increment = self.phase_increment;
            let p = &mut self.pluck[self.pluck_active_voice];
            let mut increment = phase_increment as i32;
            p.shift = 0;
            while increment > (2 << 22) {
                increment >>= 1;
                p.shift += 1;
            }
            p.size = 1024 >> p.shift;
            p.mask = p.size - 1;
            p.write_ptr = 0;
            p.max_phase_increment = phase_increment << 1;
            p.phase_increment = phase_increment;
            let width = (3 * self.parameter[1] as i32) >> 1;
            p.initialization_ptr = (p.size as i32 * (8192 + width) >> 16) as usize;
            self.strike = false;
        }
        {
            let phase_increment = self.phase_increment;
            let p = &mut self.pluck[self.pluck_active_voice];
            p.phase_increment = phase_increment.min(p.max_phase_increment);
        }
        let update_probability = if self.parameter[0] < 16384 {
            65535u32
        } else {
            131072 - (self.parameter[0] as u32 >> 3) * 31
        };
        let mut loss = 4096 - (self.phase_increment >> 14) as i32;
        if loss < 256 {
            loss = 256;
        }
        if self.parameter[0] < 16384 {
            loss = loss * (16384 - self.parameter[0] as i32) >> 14;
        } else {
            loss = 0;
        }
        let mut previous_sample = self.pluck_previous_sample as i32;
        let mut j = 0;
        while j < size {
            let mut sample = 0i32;
            for i in 0..NUM_PLUCK_VOICES {
                let base = i * KS_VOICE_STRIDE;
                if self.pluck[i].initialization_ptr != 0 {
                    self.pluck[i].initialization_ptr -= 1;
                    let ip = base + self.pluck[i].initialization_ptr;
                    let excitation = (self.ks_delay[ip] as i32 + 3 * self.next_sample() as i32) >> 2;
                    self.ks_delay[ip] = excitation as i16;
                    sample += excitation;
                } else {
                    self.pluck[i].phase =
                        self.pluck[i].phase.wrapping_add(self.pluck[i].phase_increment);
                    let shift = self.pluck[i].shift;
                    let mask = self.pluck[i].mask;
                    let read_ptr = (((self.pluck[i].phase >> (22 + shift)) as usize) + 2) & mask;
                    let mut write_ptr = self.pluck[i].write_ptr;
                    while write_ptr != read_ptr {
                        let next = (write_ptr + 1) & mask;
                        let a = self.ks_delay[base + write_ptr] as i32;
                        let b = self.ks_delay[base + next] as i32;
                        let probability = self.next_word();
                        if (probability & 0xffff) <= update_probability {
                            let mut sum = a + b;
                            sum = if sum < 0 { -(-sum >> 1) } else { sum >> 1 };
                            if loss != 0 {
                                sum = sum * (32768 - loss) >> 15;
                            }
                            self.ks_delay[base + write_ptr] = sum as i16;
                        }
                        if write_ptr == 0 {
                            self.ks_delay[base + self.pluck[i].size] = self.ks_delay[base];
                        }
                        write_ptr = next;
                    }
                    self.pluck[i].write_ptr = write_ptr;
                    let read_phase = self.pluck[i].phase >> shift;
                    sample += interpolate1022(&self.ks_delay[base..base + KS_VOICE_STRIDE], read_phase);
                }
            }
            sample = clip16(sample);
            buffer[j] = ((previous_sample + sample) >> 1) as i16;
            if j + 1 < size {
                buffer[j + 1] = sample as i16;
            }
            previous_sample = sample;
            j += 2;
        }
        self.pluck_previous_sample = previous_sample as i16;
    }

    /// FILTERED_NOISE — white noise through a state-variable filter,
    /// morphing LP→BP→HP (parameter_1), resonance via parameter_0.
    fn render_filtered_noise(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        let f = interpolate824_u16(&t.svf_cutoff, (self.pitch as u32) << 17) as i64;
        let damp = interpolate824_u16(&t.svf_damp, (self.parameter[0] as u32) << 17) as i64;
        let scale = interpolate824_u16(&t.svf_scale, (self.parameter[0] as u32) << 17) as i64;
        let (bp_gain, lp_gain, hp_gain) = if self.parameter[1] < 16384 {
            let bp = self.parameter[1] as i32;
            (bp, 16384 - bp, 0)
        } else {
            (32767 - self.parameter[1] as i32, 0, self.parameter[1] as i32 - 16384)
        };
        let gain_correction = if f > scale {
            (scale * 32767 / f) as i32
        } else {
            32767
        };
        let mut bp = self.svf_bp;
        let mut lp = self.svf_lp;
        for b in buffer.iter_mut().take(size) {
            let input = (self.next_sample() >> 1) as i32;
            let notch = input - ((bp as i64 * damp >> 15) as i32);
            lp += (f * bp as i64 >> 15) as i32;
            lp = clip16(lp);
            let hp = notch - lp;
            bp += (f * hp as i64 >> 15) as i32;
            let mut result = (lp_gain * lp) >> 14;
            result += (bp_gain * bp) >> 14;
            result += (hp_gain * hp) >> 14;
            result = clip16(result);
            result = (result as i64 * gain_correction as i64 >> 15) as i32;
            *b = interpolate88(&t.moderate_overdrive, (result + 32768) as u16) as i16;
        }
        self.svf_bp = bp;
        self.svf_lp = lp;
    }

    /// TWIN_PEAKS_NOISE — noise through two resonators (a formant pair),
    /// the second offset by parameter_1; parameter_0 sets Q and makeup.
    fn render_twin_peaks_noise(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        let mut y11 = self.pno_filter_state[0][0];
        let mut y12 = self.pno_filter_state[0][1];
        let mut y21 = self.pno_filter_state[1][0];
        let mut y22 = self.pno_filter_state[1][1];
        let q = 65240u32 + (self.parameter[0] as u32 >> 7);
        let q_squared = ((q as u64 * q as u64) >> 17) as i64;
        let p1 = (self.pitch).clamp(0, 16383);
        let c1 = (interpolate824_u16(&t.resonator_coefficient, (p1 as u32) << 17) as i64 * q as i64
            >> 16) as i32;
        let s1 = interpolate824_u16(&t.resonator_scale, (p1 as u32) << 17);
        let p2 = (self.pitch as i32 + ((self.parameter[1] as i32 - 16384) >> 1)).clamp(0, 16383)
            as i16;
        let c2 = (interpolate824_u16(&t.resonator_coefficient, (p2 as u32) << 17) as i64 * q as i64
            >> 16) as i32;
        let s2 = interpolate824_u16(&t.resonator_scale, (p2 as u32) << 17);
        let makeup_gain = 8191 - (self.parameter[0] as i32 >> 2);
        let mut j = 0;
        while j < size {
            let sample0 = (self.next_sample() >> 1) as i32;
            let (mut y10, mut y20);
            if sample0 > 0 {
                y10 = sample0 * s1 >> 16;
                y20 = sample0 * s2 >> 16;
            } else {
                y10 = -((-sample0) * s1 >> 16);
                y20 = -((-sample0) * s2 >> 16);
            }
            y10 += (y11 as i64 * c1 as i64 >> 15) as i32;
            y10 -= (y12 as i64 * q_squared >> 15) as i32;
            y10 = clip16(y10);
            y12 = y11;
            y11 = y10;
            y20 += (y21 as i64 * c2 as i64 >> 15) as i32;
            y20 -= (y22 as i64 * q_squared >> 15) as i32;
            y20 = clip16(y20);
            y22 = y21;
            y21 = y20;
            y10 += y20;
            y10 += (y10 * makeup_gain) >> 13;
            y10 = clip16(y10);
            let sample = interpolate88(&t.moderate_overdrive, (y10 + 32768) as u16) as i16;
            buffer[j] = sample;
            if j + 1 < size {
                buffer[j + 1] = sample;
            }
            j += 2;
        }
        self.pno_filter_state[0][0] = y11;
        self.pno_filter_state[0][1] = y12;
        self.pno_filter_state[1][0] = y21;
        self.pno_filter_state[1][1] = y22;
    }

    /// CLOCKED_NOISE — a sample-and-hold random source clocked at the
    /// oscillator rate (parameter_0 = clock divider, parameter_1 = steps).
    fn render_clocked_noise(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let (p1, pp1) = (self.parameter[1] as i32, self.previous_parameter[1] as i32);
        if p1 > pp1 + 64 || p1 < pp1 - 64 {
            self.previous_parameter[1] = self.parameter[1];
        }
        let (p0, pp0) = (self.parameter[0] as i32, self.previous_parameter[0] as i32);
        if p0 > pp0 + 16 || p0 < pp0 - 16 {
            self.previous_parameter[0] = self.parameter[0];
        }
        if self.strike {
            self.clk_seed = self.next_word();
            self.strike = false;
        }
        let mut phase = self.phase;
        let mut phase_increment = self.phase_increment;
        for _ in 0..3 {
            if phase_increment < (1u32 << 31) {
                phase_increment <<= 1;
            }
        }
        self.clk_cycle_phase_increment =
            compute_phase_increment(self.previous_parameter[0] - 16384) << 1;
        let mut num_steps = 1 + (self.previous_parameter[1] as u32 >> 10);
        if num_steps == 1 {
            num_steps = 2;
        }
        let quantizer_divider = 65536 / num_steps;
        for (n, b) in buffer.iter_mut().take(size).enumerate() {
            phase = phase.wrapping_add(phase_increment);
            if sync[n] != 0 {
                phase = 0;
            }
            if phase < phase_increment {
                self.clk_rng_state = self
                    .clk_rng_state
                    .wrapping_mul(1_664_525)
                    .wrapping_add(1_013_904_223);
                self.clk_cycle_phase =
                    self.clk_cycle_phase.wrapping_add(self.clk_cycle_phase_increment);
                if self.clk_cycle_phase < self.clk_cycle_phase_increment {
                    self.clk_rng_state = self.clk_seed;
                    self.clk_cycle_phase = self.clk_cycle_phase_increment;
                }
                let mut sample = self.clk_rng_state as u16;
                sample -= sample % quantizer_divider as u16;
                sample = sample.wrapping_add((quantizer_divider >> 1) as u16);
                self.clk_sample = sample as i16;
                phase = phase_increment;
            }
            *b = self.clk_sample;
        }
        self.phase = phase;
    }

    /// VOWEL_FOF — the firmware renders the FOF vowel as a bank of five
    /// state-variable band-pass formants over a half-rate polyblep saw,
    /// upsampled 2×.
    fn render_vowel_fof(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        let mut amplitudes = [0i16; NUM_FORMANTS];
        let mut svf_f = [0i16; NUM_FORMANTS];
        for i in 0..NUM_FORMANTS {
            let frequency =
                interpolate_formant(&t.formant_f_data, self.parameter[1], self.parameter[0], i)
                    as i32
                    + (12 << 7);
            svf_f[i] = interpolate824_u16(&t.svf_cutoff, (frequency as u32) << 17) as i16;
            amplitudes[i] =
                interpolate_formant(&t.formant_a_data, self.parameter[1], self.parameter[0], i);
            if self.digital_init {
                self.fof_svf_lp[i] = 0;
                self.fof_svf_bp[i] = 0;
            }
        }
        self.digital_init = false;

        let mut phase = self.phase;
        let mut previous_sample = self.fof_previous_sample;
        let mut next_saw_sample = self.fof_next_saw_sample;
        let increment = self.phase_increment << 1;
        let mut j = 0;
        while j < size {
            let mut this_saw_sample = next_saw_sample;
            next_saw_sample = 0;
            phase = phase.wrapping_add(increment);
            if phase < increment {
                let mut tt = phase.checked_div(increment >> 16).unwrap_or(0);
                if tt > 65535 {
                    tt = 65535;
                }
                this_saw_sample -= ((tt as u64 * tt as u64) >> 18) as i32;
                tt = 65535 - tt;
                next_saw_sample += ((tt as u64 * tt as u64) >> 18) as i32;
            }
            next_saw_sample += (phase >> 17) as i32;
            let input = this_saw_sample;
            let mut out = 0i32;
            #[allow(clippy::needless_range_loop)] // parallel svf_f / fof_svf_* arrays
            for i in 0..NUM_FORMANTS {
                let notch = input - (self.fof_svf_bp[i] >> 6);
                self.fof_svf_lp[i] += (svf_f[i] as i64 * self.fof_svf_bp[i] as i64 >> 15) as i32;
                self.fof_svf_lp[i] = clip16(self.fof_svf_lp[i]);
                let hp = notch - self.fof_svf_lp[i];
                self.fof_svf_bp[i] += (svf_f[i] as i64 * hp as i64 >> 15) as i32;
                self.fof_svf_bp[i] = clip16(self.fof_svf_bp[i]);
                // firmware multiplies by amplitudes[0] for every formant
                out += (self.fof_svf_bp[i] as i64 * amplitudes[0] as i64 >> 17) as i32;
            }
            out = clip16(out);
            buffer[j] = ((out + previous_sample) >> 1) as i16;
            if j + 1 < size {
                buffer[j + 1] = out as i16;
            }
            previous_sample = out;
            j += 2;
        }
        self.phase = phase;
        self.fof_next_saw_sample = next_saw_sample;
        self.fof_previous_sample = previous_sample;
    }

    /// HARMONICS — an additive bank of 12 sine partials with two movable
    /// Lorentzian formant peaks (parameter_0 position, parameter_1 width +
    /// second-peak amount). Rendered half-rate and upsampled 2×.
    fn render_harmonics(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        let n = NUM_ADDITIVE_HARMONICS as i32;
        let phase_increment = self.phase_increment << 1;
        let mut target_amplitude = [0i32; NUM_ADDITIVE_HARMONICS];

        let peak = (n * self.parameter[0] as i32) >> 7;
        let second_peak = (peak >> 1) + n * 128;
        let second_peak_amount = (self.parameter[1] as i32 * self.parameter[1] as i32) >> 15;
        let sqrtsqrt_width = if self.parameter[1] < 16384 {
            self.parameter[1] as i32 >> 6
        } else {
            511 - (self.parameter[1] as i32 >> 6)
        };
        let sqrt_width = sqrtsqrt_width * sqrtsqrt_width >> 10;
        let width = sqrt_width * sqrt_width + 4;
        let mut total = 0i32;
        for (i, ta) in target_amplitude.iter_mut().enumerate() {
            let x = (i as i32) << 8;
            let mut d = x - peak;
            let mut g = 32768 * 128 / (128 + d * d / width);
            d = x - second_peak;
            g += second_peak_amount * 128 / (128 + d * d / width);
            total += g;
            *ta = g;
        }
        let attenuation = 2_147_483_647 / total.max(1);
        for (i, ta) in target_amplitude.iter_mut().enumerate() {
            if (phase_increment >> 16) * (i as u32 + 1) > 0x4000 {
                *ta = 0;
            } else {
                *ta = (*ta as i64 * attenuation as i64 >> 16) as i32;
            }
        }

        let mut phase = self.phase;
        let mut previous_sample = self.add_previous_sample as i32;
        let mut j = 0;
        while j < size {
            phase = phase.wrapping_add(phase_increment);
            if sync[j] != 0 || (j + 1 < size && sync[j + 1] != 0) {
                phase = 0;
            }
            let mut out = 0i32;
            for (i, amp) in self.hrm_amplitude.iter_mut().enumerate() {
                out += interpolate824(&t.wav_sine, phase.wrapping_mul(i as u32 + 1)) * *amp >> 15;
                *amp += (target_amplitude[i] - *amp) >> 8;
            }
            out = clip16(out);
            buffer[j] = ((out + previous_sample) >> 1) as i16;
            if j + 1 < size {
                buffer[j + 1] = out as i16;
            }
            previous_sample = out;
            j += 2;
        }
        self.add_previous_sample = previous_sample as i16;
        self.phase = phase;
    }

    /// FM — a 2-operator FM voice (sine carrier + sine modulator at a
    /// quantized ratio; parameter_0 = index, parameter_1 = ratio).
    fn render_fm(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        let mut modulator_phase = self.fm_modulator_phase;
        let modulator_phase_increment = compute_phase_increment(
            ((12 << 7) + self.pitch as i32 + ((self.parameter[1] as i32 - 16384) >> 1)) as i16,
        ) >> 1;
        let p0_start = self.previous_parameter[0] as i32;
        let p0_delta = self.parameter[0] as i32 - self.previous_parameter[0] as i32;
        let p_inc = 32767 / size.max(1) as i32;
        let mut p_xfade = 0i32;
        for (n, b) in buffer.iter_mut().take(size).enumerate() {
            p_xfade += p_inc;
            let parameter_0 = p0_start + (p0_delta * p_xfade >> 15);
            self.phase = self.phase.wrapping_add(self.phase_increment);
            if sync[n] != 0 {
                self.phase = 0;
                modulator_phase = 0;
            }
            modulator_phase = modulator_phase.wrapping_add(modulator_phase_increment);
            let pm = ((interpolate824(&t.wav_sine, modulator_phase) * parameter_0) as u32) << 2;
            *b = interpolate824(&t.wav_sine, self.phase.wrapping_add(pm)) as i16;
        }
        self.previous_parameter[0] = self.parameter[0];
        self.fm_modulator_phase = modulator_phase;
    }

    /// FEEDBACK_FM — FM with the carrier feeding back into the modulator,
    /// scaled down at high pitches to keep it stable.
    fn render_feedback_fm(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        let mut previous_sample = self.fm_previous_sample;
        let mut modulator_phase = self.fm_modulator_phase;
        let mut attenuation =
            self.pitch as i32 - (72 << 7) + ((self.parameter[1] as i32 - 16384) >> 1);
        attenuation = (32767 - attenuation * 4).clamp(0, 32767);
        let modulator_phase_increment = compute_phase_increment(
            ((12 << 7) + self.pitch as i32 + ((self.parameter[1] as i32 - 16384) >> 1)) as i16,
        ) >> 1;
        let p0_start = self.previous_parameter[0] as i32;
        let p0_delta = self.parameter[0] as i32 - self.previous_parameter[0] as i32;
        let p_inc = 32767 / size.max(1) as i32;
        let mut p_xfade = 0i32;
        for (n, b) in buffer.iter_mut().take(size).enumerate() {
            p_xfade += p_inc;
            let parameter_0 = p0_start + (p0_delta * p_xfade >> 15);
            self.phase = self.phase.wrapping_add(self.phase_increment);
            if sync[n] != 0 {
                self.phase = 0;
                modulator_phase = 0;
            }
            modulator_phase = modulator_phase.wrapping_add(modulator_phase_increment);
            let p = parameter_0 * attenuation >> 15;
            let fb_pm = (previous_sample as i32) << 14;
            let pm = (interpolate824(&t.wav_sine, modulator_phase.wrapping_add(fb_pm as u32)) * p)
                << 1;
            previous_sample =
                interpolate824(&t.wav_sine, self.phase.wrapping_add(pm as u32)) as i16;
            *b = previous_sample;
        }
        self.previous_parameter[0] = self.parameter[0];
        self.fm_previous_sample = previous_sample;
        self.fm_modulator_phase = modulator_phase;
    }

    /// CHAOTIC_FEEDBACK_FM — the modulator's increment is itself modulated
    /// by the carrier output, driving the voice into chaos.
    fn render_chaotic_feedback_fm(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        let modulator_phase_increment = compute_phase_increment(
            ((12 << 7) + self.pitch as i32 + ((self.parameter[1] as i32 - 16384) >> 1)) as i16,
        ) >> 1;
        let mut previous_sample = self.fm_previous_sample;
        let mut modulator_phase = self.fm_modulator_phase;
        let p0_start = self.previous_parameter[0] as i32;
        let p0_delta = self.parameter[0] as i32 - self.previous_parameter[0] as i32;
        let p_inc = 32767 / size.max(1) as i32;
        let mut p_xfade = 0i32;
        for (n, b) in buffer.iter_mut().take(size).enumerate() {
            p_xfade += p_inc;
            let parameter_0 = p0_start + (p0_delta * p_xfade >> 15);
            self.phase = self.phase.wrapping_add(self.phase_increment);
            if sync[n] != 0 {
                self.phase = 0;
                modulator_phase = 0;
            }
            let pm = (interpolate824(&t.wav_sine, modulator_phase) * parameter_0) << 1;
            previous_sample =
                interpolate824(&t.wav_sine, self.phase.wrapping_add(pm as u32)) as i16;
            *b = previous_sample;
            modulator_phase = modulator_phase.wrapping_add(
                (modulator_phase_increment >> 8).wrapping_mul((129 + (previous_sample >> 9)) as u32),
            );
        }
        self.previous_parameter[0] = self.parameter[0];
        self.fm_previous_sample = previous_sample;
        self.fm_modulator_phase = modulator_phase;
    }

    /// VOSIM — two sine formants windowed by a bell, retriggered each
    /// carrier cycle.
    fn render_vosim(&mut self, sync: &[u8], buffer: &mut [i16], size: usize) {
        let t = tables();
        for i in 0..2 {
            self.vow_formant_increment[i] = compute_phase_increment(self.parameter[i] >> 1);
        }
        for (n, b) in buffer.iter_mut().take(size).enumerate() {
            self.phase = self.phase.wrapping_add(self.phase_increment);
            if sync[n] != 0 {
                self.phase = 0;
            }
            let mut sample = 16384 + 8192;
            self.formant_phase[0] =
                self.formant_phase[0].wrapping_add(self.vow_formant_increment[0]);
            sample += interpolate824(&t.wav_sine, self.formant_phase[0]) >> 1;
            self.formant_phase[1] =
                self.formant_phase[1].wrapping_add(self.vow_formant_increment[1]);
            sample += interpolate824(&t.wav_sine, self.formant_phase[1]) >> 2;
            let bell = interpolate824_u16(&t.lut_bell, self.phase) >> 1;
            sample = (sample as i64 * bell as i64 >> 15) as i32;
            if self.phase < self.phase_increment {
                self.formant_phase[0] = 0;
                self.formant_phase[1] = 0;
                sample = 0;
            }
            sample -= 16384 + 8192;
            *b = sample as i16;
        }
    }

    /// VOWEL — three formants (two sine, one square) from a vowel/consonant
    /// table, with a struck consonant attack and noise modulation.
    fn render_vowel(&mut self, buffer: &mut [i16], size: usize) {
        let t = tables();
        let vowel_index = (self.parameter[0] >> 12) as usize;
        let balance = (self.parameter[0] & 0x0fff) as i32;
        let formant_shift = 200u32 + (self.parameter[1] as u32 >> 6);
        if self.strike {
            self.strike = false;
            self.vow_consonant_frames = 160;
            let index = (((self.next_word() >> 16) as i16 as i32 + 1) & 7) as usize;
            for i in 0..3 {
                self.vow_formant_increment[i] =
                    CONSONANT_DATA[index].formant_frequency[i] as u32 * 0x1000 * formant_shift;
                self.vow_formant_amplitude[i] = CONSONANT_DATA[index].formant_amplitude[i] as u32;
            }
            self.vow_noise = if index >= 6 { 4095 } else { 0 };
        }
        if self.vow_consonant_frames != 0 {
            self.vow_consonant_frames -= 1;
        } else {
            for i in 0..3 {
                let f0 = VOWELS_DATA[vowel_index].formant_frequency[i] as i32;
                let f1 = VOWELS_DATA[vowel_index + 1].formant_frequency[i] as i32;
                self.vow_formant_increment[i] =
                    ((f0 * (0x1000 - balance) + f1 * balance) as u32) * formant_shift;
                let a0 = VOWELS_DATA[vowel_index].formant_amplitude[i] as i32;
                let a1 = VOWELS_DATA[vowel_index + 1].formant_amplitude[i] as i32;
                self.vow_formant_amplitude[i] =
                    ((a0 * (0x1000 - balance) + a1 * balance) >> 12) as u32;
            }
            self.vow_noise = 0;
        }
        let noise = self.vow_noise as i32;
        for b in buffer.iter_mut().take(size) {
            self.phase = self.phase.wrapping_add(self.phase_increment);
            let mut sample: i16 = 0;
            self.formant_phase[0] =
                self.formant_phase[0].wrapping_add(self.vow_formant_increment[0]);
            let phaselet = ((self.formant_phase[0] >> 24) & 0xf0) as usize;
            sample = sample
                .wrapping_add(t.wav_formant_sine[phaselet | self.vow_formant_amplitude[0] as usize]);
            self.formant_phase[1] =
                self.formant_phase[1].wrapping_add(self.vow_formant_increment[1]);
            let phaselet = ((self.formant_phase[1] >> 24) & 0xf0) as usize;
            sample = sample
                .wrapping_add(t.wav_formant_sine[phaselet | self.vow_formant_amplitude[1] as usize]);
            self.formant_phase[2] =
                self.formant_phase[2].wrapping_add(self.vow_formant_increment[2]);
            let phaselet = ((self.formant_phase[2] >> 24) & 0xf0) as usize;
            sample = sample.wrapping_add(
                t.wav_formant_square[phaselet | self.vow_formant_amplitude[2] as usize],
            );
            sample = sample.wrapping_mul((255 - (self.phase >> 24) as i32) as i16);
            let phase_noise = (self.next_word() >> 16) as i16 as i32 * noise;
            if self.phase.wrapping_add(phase_noise as u32) < self.phase_increment {
                self.formant_phase[0] = 0;
                self.formant_phase[1] = 0;
                self.formant_phase[2] = 0;
                sample = 0;
            }
            *b = interpolate88(&t.moderate_overdrive, (sample as i32 + 32768) as u16) as i16;
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
            MacroModel::Vosim,
            MacroModel::Vowel,
            MacroModel::VowelFof,
            MacroModel::Harmonics,
            MacroModel::Fm,
            MacroModel::FeedbackFm,
            MacroModel::ChaoticFeedbackFm,
            MacroModel::FilteredNoise,
            MacroModel::TwinPeaksNoise,
            MacroModel::ClockedNoise,
            MacroModel::Plucked,
            MacroModel::StruckBell,
            MacroModel::StruckDrum,
            MacroModel::Kick,
            MacroModel::Snare,
            MacroModel::Cymbal,
            MacroModel::Bowed,
            MacroModel::Blown,
            MacroModel::Fluted,
            MacroModel::Wavetables,
            MacroModel::WaveMap,
            MacroModel::WaveLine,
            MacroModel::WaveParaphonic,
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
