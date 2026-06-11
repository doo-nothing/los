//! # The swarm — a CS-80-flavored brass/pad voice
//!
//! Seven detuned sawtooths drifting against each other, through a
//! resonant ladder filter that *swells* open on every note — the
//! Vangelis move. Three of those swarms run as a paraphonic stack so a
//! single sequencer track plays chords: pick a spread (octaves, a
//! fifth, a minor triad…) and every incoming note becomes three swarm
//! notes fanned across the stereo field.
//!
//! The audio-rate core (the saw bank + ladder) is Faust —
//! `swarm/swarm.dsp`, generated into `swarm/swarm_gen.rs` by `just
//! dsp` (docs/writing-dsp.md). Rust owns everything musical: note
//! events, the chord table, glide, the filter-swell envelope, panning,
//! amplitude. The swell envelope is also published on the modbus as
//! `swarm/N/swl`, so the bloom that opens the filter can open other
//! things too.
//!
//! Like the other voices: `amp` unbound = a drone by explicit choice,
//! bound-but-dead = silence (a vanished envelope must never turn the
//! pad into a wall). `notes` unbound = play every track.

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

use crate::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

/// The generated Faust core: one swarm note (7 saws → ladder).
/// Regenerate with `just dsp`; the output is committed so building los
/// never needs the Faust compiler.
#[allow(
    clippy::all,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    unused_parens,
    unused_variables,
    unused_mut,
    dead_code
)]
pub mod core {
    use crate::faust::*;
    include!("swarm/swarm_gen.rs");
}

// Faust assigns ParamIndex in build_user_interface order (alphabetical
// for a flat UI). Pinned here and verified by `core_param_indices`.
const P_CUTOFF: i32 = 0;
const P_DETUNE: i32 = 1;
const P_FREQ: i32 = 2;
const P_LEVEL: i32 = 3;
const P_RES: i32 = 4;

/// Paraphonic depth: chord tones, swarm cores, pan positions.
const TONES: usize = 3;

/// Chord spreads, cycled on the chord row. Semitone offsets applied to
/// the incoming note, one per tone. Tone 0 sits center stage; keep it
/// on the root so the bass of the chord doesn't wander off-axis.
const CHORDS: [(&str, [f32; TONES]); 8] = [
    ("uni", [0.0, 0.0, 0.0]),
    ("oct", [0.0, -12.0, 12.0]),
    ("5th", [0.0, 7.0, 12.0]),
    ("sus4", [0.0, 5.0, 7.0]),
    ("min", [0.0, 3.0, 7.0]),
    ("maj", [0.0, 4.0, 7.0]),
    ("min7", [0.0, 3.0, 10.0]),
    ("maj7", [0.0, 4.0, 11.0]),
];

/// Constant-power-ish pan gains (L, R) per tone: root center, the
/// upper tones fanned. The spread is fixed — width-as-a-param wasn't
/// earning its row.
const PAN: [(f32, f32); TONES] = [(0.707, 0.707), (0.84, 0.44), (0.44, 0.84)];

/// Post-sum scale: three tones at pan ≈ unity each. Tuned live against
/// the stock voices — the saw bank averages /7 and the ladder eats more,
/// so this runs hotter than instinct suggests.
const MIX_SCALE: f32 = 0.9;

// ── parameters ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Param {
    /// Chord spread (index into CHORDS).
    Chord,
    /// Swarm detune: 0 = phase-locked unison, 1 = ±24-cent fan.
    Detune,
    /// Ladder cutoff, 0..1 over 60 Hz–12 kHz (exponential, in the core).
    Cutoff,
    /// Ladder resonance.
    Res,
    /// Filter swell: how far (and how slowly) the cutoff blooms open on
    /// each note. 0 = static filter; 1 = the full two-second brass rise.
    Swell,
    /// Portamento between notes, 0 = stepped, 1 = ~1.5 s slide.
    Glide,
    /// Output level.
    Level,
    /// Amplitude source (binding-only row, like the voice's amp).
    Amp,
    /// Note source: which sequencer track this swarm plays.
    Notes,
}

const ROWS: [Param; 9] = [
    Param::Chord,
    Param::Detune,
    Param::Cutoff,
    Param::Res,
    Param::Swell,
    Param::Glide,
    Param::Level,
    Param::Amp,
    Param::Notes,
];

/// Mod-bindable knobs, in `srcs[]` order. Amp and Notes bind too, but
/// they're pure routing rows (no manual value to replace) and live in
/// their own fields, voice-style.
const BINDABLE: [Param; 5] = [
    Param::Detune,
    Param::Cutoff,
    Param::Res,
    Param::Swell,
    Param::Level,
];

impl Param {
    fn label(self) -> &'static str {
        match self {
            Param::Chord => "chord",
            Param::Detune => "detune",
            Param::Cutoff => "cutoff",
            Param::Res => "res",
            Param::Swell => "swell",
            Param::Glide => "glide",
            Param::Level => "level",
            Param::Amp => "amp",
            Param::Notes => "notes",
        }
    }

    fn default_value(self) -> f32 {
        match self {
            Param::Chord => 1.0, // oct — huge and harmonically neutral
            Param::Detune => 0.35,
            Param::Cutoff => 0.55,
            Param::Res => 0.2,
            Param::Swell => 0.6,
            Param::Glide => 0.15,
            Param::Level => 0.8,
            Param::Amp | Param::Notes => 0.0,
        }
    }

    fn src_index(self) -> Option<usize> {
        BINDABLE.iter().position(|p| *p == self)
    }

    /// A plugged cable replaces the knob (los-wide convention). All five
    /// bindable knobs are plain 0..1.
    // not clamp(): clamp(NaN) is NaN, max/min sanitize it to 0.0
    #[allow(clippy::manual_clamp)]
    fn map_mod(self, v: f32) -> f32 {
        match self {
            // max/min instead of clamp: clamp(NaN) is NaN, and a NaN
            // from a stale modbus channel must die here, not ride into
            // the filter coefficients (NaN.max(0.0) is 0.0).
            Param::Detune | Param::Cutoff | Param::Res | Param::Swell | Param::Level => {
                v.max(0.0).min(1.0)
            }
            Param::Chord | Param::Glide | Param::Amp | Param::Notes => v,
        }
    }
}

