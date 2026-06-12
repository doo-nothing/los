//! The Peaks module shell: two channels, twelve functions each,
//! separate note sources per channel (kick on t1, snare on t2), every
//! knob bindable.
//!
//! (The DSP underneath is the MIT-licensed Mutable Instruments port —
//! see dsp.rs for the copyright and permission notice.)

use std::io;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind},
    execute,
    terminal::{enable_raw_mode, EnterAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use super::dsp::{extract_gate_flags, BassDrum, FmDrum, HighHat, SnareDrum, GATE_FLAG_LOW};
use super::mods::{
    BouncingBall, Lfo, MiniSequencer, MultistageEnvelope, NumberStation, PulseRandomizer,
    PulseShaper, LFO_SHAPES,
};
use crate::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

const FALLBACK_RATE: f32 = 48_000.0;

pub const FUNCTION_NAMES: [&str; 12] = [
    "envelope", "lfo", "tap_lfo", "bass_drum", "snare_drum", "high_hat", "fm_drum",
    "pulse_shaper", "pulse_random", "bouncing_ball", "mini_seq", "number_station",
];

/// Per-function knob labels (full control mode mappings).
fn param_labels(function: usize) -> [&'static str; 4] {
    match function {
        0 => ["attack", "decay", "sustain", "release"],
        1 => ["rate", "shape", "variation", "phase"],
        2 => ["shape", "variation", "phase", "level"],
        3 => ["freq", "punch", "tone", "decay"],
        4 => ["freq", "tone", "snappy", "decay"],
        5 => ["-", "-", "-", "decay"],
        6 => ["freq", "fm", "decay", "noise"],
        7 => ["pre-delay", "duration", "delay", "repeats"],
        8 => ["accept", "repeat", "delay", "random"],
        9 => ["gravity", "bounce", "amplitude", "velocity"],
        10 => ["step1", "step2", "step3", "step4"],
        11 => ["tone", "prob", "noise", "distort"],
        _ => ["p1", "p2", "p3", "p4"],
    }
}

/// One channel's engine bank — all twelve live so switching is free.
struct Channel {
    envelope: MultistageEnvelope,
    lfo: Lfo,
    tap_lfo: Lfo,
    bass_drum: BassDrum,
    snare_drum: SnareDrum,
    high_hat: HighHat,
    fm_drum: FmDrum,
    pulse_shaper: PulseShaper,
    pulse_randomizer: PulseRandomizer,
    bouncing_ball: BouncingBall,
    mini_sequencer: MiniSequencer,
    number_station: NumberStation,
    pulse_state: i16,
    pulse_phase: usize,
}

impl Channel {
    fn new(sample_rate: f64, seed: u32) -> Self {
        let mut tap = Lfo::new(sample_rate, seed ^ TAP_SEED);
        tap.sync = true;
        Channel {
            envelope: MultistageEnvelope::new(sample_rate),
            lfo: Lfo::new(sample_rate, seed),
            tap_lfo: tap,
            bass_drum: BassDrum::new(sample_rate),
            snare_drum: SnareDrum::new(sample_rate, seed ^ 0x1111),
            high_hat: HighHat::new(sample_rate),
            fm_drum: FmDrum::new(sample_rate, seed ^ 0x2222),
            pulse_shaper: PulseShaper::new(sample_rate),
            pulse_randomizer: PulseRandomizer::new(sample_rate, seed ^ 0x3333),
            bouncing_ball: BouncingBall::new(sample_rate),
            mini_sequencer: MiniSequencer::default(),
            number_station: NumberStation::new(sample_rate, seed ^ 0x4444),
            pulse_state: 0,
            pulse_phase: 0,
        }
    }

    /// Apply the four knobs (0..1) using the full-control-mode laws.
    fn configure(&mut self, function: usize, p: [f32; 4]) {
        let u = |x: f32| (x.clamp(0.0, 1.0) * 65535.0) as u16;
        let b = |x: f32| (x.clamp(0.0, 1.0) * 65535.0 - 32768.0) as i16;
        match function {
            0 => self
                .envelope
                .set_adsr(u(p[0]), u(p[1]), (u(p[2]) >> 1) as i16, u(p[3])),
            1 => {
                self.lfo.set_rate(u(p[0]));
                let shape = ((p[1] * 4.999) as usize).min(4);
                self.lfo.set_shape(LFO_SHAPES[shape]);
                self.lfo.set_parameter(b(p[2]));
                self.lfo.set_level(65535);
                let _ = p[3]; // reset phase folded into retrigger
            }
            2 => {
                let shape = ((p[0] * 4.999) as usize).min(4);
                self.tap_lfo.set_shape(LFO_SHAPES[shape]);
                self.tap_lfo.set_parameter(b(p[1]));
                let _ = p[2];
                self.tap_lfo.set_level(u(p[3]).max(16384));
            }
            3 => {
                self.bass_drum.set_frequency(b(p[0]));
                self.bass_drum.set_punch(u(p[1]));
                self.bass_drum.set_tone(u(p[2]));
                self.bass_drum.set_decay(u(p[3]));
            }
            4 => {
                self.snare_drum.set_frequency(b(p[0]));
                self.snare_drum.set_tone(u(p[1]));
                self.snare_drum.set_snappy(u(p[2]));
                self.snare_drum.set_decay(u(p[3]));
            }
            5 => {} // the 808 hat is gloriously knobless
            6 => {
                self.fm_drum.set_frequency(u(p[0]));
                self.fm_drum.set_fm_amount((u(p[1]) >> 2) * 3);
                self.fm_drum.set_decay(u(p[2]));
                self.fm_drum.set_noise(u(p[3]));
            }
            7 => {
                self.pulse_shaper.set_initial_delay(u(p[0]));
                self.pulse_shaper.set_duration(u(p[1]) >> 1);
                self.pulse_shaper.set_delay(u(p[2]) >> 1);
                self.pulse_shaper.set_num_repetitions(u(p[3]));
            }
            8 => {
                self.pulse_randomizer.set_acceptance_probability(u(p[0]));
                self.pulse_randomizer.set_repetition_probability(u(p[1]));
                self.pulse_randomizer.set_delay_average(u(p[2]));
                self.pulse_randomizer.set_delay_randomness(u(p[3]));
            }
            9 => {
                self.bouncing_ball.set_gravity(u(p[0]));
                self.bouncing_ball.set_bounce_loss(u(p[1]));
                self.bouncing_ball.set_initial_amplitude(u(p[2]));
                self.bouncing_ball.set_initial_velocity(b(p[3]));
            }
            10 => {
                for (i, v) in p.iter().enumerate() {
                    self.mini_sequencer.set_step(i, b(*v));
                }
                self.mini_sequencer.set_num_steps(4);
            }
            11 => {
                self.number_station.set_tone(u(p[0]));
                self.number_station.set_transition_probability(u(p[1]));
                self.number_station.set_noise(u(p[2]));
                self.number_station.set_distortion(u(p[3]));
            }
            _ => {}
        }
    }

    fn process(&mut self, function: usize, gates: &[u8], out: &mut [i16]) {
        match function {
            0 => self.envelope.process(gates, out),
            1 => self.lfo.process(gates, out),
            2 => self.tap_lfo.process(gates, out),
            3 => self.bass_drum.process(gates, out),
            4 => self.snare_drum.process(gates, out),
            5 => self.high_hat.process(gates, out),
            6 => self.fm_drum.process(gates, out),
            7 | 8 => {
                // pulse processors tick at the firmware's 6 kHz
                // control rate = once per 8 samples at 48 kHz
                for (i, o) in out.iter_mut().enumerate() {
                    let new_pulse = gates[i] & super::dsp::GATE_FLAG_RISING != 0;
                    if self.pulse_phase == 0 || new_pulse {
                        self.pulse_state = if function == 7 {
                            self.pulse_shaper.tick(new_pulse)
                        } else {
                            self.pulse_randomizer.tick(new_pulse)
                        };
                    }
                    self.pulse_phase = (self.pulse_phase + 1) % 8;
                    *o = self.pulse_state;
                }
            }
            9 => self.bouncing_ball.process(gates, out),
            10 => self.mini_sequencer.process(gates, out),
            11 => self.number_station.process(gates, out),
            _ => out.fill(0),
        }
    }
}

/// Seed scrambler for the tap LFO (avoid identical noise streams).
const TAP_SEED: u32 = 0x7a51_0f00;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Fn1,
    P1(usize),
    Notes1,
    Fn2,
    P2(usize),
    Notes2,
    Level,
    Amp,
}

