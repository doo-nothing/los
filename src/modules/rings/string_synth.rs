//! # Rings string synth — the "Disastrous Peace" easter-egg mode
//!
//! Ported from rings/dsp/string_synth_*.{h,cc} and fx/{chorus,
//! ensemble}.h (MIT, copyright 2015 Emilie Gillet, attribution
//! preserved): 12 polyblep divide-down voices × 3 harmonics with the
//! organ registration table, AD envelopes (linear attack, quartic
//! decay, drone above 0.98), the original chord tables, and the six
//! effect sections (formant ×2, chorus, ensemble, reverb ×2).

use super::dsp::{semitones_to_ratio, Limiter, NoteFilter, Svf};
use super::part::Reverb;

pub const MAX_SS_POLYPHONY: usize = 4;
pub const STRING_SYNTH_VOICES: usize = 12;
pub const MAX_CHORD_SIZE: usize = 8;
pub const NUM_HARMONICS: usize = 3;
const NUM_FORMANTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FxType {
    Formant,
    Chorus,
    Reverb,
    Formant2,
    #[default]
    Ensemble,
    Reverb2,
}

impl FxType {
    pub const ALL: [FxType; 6] = [
        FxType::Formant,
        FxType::Chorus,
        FxType::Reverb,
        FxType::Formant2,
        FxType::Ensemble,
        FxType::Reverb2,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FxType::Formant => "formant",
            FxType::Chorus => "chorus",
            FxType::Reverb => "reverb",
            FxType::Formant2 => "formant2",
            FxType::Ensemble => "ensemble",
            FxType::Reverb2 => "reverb2",
        }
    }
}

// ── polyblep oscillator ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OscShape {
    BrightSquare,
    DarkSquare,
    /// In the firmware's shape set but unused by the voice wiring
    /// (kept for completeness of the oscillator law).
    #[allow(dead_code)]
    Triangle,
}

#[inline]
fn this_blep_sample(t: f32) -> f32 {
    0.5 * t * t
}

#[inline]
fn next_blep_sample(t: f32) -> f32 {
    let t = 1.0 - t;
    -0.5 * t * t
}

/// string_synth_oscillator.h: a polyblep square+saw pair with a
/// per-shape integrator voicing.
#[derive(Debug, Clone)]
struct StringSynthOscillator {
    high: bool,
    phase: f32,
    phase_increment: f32,
    next_sample: f32,
    next_sample_saw: f32,
    filter_state: f32,
    gain: f32,
    gain_saw: f32,
}

impl StringSynthOscillator {
    fn new() -> Self {
        StringSynthOscillator {
            high: false,
            phase: 0.0,
            phase_increment: 0.01,
            next_sample: 0.0,
            next_sample_saw: 0.0,
            filter_state: 0.0,
            gain: 0.0,
            gain_saw: 0.0,
        }
    }