// ── shared state ───────────────────────────────────────────────────────────

struct SwarmState {
    chord: usize,
    detune: f32,
    cutoff: f32,
    res: f32,
    swell: f32,
    glide: f32,
    level: f32,
    /// Last note (Hz) and gate from the event ring.
    freq: f32,
    gate: bool,
    velocity: f32,
    /// Bindings for the BINDABLE knobs.
    srcs: [Option<SourceAddr>; 5],
    resolved: [Option<usize>; 5],
    /// Effective values the audio thread last used (the bound-row ghost).
    eff: [f32; 5],
    /// Amplitude source. None = 1.0 (a drone by choice); bound but
    /// unresolvable = 0.0 (dead envelope = silence, never a wall).
    amp_src: Option<SourceAddr>,
    /// Which sequencer track to play. None = all tracks.
    notes_src: Option<SourceAddr>,
    /// Live values for the UI: swell envelope, amp, glided freq.
    env_now: f32,
    amp_now: f32,
    freq_now: f32,
    selected: usize,
}

impl SwarmState {
    fn new() -> Self {
        Self {
            chord: Param::Chord.default_value() as usize,
            detune: Param::Detune.default_value(),
            cutoff: Param::Cutoff.default_value(),
            res: Param::Res.default_value(),
            swell: Param::Swell.default_value(),
            glide: Param::Glide.default_value(),
            level: Param::Level.default_value(),
            freq: 110.0,
            gate: false,
            velocity: 0.0,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [
                Param::Detune.default_value(),
                Param::Cutoff.default_value(),
                Param::Res.default_value(),
                Param::Swell.default_value(),
                Param::Level.default_value(),
            ],
            amp_src: None,
            notes_src: None,
            env_now: 0.0,
            amp_now: 0.0,
            freq_now: 110.0,
            selected: 0,
        }
    }

    fn get(&self, p: Param) -> f32 {
        match p {
            Param::Chord => self.chord as f32,
            Param::Detune => self.detune,
            Param::Cutoff => self.cutoff,
            Param::Res => self.res,
            Param::Swell => self.swell,
            Param::Glide => self.glide,
            Param::Level => self.level,
            Param::Amp => self.amp_now,
            Param::Notes => 0.0,
        }
    }

    fn set(&mut self, p: Param, v: f32) {
        match p {
            Param::Chord => self.chord = (v as usize).min(CHORDS.len() - 1),
            Param::Detune => self.detune = v.clamp(0.0, 1.0),
            Param::Cutoff => self.cutoff = v.clamp(0.0, 1.0),
            Param::Res => self.res = v.clamp(0.0, 1.0),
            Param::Swell => self.swell = v.clamp(0.0, 1.0),
            Param::Glide => self.glide = v.clamp(0.0, 1.0),
            Param::Level => self.level = v.clamp(0.0, 1.0),
            Param::Amp | Param::Notes => {}
        }
    }

    fn effective(&self, p: Param, bus: Option<&ModulationBus>) -> f32 {
        match (p.src_index().and_then(|i| self.resolved[i]), bus) {
            (Some(ch), Some(bus)) => p.map_mod(bus.get(ch)),
            _ => self.get(p),
        }
    }

    fn current(&self) -> Param {
        ROWS[self.selected.min(ROWS.len() - 1)]
    }
}

/// Voice-rule amplitude: unbound = 1.0, bound = the source's value,
/// bound-but-orphaned = 0.0.
fn amp_level(bound: bool, resolved: Option<f32>) -> f32 {
    if bound {
        resolved.unwrap_or(0.0)
    } else {
        1.0
    }
}

/// Glide time in seconds for the knob value (squared taper — the
/// musical range is all in the bottom half).
fn glide_secs(glide: f32) -> f32 {
    glide * glide * 1.5
}

/// Swell rise time in seconds: 60 ms snap → 2 s brass bloom.
fn swell_rise_secs(swell: f32) -> f32 {
    0.06 + 1.9 * swell * swell
}

/// MIDI-ish note name for a frequency ("Eb3"), for the sounding-chord
/// readout. Approximate by design — drift and glide mean we're between
/// names most of the time.
fn note_name(freq: f32) -> String {
    if freq <= 0.0 {
        return String::from("--");
    }
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B",
    ];
    let midi = (69.0 + 12.0 * (freq / 440.0).log2()).round() as i32;
    let name = NAMES[midi.rem_euclid(12) as usize];
    format!("{}{}", name, midi.div_euclid(12) - 1)
}

// ── undo ───────────────────────────────────────────────────────────────────
//
// Slots: 0..9 the rows in ROWS order, 10+i the five knob bindings,
// 20/21 the amp and notes bindings.

const SRC_SLOT_BASE: usize = 10;
const AMP_SLOT: usize = 20;
const NOTES_SLOT: usize = 21;