const ROWS: [Row; 14] = [
    Row::Fn1,
    Row::P1(0),
    Row::P1(1),
    Row::P1(2),
    Row::P1(3),
    Row::Notes1,
    Row::Fn2,
    Row::P2(0),
    Row::P2(1),
    Row::P2(2),
    Row::P2(3),
    Row::Notes2,
    Row::Level,
    Row::Amp,
];

/// CV bank, srcs[] order — all eight knobs plus the level.
const BINDABLE: [Row; 9] = [
    Row::P1(0),
    Row::P1(1),
    Row::P1(2),
    Row::P1(3),
    Row::P2(0),
    Row::P2(1),
    Row::P2(2),
    Row::P2(3),
    Row::Level,
];
const N_SRC: usize = BINDABLE.len();

struct PeaksState {
    function: [usize; 2],
    params: [[f32; 4]; 2],
    level: f32,
    gate: [bool; 2],
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    amp_src: Option<SourceAddr>,
    amp_resolved: Option<usize>,
    notes_src: [Option<SourceAddr>; 2],
    out_now: [f32; 2],
    selected: usize,
}

impl PeaksState {
    fn new() -> Self {
        let mut s = PeaksState {
            function: [3, 4], // kick + snare out of the box
            params: [[0.5; 4]; 2],
            level: 0.8,
            gate: [false; 2],
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.0; N_SRC],
            amp_src: None,
            amp_resolved: None,
            notes_src: [None, None],
            out_now: [0.0; 2],
            selected: 0,
        };
        for (k, r) in BINDABLE.iter().enumerate() {
            s.eff[k] = s.get(*r);
        }
        s
    }

    fn get(&self, r: Row) -> f32 {
        match r {
            Row::P1(i) => self.params[0][i],
            Row::P2(i) => self.params[1][i],
            Row::Level => self.level,
            _ => 0.0,
        }
    }

    fn set(&mut self, r: Row, v: f32) {
        let v = v.clamp(0.0, 1.0);
        match r {
            Row::P1(i) => self.params[0][i] = v,
            Row::P2(i) => self.params[1][i] = v,
            Row::Level => self.level = v,
            _ => {}
        }
    }
}

const SRC_SLOT_BASE: usize = 50;
const AMP_SLOT: usize = 40;
const NOTES1_SLOT: usize = 41;
const NOTES2_SLOT: usize = 42;