    fn render(
        &mut self,
        shape: OscShape,
        interpolate_pitch: bool,
        target_increment: f32,
        mut target_gain: f32,
        target_gain_saw: f32,
        out: &mut [f32],
    ) {
        if target_increment >= 0.17 {
            target_gain *= 1.0 - (target_increment - 0.17) * 12.5;
            if target_increment >= 0.25 {
                return;
            }
        }
        let size = out.len();
        let inc_inc = (target_increment - self.phase_increment) / size as f32;
        let gain_inc = (target_gain - self.gain) / size as f32;
        let gain_saw_inc = (target_gain_saw - self.gain_saw) / size as f32;
        let mut phase_increment_m = self.phase_increment;
        let mut gain_m = self.gain;
        let mut gain_saw_m = self.gain_saw;
        self.phase_increment = target_increment;
        self.gain = target_gain;
        self.gain_saw = target_gain_saw;

        let mut phase = self.phase;
        let mut next_sample = self.next_sample;
        let mut next_sample_saw = self.next_sample_saw;
        let mut filter_state = self.filter_state;
        let mut high = self.high;

        for o in out.iter_mut() {
            let mut this_sample = next_sample;
            let mut this_sample_saw = next_sample_saw;
            next_sample = 0.0;
            next_sample_saw = 0.0;

            phase_increment_m += inc_inc;
            let increment = if interpolate_pitch {
                phase_increment_m
            } else {
                target_increment
            };
            phase += increment;

            const PW: f32 = 0.5;
            if !high && phase >= PW {
                let t = (phase - PW) / increment;
                this_sample += this_blep_sample(t);
                next_sample += next_blep_sample(t);
                high = true;
            }
            if phase >= 1.0 {
                phase -= 1.0;
                let t = phase / increment;
                let a = this_blep_sample(t);
                let b = next_blep_sample(t);
                this_sample -= a;
                next_sample -= b;
                this_sample_saw -= a;
                next_sample_saw -= b;
                high = false;
            }
            next_sample += if phase < PW { 0.0 } else { 1.0 };
            next_sample_saw += phase;

            let sample = match shape {
                OscShape::Triangle => {
                    let integrator_coefficient = increment * 0.125;
                    let s = 64.0 * (this_sample - 0.5);
                    filter_state += integrator_coefficient * (s - filter_state);
                    filter_state
                }
                OscShape::DarkSquare => {
                    let integrator_coefficient = increment * 2.0;
                    let s = 4.0 * (this_sample - 0.5);
                    filter_state += integrator_coefficient * (s - filter_state);
                    filter_state
                }
                OscShape::BrightSquare => {
                    let integrator_coefficient = increment * 2.0;
                    let s = 2.0 * this_sample - 1.0;
                    filter_state += integrator_coefficient * (s - filter_state);
                    (s - filter_state) * 0.5
                }
            };
            let saw = 2.0 * this_sample_saw - 1.0;
            gain_m += gain_inc;
            gain_saw_m += gain_saw_inc;
            *o += sample * gain_m + saw * gain_saw_m;
        }

        self.phase = phase;
        self.next_sample = next_sample;
        self.next_sample_saw = next_sample_saw;
        self.filter_state = filter_state;
        self.high = high;
    }
}

/// string_synth_voice.h: harmonic 0 is a pitch-interpolated dark
/// square, the octaves above are bright squares.
#[derive(Debug, Clone)]
struct StringSynthVoice {
    oscillator: [StringSynthOscillator; NUM_HARMONICS],
}

impl StringSynthVoice {
    fn new() -> Self {
        StringSynthVoice {
            oscillator: [
                StringSynthOscillator::new(),
                StringSynthOscillator::new(),
                StringSynthOscillator::new(),
            ],
        }
    }

    fn render(
        &mut self,
        mut frequency: f32,
        amplitudes: &[f32],
        summed_harmonics: usize,
        out: &mut [f32],
    ) {
        self.oscillator[0].render(
            OscShape::DarkSquare,
            true,
            frequency,
            amplitudes[0],
            amplitudes[1],
            out,
        );
        let mut a = 2;
        for i in 1..summed_harmonics.min(NUM_HARMONICS) {
            frequency *= 2.0;
            self.oscillator[i].render(
                OscShape::BrightSquare,
                false,
                frequency,
                amplitudes[a],
                amplitudes[a + 1],
                out,
            );
            a += 2;
        }
    }
}

// ── AD envelope ────────────────────────────────────────────────────────────

pub const ENVELOPE_FLAG_RISING_EDGE: u8 = 1;
pub const ENVELOPE_FLAG_FALLING_EDGE: u8 = 2;
pub const ENVELOPE_FLAG_GATE: u8 = 4;

/// string_synth_envelope.h with the set_ad config (linear attack,
/// quartic decay, no sustain).
#[derive(Debug, Clone)]
struct StringSynthEnvelope {
    attack_rate: f32,
    decay_rate: f32,
    segment: usize,
    phase: f32,
    start_value: f32,
    value: f32,
}

impl StringSynthEnvelope {
    fn new() -> Self {
        StringSynthEnvelope {
            attack_rate: 0.1,
            decay_rate: 0.001,
            segment: 2,
            phase: 0.0,
            start_value: 0.0,
            value: 0.0,
        }
    }

    fn set_ad(&mut self, attack: f32, decay: f32) {
        self.attack_rate = attack;
        self.decay_rate = decay;
    }