impl crate::undo::ParamUndo for SwarmState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        match slot {
            AMP_SLOT => return Some(V::Src(self.amp_src.as_ref().map(|a| a.to_string()))),
            NOTES_SLOT => return Some(V::Src(self.notes_src.as_ref().map(|a| a.to_string()))),
            _ => {}
        }
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            if i < self.srcs.len() {
                return Some(V::Src(self.srcs[i].as_ref().map(|a| a.to_string())));
            }
        }
        let p = *ROWS.get(slot)?;
        Some(match p {
            Param::Chord => V::Usize(self.chord),
            _ => V::F32(self.get(p)),
        })
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        match (slot, &value) {
            (AMP_SLOT, V::Src(v)) => {
                self.amp_src = v.as_deref().and_then(SourceAddr::parse);
                return;
            }
            (NOTES_SLOT, V::Src(v)) => {
                self.notes_src = v.as_deref().and_then(SourceAddr::parse);
                return;
            }
            _ => {}
        }
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            if let (Some(s), V::Src(v)) = (self.srcs.get_mut(i), &value) {
                *s = v.as_deref().and_then(SourceAddr::parse);
                self.resolved[i] = None;
            }
            return;
        }
        let Some(p) = ROWS.get(slot).copied() else {
            return;
        };
        match (p, value) {
            (Param::Chord, V::Usize(v)) => self.chord = v.min(CHORDS.len() - 1),
            (_, V::F32(v)) => self.set(p, v),
            _ => {}
        }
    }
}

// ── persistence ────────────────────────────────────────────────────────────

fn snapshot_params(s: &SwarmState) -> state::SwarmParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::SwarmParams {
        format: state::STATE_FORMAT,
        chord: Some(CHORDS[s.chord].0.to_string()),
        detune: Some(s.detune),
        cutoff: Some(s.cutoff),
        res: Some(s.res),
        swell: Some(s.swell),
        glide: Some(s.glide),
        level: Some(s.level),
        freq: Some(s.freq),
        gate: Some(s.gate),
        detune_src: src(0),
        cutoff_src: src(1),
        res_src: src(2),
        swell_src: src(3),
        level_src: src(4),
        amp_src: s.amp_src.as_ref().map(|a| a.to_string()),
        notes_src: s.notes_src.as_ref().map(|a| a.to_string()),
    }
}

fn apply_params(s: &mut SwarmState, p: &state::SwarmParams) {
    if let Some(ref name) = p.chord {
        if let Some(i) = CHORDS.iter().position(|(n, _)| n == name) {
            s.chord = i;
        }
    }
    if let Some(v) = p.detune {
        s.set(Param::Detune, v);
    }
    if let Some(v) = p.cutoff {
        s.set(Param::Cutoff, v);
    }
    if let Some(v) = p.res {
        s.set(Param::Res, v);
    }
    if let Some(v) = p.swell {
        s.set(Param::Swell, v);
    }
    if let Some(v) = p.glide {
        s.set(Param::Glide, v);
    }
    if let Some(v) = p.level {
        s.set(Param::Level, v);
    }
    if let Some(v) = p.freq {
        s.freq = v;
        s.freq_now = v;
    }
    if let Some(v) = p.gate {
        s.gate = v;
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.detune_src),
        parse(&p.cutoff_src),
        parse(&p.res_src),
        parse(&p.swell_src),
        parse(&p.level_src),
    ];
    s.amp_src = parse(&p.amp_src);
    s.notes_src = parse(&p.notes_src);
    s.resolved = Default::default();
}

// ── the audio thread ───────────────────────────────────────────────────────

const FALLBACK_RATE: f32 = 48_000.0;