impl crate::undo::ParamUndo for PeaksState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        match slot {
            AMP_SLOT => return Some(V::Src(self.amp_src.as_ref().map(|a| a.to_string()))),
            NOTES1_SLOT => {
                return Some(V::Src(self.notes_src[0].as_ref().map(|a| a.to_string())))
            }
            NOTES2_SLOT => {
                return Some(V::Src(self.notes_src[1].as_ref().map(|a| a.to_string())))
            }
            _ => {}
        }
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            if i < N_SRC {
                return Some(V::Src(self.srcs[i].as_ref().map(|a| a.to_string())));
            }
            return None;
        }
        let r = *ROWS.get(slot)?;
        Some(match r {
            Row::Fn1 => V::Usize(self.function[0]),
            Row::Fn2 => V::Usize(self.function[1]),
            Row::Notes1 | Row::Notes2 | Row::Amp => return None,
            _ => V::F32(self.get(r)),
        })
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        let parse = |v: &Option<String>| v.as_deref().and_then(SourceAddr::parse);
        match (slot, &value) {
            (AMP_SLOT, V::Src(v)) => {
                self.amp_src = parse(v);
                self.amp_resolved = None;
                return;
            }
            (NOTES1_SLOT, V::Src(v)) => {
                self.notes_src[0] = parse(v);
                return;
            }
            (NOTES2_SLOT, V::Src(v)) => {
                self.notes_src[1] = parse(v);
                return;
            }
            _ => {}
        }
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            if i < N_SRC {
                if let V::Src(v) = &value {
                    self.srcs[i] = parse(v);
                    self.resolved[i] = None;
                }
            }
            return;
        }
        match (ROWS.get(slot).copied(), value) {
            (Some(Row::Fn1), V::Usize(v)) => self.function[0] = v.min(11),
            (Some(Row::Fn2), V::Usize(v)) => self.function[1] = v.min(11),
            (Some(r), V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &PeaksState) -> state::PeaksParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::PeaksParams {
        format: state::STATE_FORMAT,
        fn1: Some(FUNCTION_NAMES[s.function[0].min(11)].into()),
        fn2: Some(FUNCTION_NAMES[s.function[1].min(11)].into()),
        p1: s.params[0].to_vec(),
        p2: s.params[1].to_vec(),
        level: Some(s.level),
        p1a_src: src(0),
        p1b_src: src(1),
        p1c_src: src(2),
        p1d_src: src(3),
        p2a_src: src(4),
        p2b_src: src(5),
        p2c_src: src(6),
        p2d_src: src(7),
        level_src: src(8),
        amp_src: s.amp_src.as_ref().map(|a| a.to_string()),
        notes1_src: s.notes_src[0].as_ref().map(|a| a.to_string()),
        notes2_src: s.notes_src[1].as_ref().map(|a| a.to_string()),
    }
}

fn apply_params(s: &mut PeaksState, p: &state::PeaksParams) {
    if let Some(f) = p.fn1.as_deref() {
        if let Some(i) = FUNCTION_NAMES.iter().position(|x| *x == f) {
            s.function[0] = i;
        }
    }
    if let Some(f) = p.fn2.as_deref() {
        if let Some(i) = FUNCTION_NAMES.iter().position(|x| *x == f) {
            s.function[1] = i;
        }
    }
    for (i, v) in p.p1.iter().take(4).enumerate() {
        s.params[0][i] = v.clamp(0.0, 1.0);
    }
    for (i, v) in p.p2.iter().take(4).enumerate() {
        s.params[1][i] = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.level {
        s.level = v.clamp(0.0, 1.0);
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.p1a_src),
        parse(&p.p1b_src),
        parse(&p.p1c_src),
        parse(&p.p1d_src),
        parse(&p.p2a_src),
        parse(&p.p2b_src),
        parse(&p.p2c_src),
        parse(&p.p2d_src),
        parse(&p.level_src),
    ];
    s.amp_src = parse(&p.amp_src);
    s.notes_src[0] = parse(&p.notes1_src);
    s.notes_src[1] = parse(&p.notes2_src);
    s.resolved = Default::default();
    s.amp_resolved = None;
}

// ── audio thread ───────────────────────────────────────────────────────────