    fn process(&mut self, flags: u8) -> f32 {
        const LEVELS: [f32; 3] = [0.0, 1.0, 0.0];
        const NUM_SEGMENTS: usize = 2;
        if flags & ENVELOPE_FLAG_RISING_EDGE != 0 {
            self.start_value = if self.segment == NUM_SEGMENTS {
                LEVELS[0]
            } else {
                self.value
            };
            self.segment = 0;
            self.phase = 0.0;
        } else if self.phase >= 1.0 {
            self.start_value = LEVELS[(self.segment + 1).min(2)];
            self.segment += 1;
            self.phase = 0.0;
        }
        let done = self.segment >= NUM_SEGMENTS;
        let phase_increment = if done {
            0.0
        } else if self.segment == 0 {
            self.attack_rate
        } else {
            self.decay_rate
        };
        let mut t = self.phase;
        // segment 1 (decay) is quartic
        if self.segment == 1 {
            t = 1.0 - t;
            t *= t;
            t *= t;
            t = 1.0 - t;
        }
        self.phase += phase_increment;
        let target = LEVELS[(self.segment + 1).min(2)];
        self.value = self.start_value + (target - self.start_value) * t;
        self.value
    }
}

// ── chorus / ensemble ──────────────────────────────────────────────────────

/// fx/chorus.h: one 2047-sample line, two sine LFO tap pairs. LFO
/// phase increments are per-sample at 48 kHz — scaled to ours.
struct Chorus {
    line: Vec<f32>,
    pos: usize,
    phase_1: f32,
    phase_2: f32,
    pub amount: f32,
    depth: f32,
    rate_scale: f32,
}

impl Chorus {
    fn new(sample_rate: f32) -> Self {
        let s = sample_rate / 48_000.0;
        Chorus {
            line: vec![0.0; ((2048.0 * s) as usize).max(16)],
            pos: 0,
            phase_1: 0.0,
            phase_2: 0.0,
            amount: 0.0,
            depth: 0.0,
            rate_scale: s,
        }
    }

    fn set_depth(&mut self, depth: f32) {
        self.depth = depth * 384.0;
    }

    #[inline]
    fn read_at(&self, offset: f32) -> f32 {
        let n = self.line.len();
        let offset = offset.clamp(1.0, (n - 2) as f32);
        let int = offset as usize;
        let frac = offset - int as f32;
        let a = self.line[(self.pos + n - 1 - int) % n];
        let b = self.line[(self.pos + n - 2 - int) % n];
        a + (b - a) * frac
    }

    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let s = self.rate_scale;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let dry_amount = 1.0 - self.amount * 0.5;
            self.phase_1 += 4.17e-6 / s;
            if self.phase_1 >= 1.0 {
                self.phase_1 -= 1.0;
            }
            self.phase_2 += 5.417e-6 / s;
            if self.phase_2 >= 1.0 {
                self.phase_2 -= 1.0;
            }
            let sin_1 = (self.phase_1 * std::f32::consts::TAU).sin();
            let cos_1 = (self.phase_1 * std::f32::consts::TAU).cos();
            let sin_2 = (self.phase_2 * std::f32::consts::TAU).sin();
            let cos_2 = (self.phase_2 * std::f32::consts::TAU).cos();

            let written = (*l + *r) * 0.5;
            self.line[self.pos] = written;
            self.pos = (self.pos + 1) % self.line.len();

            let wet = self.read_at((sin_1 * self.depth + 1200.0) * s) * 0.5
                + self.read_at((sin_2 * self.depth + 800.0) * s) * 0.5;
            *l = wet * self.amount + *l * dry_amount;
            let wet = self.read_at((cos_1 * self.depth + 800.0) * s) * 0.5
                + self.read_at((cos_2 * self.depth + 1200.0) * s) * 0.5;
            *r = wet * self.amount + *r * dry_amount;
        }
    }
}

/// fx/ensemble.h: two lines, 3-phase slow+fast sine modulation.
struct Ensemble {
    line_l: Vec<f32>,
    line_r: Vec<f32>,
    pos: usize,
    phase_1: f32,
    phase_2: f32,
    pub amount: f32,
    depth: f32,
    rate_scale: f32,
}

impl Ensemble {
    fn new(sample_rate: f32) -> Self {
        let s = sample_rate / 48_000.0;
        let n = ((2048.0 * s) as usize).max(16);
        Ensemble {
            line_l: vec![0.0; n],
            line_r: vec![0.0; n],
            pos: 0,
            phase_1: 0.0,
            phase_2: 0.0,
            amount: 0.0,
            depth: 0.0,
            rate_scale: s,
        }
    }

    fn set_depth(&mut self, depth: f32) {
        self.depth = depth * 128.0;
    }

