//! # Rings Part — the polyphonic resonator dispatcher
//!
//! Ported from rings/dsp/part.cc and fx/reverb.h (MIT, copyright
//! 2015 Emilie Gillet, attribution preserved): all six resonator
//! models (modal, sympathetic string, inharmonic string, FM voice,
//! quantized sympathetic chords, string-and-reverb), polyphony 1–4
//! with round-robin / ping-pattern voice allocation, the original
//! chord table, the strum-driven note filter, and the output
//! limiter with per-model gains.

use super::dsp::{semitones_to_ratio, Limiter, NoteFilter, Plucker, Rng, Svf};
use super::models::{FmVoice, Resonator, String, MAX_MODES};

pub const MAX_POLYPHONY: usize = 4;
pub const NUM_STRINGS: usize = MAX_POLYPHONY * 2;
/// Control block the engine renders in (the firmware's kMaxBlockSize
/// is 24 at 48 kHz; los hosts hand us 64-frame slots, which the
/// note-filter control rate accounts for).
pub const MAX_BLOCK: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResonatorModel {
    #[default]
    Modal,
    SympatheticString,
    InharmonicString,
    FmVoice,
    SympatheticStringQuantized,
    StringAndReverb,
}

impl ResonatorModel {
    pub const ALL: [ResonatorModel; 6] = [
        ResonatorModel::Modal,
        ResonatorModel::SympatheticString,
        ResonatorModel::InharmonicString,
        ResonatorModel::FmVoice,
        ResonatorModel::SympatheticStringQuantized,
        ResonatorModel::StringAndReverb,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ResonatorModel::Modal => "modal",
            ResonatorModel::SympatheticString => "sympathetic",
            ResonatorModel::InharmonicString => "string",
            ResonatorModel::FmVoice => "fm",
            ResonatorModel::SympatheticStringQuantized => "chords",
            ResonatorModel::StringAndReverb => "string+verb",
        }
    }

    fn gain(self) -> f32 {
        match self {
            ResonatorModel::Modal => 1.4,
            ResonatorModel::SympatheticString => 1.0,
            ResonatorModel::InharmonicString => 1.4,
            ResonatorModel::FmVoice => 0.7,
            ResonatorModel::SympatheticStringQuantized => 1.0,
            ResonatorModel::StringAndReverb => 1.4,
        }
    }

}

#[derive(Debug, Clone, Copy)]
pub struct Patch {
    pub structure: f32,
    pub brightness: f32,
    pub damping: f32,
    pub position: f32,
}