fn audio_thread(shared: Arc<Mutex<PeaksState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_peaks_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating peaks ring")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    // two claims: each channel's output doubles as a modulation source
    manifest.register("peaks", instance, Some(&shm_name), 2)?;
    let mod_base = manifest.claimed_base();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();
    let mut events = EventRingbuf::open_dynamic().ok();
    let transport = ShmTransport::open().ok();
    let sample_rate = transport
        .as_ref()
        .map(|t| t.sample_rate() as f32)
        .filter(|r| *r > 0.0)
        .unwrap_or(FALLBACK_RATE) as f64;

    let channels = ringbuf.channels() as usize;
    let slot_frames = ringbuf.slot_frames() as usize;
    let mut block = vec![0.0_f32; ringbuf.slot_len()];
    let mut gates = [vec![GATE_FLAG_LOW; slot_frames], vec![GATE_FLAG_LOW; slot_frames]];
    let mut outs = [vec![0i16; slot_frames], vec![0i16; slot_frames]];

    let mut engine = [
        Channel::new(sample_rate, 0x9e15 ^ instance as u32),
        Channel::new(sample_rate, 0x51ab ^ instance as u32),
    ];
    let mut note_filter: [Option<u8>; 2] = [None, None];
    let mut prev_flag = [GATE_FLAG_LOW; 2];
    let mut gain_smooth = 0.0_f32;
    let mut blocks: u64 = 0;

    loop {
        if blocks.is_multiple_of(128) {
            if events.is_none() {
                events = EventRingbuf::open_dynamic().ok();
            }
            let entries = manifest.entries();
            let mut s = shared.lock().unwrap();
            for k in 0..N_SRC {
                s.resolved[k] = s.srcs[k]
                    .as_ref()
                    .and_then(|a| routing::resolve(&entries, a));
            }
            s.amp_resolved = s
                .amp_src
                .as_ref()
                .and_then(|a| routing::resolve(&entries, a));
            #[allow(clippy::needless_range_loop)] // c strides
            // several parallel per-channel arrays
            for c in 0..2 {
                note_filter[c] = s.notes_src[c].as_ref().and_then(routing::note_source_track);
            }
            let mask = s
                .resolved
                .iter()
                .flatten()
                .chain(s.amp_resolved.iter())
                .filter(|&&c| c < 64)
                .fold(0u64, |m, &c| m | (1 << c));
            let notes = note_filter
                .iter()
                .flatten()
                .filter(|&&t| t < 8)
                .fold(0u8, |m, &t| m | (1 << t));
            manifest.publish_consumes(mask, notes);
        }

        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                let mut s = shared.lock().unwrap();
            #[allow(clippy::needless_range_loop)] // c strides
            // several parallel per-channel arrays
                for c in 0..2 {
                    // a channel with no binding listens to every track,
                    // like the other voices
                    let listens = match note_filter[c] {
                        Some(t) => event.source == t,
                        None => true,
                    };
                    if !listens {
                        continue;
                    }
                    if event.is_note_on() {
                        s.gate[c] = true;
                    } else if event.is_note_off() {
                        s.gate[c] = false;
                    }
                }
            }
        }

        let (functions, params, level, amp, gate) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            // max/min, not clamp: NaN from a stale channel dies here
            #[allow(clippy::manual_clamp)]
            let cv = |k: usize, manual: f32, s: &PeaksState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let mut params = s.params;
            let mut level_eff = s.level;
            for (k, r) in BINDABLE.iter().enumerate() {
                let manual = s.get(*r);
                let v = cv(k, manual, &s);
                match *r {
                    Row::P1(i) => params[0][i] = v,
                    Row::P2(i) => params[1][i] = v,
                    Row::Level => level_eff = v,
                    _ => {}
                }
                s.eff[k] = v;
            }
            let amp = match (s.amp_src.is_some(), s.amp_resolved, bus) {
                (false, _, _) => 1.0,
                (true, Some(ch), Some(b)) => b.get(ch).clamp(0.0, 1.0),
                (true, _, _) => 0.0,
            };
            (s.function, params, level_eff, amp, s.gate)
        };

        for c in 0..2 {
            for g in gates[c].iter_mut() {
                prev_flag[c] = extract_gate_flags(prev_flag[c], gate[c]);
                *g = prev_flag[c];
            }
            engine[c].configure(functions[c], params[c]);
            let (gbuf, obuf) = (&gates[c], &mut outs[c]);
            engine[c].process(functions[c], gbuf, obuf);
        }

        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            let mut s = shared.lock().unwrap();
            #[allow(clippy::needless_range_loop)] // c strides
            // several parallel per-channel arrays
            for c in 0..2 {
                let v = (outs[c][slot_frames - 1] as f32 / 32768.0 + 1.0) * 0.5;
                bus.set(base + c, v.clamp(0.0, 1.0));
                s.out_now[c] = v.clamp(0.0, 1.0);
            }
        }

        let gain_target = amp * level;
        let g_alpha = 1.0 - (-1.0 / (0.0007 * sample_rate as f32)).exp();
        for f in 0..slot_frames {
            gain_smooth += (gain_target - gain_smooth) * g_alpha;
            block[f * channels] = outs[0][f] as f32 / 32768.0 * gain_smooth;
            if channels > 1 {
                block[f * channels + 1] = outs[1][f] as f32 / 32768.0 * gain_smooth;
            }
        }
        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            engine = [
                Channel::new(sample_rate, 0x9e15 ^ instance as u32),
                Channel::new(sample_rate, 0x51ab ^ instance as u32),
            ];
        }
        while ringbuf.write(&block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }
        blocks += 1;
    }
}

// ── ui ─────────────────────────────────────────────────────────────────────

fn row_label(s: &PeaksState, r: Row) -> String {
    match r {
        Row::Fn1 => "fn 1".into(),
        Row::P1(i) => format!("· {}", param_labels(s.function[0])[i]),
        Row::Notes1 => "notes 1".into(),
        Row::Fn2 => "fn 2".into(),
        Row::P2(i) => format!("· {}", param_labels(s.function[1])[i]),
        Row::Notes2 => "notes 2".into(),
        Row::Level => "level".into(),
        Row::Amp => "amp".into(),
    }
}

fn src_index(r: Row) -> Option<usize> {
    BINDABLE.iter().position(|b| *b == r)
}

fn binding_slot(r: Row) -> Option<usize> {
    match r {
        Row::Amp => Some(AMP_SLOT),
        Row::Notes1 => Some(NOTES1_SLOT),
        Row::Notes2 => Some(NOTES2_SLOT),
        _ => src_index(r).map(|i| SRC_SLOT_BASE + i),
    }
}