    #[inline]
    fn read_line(line: &[f32], pos: usize, offset: f32) -> f32 {
        let n = line.len();
        let offset = offset.clamp(1.0, (n - 2) as f32);
        let int = offset as usize;
        let frac = offset - int as f32;
        let a = line[(pos + n - 1 - int) % n];
        let b = line[(pos + n - 2 - int) % n];
        a + (b - a) * frac
    }

    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let s = self.rate_scale;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let dry_amount = 1.0 - self.amount * 0.5;
            self.phase_1 += 1.57e-5 / s;
            if self.phase_1 >= 1.0 {
                self.phase_1 -= 1.0;
            }
            self.phase_2 += 1.37e-4 / s;
            if self.phase_2 >= 1.0 {
                self.phase_2 -= 1.0;
            }
            let tri = |p: f32| (p * std::f32::consts::TAU).sin();
            let slow_0 = tri(self.phase_1);
            let slow_120 = tri(self.phase_1 + 1.0 / 3.0);
            let slow_240 = tri(self.phase_1 + 2.0 / 3.0);
            let fast_0 = tri(self.phase_2);
            let fast_120 = tri(self.phase_2 + 1.0 / 3.0);
            let fast_240 = tri(self.phase_2 + 2.0 / 3.0);

            let a = self.depth;
            let b = self.depth * 0.1;
            let mod_1 = slow_0 * a + fast_0 * b;
            let mod_2 = slow_120 * a + fast_120 * b;
            let mod_3 = slow_240 * a + fast_240 * b;

            self.line_l[self.pos] = *l;
            self.line_r[self.pos] = *r;
            self.pos = (self.pos + 1) % self.line_l.len();

            let wet = Self::read_line(&self.line_l, self.pos, (mod_1 + 1024.0) * s) * 0.33
                + Self::read_line(&self.line_l, self.pos, (mod_2 + 1024.0) * s) * 0.33
                + Self::read_line(&self.line_r, self.pos, (mod_3 + 1024.0) * s) * 0.33;
            *l = wet * self.amount + *l * dry_amount;
            let wet = Self::read_line(&self.line_r, self.pos, (mod_1 + 1024.0) * s) * 0.33
                + Self::read_line(&self.line_r, self.pos, (mod_2 + 1024.0) * s) * 0.33
                + Self::read_line(&self.line_l, self.pos, (mod_3 + 1024.0) * s) * 0.33;
            *r = wet * self.amount + *r * dry_amount;
        }
    }
}

// ── registration / chords / formants ───────────────────────────────────────