impl Default for Patch {
    fn default() -> Self {
        Patch {
            structure: 0.25,
            brightness: 0.5,
            damping: 0.5,
            position: 0.3,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PerformanceState {
    pub strum: bool,
    pub internal_exciter: bool,
    /// MIDI tonic of the resonator bank.
    pub tonic: f32,
    /// MIDI note (relative; tonic + note is the sounding pitch).
    pub note: f32,
    pub fm: f32,
    pub chord: usize,
}

// The original chord table (part.cc, non-BRYAN_CHORDS build), by
// polyphony class: 8-string, 4-string, and the two 2-string layouts.
const CHORDS_8: [[f32; 8]; 11] = [
    [-12.0, 0.0, 0.01, 0.02, 0.03, 11.98, 11.99, 12.0],
    [-12.0, 0.0, 3.0, 3.01, 7.0, 9.99, 10.0, 19.0],
    [-12.0, 0.0, 3.0, 3.01, 7.0, 11.99, 12.0, 19.0],
    [-12.0, 0.0, 3.0, 3.01, 7.0, 13.99, 14.0, 19.0],
    [-12.0, 0.0, 3.0, 3.01, 7.0, 16.99, 17.0, 19.0],
    [-12.0, 0.0, 6.98, 6.99, 7.0, 12.00, 18.99, 19.0],
    [-12.0, 0.0, 3.99, 4.0, 7.0, 16.99, 17.0, 19.0],
    [-12.0, 0.0, 3.99, 4.0, 7.0, 13.99, 14.0, 19.0],
    [-12.0, 0.0, 3.99, 4.0, 7.0, 11.99, 12.0, 19.0],
    [-12.0, 0.0, 3.99, 4.0, 7.0, 10.99, 11.0, 19.0],
    [-12.0, 0.0, 4.99, 5.0, 7.0, 11.99, 12.0, 17.0],
];
const CHORDS_4: [[f32; 4]; 11] = [
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
const CHORDS_2: [[f32; 2]; 11] = [
    [0.0, -12.0],
    [0.0, 0.01],
    [0.0, 2.0],
    [0.0, 3.0],
    [0.0, 4.0],
    [0.0, 5.0],
    [0.0, 7.0],
    [0.0, 10.0],
    [0.0, 11.0],
    [0.0, 12.0],
    [-12.0, 12.0],
];

fn chord_note(polyphony: usize, chord: usize, string: usize) -> f32 {
    let chord = chord.min(10);
    match polyphony {
        1 => CHORDS_8[chord][string.min(7)],
        2 => CHORDS_4[chord][string.min(3)],
        _ => CHORDS_2[chord][string.min(1)],
    }
}

/// part.h Squash: a steep smoothstep that pins the crossfade between
/// sympathetic-string tunings to the plateaus.
fn squash(x: f32) -> f32 {
    if x < 0.5 {
        let mut x = x * 2.0;
        x *= x;
        x *= x;
        x *= x;
        x *= x;
        x * 0.5
    } else {
        let mut x = 2.0 - 2.0 * x;
        x *= x;
        x *= x;
        x *= x;
        x *= x;
        1.0 - 0.5 * x
    }
}

// ── the rings space reverb (fx/reverb.h) ───────────────────────────────────

/// Same Griesinger family as the elements reverb, but with BOTH loop
/// taps modulated (del2 @ 6261±50 by a 0.3 Hz LFO, del1 @ 4460±40 by
/// 0.5 Hz) and no ap1 smear. Buffers are scaled from the firmware's
/// 48 kHz sizes; modulated reads clamp inside the line (the elements
/// underflow lesson, pinned there by test).
struct Ap {
    buf: Vec<f32>,
    pos: usize,
}

impl Ap {
    fn new(len: usize) -> Self {
        Ap {
            buf: vec![0.0; len.max(4)],
            pos: 0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32, k: f32) -> f32 {
        let tail = self.buf[self.pos];
        let w = x + tail * k;
        self.buf[self.pos] = w;
        self.pos = (self.pos + 1) % self.buf.len();
        tail - w * k
    }
}

struct Delay {
    buf: Vec<f32>,
    pos: usize,
}

impl Delay {
    fn new(len: usize) -> Self {
        Delay {
            buf: vec![0.0; len.max(8)],
            pos: 0,
        }
    }

    #[inline]
    fn write(&mut self, v: f32) {
        self.buf[self.pos] = v;
        self.pos = (self.pos + 1) % self.buf.len();
    }

    /// Interpolated modulated read, offset clamped inside the line.
    #[inline]
    fn read_mod(&self, offset: f32) -> f32 {
        let n = self.buf.len();
        let offset = offset.clamp(1.0, (n - 2) as f32);
        let int = offset as usize;
        let frac = offset - int as f32;
        let a = self.buf[(self.pos + n - 1 - int) % n];
        let b = self.buf[(self.pos + n - 2 - int) % n];
        a + (b - a) * frac
    }
}

pub struct Reverb {
    ap: [Ap; 4],
    dap1a: Ap,
    dap1b: Ap,
    del1: Delay,
    dap2a: Ap,
    dap2b: Ap,
    del2: Delay,
    lp1: f32,
    lp2: f32,
    lfo_phase: [f32; 2],
    lfo_inc: [f32; 2],
    rate_scale: f32,
    pub amount: f32,
    pub diffusion: f32,
    pub time: f32,
    pub input_gain: f32,
    pub lp: f32,
}

impl Reverb {
    pub fn new(sample_rate: f32) -> Self {
        let s = sample_rate / 48_000.0;
        let sz = |n: f32| ((n * s) as usize).max(8);
        Reverb {
            ap: [
                Ap::new(sz(150.0)),
                Ap::new(sz(214.0)),
                Ap::new(sz(319.0)),
                Ap::new(sz(527.0)),
            ],
            dap1a: Ap::new(sz(2182.0)),
            dap1b: Ap::new(sz(2690.0)),
            del1: Delay::new(sz(4501.0)),
            dap2a: Ap::new(sz(2525.0)),
            dap2b: Ap::new(sz(2197.0)),
            del2: Delay::new(sz(6312.0)),
            lp1: 0.0,
            lp2: 0.0,
            lfo_phase: [0.0, 0.0],
            lfo_inc: [0.5 / sample_rate, 0.3 / sample_rate],
            rate_scale: s,
            amount: 0.0,
            diffusion: 0.625,
            time: 0.35,
            input_gain: 0.2,
            lp: 0.7,
        }
    }

    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let kap = self.diffusion;
        let klp = self.lp;
        let krt = self.time;
        let amount = self.amount;
        let gain = self.input_gain;
        let s = self.rate_scale;
        for i in 0..left.len() {
            for (phase, inc) in self.lfo_phase.iter_mut().zip(self.lfo_inc.iter()) {
                *phase += inc;
                if *phase >= 1.0 {
                    *phase -= 1.0;
                }
            }
            let lfo1 = 0.5 - 0.5 * (self.lfo_phase[0] * std::f32::consts::TAU).cos();
            let lfo2 = 0.5 - 0.5 * (self.lfo_phase[1] * std::f32::consts::TAU).cos();

            let mut acc = (left[i] + right[i]) * gain;
            for ap in self.ap.iter_mut() {
                acc = ap.process(acc, kap);
            }
            let apout = acc;

            // branch 1: modulated del2 tail; write-then-scale (the
            // elements decay-test lesson)
            acc = apout + self.del2.read_mod((6261.0 + 50.0 * lfo2) * s) * krt;
            self.lp1 += klp * (acc - self.lp1);
            let mut b = self.lp1;
            b = self.dap1a.process(b, -kap);
            b = self.dap1b.process(b, kap);
            self.del1.write(b);
            let wet = b * 2.0;
            left[i] += (wet - left[i]) * amount;

            // branch 2: modulated del1 tail
            acc = apout + self.del1.read_mod((4460.0 + 40.0 * lfo1) * s) * krt;
            self.lp2 += klp * (acc - self.lp2);
            let mut b = self.lp2;
            b = self.dap2a.process(b, kap);
            b = self.dap2b.process(b, -kap);
            self.del2.write(b);
            let wet = b * 2.0;
            right[i] += (wet - right[i]) * amount;
        }
    }
}

// ── the part ───────────────────────────────────────────────────────────────

const PING_PATTERN: [usize; 8] = [1, 0, 2, 1, 0, 2, 1, 0];

pub struct Part {
    pub model: ResonatorModel,
    polyphony: usize,
    dirty: bool,

    active_voice: usize,
    step_counter: usize,
    note: [f32; MAX_POLYPHONY],

    resonator: Vec<Resonator>,
    string: Vec<String>,
    fm_voice: Vec<FmVoice>,
    lfo: Vec<super::dsp::CosineOscillator>,

    excitation_filter: Vec<Svf>,
    plucker: Vec<Plucker>,
    dc_blocker: Vec<super::dsp::DcBlocker>,

    note_filter: NoteFilter,
    reverb: Reverb,
    limiter: Limiter,

    resonator_input: Vec<f32>,
    sympathetic_input: Vec<f32>,
    noise_burst: Vec<f32>,
    out_buffer: Vec<f32>,
    aux_buffer: Vec<f32>,

    sample_rate: f32,
    a3: f32,
}

impl Part {
    pub fn new(sample_rate: f32, block: usize, seed: u32) -> Self {
        let block = block.max(8);
        let mut rng = Rng::new(seed ^ 0x9e37_79b9);
        let mut part = Part {
            model: ResonatorModel::Modal,
            polyphony: 1,
            dirty: true,
            active_voice: 0,
            step_counter: 0,
            note: [0.0; MAX_POLYPHONY],
            resonator: (0..MAX_POLYPHONY).map(|_| Resonator::new()).collect(),
            string: (0..NUM_STRINGS)
                .map(|_| String::new(false, sample_rate, rng.word()))
                .collect(),
            fm_voice: (0..MAX_POLYPHONY).map(|_| FmVoice::new(sample_rate)).collect(),
            lfo: (0..NUM_STRINGS)
                .map(|_| super::dsp::CosineOscillator::default())
                .collect(),
            excitation_filter: (0..MAX_POLYPHONY).map(|_| Svf::new()).collect(),
            plucker: (0..MAX_POLYPHONY).map(|_| Plucker::new(rng.word())).collect(),
            dc_blocker: (0..MAX_POLYPHONY)
                .map(|_| super::dsp::DcBlocker::new(1.0 - 10.0 / sample_rate))
                .collect(),
            note_filter: NoteFilter::new(sample_rate / block as f32),
            reverb: Reverb::new(sample_rate),
            limiter: Limiter::new(),
            resonator_input: vec![0.0; block],
            sympathetic_input: vec![0.0; block],
            noise_burst: vec![0.0; block],
            out_buffer: vec![0.0; block],
            aux_buffer: vec![0.0; block],
            sample_rate,
            a3: 440.0 / sample_rate,
        };
        part.configure_resonators();
        part
    }

    pub fn polyphony(&self) -> usize {
        self.polyphony
    }

    pub fn set_polyphony(&mut self, polyphony: usize) {
        let polyphony = polyphony.clamp(1, MAX_POLYPHONY);
        if polyphony == self.polyphony {
            return;
        }
        let old = self.polyphony;
        self.polyphony = polyphony;
        for i in old..polyphony {
            self.note[i] = self.note[0] + i as f32 * 0.05;
        }
        self.dirty = true;
    }

    pub fn set_model(&mut self, model: ResonatorModel) {
        if model != self.model {
            self.model = model;
            self.dirty = true;
        }
    }

    fn configure_resonators(&mut self) {
        if !self.dirty {
            return;
        }
        match self.model {
            ResonatorModel::Modal => {
                let resolution = (MAX_MODES / self.polyphony).saturating_sub(4);
                for r in self.resonator.iter_mut().take(self.polyphony) {
                    *r = Resonator::new();
                    r.set_resolution(resolution);
                }
            }
            ResonatorModel::SympatheticString
            | ResonatorModel::InharmonicString
            | ResonatorModel::SympatheticStringQuantized
            | ResonatorModel::StringAndReverb => {
                const LFO_FREQUENCIES: [f32; 7] = [0.5, 0.4, 0.35, 0.23, 0.211, 0.2, 0.171];
                let has_dispersion = matches!(
                    self.model,
                    ResonatorModel::InharmonicString | ResonatorModel::StringAndReverb
                );
                let block = self.out_buffer.len() as f32;
                let mut rng = Rng::new(0x5117_0001);
                for i in 0..NUM_STRINGS {
                    self.string[i] = String::new(has_dispersion, self.sample_rate, rng.word());
                    let f_lfo = block / self.sample_rate
                        * LFO_FREQUENCIES[i.min(LFO_FREQUENCIES.len() - 1)];
                    self.lfo[i].init_approximate(f_lfo);
                }
                for p in self.plucker.iter_mut().take(self.polyphony) {
                    *p = Plucker::new(rng.word());
                }
            }
            ResonatorModel::FmVoice => {
                for v in self.fm_voice.iter_mut().take(self.polyphony) {
                    *v = FmVoice::new(self.sample_rate);
                }
            }
        }
        if self.active_voice >= self.polyphony {
            self.active_voice = 0;
        }
        self.dirty = false;
    }

    /// part.cc ComputeSympatheticStringsNotes.
    fn sympathetic_notes(
        &self,
        tonic: f32,
        note: f32,
        mut parameter: f32,
        destination: &mut [f32],
        num_strings: usize,
    ) {
        let notes = [
            tonic,
            note - 12.0,
            note - 7.019_55,
            note,
            note + 7.019_55,
            note + 12.0,
            note + 19.019_55,
            note + 24.0,
            note + 24.0,
        ];
        const DETUNINGS: [f32; 4] = [0.013, 0.011, 0.007, 0.017];

        if parameter >= 2.0 {
            // quantized chords
            let chord_index = (parameter - 2.0) as usize;
            for (i, d) in destination.iter_mut().take(num_strings).enumerate() {
                *d = chord_note(self.polyphony, chord_index, i) + note;
            }
            return;
        }

        let num_detuned = (num_strings - 1) >> 1;
        let first_detuned = num_strings - num_detuned;

        for i in 0..first_detuned {
            let mut n = 3.0;
            if i != 0 {
                n = parameter * 7.0;
                parameter += (1.0 - parameter) * 0.2;
            }
            let integral = (n as usize).min(7);
            let fractional = squash(n - integral as f32);
            let a = notes[integral];
            let b = notes[integral + 1];
            let n = a + (b - a) * fractional;
            destination[i] = n;
            if i + first_detuned < num_strings {
                destination[i + first_detuned] = n + DETUNINGS[i & 3];
            }
        }
    }

    pub fn process(
        &mut self,
        performance_state: &PerformanceState,
        patch: &Patch,
        input: &[f32],
        out: &mut [f32],
        aux: &mut [f32],
    ) {
        let size = input.len().min(self.out_buffer.len());
        self.configure_resonators();

        self.note_filter
            .process(performance_state.note, performance_state.strum);

        if performance_state.strum {
            self.note[self.active_voice] = self.note_filter.stable_note();
            if self.polyphony > 1 && self.polyphony & 1 == 1 {
                self.active_voice = PING_PATTERN[self.step_counter % 8];
                self.step_counter = (self.step_counter + 1) % 8;
            } else {
                self.active_voice = (self.active_voice + 1) % self.polyphony;
            }
        }
        self.note[self.active_voice] = self.note_filter.note();

        out[..size].fill(0.0);
        aux[..size].fill(0.0);

        for voice in 0..self.polyphony {
            let cutoff = patch.brightness * (2.0 - patch.brightness);
            let note = self.note[voice] + performance_state.tonic + performance_state.fm;
            let frequency = semitones_to_ratio(note - 69.0) * self.a3;
            let filter_cutoff_range = if performance_state.internal_exciter {
                frequency * semitones_to_ratio((cutoff - 0.5) * 96.0)
            } else {
                0.4 * semitones_to_ratio((cutoff - 1.0) * 108.0)
            };
            let filter_cutoff = if voice == self.active_voice {
                filter_cutoff_range.min(0.499)
            } else {
                (10.0 / self.sample_rate).min(0.499)
            };
            let filter_q = if performance_state.internal_exciter {
                1.5
            } else {
                0.8
            };

            self.excitation_filter[voice].set_f_q(filter_cutoff, filter_q);
            if voice == self.active_voice {
                self.resonator_input[..size].copy_from_slice(&input[..size]);
            } else {
                self.resonator_input[..size].fill(0.0);
            }

            match self.model {
                ResonatorModel::Modal => self.render_modal_voice(
                    voice,
                    performance_state,
                    patch,
                    frequency,
                    filter_cutoff,
                    size,
                ),
                ResonatorModel::FmVoice => self.render_fm_voice(
                    voice,
                    performance_state,
                    patch,
                    frequency,
                    size,
                ),
                ResonatorModel::SympatheticString
                | ResonatorModel::InharmonicString
                | ResonatorModel::SympatheticStringQuantized
                | ResonatorModel::StringAndReverb => self.render_string_voice(
                    voice,
                    performance_state,
                    patch,
                    frequency,
                    filter_cutoff,
                    size,
                ),
            }

            if self.polyphony == 1 {
                for i in 0..size {
                    out[i] += self.out_buffer[i];
                    aux[i] += self.aux_buffer[i];
                }
            } else {
                // odd/even voices to individual outputs
                let dest: &mut [f32] = if voice & 1 == 1 { aux } else { out };
                for ((d, o), a) in dest[..size]
                    .iter_mut()
                    .zip(self.out_buffer[..size].iter())
                    .zip(self.aux_buffer[..size].iter())
                {
                    *d += o - a;
                }
            }
        }

        if self.model == ResonatorModel::StringAndReverb {
            for i in 0..size {
                let l = out[i];
                let r = aux[i];
                out[i] = l * patch.position + (1.0 - patch.position) * r;
                aux[i] = r * patch.position + (1.0 - patch.position) * l;
            }
            self.reverb.amount = 0.1 + patch.damping * 0.5;
            self.reverb.diffusion = 0.625;
            self.reverb.time = 0.35 + 0.63 * patch.damping;
            self.reverb.input_gain = 0.2;
            self.reverb.lp = 0.3 + patch.brightness * 0.6;
            self.reverb.process(&mut out[..size], &mut aux[..size]);
            for a in aux[..size].iter_mut() {
                *a = -*a;
            }
        }

        let gain = self.model.gain();
        let (o, a) = (&mut out[..size], &mut aux[..size]);
        self.limiter.process(o, a, gain);
    }

    fn render_modal_voice(
        &mut self,
        voice: usize,
        performance_state: &PerformanceState,
        patch: &Patch,
        frequency: f32,
        filter_cutoff: f32,
        size: usize,
    ) {
        // internal exciter: a pulse, pre-filter
        if performance_state.internal_exciter
            && voice == self.active_voice
            && performance_state.strum
        {
            self.resonator_input[0] +=
                0.25 * semitones_to_ratio(filter_cutoff * filter_cutoff * 24.0) / filter_cutoff;
        }
        for v in self.resonator_input[..size].iter_mut() {
            *v = self.excitation_filter[voice].process_lp(*v);
        }
        let r = &mut self.resonator[voice];
        r.frequency = frequency;
        r.structure = patch.structure;
        r.brightness = patch.brightness * patch.brightness;
        r.position = patch.position;
        r.damping = patch.damping;
        r.process(
            &self.resonator_input[..size],
            &mut self.out_buffer[..size],
            &mut self.aux_buffer[..size],
        );
    }

    fn render_fm_voice(
        &mut self,
        voice: usize,
        performance_state: &PerformanceState,
        patch: &Patch,
        frequency: f32,
        size: usize,
    ) {
        let v = &mut self.fm_voice[voice];
        if performance_state.internal_exciter
            && voice == self.active_voice
            && performance_state.strum
        {
            v.trigger_internal_envelope();
        }
        v.carrier_frequency = frequency;
        v.ratio = patch.structure;
        v.brightness = patch.brightness;
        v.feedback_amount = patch.position;
        v.damping = patch.damping;
        v.process(
            &self.resonator_input[..size],
            &mut self.out_buffer[..size],
            &mut self.aux_buffer[..size],
        );
    }

    fn render_string_voice(
        &mut self,
        voice: usize,
        performance_state: &PerformanceState,
        patch: &Patch,
        frequency: f32,
        filter_cutoff: f32,
        size: usize,
    ) {
        let mut num_strings = 1;
        let mut frequencies = [0.0_f32; NUM_STRINGS];

        if matches!(
            self.model,
            ResonatorModel::SympatheticString | ResonatorModel::SympatheticStringQuantized
        ) {
            num_strings = 2 * MAX_POLYPHONY / self.polyphony;
            let parameter = if self.model == ResonatorModel::SympatheticString {
                patch.structure
            } else {
                2.0 + performance_state.chord as f32
            };
            let mut dest = [0.0_f32; NUM_STRINGS];
            self.sympathetic_notes(
                performance_state.tonic + performance_state.fm,
                performance_state.tonic + self.note[voice] + performance_state.fm,
                parameter,
                &mut dest,
                num_strings,
            );
            for i in 0..num_strings {
                frequencies[i] = semitones_to_ratio(dest[i] - 69.0) * self.a3;
            }
        } else {
            frequencies[0] = frequency;
        }

        if voice == self.active_voice {
            let gain = 1.0 / ((num_strings as f32) * 2.0).sqrt();
            for v in self.resonator_input[..size].iter_mut() {
                *v *= gain;
            }
        }

        for v in self.resonator_input[..size].iter_mut() {
            *v = self.excitation_filter[voice].process_lp(*v);
        }

        if performance_state.internal_exciter {
            if voice == self.active_voice && performance_state.strum {
                self.plucker[voice].trigger(frequency, filter_cutoff * 8.0, patch.position);
            }
            self.plucker[voice].process(&mut self.noise_burst[..size]);
            for (r, n) in self.resonator_input[..size]
                .iter_mut()
                .zip(self.noise_burst[..size].iter())
            {
                *r += n;
            }
        }
        for v in self.resonator_input[..size].iter_mut() {
            *v = self.dc_blocker[voice].process(*v);
        }

        self.out_buffer[..size].fill(0.0);
        self.aux_buffer[..size].fill(0.0);

        let structure = patch.structure;
        let dispersion = if structure < 0.24 {
            (structure - 0.24) * 4.166
        } else if structure > 0.26 {
            (structure - 0.26) * 1.351_35
        } else {
            0.0
        };

        #[allow(clippy::needless_range_loop)] // `string` indexes three
        // parallel arrays plus a derived stride — a zip would obscure it
        for string in 0..num_strings {
            let i = voice + string * self.polyphony;
            let lfo_value = self.lfo[i].next();

            let mut brightness = patch.brightness;
            let mut damping = patch.damping;
            let mut position = patch.position;
            let mut glide = 1.0;
            let string_index = string as f32 / num_strings as f32;
            let mut use_sympathetic_input = false;

            if self.model == ResonatorModel::StringAndReverb {
                damping *= 2.0 - damping;
            }

            // string 0 is the main source under the internal exciter;
            // the rest ring by sympathetic resonance
            if string > 0 && performance_state.internal_exciter {
                brightness *= 2.0 - brightness;
                brightness *= 2.0 - brightness;
                damping = 0.7 + patch.damping * 0.27;
                let amount = (0.5 - (0.5 - patch.position).abs()) * 0.9;
                position = patch.position + lfo_value * amount;
                glide = semitones_to_ratio((brightness - 1.0) * 36.0);
                use_sympathetic_input = true;
            }

            let s = &mut self.string[i];
            s.dispersion = dispersion;
            s.glide_frequency(frequencies[string], glide);
            s.brightness = brightness;
            s.position = position;
            s.damping = damping + string_index * (0.95 - damping);
            if use_sympathetic_input {
                // split borrow: clone the small input window — the
                // sympathetic feed is one block of f32s
                let sym = self.sympathetic_input[..size].to_vec();
                s.process(
                    &sym,
                    &mut self.out_buffer[..size],
                    &mut self.aux_buffer[..size],
                );
            } else {
                let inp = self.resonator_input[..size].to_vec();
                s.process(
                    &inp,
                    &mut self.out_buffer[..size],
                    &mut self.aux_buffer[..size],
                );
            }

            if string == 0 {
                // was 0.1, Ben Wilson -> 0.2 (upstream comment)
                let gain = 0.2 / num_strings as f32;
                for i in 0..size {
                    let sum = self.out_buffer[i] - self.aux_buffer[i];
                    self.sympathetic_input[i] = gain * sum;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strum(part: &mut Part, note: f32, internal: bool) -> (Vec<f32>, Vec<f32>) {
        let mut out = vec![0.0; 64];
        let mut aux = vec![0.0; 64];
        let input = vec![0.0; 64];
        let ps = PerformanceState {
            strum: true,
            internal_exciter: internal,
            tonic: 0.0,
            note,
            fm: 0.0,
            chord: 0,
        };
        part.process(&ps, &Patch::default(), &input, &mut out, &mut aux);
        (out, aux)
    }

    fn ring_out(part: &mut Part, blocks: usize) -> f32 {
        let mut energy = 0.0;
        let input = vec![0.0; 64];
        let mut out = vec![0.0; 64];
        let mut aux = vec![0.0; 64];
        let ps = PerformanceState {
            internal_exciter: true,
            note: 69.0,
            ..Default::default()
        };
        for _ in 0..blocks {
            part.process(&ps, &Patch::default(), &input, &mut out, &mut aux);
            energy += out.iter().chain(aux.iter()).map(|v| v * v).sum::<f32>();
        }
        energy
    }

    #[test]
    fn every_model_sounds_on_internal_strum_and_stays_finite() {
        for model in ResonatorModel::ALL {
            for polyphony in [1, 2, 3, 4] {
                let mut part = Part::new(48_000.0, 64, 0xabcd);
                part.set_model(model);
                part.set_polyphony(polyphony);
                strum(&mut part, 69.0, true);
                let energy = ring_out(&mut part, 100);
                assert!(
                    energy > 1e-9,
                    "{} poly {polyphony} sounds: {energy}",
                    model.label()
                );
                assert!(energy.is_finite(), "{} stays finite", model.label());
            }
        }
    }

    #[test]
    fn voice_allocation_round_robins_and_ping_pongs() {
        let mut part = Part::new(48_000.0, 64, 0x11);
        part.set_polyphony(2);
        let mut seen = vec![];
        for note in [60.0, 64.0, 67.0, 71.0] {
            strum(&mut part, note, true);
            seen.push(part.active_voice);
        }
        assert_eq!(seen, vec![1, 0, 1, 0], "even polyphony round-robins");

        let mut part = Part::new(48_000.0, 64, 0x12);
        part.set_polyphony(3);
        let mut seen = vec![];
        for i in 0..8 {
            strum(&mut part, 60.0 + i as f32, true);
            seen.push(part.active_voice);
        }
        assert_eq!(
            seen,
            PING_PATTERN.to_vec(),
            "odd polyphony follows the ping pattern"
        );
    }

    #[test]
    fn chord_tables_match_upstream_anchors() {
        assert_eq!(chord_note(1, 0, 0), -12.0);
        assert_eq!(chord_note(1, 10, 7), 17.0);
        assert_eq!(chord_note(2, 5, 3), 19.0);
        assert_eq!(chord_note(4, 10, 0), -12.0);
    }

    #[test]
    fn squash_pins_plateaus() {
        assert!(squash(0.05) < 0.001, "low end pinned to 0");
        assert!(squash(0.95) > 0.999, "high end pinned to 1");
        assert!((squash(0.5) - 0.5).abs() < 1e-6, "midpoint exact");
    }

    #[test]
    fn string_and_reverb_has_a_longer_tail_than_dry_string() {
        let tail = |model: ResonatorModel| -> f32 {
            let mut part = Part::new(48_000.0, 64, 0x77);
            part.set_model(model);
            strum(&mut part, 69.0, true);
            // let it ring, measure late tail only
            let mut late = 0.0;
            let input = vec![0.0; 64];
            let mut out = vec![0.0; 64];
            let mut aux = vec![0.0; 64];
            let ps = PerformanceState {
                internal_exciter: true,
                note: 69.0,
                ..Default::default()
            };
            for blk in 0..300 {
                part.process(&ps, &Patch::default(), &input, &mut out, &mut aux);
                if blk > 250 {
                    late += out.iter().chain(aux.iter()).map(|v| v * v).sum::<f32>();
                }
            }
            late
        };
        let dry = tail(ResonatorModel::InharmonicString);
        let wet = tail(ResonatorModel::StringAndReverb);
        assert!(
            wet > dry,
            "the reverb extends the tail: dry {dry} vs wet {wet}"
        );
    }
}