fn row_text(s: &PeaksState, r: Row) -> String {
    match r {
        Row::Fn1 => FUNCTION_NAMES[s.function[0].min(11)].into(),
        Row::Fn2 => FUNCTION_NAMES[s.function[1].min(11)].into(),
        Row::Notes1 => s.notes_src[0]
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(all tracks)".into()),
        Row::Notes2 => s.notes_src[1]
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(all tracks)".into()),
        Row::Amp => s
            .amp_src
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(unbound)".into()),
        _ => format!("{:.0}%", s.get(r) * 100.0),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &PeaksState,
    instance: usize,
    show_help: bool,
    overlay: Option<&str>,
    picker: Option<(Vec<String>, usize)>,
    entries: &[crate::shm::ManifestEntry],
) -> Result<()> {
    use crate::theme;
    terminal.draw(|f| {
        let area = f.area();
        let w = area.width as usize;
        let h = area.height as usize;
        let mut lines: Vec<Line> = Vec::new();
        lines.push(theme::header(
            "PEAKS",
            &format!("dual fn {}", instance),
            "",
            w,
        ));
        let mut meter_spans = vec![Span::styled("  out ".to_string(), theme::chrome())];
        for v in s.out_now.iter() {
            meter_spans.push(Span::styled(
                theme::meter_char(*v).to_string(),
                theme::signal(theme::cv_ramp(*v)),
            ));
            meter_spans.push(Span::raw(" "));
        }
        lines.push(Line::from(meter_spans));
        lines.push(Line::from(""));

        let bar_w = theme::bar_width(w, 32);
        for (i, r) in ROWS.iter().enumerate() {
            let selected = i == s.selected;
            let label_style = if selected {
                theme::selected()
            } else {
                theme::chrome()
            };
            let mut spans: Vec<Span> = vec![Span::styled(
                format!(" {:<11}", row_label(s, *r)),
                label_style,
            )];
            let bound = src_index(*r).is_some_and(|k| s.srcs[k].is_some())
                || (*r == Row::Amp && s.amp_src.is_some())
                || (*r == Row::Notes1 && s.notes_src[0].is_some())
                || (*r == Row::Notes2 && s.notes_src[1].is_some());
            let hue = src_index(*r)
                .and_then(|k| s.srcs[k].as_ref())
                .or(match r {
                    Row::Amp => s.amp_src.as_ref(),
                    Row::Notes1 => s.notes_src[0].as_ref(),
                    Row::Notes2 => s.notes_src[1].as_ref(),
                    _ => None,
                })
                .map(|a| routing::cable_color(entries, a));
            if src_index(*r).is_some() {
                let shown = match src_index(*r) {
                    Some(k) if s.srcs[k].is_some() => s.eff[k],
                    _ => s.get(*r),
                };
                spans.extend(theme::bar(shown, None, bar_w, hue.unwrap_or_else(theme::cv)));
            } else {
                spans.push(Span::styled(" ".repeat(bar_w), theme::dim()));
            }
            let vstyle = if selected {
                theme::selected()
            } else if let Some(hu) = hue {
                theme::signal(hu)
            } else {
                theme::value()
            };
            let mark = if bound { theme::BIND } else { ' ' };
            spans.push(Span::styled(
                format!(" {}{}", mark, row_text(s, *r)),
                vstyle,
            ));
            lines.push(Line::from(spans));
        }

        theme::anchor_bottom(&mut lines, h, 2);
        lines.push(theme::rule(w));
        lines.push(theme::status("NORMAL", overlay.unwrap_or(""), "", w));
        f.render_widget(Paragraph::new(lines), area);

        if show_help {
            let help = Paragraph::new(vec![
                Line::from("━━━ PEAKS · dual function gen (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  fn 1/2      envelope · lfo · tap_lfo · bass_drum"),
                Line::from("              snare_drum · high_hat · fm_drum"),
                Line::from("              pulse_shaper · pulse_random"),
                Line::from("              bouncing_ball · mini_seq · number_station"),
                Line::from("  knobs       the four params re-label per function"),
                Line::from("  notes 1/2   separate note tracks per channel"),
                Line::from("              (kick on t1, snare on t2)"),
                Line::from(""),
                Line::from("Channel 1 = left, channel 2 = right; both publish"),
                Line::from("peaks/N/ch1+ch2 on the bus (envelopes! tap LFOs!)."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" PEAKS ", theme::chrome_hi())),
            );
            f.render_widget(help, area);
        }

        if let Some((rows, sel)) = picker {
            let ph = (rows.len() as u16 + 2).min(area.height);
            let pw = rows.iter().map(|r| r.len()).max().unwrap_or(10).max(20) as u16 + 4;
            let rect = ratatui::layout::Rect::new(
                (area.width.saturating_sub(pw)) / 2,
                (area.height.saturating_sub(ph)) / 2,
                pw.min(area.width),
                ph,
            );
            f.render_widget(ratatui::widgets::Clear, rect);
            let items: Vec<ratatui::widgets::ListItem> = rows
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    let style = if i == sel {
                        theme::selected()
                    } else {
                        theme::value()
                    };
                    ratatui::widgets::ListItem::new(row.clone()).style(style)
                })
                .collect();
            let list = ratatui::widgets::List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" bind ", theme::chrome_hi())),
            );
            f.render_widget(list, rect);
        }
    })?;
    Ok(())
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("peaks", instance);

    let shared = Arc::new(Mutex::new(PeaksState::new()));
    if let Ok(p) = state::load_module_state::<state::PeaksParams>("peaks", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let audio_builder = thread::Builder::new()
        .name(String::from("peaks-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = audio_builder.spawn(move || {
        // black box: a dead audio thread must leave a trace
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            audio_thread(audio_state, instance)
        }));
        let msg = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("error: {e}"),
            Err(p) => format!(
                "PANIC: {}",
                p.downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string payload>".into())
            ),
        };
        eprintln!("[peaks {instance}] audio thread died: {msg}");
        let path = crate::state::tmp_dir().join(format!("peaks_{instance}.crash"));
        let _ = std::fs::write(path, &msg);
    });

    for attempt in 0..20 {
        match enable_raw_mode() {
            Ok(()) => break,
            Err(e) if attempt < 19 => {
                let _ = e;
                thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(anyhow::anyhow!("enabling raw mode: {}", e)),
        }
    }
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let mut show_help = false;
    let mut count = crate::keys::Count::default();
    let mut pending_g = false;
    let mut history = crate::undo::ParamHistory::default();
    let mut ex = crate::excmd::ExLine::default();
    let mut picker = crate::picker::Picker::default();
    let mut ex_msg: Option<String> = None;
    let mut patch_name: Option<String> = None;
    let mut should_quit = false;
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();
    let manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let mut ui_entries: Vec<crate::shm::ManifestEntry> = Vec::new();
    let mut ui_entries_at: Option<Instant> = None;
    let mut baseline =
        state::to_toml_string(&snapshot_params(&shared.lock().unwrap())).unwrap_or_default();

    loop {
        if state::check_save_signal() {
            let params = snapshot_params(&shared.lock().unwrap());
            let _ = state::save_module_state("peaks", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::PeaksParams>("peaks", instance) {
                apply_params(&mut shared.lock().unwrap(), &p);
            }
        }
        if ui_entries_at.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
            ui_entries = manifest.entries();
            ui_entries_at = Some(Instant::now());
        }

        let overlay = if ex.is_active() {
            Some(ex.display())
        } else {
            ex_msg.clone()
        };
        {
            let s = shared.lock().unwrap();
            let picker_rows = picker.is_active().then(|| picker.rows());
            draw_ui(
                &mut terminal,
                &s,
                instance,
                show_help,
                overlay.as_deref(),
                picker_rows,
                &ui_entries,
            )?;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let ev = event::read()?;

        if let Event::Mouse(m) = ev {
            if picker.is_active() || ex.is_active() {
                continue;
            }
            match m.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let steps: f32 = if m.kind == MouseEventKind::ScrollUp {
                        0.01
                    } else {
                        -0.01
                    };
                    use crate::undo::ParamUndo;
                    let mut s = shared.lock().unwrap();
                    let slot = s.selected;
                    let r = ROWS[slot.min(ROWS.len() - 1)];
                    if src_index(r).is_none() {
                        continue;
                    }
                    let old = s.get_param(slot);
                    let v = s.get(r) + steps;
                    s.set(r, v);
                    let new = s.get_param(slot);
                    if let (Some(old), Some(new)) = (old, new) {
                        history.record(slot, "Adjust", old, new);
                    }
                }
                MouseEventKind::Down(_) => {
                    let row = (m.row as usize).saturating_sub(3);
                    if row < ROWS.len() {
                        shared.lock().unwrap().selected = row;
                    }
                }
                _ => {}
            }
            continue;
        }
        let Event::Key(key) = ev else { continue };
        ex_msg = None;

        if picker.is_active() {
            if let crate::picker::PickerEvent::Chosen(addr) = picker.handle_key(key.code) {
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                if let Some(slot) = binding_slot(r) {
                    let old = s.get_param(slot);
                    let text = addr.as_ref().map(|a| a.to_string());
                    match r {
                        Row::Amp => {
                            s.amp_src = addr.clone();
                            s.amp_resolved = None;
                        }
                        Row::Notes1 => s.notes_src[0] = addr.clone(),
                        Row::Notes2 => s.notes_src[1] = addr.clone(),
                        _ => {
                            if let Some(k) = src_index(r) {
                                s.srcs[k] = addr.clone();
                                s.resolved[k] = None;
                            }
                        }
                    }
                    if let Some(old) = old {
                        history.record(slot, "Bind", old, ParamValue::Src(text));
                    }
                }
            }
            continue;
        }
        if ex.is_active() {
            let completer =
                crate::excmd::standard_completer(crate::excmd::patch_names(&state::patches_dir()));
            if let crate::excmd::ExEvent::Submit(cmd) = ex.handle_key(key.code, &completer) {
                use crate::excmd::ExCommand;
                let params = snapshot_params(&shared.lock().unwrap());
                match cmd {
                    ExCommand::Write(name) => {
                        ex_msg = Some(
                            match crate::excmd::ex_write(
                                name,
                                &mut patch_name,
                                &mut baseline,
                                &params,
                            ) {
                                Ok(m) | Err(m) => m,
                            },
                        );
                    }
                    ExCommand::Edit(name) => match state::load_patch::<state::PeaksParams>(&name) {
                        Ok(p) => {
                            apply_params(&mut shared.lock().unwrap(), &p);
                            baseline = state::to_toml_string(&snapshot_params(
                                &shared.lock().unwrap(),
                            ))
                            .unwrap_or_default();
                            patch_name = Some(name.clone());
                            ex_msg = Some(format!("Loaded {}", name));
                        }
                        Err(e) => ex_msg = Some(e.to_string()),
                    },
                    ExCommand::Quit { force } => {
                        if !force && crate::excmd::is_dirty(&params, &baseline) {
                            ex_msg = Some(String::from(
                                "Unsaved changes (:q! to discard, :w <name> to save)",
                            ));
                        } else {
                            should_quit = true;
                        }
                    }
                    ExCommand::WriteQuit(name) => {
                        match crate::excmd::ex_write(name, &mut patch_name, &mut baseline, &params)
                        {
                            Ok(_) => should_quit = true,
                            Err(m) => ex_msg = Some(m),
                        }
                    }
                    ExCommand::Set(k, v) => {
                        let mut s = shared.lock().unwrap();
                        ex_msg = Some(ex_set(&mut s, &mut history, &k, &v));
                    }
                    ExCommand::Unknown(c) => ex_msg = Some(format!("Not a command: {}", c)),
                }
            }
            if should_quit {
                break;
            }
            continue;
        }

        if !matches!(key.code, KeyCode::Char('g')) {
            pending_g = false;
        }
        if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
            let n = count.take();
            let mut s = shared.lock().unwrap();
            ex_msg = Some(crate::undo::history_status("Redo", n, || {
                history.redo(&mut *s)
            }));
            continue;
        }
        if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
            let params = snapshot_params(&shared.lock().unwrap());
            let _ = state::save_module_state("peaks", instance, &params);
            ex_msg = Some(String::from("Saved"));
            continue;
        }

        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
            KeyCode::Char('j') | KeyCode::Down => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, n, ROWS.len());
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, -n, ROWS.len());
            }
            KeyCode::Char(c @ ('h' | 'l' | 'H' | 'L')) => {
                let n = count.take() as i32;
                let (steps, coarse) = match c {
                    'h' => (-n, false),
                    'l' => (n, false),
                    'H' => (-n, true),
                    _ => (n, true),
                };
                use crate::keys::step_f32;
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                let slot = s.selected;
                let r = ROWS[slot.min(ROWS.len() - 1)];
                if matches!(r, Row::Notes1 | Row::Notes2 | Row::Amp) {
                    ex_msg = Some("routing row: @ binds, x unbinds".into());
                    continue;
                }
                let old = s.get_param(slot);
                match r {
                    Row::Fn1 => {
                        s.function[0] =
                            (s.function[0] as i32 + steps.signum()).rem_euclid(12) as usize
                    }
                    Row::Fn2 => {
                        s.function[1] =
                            (s.function[1] as i32 + steps.signum()).rem_euclid(12) as usize
                    }
                    _ => {
                        let v = step_f32(s.get(r), steps, 0.01, coarse, 0.0, 1.0);
                        s.set(r, v);
                    }
                }
                let new = s.get_param(slot);
                if let (Some(old), Some(new)) = (old, new) {
                    history.record(slot, "Adjust", old, new);
                }
            }
            KeyCode::Char('0') => {
                count.clear();
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                if matches!(r, Row::Notes1 | Row::Notes2 | Row::Amp) {
                    continue;
                }
                let slot = s.selected;
                let old = s.get_param(slot);
                let def = PeaksState::new();
                if let Some(v) = def.get_param(slot) {
                    s.set_param(slot, v.clone());
                    if let Some(old) = old {
                        history.record(slot, "Reset", old, v);
                    }
                }
            }
            KeyCode::Char('@') => {
                count.clear();
                let s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                if binding_slot(r).is_some() {
                    let current = match r {
                        Row::Amp => s.amp_src.clone(),
                        Row::Notes1 => s.notes_src[0].clone(),
                        Row::Notes2 => s.notes_src[1].clone(),
                        _ => src_index(r).and_then(|k| s.srcs[k].clone()),
                    };
                    drop(s);
                    let sources = Manifest::open()
                        .map(|m| routing::live_sources(&m.entries()))
                        .unwrap_or_default();
                    picker.open(sources, current.as_ref());
                } else {
                    ex_msg = Some("function rows cycle with h/l".into());
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                if let Some(slot) = binding_slot(r) {
                    let had = match r {
                        Row::Amp => s.amp_src.is_some(),
                        Row::Notes1 => s.notes_src[0].is_some(),
                        Row::Notes2 => s.notes_src[1].is_some(),
                        _ => src_index(r).is_some_and(|k| s.srcs[k].is_some()),
                    };
                    if had {
                        let old = s.get_param(slot);
                        match r {
                            Row::Amp => {
                                s.amp_src = None;
                                s.amp_resolved = None;
                            }
                            Row::Notes1 => s.notes_src[0] = None,
                            Row::Notes2 => s.notes_src[1] = None,
                            _ => {
                                if let Some(k) = src_index(r) {
                                    s.srcs[k] = None;
                                    s.resolved[k] = None;
                                }
                            }
                        }
                        if let Some(old) = old {
                            history.record(slot, "Unbind", old, ParamValue::Src(None));
                        }
                    }
                }
            }
            KeyCode::Char('u') => {
                let n = count.take();
                let mut s = shared.lock().unwrap();
                ex_msg = Some(crate::undo::history_status("Undo", n, || {
                    history.undo(&mut *s)
                }));
            }
            KeyCode::Char('g') => {
                count.clear();
                if pending_g {
                    pending_g = false;
                    shared.lock().unwrap().selected = 0;
                } else {
                    pending_g = true;
                }
            }
            KeyCode::Char('G') => {
                count.clear();
                shared.lock().unwrap().selected = ROWS.len() - 1;
            }
            KeyCode::Char(' ') => {
                if transport_ui.is_none() {
                    transport_ui = ShmTransport::open().ok();
                }
                if let Some(ref mut t) = transport_ui {
                    t.toggle_playing();
                }
            }
            KeyCode::Char(':') => {
                count.clear();
                ex.open();
            }
            KeyCode::Char('?') => {
                count.clear();
                show_help = !show_help;
            }
            _ => {
                count.clear();
            }
        }
        if should_quit {
            break;
        }
    }

    crossterm::terminal::disable_raw_mode()?;
    execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    Ok(())
}