fn audio_thread(state: Arc<Mutex<SwarmState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_swarm_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating SHM audio ringbuffer")?;

    // One modbus channel: the swell envelope, published as swarm/N/swl.
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("swarm", instance, Some(&shm_name), 1)?;
    let mod_base = manifest.claimed_base();

    // Swarm shares the top of the voice consumer range (swarm N reads
    // slot 7−N) — expanding the event ring's consumer table is an SHM
    // layout bump, and nobody runs eight voices and a swarm. Documented
    // in shm::consumer_id.
    let consumer_id = crate::shm::consumer_id("swarm", instance);
    let mut events = EventRingbuf::open(consumer_id).ok();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();

    let channels = ringbuf.channels() as usize;
    let slot_frames = ringbuf.slot_frames() as usize;
    let mut block = vec![0.0_f32; ringbuf.slot_len()];

    let mut transport = ShmTransport::open().ok();
    let rate_of = |t: &Option<ShmTransport>| {
        t.as_ref()
            .map(|t| t.sample_rate() as f32)
            .filter(|r| *r > 0.0)
            .unwrap_or(FALLBACK_RATE)
    };
    let mut sample_rate = rate_of(&transport);
    let mut slot_duration =
        Duration::from_nanos((slot_frames as f64 / sample_rate as f64 * 1e9) as u64);

    // The paraphonic stack: one core per chord tone.
    let mut cores: Vec<core::Swarm> = (0..TONES).map(|_| core::Swarm::new()).collect();
    let init_cores = |cores: &mut Vec<core::Swarm>, rate: f32| {
        for c in cores.iter_mut() {
            c.init(rate as i32);
            // Level lives in Rust (per-sample smoothing); the core's own
            // slider stays wide open.
            c.set_param(crate::faust::ParamIndex(P_LEVEL), 1.0);
        }
    };
    init_cores(&mut cores, sample_rate);

    let mut tone = vec![0.0_f32; slot_frames];
    let mut freq_smooth = 110.0_f32; // glided base note
    let mut env = 0.0_f32; // swell envelope
    let mut gain_smooth = 0.0_f32; // per-sample amp smoothing state
    let mut ch_amp: Option<usize> = None;
    let mut note_filter: Option<u8> = None;
    let mut blocks: u64 = 0;

    loop {
        let tick = Instant::now();

        if blocks.is_multiple_of(128) {
            if transport.is_none() {
                transport = ShmTransport::open().ok();
            }
            if events.is_none() {
                events = EventRingbuf::open(consumer_id).ok();
            }
            let now_rate = rate_of(&transport);
            if (now_rate - sample_rate).abs() > 0.5 {
                sample_rate = now_rate;
                slot_duration =
                    Duration::from_nanos((slot_frames as f64 / sample_rate as f64 * 1e9) as u64);
                // Faust cores bake the rate into coefficients at init.
                cores = (0..TONES).map(|_| core::Swarm::new()).collect();
                init_cores(&mut cores, sample_rate);
            }
            let entries = manifest.entries();
            let mut s = state.lock().unwrap();
            for i in 0..s.srcs.len() {
                s.resolved[i] = s.srcs[i]
                    .as_ref()
                    .and_then(|a| routing::resolve(&entries, a));
            }
            ch_amp = s
                .amp_src
                .as_ref()
                .and_then(|a| routing::resolve(&entries, a));
            note_filter = s.notes_src.as_ref().and_then(routing::note_source_track);
            // who's-listening markers for the sequencer UI
            let mask = s
                .resolved
                .iter()
                .flatten()
                .chain(ch_amp.iter())
                .filter(|&&c| c < 64)
                .fold(0u64, |m, &c| m | (1 << c));
            let notes = note_filter.filter(|&t| t < 8).map_or(0u8, |t| 1 << t);
            manifest.publish_consumes(mask, notes);
        }

        // Drain note events (with the track filter, voice-style).
        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                if let Some(t) = note_filter {
                    if event.source != t {
                        continue;
                    }
                }
                let mut s = state.lock().unwrap();
                if event.is_note_on() {
                    if event.value.is_finite() && event.value > 0.0 {
                        s.freq = event.value;
                    }
                    s.velocity = event.param as f32 / 127.0;
                    s.gate = true;
                } else if event.is_note_off() {
                    s.gate = false;
                }
            }
        }

        // Snapshot effective params under one short lock.
        let (chord, detune, cutoff, res, swell, glide, level, freq, gate, velocity, amp_bound) = {
            let mut s = state.lock().unwrap();
            let bus = modbus.as_ref();
            let eff = (
                s.chord,
                s.effective(Param::Detune, bus),
                s.effective(Param::Cutoff, bus),
                s.effective(Param::Res, bus),
                s.effective(Param::Swell, bus),
                s.glide,
                s.effective(Param::Level, bus),
                s.freq,
                s.gate,
                // gate on but no note_on seen yet (fresh session): sound
                // anyway, exactly like voice.rs
                if s.gate && s.velocity < 0.001 {
                    1.0
                } else {
                    s.velocity
                },
                s.amp_src.is_some(),
            );
            s.eff = [eff.1, eff.2, eff.3, eff.4, eff.6];
            eff
        };

        let amp = amp_level(
            amp_bound,
            ch_amp.and_then(|c| modbus.as_ref().map(|m| m.get(c))),
        );

        let block_dt = slot_frames as f32 / sample_rate;

        // Glide: one-pole toward the incoming note.
        let g_secs = glide_secs(glide);
        let g_coeff = if g_secs <= 0.0 {
            1.0
        } else {
            1.0 - (-block_dt / g_secs).exp()
        };
        freq_smooth += (freq - freq_smooth) * g_coeff;

        // The swell envelope: blooms open while the gate holds, relaxes
        // on release. This is the brass.
        let tau = if gate {
            swell_rise_secs(swell)
        } else {
            0.45
        };
        let target = if gate { 1.0 } else { 0.0 };
        env += (target - env) * (1.0 - (-block_dt / tau).exp());

        // The swell rides the cutoff down-then-up: at swell 0 the filter
        // sits at the knob; at swell 1 each note sweeps from closed up
        // to the knob as the envelope rises.
        let cutoff_now = cutoff * (1.0 - swell * (1.0 - env));

        // Publish the bloom for other modules (swarm/N/swl).
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            bus.set(base, env);
        }
        if let Ok(mut s) = state.lock() {
            s.env_now = env;
            s.amp_now = amp;
            s.freq_now = freq_smooth;
        }

        // Render the stack and fan it across the field.
        block.fill(0.0);
        let spread = CHORDS[chord].1;
        for (i, c) in cores.iter_mut().enumerate() {
            let f = (freq_smooth * (spread[i] / 12.0).exp2()).clamp(20.0, 4000.0);
            c.set_param(crate::faust::ParamIndex(P_FREQ), f);
            c.set_param(crate::faust::ParamIndex(P_DETUNE), detune);
            c.set_param(crate::faust::ParamIndex(P_CUTOFF), cutoff_now);
            c.set_param(crate::faust::ParamIndex(P_RES), res);
            let ins: [&[f32]; 0] = [];
            let mut outs = [&mut tone[..]];
            c.compute(slot_frames, &ins, &mut outs);
            let (gl, gr) = PAN[i];
            for (frame, s) in tone.iter().enumerate() {
                block[frame * channels] += s * gl;
                if channels > 1 {
                    block[frame * channels + 1] += s * gr;
                }
            }
        }

        // Per-sample smoothed gain (kills block-rate zipper on fast amp
        // envelopes — same trick as voice.rs, ~0.7 ms).
        let gain_target = amp * velocity * level * MIX_SCALE;
        let g_alpha = 1.0 - (-1.0 / (0.0007 * sample_rate)).exp();
        for frame in 0..slot_frames {
            gain_smooth += (gain_target - gain_smooth) * g_alpha;
            for ch in 0..channels {
                block[frame * channels + ch] *= gain_smooth;
            }
        }

        // NaN watchdog: a poisoned ladder latches NaN in its state
        // forever. Ship silence for this block and rebuild the cores —
        // self-healing beats a permanently dead voice.
        if block.iter().any(|s| !s.is_finite()) {
            block.fill(0.0);
            cores = (0..TONES).map(|_| core::Swarm::new()).collect();
            init_cores(&mut cores, sample_rate);
        }
        while ringbuf.write(&block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }

        blocks += 1;
        let elapsed = tick.elapsed();
        if elapsed < slot_duration {
            thread::sleep(slot_duration - elapsed);
        }
    }
}