const REGISTRATIONS: [[f32; NUM_HARMONICS * 2]; 11] = [
    [1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    [1.0, 1.0, 0.0, 0.0, 0.0, 0.0],
    [1.0, 0.0, 1.0, 0.0, 0.0, 0.0],
    [1.0, 0.1, 0.0, 0.0, 1.0, 0.0],
    [1.0, 0.5, 1.0, 0.0, 1.0, 0.0],
    [1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
    [0.0, 1.0, 1.0, 1.0, 1.0, 0.0],
    [0.0, 0.5, 1.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 1.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
];

// Original chord tables (non-BRYAN_CHORDS): poly 1 = 8 notes,
// 2 = 6, 3 = 4, 4 = 3 (chord_size = min(12/poly, 8)).
const SS_CHORDS_1: [[f32; 8]; 11] = [
    [-24.0, -12.0, 0.0, 0.01, 0.02, 11.99, 12.0, 24.0],
    [-24.0, -12.0, 0.0, 3.0, 7.0, 10.0, 19.0, 24.0],
    [-24.0, -12.0, 0.0, 3.0, 7.0, 12.0, 19.0, 24.0],
    [-24.0, -12.0, 0.0, 3.0, 7.0, 14.0, 19.0, 24.0],
    [-24.0, -12.0, 0.0, 3.0, 7.0, 17.0, 19.0, 24.0],
    [-24.0, -12.0, 0.0, 6.99, 7.0, 18.99, 19.0, 24.0],
    [-24.0, -12.0, 0.0, 4.0, 7.0, 17.0, 19.0, 24.0],
    [-24.0, -12.0, 0.0, 4.0, 7.0, 14.0, 19.0, 24.0],
    [-24.0, -12.0, 0.0, 4.0, 7.0, 12.0, 19.0, 24.0],
    [-24.0, -12.0, 0.0, 4.0, 7.0, 11.0, 19.0, 24.0],
    [-24.0, -12.0, 0.0, 5.0, 7.0, 12.0, 17.0, 24.0],
];
const SS_CHORDS_2: [[f32; 6]; 11] = [
    [-24.0, -12.0, 0.0, 0.01, 12.0, 12.01],
    [-24.0, -12.0, 0.0, 3.00, 7.0, 10.0],
    [-24.0, -12.0, 0.0, 3.00, 7.0, 12.0],
    [-24.0, -12.0, 0.0, 3.00, 7.0, 14.0],
    [-24.0, -12.0, 0.0, 3.00, 7.0, 17.0],
    [-24.0, -12.0, 0.0, 6.99, 12.0, 19.0],
    [-24.0, -12.0, 0.0, 4.00, 7.0, 17.0],
    [-24.0, -12.0, 0.0, 4.00, 7.0, 14.0],
    [-24.0, -12.0, 0.0, 4.00, 7.0, 12.0],
    [-24.0, -12.0, 0.0, 4.00, 7.0, 11.0],
    [-24.0, -12.0, 0.0, 5.00, 7.0, 12.0],
];
const SS_CHORDS_3: [[f32; 4]; 11] = [
    [-12.0, 0.0, 0.01, 12.0],
    [-12.0, 3.0, 7.0, 10.0],
    [-12.0, 3.0, 7.0, 12.0],
    [-12.0, 3.0, 7.0, 14.0],
    [-12.0, 3.0, 7.0, 17.0],
    [-12.0, 7.0, 12.0, 19.0],
    [-12.0, 4.0, 7.0, 17.0],
    [-12.0, 4.0, 7.0, 14.0],
    [-12.0, 4.0, 7.0, 12.0],
    [-12.0, 4.0, 7.0, 11.0],
    [-12.0, 5.0, 7.0, 12.0],
];
const SS_CHORDS_4: [[f32; 3]; 11] = [
    [0.0, 0.01, 12.0],
    [0.0, 3.0, 10.0],
    [0.0, 3.0, 7.0],
    [0.0, 3.0, 14.0],
    [0.0, 3.0, 17.0],
    [0.0, 7.0, 19.0],
    [0.0, 4.0, 17.0],
    [0.0, 4.0, 14.0],
    [0.0, 4.0, 7.0],
    [0.0, 4.0, 11.0],
    [0.0, 5.0, 7.0],
];

fn ss_chord_note(polyphony: usize, chord: usize, i: usize) -> f32 {
    let chord = chord.min(10);
    match polyphony {
        1 => SS_CHORDS_1[chord][i.min(7)],
        2 => SS_CHORDS_2[chord][i.min(5)],
        3 => SS_CHORDS_3[chord][i.min(3)],
        _ => SS_CHORDS_4[chord][i.min(2)],
    }
}

const FORMANTS: [[f32; NUM_FORMANTS]; 5] = [
    [700.0, 1100.0, 2400.0],
    [500.0, 1300.0, 1700.0],
    [400.0, 2000.0, 2500.0],
    [600.0, 800.0, 2400.0],
    [300.0, 900.0, 2200.0],
];

// ── the string synth part ──────────────────────────────────────────────────

pub struct StringSynthPart {
    active_group: usize,
    acquisition_delay: usize,
    polyphony: usize,
    pub fx_type: FxType,
    clear_fx: bool,

    voice: Vec<StringSynthVoice>,
    group_tonic: [f32; MAX_SS_POLYPHONY],
    group_chord: [usize; MAX_SS_POLYPHONY],
    envelope: Vec<StringSynthEnvelope>,
    formant_filter: [Svf; NUM_FORMANTS],

    note_filter: NoteFilter,
    limiter: Limiter,
    reverb: Reverb,
    chorus: Chorus,
    ensemble: Ensemble,

    filter_in: Vec<f32>,
    filter_out: Vec<f32>,

    sample_rate: f32,
    a3: f32,
    block: usize,
}

impl StringSynthPart {
    pub fn new(sample_rate: f32, block: usize) -> Self {
        let block = block.max(8);
        StringSynthPart {
            active_group: 0,
            acquisition_delay: 0,
            polyphony: 1,
            fx_type: FxType::Ensemble,
            clear_fx: false,
            voice: (0..STRING_SYNTH_VOICES).map(|_| StringSynthVoice::new()).collect(),
            group_tonic: [0.0; MAX_SS_POLYPHONY],
            group_chord: [0; MAX_SS_POLYPHONY],
            envelope: (0..MAX_SS_POLYPHONY).map(|_| StringSynthEnvelope::new()).collect(),
            formant_filter: [Svf::new(), Svf::new(), Svf::new()],
            note_filter: NoteFilter::new(sample_rate / block as f32),
            limiter: Limiter::new(),
            reverb: Reverb::new(sample_rate),
            chorus: Chorus::new(sample_rate),
            ensemble: Ensemble::new(sample_rate),
            filter_in: vec![0.0; block],
            filter_out: vec![0.0; block],
            sample_rate,
            a3: 440.0 / sample_rate,
            block,
        }
    }

    pub fn set_polyphony(&mut self, polyphony: usize) {
        let polyphony = polyphony.clamp(1, MAX_SS_POLYPHONY);
        let old = self.polyphony;
        self.polyphony = polyphony;
        for i in old..polyphony {
            self.group_tonic[i] = self.group_tonic[0] + i as f32 * 0.01;
        }
        if self.active_group >= polyphony {
            self.active_group = 0;
        }
    }

    pub fn set_fx(&mut self, fx: FxType) {
        // a buffer-sharing change in the firmware needs a clear; here
        // each fx owns its memory but the reverb tail still deserves
        // a flush when switching families
        if (fx as usize % 3) != (self.fx_type as usize % 3) {
            self.clear_fx = true;
        }
        self.fx_type = fx;
    }

    fn compute_registration(&self, gain: f32, registration: f32, amplitudes: &mut [f32]) {
        let registration = registration.clamp(0.0, 1.0) * (11.0 - 1.001);
        let integral = registration as usize;
        let fractional = registration - integral as f32;
        let mut total = 0.0;
        for i in 0..NUM_HARMONICS * 2 {
            let a = REGISTRATIONS[integral][i];
            let b = REGISTRATIONS[(integral + 1).min(10)][i];
            amplitudes[i] = a + (b - a) * fractional;
            total += amplitudes[i];
        }
        for a in amplitudes.iter_mut().take(NUM_HARMONICS * 2) {
            *a = gain * *a / total;
        }
    }

    fn process_envelopes(&mut self, shape: f32, flags: &[u8], values: &mut [f32]) {
        let decay = shape;
        let attack = if shape < 0.5 { 0.0 } else { (shape - 0.5) * 2.0 };
        let period = self.sample_rate / self.block as f32;
        let attack_time = semitones_to_ratio(attack * 96.0) * 0.005 * period;
        let decay_time = semitones_to_ratio(decay * 84.0) * 0.180 * period;
        let attack_rate = 1.0 / attack_time;
        let decay_rate = 1.0 / decay_time;
        for i in 0..self.polyphony {
            let drone = if shape < 0.98 {
                0.0
            } else {
                ((shape - 0.98) * 55.0).min(1.0)
            };
            self.envelope[i].set_ad(attack_rate, decay_rate);
            let value = self.envelope[i].process(flags[i]);
            values[i] = value + (1.0 - value) * drone;
        }
    }

    fn process_formant_filter(
        &mut self,
        vowel: f32,
        shift: f32,
        resonance: f32,
        out: &mut [f32],
        aux: &mut [f32],
    ) {
        let size = out.len();
        for i in 0..size {
            self.filter_in[i] = out[i] + aux[i];
        }
        out.fill(0.0);
        aux.fill(0.0);
        let vowel = vowel.clamp(0.0, 1.0) * (5.0 - 1.001);
        let integral = vowel as usize;
        let fractional = vowel - integral as f32;
        #[allow(clippy::needless_range_loop)] // i indexes two parallel
        // tables and a filter bank; a zip would obscure the math
        for i in 0..NUM_FORMANTS {
            let a = FORMANTS[integral][i];
            let b = FORMANTS[(integral + 1).min(4)][i];
            let f = (a + (b - a) * fractional) * shift;
            self.formant_filter[i].set_f_q(f / self.sample_rate, resonance);
            for j in 0..size {
                self.filter_out[j] = self.formant_filter[i].process_bp(self.filter_in[j]);
            }
            let pan = i as f32 * 0.3 + 0.2;
            for j in 0..size {
                out[j] += self.filter_out[j] * pan * 0.5;
                aux[j] += self.filter_out[j] * (1.0 - pan) * 0.5;
            }
        }
    }

    pub fn process(
        &mut self,
        performance_state: &super::part::PerformanceState,
        patch: &super::part::Patch,
        input: &[f32],
        out: &mut [f32],
        aux: &mut [f32],
    ) {
        let size = input.len().min(self.filter_in.len());
        let mut envelope_flags = [0_u8; MAX_SS_POLYPHONY];

        self.note_filter
            .process(performance_state.note, performance_state.strum);
        if performance_state.strum {
            self.group_tonic[self.active_group] = self.note_filter.stable_note();
            envelope_flags[self.active_group] = ENVELOPE_FLAG_FALLING_EDGE;
            self.active_group = (self.active_group + 1) % self.polyphony;
            envelope_flags[self.active_group] = ENVELOPE_FLAG_RISING_EDGE;
            self.acquisition_delay = 3;
        }
        if self.acquisition_delay > 0 {
            self.acquisition_delay -= 1;
        } else {
            self.group_tonic[self.active_group] = self.note_filter.note();
            self.group_chord[self.active_group] = performance_state.chord;
            envelope_flags[self.active_group] |= ENVELOPE_FLAG_GATE;
        }

        let mut envelope_values = [0.0_f32; MAX_SS_POLYPHONY];
        let flags = envelope_flags;
        self.process_envelopes(patch.damping, &flags, &mut envelope_values);

        out[..size].copy_from_slice(&input[..size]);
        aux[..size].copy_from_slice(&input[..size]);

        let chord_size = (STRING_SYNTH_VOICES / self.polyphony).min(MAX_CHORD_SIZE);
        #[allow(clippy::needless_range_loop)] // group strides the
        // voice array (group*chord_size + note) — not a plain iter
        for group in 0..self.polyphony {
            let mut harmonics = [0.0_f32; NUM_HARMONICS * 2];
            self.compute_registration(
                envelope_values[group] * 0.25,
                patch.brightness,
                &mut harmonics,
            );
            for chord_note in 0..chord_size {
                let n = ss_chord_note(self.polyphony, self.group_chord[group], chord_note);
                let note_amplitude = if (0.0..=17.0).contains(&n) { 1.0 } else { 0.7 };
                let note = self.group_tonic[group]
                    + performance_state.tonic
                    + performance_state.fm
                    + n;
                let mut amplitudes = [0.0_f32; NUM_HARMONICS * 2];
                for i in 0..NUM_HARMONICS * 2 {
                    amplitudes[i] = note_amplitude * harmonics[i];
                }
                // fold truncated harmonics
                let num_harmonics = if self.polyphony >= 2 && chord_note < 2 {
                    NUM_HARMONICS - 1
                } else {
                    NUM_HARMONICS
                };
                for i in num_harmonics..NUM_HARMONICS {
                    amplitudes[2 * (num_harmonics - 1)] += amplitudes[2 * i];
                    amplitudes[2 * (num_harmonics - 1) + 1] += amplitudes[2 * i + 1];
                }
                let frequency = semitones_to_ratio(note - 69.0) * self.a3;
                let dest: &mut [f32] = if (group + chord_note) & 1 == 1 {
                    &mut out[..size]
                } else {
                    &mut aux[..size]
                };
                self.voice[group * chord_size + chord_note].render(
                    frequency,
                    &amplitudes,
                    num_harmonics,
                    dest,
                );
            }
        }

        if self.clear_fx {
            self.reverb = Reverb::new(self.sample_rate);
            self.clear_fx = false;
        }
        match self.fx_type {
            FxType::Formant | FxType::Formant2 => {
                let (shift, resonance) = if self.fx_type == FxType::Formant {
                    (1.0, 25.0)
                } else {
                    (1.1, 10.0)
                };
                let (o, a) = (&mut out[..size], &mut aux[..size]);
                // borrow dance: formant uses internal scratch buffers
                let mut o2 = o.to_vec();
                let mut a2 = a.to_vec();
                self.process_formant_filter(patch.position, shift, resonance, &mut o2, &mut a2);
                o.copy_from_slice(&o2);
                a.copy_from_slice(&a2);
            }
            FxType::Chorus => {
                self.chorus.amount = patch.position;
                self.chorus.set_depth(0.15 + 0.5 * patch.position);
                self.chorus.process(&mut out[..size], &mut aux[..size]);
            }
            FxType::Ensemble => {
                self.ensemble.amount = patch.position * (2.0 - patch.position);
                self.ensemble
                    .set_depth(0.2 + 0.8 * patch.position * patch.position);
                self.ensemble.process(&mut out[..size], &mut aux[..size]);
            }
            FxType::Reverb | FxType::Reverb2 => {
                self.reverb.amount = patch.position * 0.5;
                self.reverb.diffusion = 0.625;
                self.reverb.time = if self.fx_type == FxType::Reverb {
                    0.5 + 0.49 * patch.position
                } else {
                    0.3 + 0.6 * patch.position
                };
                self.reverb.input_gain = 0.2;
                self.reverb.lp = if self.fx_type == FxType::Reverb { 0.3 } else { 0.6 };
                self.reverb.process(&mut out[..size], &mut aux[..size]);
            }
        }

        // prevent main-signal cancellation when EVEN sums with ODD
        for a in aux[..size].iter_mut() {
            *a = -*a;
        }
        let (o, a) = (&mut out[..size], &mut aux[..size]);
        self.limiter.process(o, a, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::rings::part::{Patch, PerformanceState};

    #[test]
    fn string_synth_sounds_with_every_fx_and_polyphony() {
        for fx in FxType::ALL {
            for polyphony in [1, 2, 3, 4] {
                let mut part = StringSynthPart::new(48_000.0, 64);
                part.set_polyphony(polyphony);
                part.set_fx(fx);
                let input = vec![0.0; 64];
                let mut out = vec![0.0; 64];
                let mut aux = vec![0.0; 64];
                let mut ps = PerformanceState {
                    strum: true,
                    internal_exciter: true,
                    note: 60.0,
                    ..Default::default()
                };
                let mut energy = 0.0;
                for _ in 0..50 {
                    part.process(&ps, &Patch::default(), &input, &mut out, &mut aux);
                    ps.strum = false;
                    energy += out.iter().chain(aux.iter()).map(|v| v * v).sum::<f32>();
                }
                assert!(
                    energy > 1e-6 && energy.is_finite(),
                    "{} poly {polyphony}: {energy}",
                    fx.label()
                );
            }
        }
    }

    #[test]
    fn registration_crossfades_and_normalizes() {
        let part = StringSynthPart::new(48_000.0, 64);
        let mut amps = [0.0_f32; 6];
        part.compute_registration(1.0, 0.0, &mut amps);
        assert!((amps[0] - 1.0).abs() < 1e-5, "pure 8' at 0: {amps:?}");
        part.compute_registration(1.0, 1.0, &mut amps);
        // the firmware's (N-1.001) scale stops a hair short of the
        // last row — 0.1% of the previous registration bleeds through
        assert!((amps[5] - 1.0).abs() < 2e-3, "top saw at 1: {amps:?}");
        part.compute_registration(2.0, 0.5, &mut amps);
        let total: f32 = amps.iter().sum();
        assert!((total - 2.0).abs() < 1e-4, "normalized to gain: {total}");
    }

    #[test]
    fn envelope_attacks_then_quartic_decays() {
        let mut e = StringSynthEnvelope::new();
        e.set_ad(0.5, 0.01);
        let mut v = e.process(ENVELOPE_FLAG_RISING_EDGE);
        for _ in 0..3 {
            v = e.process(0);
        }
        assert!(v > 0.9, "fast attack reaches the top: {v}");
        let mut last = v;
        let mut decayed = false;
        for _ in 0..200 {
            let v = e.process(0);
            if v < last {
                decayed = true;
            }
            last = v;
        }
        assert!(decayed && last < 0.5, "quartic decay falls: {last}");
    }

    #[test]
    fn chord_size_matches_polyphony_budget() {
        // 12 voices split: poly1 -> 8 (capped), 2 -> 6, 3 -> 4, 4 -> 3
        for (poly, expected) in [(1, 8), (2, 6), (3, 4), (4, 3)] {
            let size = (STRING_SYNTH_VOICES / poly).min(MAX_CHORD_SIZE);
            assert_eq!(size, expected, "poly {poly}");
        }
    }
}