fn ex_set(
    s: &mut PeaksState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    // fn1/fn2 take function names; p1a..p1d / p2a..p2d the knobs
    match key {
        "fn1" | "fn2" => {
            let Some(i) = FUNCTION_NAMES.iter().position(|f| *f == value) else {
                return format!("{key}: one of {}", FUNCTION_NAMES.join(" "));
            };
            if key == "fn1" {
                s.function[0] = i;
            } else {
                s.function[1] = i;
            }
            return format!("{key} = {value}");
        }
        "notes1" | "notes2" | "amp" => {
            let slot = match key {
                "notes1" => NOTES1_SLOT,
                "notes2" => NOTES2_SLOT,
                _ => AMP_SLOT,
            };
            let v = if value == "-" {
                V::Src(None)
            } else {
                V::Src(Some(value.to_string()))
            };
            let old = s.get_param(slot);
            s.set_param(slot, v.clone());
            if let Some(old) = old {
                history.record(slot, "Set", old, v);
            }
            return format!("{key} set");
        }
        _ => {}
    }
    let parse_knob = |k: &str| -> Option<(usize, usize)> {
        let ch = match k.get(..2) {
            Some("p1") => 0,
            Some("p2") => 1,
            _ => return None,
        };
        let idx = match k.get(2..3) {
            Some("a") => 0,
            Some("b") => 1,
            Some("c") => 2,
            Some("d") => 3,
            _ => return None,
        };
        Some((ch, idx))
    };
    let Some((ch, idx)) = parse_knob(key) else {
        if key == "level" {
            return match value.parse::<f32>() {
                Ok(v) => {
                    s.set(Row::Level, v);
                    format!("level = {:.0}%", s.level * 100.0)
                }
                Err(_) => {
                    let bind_slot = SRC_SLOT_BASE + 8;
                    let v = if value == "-" {
                        V::Src(None)
                    } else {
                        V::Src(Some(value.to_string()))
                    };
                    s.set_param(bind_slot, v);
                    "level cable updated".into()
                }
            };
        }
        return format!("Unknown setting: {key}");
    };
    let row = if ch == 0 { Row::P1(idx) } else { Row::P2(idx) };
    match value.parse::<f32>() {
        Ok(v) => {
            s.set(row, v);
            format!("{key} = {:.0}%", s.get(row) * 100.0)
        }
        Err(_) => {
            let Some(k) = src_index(row) else {
                return format!("{key}: not bindable");
            };
            let v = if value == "-" {
                V::Src(None)
            } else {
                if SourceAddr::parse(value).is_none() {
                    return format!("{key}: not a number or source: {value}");
                }
                V::Src(Some(value.to_string()))
            };
            let bind_slot = SRC_SLOT_BASE + k;
            let old = s.get_param(bind_slot);
            s.set_param(bind_slot, v.clone());
            if let Some(old) = old {
                history.record(bind_slot, "Set", old, v);
            }
            format!("{key} cable updated")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = PeaksState::new();
        s.function = [6, 11];
        s.params[0][2] = 0.7;
        s.notes_src[1] = SourceAddr::parse("sequencer/0/t4");
        s.srcs[3] = SourceAddr::parse("lfo/0/s1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::PeaksParams = toml::from_str(&toml).expect("parses");
        let mut s2 = PeaksState::new();
        apply_params(&mut s2, &back);
        assert_eq!(s2.function, [6, 11]);
        assert!((s2.params[0][2] - 0.7).abs() < 1e-6);
        assert_eq!(
            s2.notes_src[1].as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t4".into())
        );
        assert!(s2.srcs[3].is_some());
    }

    #[test]
    fn every_value_row_is_bindable() {
        use crate::undo::ParamUndo;
        use crate::undo::ParamValue as V;
        for r in ROWS {
            if matches!(
                r,
                Row::Fn1 | Row::Fn2 | Row::Notes1 | Row::Notes2 | Row::Amp
            ) {
                continue;
            }
            let i = src_index(r).unwrap_or_else(|| panic!("{r:?} must be bindable"));
            let mut s = PeaksState::new();
            s.set_param(SRC_SLOT_BASE + i, V::Src(Some("lfo/0/a1".into())));
            assert!(s.srcs[i].is_some(), "{r:?} binds");
        }
    }

    #[test]
    fn ex_set_functions_knobs_and_cables() {
        let mut s = PeaksState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "fn1", "fm_drum").contains("fm_drum"));
        assert_eq!(s.function[0], 6);
        assert!(ex_set(&mut s, &mut h, "p1b", "0.9").contains("90%"));
        assert!(ex_set(&mut s, &mut h, "p2c", "lfo/0/s1").contains("cable"));
        assert!(s.srcs[6].is_some());
        assert!(ex_set(&mut s, &mut h, "fn1", "nope").contains("one of"));
    }

    #[test]
    fn channel_configure_covers_every_function() {
        // smoke: every function configures and renders without panic
        // (debug build catches integer trouble)
        for f in 0..12 {
            let mut ch = Channel::new(48_000.0, 0x1234);
            ch.configure(f, [0.3, 0.6, 0.4, 0.8]);
            let mut gates = vec![GATE_FLAG_LOW; 512];
            gates[0] = super::super::dsp::GATE_FLAG_HIGH | super::super::dsp::GATE_FLAG_RISING;
            for g in gates.iter_mut().take(256).skip(1) {
                *g = super::super::dsp::GATE_FLAG_HIGH;
            }
            gates[256] = super::super::dsp::GATE_FLAG_FALLING;
            let mut out = vec![0i16; 512];
            for _ in 0..20 {
                ch.process(f, &gates, &mut out);
            }
        }
    }
}