// ── rendering ──────────────────────────────────────────────────────────────

fn norm(p: Param, v: f32) -> f32 {
    match p {
        Param::Chord => v / (CHORDS.len() - 1) as f32,
        Param::Detune
        | Param::Cutoff
        | Param::Res
        | Param::Swell
        | Param::Glide
        | Param::Level
        | Param::Amp => v.clamp(0.0, 1.0),
        Param::Notes => 0.0,
    }
}

fn param_text(p: Param, v: f32) -> String {
    match p {
        Param::Chord => CHORDS[(v as usize).min(CHORDS.len() - 1)].0.to_string(),
        Param::Detune | Param::Cutoff | Param::Res | Param::Level | Param::Amp => {
            format!("{:.0}%", v * 100.0)
        }
        Param::Swell => format!("{:.0}% · {:.2}s", v * 100.0, swell_rise_secs(v)),
        Param::Glide => format!("{:.2}s", glide_secs(v)),
        Param::Notes => String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &SwarmState,
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
            "SWARM",
            &format!("brass {}", instance),
            "",
            w,
        ));

        // The sounding chord, by name, plus the swell as a live meter —
        // a pad you can read: "Eb2 Bb2 Eb3 ▆" is the whole story.
        let spread = CHORDS[s.chord].1;
        let mut spans = vec![Span::styled("  ".to_string(), theme::chrome())];
        for (i, semis) in spread.iter().enumerate() {
            let f = s.freq_now * (semis / 12.0).exp2();
            let midi = (69.0 + 12.0 * (f.max(1.0) / 440.0).log2()).clamp(0.0, 127.0) as u8;
            let hue = theme::pitch_color(midi);
            let style = if s.gate {
                theme::signal(hue)
            } else {
                theme::dim()
            };
            spans.push(Span::styled(format!("{:<5}", note_name(f)), style));
            let _ = i;
        }
        spans.push(Span::styled(" swl ".to_string(), theme::chrome()));
        spans.push(Span::styled(
            theme::meter_char(s.env_now).to_string(),
            theme::signal(theme::cv_ramp(s.env_now)),
        ));
        spans.push(Span::styled(
            format!(" {}", if s.gate { "●" } else { "○" }),
            if s.gate { theme::value() } else { theme::dim() },
        ));
        lines.push(Line::from(spans));
        lines.push(Line::from(""));

        let bar_w = theme::bar_width(w, 30);
        for (row, p) in ROWS.iter().enumerate() {
            let selected = row == s.selected;
            let label_style = if selected {
                theme::selected()
            } else {
                theme::chrome()
            };
            let mut spans: Vec<Span> =
                vec![Span::styled(format!(" {:<7}", p.label()), label_style)];

            // The routing rows render as cables, not bars.
            if matches!(p, Param::Amp | Param::Notes) {
                let (addr, fallback) = match p {
                    Param::Amp => (s.amp_src.as_ref(), "(unbound · drone)"),
                    _ => (s.notes_src.as_ref(), "(all tracks)"),
                };
                match addr {
                    Some(a) => {
                        let hue = routing::cable_color(entries, a);
                        let extra = if *p == Param::Amp {
                            format!(" ▸{:.0}%", s.amp_now * 100.0)
                        } else {
                            String::new()
                        };
                        spans.push(Span::styled(
                            format!("{} {}{}", theme::BIND, a, extra),
                            theme::signal(hue),
                        ));
                    }
                    None => spans.push(Span::styled(fallback.to_string(), theme::dim())),
                }
                lines.push(Line::from(spans));
                continue;
            }

            let bound = p.src_index().is_some_and(|i| s.srcs[i].is_some());
            let shown = match p.src_index() {
                Some(i) if bound => s.eff[i],
                _ => s.get(*p),
            };
            let hue = p
                .src_index()
                .and_then(|i| s.srcs[i].as_ref())
                .map(|a| routing::cable_color(entries, a));
            spans.extend(theme::bar(
                norm(*p, shown),
                None,
                bar_w,
                hue.unwrap_or_else(theme::cv),
            ));
            let vstyle = if selected {
                theme::selected()
            } else if let Some(hue) = hue {
                theme::signal(hue)
            } else {
                theme::value()
            };
            let mark = if bound { theme::BIND } else { ' ' };
            spans.push(Span::styled(
                format!(" {}{}", mark, param_text(*p, shown)),
                vstyle,
            ));
            if let Some(addr) = p.src_index().and_then(|i| s.srcs[i].as_ref()) {
                spans.push(Span::styled(format!("  ◂ {}", addr), theme::dim()));
            }
            lines.push(Line::from(spans));
        }

        theme::anchor_bottom(&mut lines, h, 2);
        lines.push(theme::rule(w));
        lines.push(theme::status("NORMAL", overlay.unwrap_or(""), "", w));
        f.render_widget(Paragraph::new(lines), area);

        if show_help {
            let help = Paragraph::new(vec![
                Line::from("━━━ SWARM · seven saws and a ladder ━━━"),
                Line::from(""),
                Line::from("  j/k        Select param · h/l adjust (H/L coarse)"),
                Line::from("  chord      Spread: uni oct 5th sus4 min maj min7 maj7"),
                Line::from("  swell      The CS-80 move: cutoff blooms per note"),
                Line::from("  glide      Portamento between notes"),
                Line::from("  @ / x      Bind / unbind (amp, notes, any knob)"),
                Line::from("  0          Reset · u/^r undo · :w/:e patches"),
                Line::from(""),
                Line::from("Three detuned-saw swarms through a resonant ladder,"),
                Line::from("fanned L·C·R as a chord. The swell envelope is"),
                Line::from("published as swarm/N/swl — patch the bloom anywhere."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" SWARM ", theme::chrome_hi())),
            );
            f.render_widget(help, area);
        }

        if let Some((rows, sel)) = picker {
            let ph = (rows.len() as u16 + 2).min(area.height);
            let pw = rows.iter().map(|r| r.len()).max().unwrap_or(10).max(20) as u16 + 4;
            let r = ratatui::layout::Rect::new(
                (area.width.saturating_sub(pw)) / 2,
                (area.height.saturating_sub(ph)) / 2,
                pw.min(area.width),
                ph,
            );
            f.render_widget(ratatui::widgets::Clear, r);
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
                    .title(Span::styled(" bind param ", theme::chrome_hi())),
            );
            f.render_widget(list, r);
        }
    })?;
    Ok(())
}

// ── editing helpers ────────────────────────────────────────────────────────

fn adjust_param(s: &mut SwarmState, steps: i32, coarse: bool) {
    use crate::keys::step_f32;
    let p = s.current();
    let v = s.get(p);
    let new = match p {
        Param::Chord => crate::keys::cycle(s.chord, steps, CHORDS.len()) as f32,
        Param::Detune
        | Param::Cutoff
        | Param::Res
        | Param::Swell
        | Param::Glide
        | Param::Level => step_f32(v, steps, 0.01, coarse, 0.0, 1.0),
        Param::Amp | Param::Notes => return, // routing rows: @ and x only
    };
    s.set(p, new);
}

/// Undo slot for the current row's *binding*, when it has one.
fn binding_slot(p: Param) -> Option<usize> {
    match p {
        Param::Amp => Some(AMP_SLOT),
        Param::Notes => Some(NOTES_SLOT),
        _ => p.src_index().map(|i| SRC_SLOT_BASE + i),
    }
}

/// Read or write the current row's binding through one accessor so the
/// `@`/`x` handlers can't diverge between the three binding kinds.
fn binding_mut(s: &mut SwarmState, p: Param) -> Option<&mut Option<SourceAddr>> {
    match p {
        Param::Amp => Some(&mut s.amp_src),
        Param::Notes => Some(&mut s.notes_src),
        _ => {
            let i = p.src_index()?;
            Some(&mut s.srcs[i])
        }
    }
}

// ── entry point ────────────────────────────────────────────────────────────

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("swarm", instance);

    let shared = Arc::new(Mutex::new(SwarmState::new()));

    if let Ok(p) = state::load_module_state::<state::SwarmParams>("swarm", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    // 8 MB stack: Faust cores carry their state inline and debug builds
    // don't elide the construction copies (the delay's lesson).
    let audio_builder = thread::Builder::new()
        .name(String::from("swarm-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = audio_builder.spawn(move || {
        if let Err(e) = audio_thread(audio_state, instance) {
            eprintln!("[swarm {}] audio thread error: {}", instance, e);
        }
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
            let _ = state::save_module_state("swarm", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::SwarmParams>("swarm", instance) {
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
                    let steps = if m.kind == MouseEventKind::ScrollUp {
                        1
                    } else {
                        -1
                    };
                    use crate::undo::ParamUndo;
                    let mut s = shared.lock().unwrap();
                    let slot = s.selected;
                    let old = s.get_param(slot);
                    adjust_param(&mut s, steps, false);
                    let new = s.get_param(slot);
                    if let (Some(old), Some(new)) = (old, new) {
                        history.record(slot, "Adjust", old, new);
                    }
                }
                MouseEventKind::Down(_) => {
                    // Rows start under header + chord readout + blank.
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
                let p = s.current();
                if let Some(slot) = binding_slot(p) {
                    let old = s.get_param(slot);
                    if let Some(b) = binding_mut(&mut s, p) {
                        *b = addr.clone();
                    }
                    if let Some(i) = p.src_index() {
                        s.resolved[i] = None;
                    }
                    if let Some(old) = old {
                        history.record(
                            slot,
                            "Bind",
                            old,
                            ParamValue::Src(addr.map(|a| a.to_string())),
                        );
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
                    ExCommand::Edit(name) => match state::load_patch::<state::SwarmParams>(&name) {
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
            let _ = state::save_module_state("swarm", instance, &params);
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
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                if matches!(s.current(), Param::Amp | Param::Notes) {
                    ex_msg = Some(format!("{}: @ binds, x unbinds", s.current().label()));
                    continue;
                }
                let slot = s.selected;
                let old = s.get_param(slot);
                adjust_param(&mut s, steps, coarse);
                let new = s.get_param(slot);
                if let (Some(old), Some(new)) = (old, new) {
                    history.record(slot, "Adjust", old, new);
                }
            }
            KeyCode::Char('0') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let p = s.current();
                if matches!(p, Param::Amp | Param::Notes) {
                    continue; // unbinding is x, deliberately
                }
                let slot = s.selected;
                let old = s.get_param(slot);
                s.set(p, p.default_value());
                if let Some(old) = old {
                    let new = s
                        .get_param(slot)
                        .unwrap_or(ParamValue::F32(p.default_value()));
                    history.record(slot, "Reset", old, new);
                }
            }
            KeyCode::Char('@') => {
                count.clear();
                let s = shared.lock().unwrap();
                let p = s.current();
                if binding_slot(p).is_some() {
                    let current = match p {
                        Param::Amp => s.amp_src.clone(),
                        Param::Notes => s.notes_src.clone(),
                        _ => p.src_index().and_then(|i| s.srcs[i].clone()),
                    };
                    drop(s);
                    let sources = Manifest::open()
                        .map(|m| routing::live_sources(&m.entries()))
                        .unwrap_or_default();
                    picker.open(sources, current.as_ref());
                } else {
                    ex_msg = Some(format!("{} is not bindable", p.label()));
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let p = s.current();
                if let Some(slot) = binding_slot(p) {
                    let had = binding_mut(&mut s, p).is_some_and(|b| b.is_some());
                    if had {
                        let old = s.get_param(slot);
                        if let Some(b) = binding_mut(&mut s, p) {
                            *b = None;
                        }
                        if let Some(i) = p.src_index() {
                            s.resolved[i] = None;
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

/// `:set <key> <value>` — chord by name, the knobs numerically, amp and
/// notes by source address (`:set amp envelope/0/ch1`, `:set notes -`
/// to unbind).
fn ex_set(
    s: &mut SwarmState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::{ParamUndo, ParamValue};
    let Some(p) = ROWS.iter().find(|p| p.label() == key).copied() else {
        return format!(
            "Unknown setting: {} (chord detune cutoff res swell glide level amp notes)",
            key
        );
    };
    if matches!(p, Param::Amp | Param::Notes) {
        let slot = binding_slot(p).unwrap_or(AMP_SLOT);
        let old = s.get_param(slot);
        let addr = (value != "-").then(|| SourceAddr::parse(value)).flatten();
        if value != "-" && addr.is_none() {
            return format!("{}: not a source address: {}", key, value);
        }
        let text = addr.as_ref().map(|a| a.to_string());
        if let Some(b) = binding_mut(s, p) {
            *b = addr;
        }
        if let Some(old) = old {
            history.record(slot, "Bind", old, ParamValue::Src(text.clone()));
        }
        return format!("{} = {}", key, text.unwrap_or_else(|| "(unbound)".into()));
    }
    let parsed = match p {
        Param::Chord => CHORDS
            .iter()
            .position(|(n, _)| *n == value)
            .map(|i| i as f32)
            .ok_or_else(|| {
                format!(
                    "chord: one of {}",
                    CHORDS.map(|(n, _)| n).join(" ")
                )
            }),
        _ => value
            .parse::<f32>()
            .map_err(|_| format!("{}: not a number: {}", key, value)),
    };
    match parsed {
        Ok(v) => {
            let slot = ROWS.iter().position(|r| *r == p).unwrap_or(0);
            let old = s.get_param(slot);
            s.set(p, v);
            let new = s.get_param(slot);
            if let (Some(old), Some(new)) = (old, new) {
                history.record(slot, "Set", old, new);
            }
            format!("{} = {}", p.label(), param_text(p, s.get(p)))
        }
        Err(m) => m,
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod probe {
    include!("/tmp/swarm_probe.rs");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_shape_and_param_indices() {
        // 0 in → 1 out: a generator, not an fx.
        assert_eq!(core::FAUST_INPUTS, 0);
        assert_eq!(core::FAUST_OUTPUTS, 1);
        // Pin the alphabetical ParamIndex assignment the constants assume.
        let mut map = crate::faust::ParamMap::default();
        core::Swarm::build_user_interface_static(&mut map);
        let idx = |name: &str| {
            map.params
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, i, _)| *i)
                .unwrap_or(-1)
        };
        assert_eq!(idx("cutoff"), P_CUTOFF);
        assert_eq!(idx("detune"), P_DETUNE);
        assert_eq!(idx("freq"), P_FREQ);
        assert_eq!(idx("level"), P_LEVEL);
        assert_eq!(idx("res"), P_RES);
    }

    #[test]
    fn core_makes_sound_and_filter_works() {
        let render = |cutoff: f32| -> f32 {
            let mut c = core::Swarm::new();
            c.init(48_000);
            c.set_param(crate::faust::ParamIndex(P_FREQ), 110.0);
            c.set_param(crate::faust::ParamIndex(P_CUTOFF), cutoff);
            c.set_param(crate::faust::ParamIndex(P_LEVEL), 1.0);
            let mut out = vec![0.0_f32; 64];
            let mut energy = 0.0;
            // run past si.smoo's settle
            for _ in 0..200 {
                let ins: [&[f32]; 0] = [];
                let mut outs = [&mut out[..]];
                c.compute(64, &ins, &mut outs);
                energy += out.iter().map(|s| s * s).sum::<f32>();
            }
            energy
        };
        let open = render(0.9);
        let closed = render(0.05);
        assert!(open > 1e-3, "open swarm is audible, got {}", open);
        assert!(open.is_finite() && closed.is_finite());
        // A 110 Hz saw through a nearly-closed ladder loses its harmonics:
        // meaningfully less energy than wide open.
        assert!(
            closed < open * 0.7,
            "ladder attenuates: closed {} vs open {}",
            closed,
            open
        );
    }

    #[test]
    fn chord_spreads_and_note_names() {
        // min over Eb2: Eb2 Gb2 Bb2
        let eb2 = 77.78_f32;
        let min = CHORDS.iter().find(|(n, _)| *n == "min").unwrap().1;
        let names: Vec<String> = min
            .iter()
            .map(|s| note_name(eb2 * (s / 12.0).exp2()))
            .collect();
        assert_eq!(names, ["Eb2", "F#2", "Bb2"]); // Gb spelled F#
        assert_eq!(note_name(440.0), "A4");
        assert_eq!(note_name(0.0), "--");
    }

    #[test]
    fn adjust_clamps_and_cycles() {
        let mut s = SwarmState::new();
        s.selected = 0; // chord cycles
        adjust_param(&mut s, 3, false);
        assert_eq!(s.chord, (Param::Chord.default_value() as usize + 3) % CHORDS.len());
        s.selected = 2; // cutoff
        adjust_param(&mut s, 1000, true);
        assert_eq!(s.cutoff, 1.0, "clamps at the top");
        s.selected = 7; // amp: routing row, adjust is a no-op
        adjust_param(&mut s, 5, false);
        assert_eq!(s.amp_src, None);
    }

    #[test]
    fn amp_follows_the_voice_rule() {
        assert_eq!(amp_level(false, None), 1.0, "unbound = drone by choice");
        assert_eq!(amp_level(true, None), 0.0, "orphaned binding = silence");
        assert_eq!(amp_level(true, Some(0.6)), 0.6);
    }

    #[test]
    fn undo_slots_round_trip() {
        use crate::undo::{ParamUndo, ParamValue as V};
        let mut s = SwarmState::new();
        s.set_param(0, V::Usize(4));
        assert_eq!(s.chord, 4);
        s.set_param(4, V::F32(0.9));
        assert!((s.swell - 0.9).abs() < 1e-6);
        s.set_param(SRC_SLOT_BASE + 1, V::Src(Some("envelope/0/ch2".into())));
        assert_eq!(
            s.srcs[1].as_ref().map(|a| a.to_string()),
            Some("envelope/0/ch2".into())
        );
        s.set_param(AMP_SLOT, V::Src(Some("envelope/1/ch1".into())));
        assert_eq!(
            s.amp_src.as_ref().map(|a| a.to_string()),
            Some("envelope/1/ch1".into())
        );
        s.set_param(NOTES_SLOT, V::Src(Some("sequencer/0/t3".into())));
        assert_eq!(
            s.notes_src.as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t3".into())
        );
        s.set_param(AMP_SLOT, V::Src(None));
        assert!(s.amp_src.is_none());
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = SwarmState::new();
        s.chord = 6; // min7
        s.swell = 0.9;
        s.glide = 0.4;
        s.amp_src = SourceAddr::parse("envelope/2/ch1");
        s.notes_src = SourceAddr::parse("sequencer/1/t2");
        s.srcs[1] = SourceAddr::parse("envelope/1/ch3"); // cutoff
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::SwarmParams = toml::from_str(&toml).expect("parses");
        let mut s2 = SwarmState::new();
        apply_params(&mut s2, &back);
        assert_eq!(s2.chord, 6);
        assert!((s2.swell - 0.9).abs() < 1e-6);
        assert_eq!(
            s2.amp_src.as_ref().map(|a| a.to_string()),
            Some("envelope/2/ch1".into())
        );
        assert_eq!(
            s2.notes_src.as_ref().map(|a| a.to_string()),
            Some("sequencer/1/t2".into())
        );
        assert_eq!(
            s2.srcs[1].as_ref().map(|a| a.to_string()),
            Some("envelope/1/ch3".into())
        );
        // empty file = defaults stand
        let empty: state::SwarmParams = toml::from_str("").expect("parses");
        let mut s3 = SwarmState::new();
        apply_params(&mut s3, &empty);
        assert_eq!(s3.chord, Param::Chord.default_value() as usize);
    }

    #[test]
    fn ex_set_parses_and_rejects() {
        let mut s = SwarmState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert_eq!(ex_set(&mut s, &mut h, "chord", "min7"), "chord = min7");
        assert!(ex_set(&mut s, &mut h, "chord", "dim").contains("one of"));
        assert_eq!(ex_set(&mut s, &mut h, "swell", "0.8"), "swell = 80% · 1.28s");
        assert!(ex_set(&mut s, &mut h, "cutoff", "wide").contains("not a number"));
        assert!(ex_set(&mut s, &mut h, "wow", "1").contains("Unknown setting"));
        assert!(ex_set(&mut s, &mut h, "amp", "envelope/0/ch1").contains("envelope/0/ch1"));
        assert_eq!(ex_set(&mut s, &mut h, "amp", "-"), "amp = (unbound)");
        assert!(s.amp_src.is_none());
        assert!(ex_set(&mut s, &mut h, "notes", "nonsense").contains("not a source"));
    }

    #[test]
    fn norm_and_text_cover_every_row() {
        for p in ROWS {
            let v = p.default_value();
            let n = norm(p, v);
            assert!((0.0..=1.0).contains(&n), "{:?} norm {} out of range", p, n);
        }
        assert_eq!(param_text(Param::Chord, 1.0), "oct");
        assert_eq!(param_text(Param::Detune, 0.35), "35%");
    }

    #[test]
    fn map_mod_kills_nan() {
        // clamp(NaN) is NaN — the mapping must sanitize, because one NaN
        // in a filter coefficient poisons the whole print bus downstream.
        for p in BINDABLE {
            assert_eq!(p.map_mod(f32::NAN), 0.0, "{:?} lets NaN through", p);
            assert_eq!(p.map_mod(f32::INFINITY), 1.0);
            assert_eq!(p.map_mod(2.5), 1.0);
        }
    }

    #[test]
    fn swell_and_glide_tapers() {
        assert!(swell_rise_secs(0.0) < 0.1, "swell 0 still snaps");
        assert!(swell_rise_secs(1.0) > 1.5, "swell 1 is the long bloom");
        assert_eq!(glide_secs(0.0), 0.0);
        assert!((glide_secs(1.0) - 1.5).abs() < 1e-6);
    }
}
