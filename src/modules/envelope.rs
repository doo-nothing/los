//! `los envelope` (alias `maths`) — a Make Noise Maths–inspired function
//! generator (docs/plans/maths.md).
//!
//! Up to [`MAX_CHANNELS`] channels, each a full function generator with
//! Rise/Fall times spanning 0.5 ms – 25 min, a Vari-Response curve
//! (log ↔ linear ↔ exponential, modeled as analog RC charge curves), a
//! per-channel attenuverter + DC offset, cycle mode, a signal input (slew
//! limiting / portamento), and triggers from notes, modbus rising edges
//! (self-patching: bind a trigger to `envelope/0/eoc`), or manual `t`.
//! Outputs: every channel, SUM/OR/AND/INV buses, EOR (end of rise, ch 1)
//! and EOC (end of cycle, last channel) gates — all claimable modbus
//! sources — plus an audio-rate output (DC-blocked attenuverted sum) so a
//! cycling channel is *audible* through the mixer.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{self, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{enable_raw_mode, EnterAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    style::Style,
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use crate::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

pub const MAX_CHANNELS: usize = 6;
const DEFAULT_CHANNELS: usize = 4;
/// Claimed modbus outputs: ch1..ch6, sum, or, and, inv, eor, eoc.
const CLAIMED_OUTPUTS: u32 = MAX_CHANNELS as u32 + 6;
const SAMPLE_RATE: f64 = 48000.0;
const BLOCK_SIZE: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Off,
    Rise,
    Sustain,
    Fall,
}

#[derive(Clone, Copy)]
struct EnvelopeChannel {
    stage: Stage,
    phase: f32,
    output: f32,
    // vactrol model components (pluck mode)
    pluck_fast: f32,
    pluck_slow: f32,
}

impl Default for EnvelopeChannel {
    fn default() -> Self {
        Self {
            stage: Stage::Off,
            phase: 0.0,
            output: 0.0,
            pluck_fast: 0.0,
            pluck_slow: 0.0,
        }
    }
}

/// What fires a channel: any note event, nothing (manual `t`/cycle only),
/// a specific sequencer track's notes, or — for any non-note source — a
/// rising edge on its modbus channel (the self-patching path).
#[derive(Clone, Debug, PartialEq)]
enum Trigger {
    Any,
    Off,
    Source(SourceAddr),
}

impl Trigger {
    /// Stable string form used in params/undo: absent = Any, "off" = Off.
    fn to_param(&self) -> Option<String> {
        match self {
            Trigger::Any => None,
            Trigger::Off => Some(String::from("off")),
            Trigger::Source(a) => Some(a.to_string()),
        }
    }

    fn from_param(s: Option<&str>) -> Self {
        match s {
            None => Trigger::Any,
            Some("off") => Trigger::Off,
            Some(a) => SourceAddr::parse(a)
                .map(Trigger::Source)
                .unwrap_or(Trigger::Any),
        }
    }
}

#[derive(Clone)]
struct ChannelParams {
    rise_param: f32, // 0.0-1.0 → 0.5ms to 25min (exponential taper)
    fall_param: f32,
    shape_param: f32, // 0.0 log (RC) … 0.5 linear … 1.0 exponential
    loop_mode: bool,
    attenuverter: f32, // -1.0 to 1.0
    offset: f32,       // -1.0 to 1.0, post-attenuverter DC offset
    /// Vactrol-style nonlinear fall (0 = vari-response fall, 1 = full
    /// Natural Gates tail). Decay becomes level-dependent: fast while hot,
    /// slowing as it cools.
    pluck: f32,
    /// Note-input semantics: false = **trig** (a note fires the full
    /// rise→fall transient; note_off is ignored — Maths' trigger input),
    /// true = **gate** (sustain at the top until note_off — the signal
    /// input held high). Default trig: a trigger should be a trigger.
    gate_mode: bool,
    // Receiver-side bindings (routing.rs source addresses).
    trigger: Trigger,
    /// When bound, the channel stops generating and slews this source
    /// (Maths' signal input: portamento / lag / CV smoothing).
    signal_src: Option<SourceAddr>,
    rise_src: Option<SourceAddr>,
    fall_src: Option<SourceAddr>,
    shape_src: Option<SourceAddr>,
    atten_src: Option<SourceAddr>,
    offset_src: Option<SourceAddr>,
    pluck_src: Option<SourceAddr>,
}

impl Default for ChannelParams {
    fn default() -> Self {
        Self {
            rise_param: 0.0,  // instant strike
            fall_param: 0.38, // ~145ms decay
            shape_param: 0.9, // strongly exponential: snappy spikes
            loop_mode: false,
            attenuverter: 1.0,
            offset: 0.0,
            pluck: 0.75, // deep vactrol snap+ring
            gate_mode: false,
            trigger: Trigger::Any,
            signal_src: None,
            rise_src: None,
            fall_src: None,
            shape_src: None,
            atten_src: None,
            offset_src: None,
            pluck_src: None,
        }
    }
}

// ── engine math ─────────────────────────────────────────────────────────────

/// Time-range constants: 0.5 ms to 25 minutes (Maths-spec).
const TIME_MIN: f32 = 0.0005;
const TIME_RANGE: f32 = 3_000_000.0; // TIME_MIN * RANGE = 1500s = 25min

/// Exponential parameter → seconds. 0.0 → instant (0s), then 0.5ms at the
/// first step up to 25min. Zero-attack envelopes are essential for plucks.
fn param_to_time(param: f32) -> f32 {
    let p = param.clamp(0.0, 1.0);
    if p <= 0.0 {
        0.0
    } else {
        TIME_MIN * TIME_RANGE.powf(p)
    }
}

/// Seconds → parameter (inverse of `param_to_time`).
fn time_to_param(time: f32) -> f32 {
    let t = time.clamp(TIME_MIN, TIME_MIN * TIME_RANGE);
    (t / TIME_MIN).ln() / TIME_RANGE.ln()
}

/// Parse a `:set rise`/`:set fall` value: "100ms", "2s", "1.5m", or a bare
/// 0–1 parameter. Returns the parameter.
fn parse_time_param(v: &str) -> Option<f32> {
    let v = v.trim();
    let (num, mult) = if let Some(n) = v.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = v.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = v.strip_suffix('m') {
        (n, 60.0)
    } else {
        let p: f32 = v.parse().ok()?;
        return (0.0..=1.0).contains(&p).then_some(p);
    };
    let t: f32 = num.trim().parse().ok()?;
    if t == 0.0 {
        return Some(0.0);
    }
    (t > 0.0).then(|| time_to_param(t * mult))
}

/// Display a time with auto units.
fn format_time(t: f32) -> String {
    if t <= 0.0 {
        return String::from("0 (instant)");
    }
    if t < 0.01 {
        format!("{:.2}ms", t * 1000.0)
    } else if t < 1.0 {
        format!("{:.0}ms", t * 1000.0)
    } else if t < 60.0 {
        format!("{:.2}s", t)
    } else {
        format!("{:.1}m", t / 60.0)
    }
}

/// Maths Vari-Response: one parameter morphs the segment curve from
/// logarithmic (RC charge — fast start, asymptotic end) through exactly
/// linear (0.5) to exponential (slow start, accelerating end). Modeled on
/// the analog curves: f(x) = (e^(τx) − 1) / (e^τ − 1), τ ∈ [−9, +9] —
/// wider than the hardware sweep for extreme staccato spike shapes.
fn vari_response(x: f32, shape: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    let tau = (shape.clamp(0.0, 1.0) - 0.5) * 18.0;
    if tau.abs() < 0.01 {
        x
    } else {
        (tau * x).exp_m1() / tau.exp_m1()
    }
}

/// Inverse of [`vari_response`]: the phase at which the curve outputs `y`.
/// Retriggers and cycle restarts use it to CONTINUE the rise from the
/// current level — output must never step down in one sample (the click).
fn vari_inverse(y: f32, shape: f32) -> f32 {
    let y = y.clamp(0.0, 1.0);
    let tau = (shape.clamp(0.0, 1.0) - 0.5) * 18.0;
    if tau.abs() < 0.01 {
        y
    } else {
        (1.0 + y * tau.exp_m1()).ln() / tau
    }
}

/// One sample of slew limiting toward `target` (signal-input mode).
/// Rate comes from the rise/fall time for the movement direction; the
/// response blends a constant-rate (linear) ramp with an RC proportional
/// approach on the log side. (Exp-side slew approximates linear.)
fn slew_step(out: f32, target: f32, dt: f32, rise_t: f32, fall_t: f32, shape: f32) -> f32 {
    let diff = target - out;
    if diff == 0.0 {
        return out;
    }
    let time = if diff > 0.0 { rise_t } else { fall_t };
    if time <= 0.0 {
        return target;
    }
    let time = time.max(TIME_MIN);
    let lin = dt / time; // full-scale per `time`
    let rc = (dt / (time * 0.2)) * diff.abs(); // τ ≈ time/5
    let rc_weight = ((0.5 - shape.clamp(0.0, 1.0)) * 2.0).max(0.0);
    let step = (lin + (rc - lin) * rc_weight).min(diff.abs());
    out + diff.signum() * step
}

/// One sample of vactrol-style decay (Natural Gates-inspired).
///
/// A struck vactrol doesn't decay on one time constant: it snaps quickly
/// through the first ~10 dB, then *rings out* on a much slower constant
/// ("vactrol memory"). We model two exponentially decaying components mixed
/// by the pluck amount: τ_fast = 0.1·fall (the snap, dropping ~10dB before
/// the ring takes over), τ_slow growing from 0.8·fall to 4.8·fall (the
/// ring), with the slow component's weight rising 0.1 → 0.35 as pluck
/// increases. Returns (fast, slow, output).
fn pluck_decay(fast: f32, slow: f32, dt: f32, fall_time: f32, pluck: f32) -> (f32, f32, f32) {
    let p = pluck.clamp(0.0, 1.0);
    let tf = (fall_time * 0.1).max(TIME_MIN);
    let ts = (fall_time * (0.8 + 4.0 * p)).max(TIME_MIN);
    let fast = (fast - dt * fast / tf).max(0.0);
    let slow = (slow - dt * slow / ts).max(0.0);
    let w = 0.1 + 0.25 * p;
    let out = (1.0 - w) * fast + w * slow;
    (fast, slow, out)
}

#[derive(Clone)]
struct EnvelopeState {
    channels: Vec<EnvelopeChannel>,
    params: Vec<ChannelParams>,
    current_channel: usize,
    gate: bool,
    events_received: u64,
    /// Modbus base channel claimed at registration (outputs write here).
    mod_base: Option<usize>,
}

impl Default for EnvelopeState {
    fn default() -> Self {
        let mut params = vec![ChannelParams::default(); DEFAULT_CHANNELS];
        // Default patch: odd channels are the voices' pluck envelopes
        // (ch1 <- seq t1, ch3 <- seq t3, matching each voice's amp
        // binding); even channels stay unwired, free for patching.
        for (i, p) in params.iter_mut().enumerate() {
            p.trigger = if i % 2 == 0 {
                SourceAddr::parse(&format!("sequencer/0/t{}", i + 1))
                    .map(Trigger::Source)
                    .unwrap_or(Trigger::Off)
            } else {
                Trigger::Off
            };
        }
        Self {
            channels: vec![EnvelopeChannel::default(); DEFAULT_CHANNELS],
            params,
            current_channel: 0,
            gate: false,
            events_received: 0,
            mod_base: None,
        }
    }
}

fn add_channel(s: &mut EnvelopeState) -> bool {
    if s.params.len() >= MAX_CHANNELS {
        return false;
    }
    s.params.push(ChannelParams::default());
    s.channels.push(EnvelopeChannel::default());
    s.current_channel = s.params.len() - 1;
    true
}

fn remove_channel(s: &mut EnvelopeState) -> bool {
    if s.params.len() <= 1 {
        return false;
    }
    let at = s.current_channel.min(s.params.len() - 1);
    s.params.remove(at);
    s.channels.remove(at);
    s.current_channel = s.current_channel.min(s.params.len() - 1);
    true
}

// ── rows / undo ─────────────────────────────────────────────────────────────

// Rows: 0 rise, 1 fall, 2 shape, 3 atten, 4 offset, 5 pluck, 6 signal, 7 trigger.
const NUM_ROWS: usize = 8;
const ROW_PLUCK: usize = 5;
const ROW_SIGNAL: usize = 6;
const ROW_TRIGGER: usize = 7;

/// Undo slots: ch*32 + row for values (0–4), loop (5), pluck (6), gate-mode (7);
/// ch*32 + 8 + n for the six bindings (rise/fall/shape/atten/signal/trigger).
const CH_SLOT_STRIDE: usize = 32;
const BIND_OFF: usize = 8;

impl crate::undo::ParamUndo for EnvelopeState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        let (ch, row) = (slot / CH_SLOT_STRIDE, slot % CH_SLOT_STRIDE);
        let p = self.params.get(ch)?;
        match row {
            0 => Some(V::F32(p.rise_param)),
            1 => Some(V::F32(p.fall_param)),
            2 => Some(V::F32(p.shape_param)),
            3 => Some(V::F32(p.attenuverter)),
            4 => Some(V::F32(p.offset)),
            5 => Some(V::Bool(p.loop_mode)),
            6 => Some(V::F32(p.pluck)),
            7 => Some(V::Bool(p.gate_mode)),
            r if (BIND_OFF..BIND_OFF + 8).contains(&r) => {
                let b = match r - BIND_OFF {
                    0 => p.rise_src.as_ref().map(|a| a.to_string()),
                    1 => p.fall_src.as_ref().map(|a| a.to_string()),
                    2 => p.shape_src.as_ref().map(|a| a.to_string()),
                    3 => p.atten_src.as_ref().map(|a| a.to_string()),
                    4 => p.signal_src.as_ref().map(|a| a.to_string()),
                    6 => p.offset_src.as_ref().map(|a| a.to_string()),
                    7 => p.pluck_src.as_ref().map(|a| a.to_string()),
                    _ => p.trigger.to_param(),
                };
                Some(V::Src(b))
            }
            _ => None,
        }
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        let (ch, row) = (slot / CH_SLOT_STRIDE, slot % CH_SLOT_STRIDE);
        let Some(p) = self.params.get_mut(ch) else {
            return;
        };
        match (row, value) {
            (0, V::F32(v)) => p.rise_param = v,
            (1, V::F32(v)) => p.fall_param = v,
            (2, V::F32(v)) => p.shape_param = v,
            (3, V::F32(v)) => p.attenuverter = v,
            (4, V::F32(v)) => p.offset = v,
            (5, V::Bool(v)) => p.loop_mode = v,
            (6, V::F32(v)) => p.pluck = v,
            (7, V::Bool(v)) => p.gate_mode = v,
            (r, V::Src(a)) if (BIND_OFF..BIND_OFF + 8).contains(&r) => {
                if r - BIND_OFF == 5 {
                    p.trigger = Trigger::from_param(a.as_deref());
                } else {
                    let addr = a.as_deref().and_then(SourceAddr::parse);
                    match r - BIND_OFF {
                        0 => p.rise_src = addr,
                        1 => p.fall_src = addr,
                        2 => p.shape_src = addr,
                        3 => p.atten_src = addr,
                        6 => p.offset_src = addr,
                        7 => p.pluck_src = addr,
                        _ => p.signal_src = addr,
                    }
                }
            }
            _ => {}
        }
    }
}

/// The undo slot for the selected UI row on a channel.
fn row_slot(ch: usize, row: usize) -> usize {
    match row {
        0..=4 => ch * CH_SLOT_STRIDE + row,
        ROW_PLUCK => ch * CH_SLOT_STRIDE + 6,
        ROW_SIGNAL => ch * CH_SLOT_STRIDE + BIND_OFF + 4,
        _ => ch * CH_SLOT_STRIDE + BIND_OFF + 5,
    }
}

/// Adjust a value row on the current channel (doctrine: h/l fine, H/L ×10).
fn adjust(s: &mut EnvelopeState, row: usize, steps: i32, coarse: bool) {
    use crate::keys::step_f32;
    let ch = s.current_channel;
    let p = &mut s.params[ch];
    match row {
        0 => p.rise_param = step_f32(p.rise_param, steps, 0.005, coarse, 0.0, 1.0),
        1 => p.fall_param = step_f32(p.fall_param, steps, 0.005, coarse, 0.0, 1.0),
        2 => p.shape_param = step_f32(p.shape_param, steps, 0.005, coarse, 0.0, 1.0),
        3 => p.attenuverter = step_f32(p.attenuverter, steps, 0.05, coarse, -1.0, 1.0),
        4 => p.offset = step_f32(p.offset, steps, 0.05, coarse, -1.0, 1.0),
        ROW_PLUCK => p.pluck = step_f32(p.pluck, steps, 0.05, coarse, 0.0, 1.0),
        _ => {} // signal/trigger rows are binding-only (@ opens the picker)
    }
}

// ── params snapshot/apply ───────────────────────────────────────────────────

fn snapshot_params(s: &EnvelopeState) -> state::EnvelopeParams {
    state::EnvelopeParams {
        format: state::STATE_FORMAT,
        channels: s
            .params
            .iter()
            .map(|p| state::EnvelopeChannelParams {
                rise: p.rise_param,
                fall: p.fall_param,
                shape: p.shape_param,
                loop_mode: p.loop_mode,
                attenuverter: p.attenuverter,
                offset: p.offset,
                pluck: p.pluck,
                gate_mode: p.gate_mode,
                signal_src: p.signal_src.as_ref().map(|a| a.to_string()),
                trigger_src: p.trigger.to_param(),
                rise_src: p.rise_src.as_ref().map(|a| a.to_string()),
                fall_src: p.fall_src.as_ref().map(|a| a.to_string()),
                shape_src: p.shape_src.as_ref().map(|a| a.to_string()),
                atten_src: p.atten_src.as_ref().map(|a| a.to_string()),
                offset_src: p.offset_src.as_ref().map(|a| a.to_string()),
                pluck_src: p.pluck_src.as_ref().map(|a| a.to_string()),
            })
            .collect(),
        logic_outputs: state::LogicOutputConfig {
            sum_enabled: true,
            or_enabled: true,
            and_enabled: true,
        },
    }
}

fn apply_params(s: &mut EnvelopeState, params: &state::EnvelopeParams) {
    // Format-2 files own the channel count and bindings; older files only
    // contribute values for channels that already exist (and keep the
    // default ch1 trigger -> sequencer/0/t1).
    if params.format >= state::STATE_FORMAT && !params.channels.is_empty() {
        let n = params.channels.len().clamp(1, MAX_CHANNELS);
        s.params.resize(n, ChannelParams::default());
        s.channels.resize(n, EnvelopeChannel::default());
        s.current_channel = s.current_channel.min(n - 1);
    }
    for (i, ch) in params.channels.iter().enumerate().take(s.params.len()) {
        s.params[i].rise_param = ch.rise;
        s.params[i].fall_param = ch.fall;
        s.params[i].shape_param = ch.shape;
        s.params[i].loop_mode = ch.loop_mode;
        s.params[i].attenuverter = ch.attenuverter;
        if params.format >= state::STATE_FORMAT {
            s.params[i].offset = ch.offset;
            s.params[i].pluck = ch.pluck;
            s.params[i].gate_mode = ch.gate_mode;
            s.params[i].signal_src = ch.signal_src.as_deref().and_then(SourceAddr::parse);
            s.params[i].trigger = Trigger::from_param(ch.trigger_src.as_deref());
            s.params[i].rise_src = ch.rise_src.as_deref().and_then(SourceAddr::parse);
            s.params[i].fall_src = ch.fall_src.as_deref().and_then(SourceAddr::parse);
            s.params[i].shape_src = ch.shape_src.as_deref().and_then(SourceAddr::parse);
            s.params[i].atten_src = ch.atten_src.as_deref().and_then(SourceAddr::parse);
            s.params[i].offset_src = ch.offset_src.as_deref().and_then(SourceAddr::parse);
            s.params[i].pluck_src = ch.pluck_src.as_deref().and_then(SourceAddr::parse);
        }
    }
}

// ── engine thread ───────────────────────────────────────────────────────────

/// Resolved trigger source, refreshed through the manifest.
#[derive(Clone, Copy, PartialEq, Debug)]
enum RTrig {
    AnyNote,
    Off,
    /// notes from this sequencer track
    Note(u8),
    /// rising edge (>0.5) on this modbus channel
    Edge(usize),
}

#[derive(Clone, Copy, Default)]
struct ResolvedChannel {
    trig: Option<RTrig>, // None until first refresh
    mods: [Option<usize>; 6],
    signal: Option<usize>,
}

/// Effective per-block channel settings after modulation resolution.
struct StageParams {
    rise_time: f32,
    fall_time: f32,
    shape: f32,
    loop_mode: bool,
    gate_mode: bool,
    pluck: f32,
}

/// Advance one channel by one sample. Returns true when a cycle completed
/// (fall reached bottom) — the EOC pulse.
fn advance_stage(ch: &mut EnvelopeChannel, p: &StageParams, dt: f32) -> bool {
    let mut cycle_done = false;
    match ch.stage {
        Stage::Off => {
            ch.output = 0.0;
            if p.loop_mode {
                ch.stage = Stage::Rise;
                ch.phase = 0.0;
            }
        }
        Stage::Rise => {
            if p.rise_time <= 0.0 {
                ch.phase = 1.0;
            } else {
                ch.phase += dt / p.rise_time;
            }
            if ch.phase >= 1.0 {
                ch.phase = 1.0;
                ch.output = 1.0;
                if p.gate_mode && !p.loop_mode {
                    ch.stage = Stage::Sustain;
                } else {
                    // trig semantics (and cycling): straight into the fall
                    ch.stage = Stage::Fall;
                    ch.phase = 0.0;
                }
            } else {
                ch.output = vari_response(ch.phase, p.shape);
            }
        }
        Stage::Sustain => {
            ch.output = 1.0;
            if !p.gate_mode {
                // gate flipped to trig mid-sustain: no note_off will ever
                // release this stage now — fall instead of holding forever
                ch.stage = Stage::Fall;
                ch.phase = 0.0;
            }
        }
        Stage::Fall => {
            let done = if p.pluck > 0.0 {
                // vactrol model: snap + ring; ends when the tail decays
                // below audibility
                if ch.phase == 0.0 {
                    ch.pluck_fast = ch.output;
                    ch.pluck_slow = ch.output;
                    ch.phase = 0.5; // initialized marker
                }
                // a cycle clock rides alongside the tail: phase walks
                // 0.5 → 1.5 over one fall_time
                ch.phase += dt / p.fall_time.max(TIME_MIN);
                let (f2, s2, out) =
                    pluck_decay(ch.pluck_fast, ch.pluck_slow, dt, p.fall_time, p.pluck);
                ch.pluck_fast = f2;
                ch.pluck_slow = s2;
                ch.output = out;
                if p.loop_mode {
                    // cycling restarts on TIME (the EOC), not at -60dB of
                    // vactrol tail — with pluck up, waiting for the tail
                    // made one "cycle" take ~10 seconds and read as frozen
                    out < 0.001 || ch.phase >= 1.5
                } else {
                    out < 0.001
                }
            } else if p.fall_time <= 0.0 {
                true
            } else {
                ch.phase += dt / p.fall_time;
                if ch.phase < 1.0 {
                    ch.output = 1.0 - vari_response(ch.phase, p.shape);
                }
                ch.phase >= 1.0
            };
            if done {
                cycle_done = true;
                if p.loop_mode {
                    // restart the rise FROM the remaining tail (time-based
                    // restarts land while the vactrol ring is still
                    // audible — zeroing it clicked once per cycle)
                    ch.stage = Stage::Rise;
                    ch.phase = vari_inverse(ch.output, p.shape);
                } else {
                    ch.phase = 1.0;
                    ch.output = 0.0;
                    ch.stage = Stage::Off;
                }
            }
        }
    }
    cycle_done
}

fn env_thread(
    state: Arc<Mutex<EnvelopeState>>,
    shutdown: std::sync::mpsc::Receiver<()>,
    instance: usize,
) -> Result<()> {
    let mut events = EventRingbuf::open_dynamic().ok();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let _transport = ShmTransport::open().ok();

    // Audio-rate output: a cycling channel is a sound source. The mixer
    // discovers this ringbuf through our manifest registration.
    let shm_name = format!("/los_audio_envelope_{}", instance);
    let mut audio = AudioRingbuf::open(&shm_name)
        .or_else(|_| AudioRingbuf::create(&shm_name))
        .ok();

    let dt = 1.0 / SAMPLE_RATE as f32;
    let block_dur = Duration::from_nanos((1_000_000_000u64 * BLOCK_SIZE as u64) / 48_000);

    let mut resolved = [ResolvedChannel::default(); MAX_CHANNELS];
    let mut edge_prev = [0.0f32; MAX_CHANNELS];
    let mut refresh_in = 0u32;
    // one-pole DC blocker for the audio out (envelope sweeps are near-DC)
    let (mut dc_x1, mut dc_y1) = (0.0f32, 0.0f32);
    let mut audio_buf = vec![0.0f32; BLOCK_SIZE * 2];
    let mut next_deadline = Instant::now();

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        if events.is_none() {
            events = EventRingbuf::open_dynamic().ok();
        }
        if modbus.is_none() {
            modbus = ModulationBus::open().ok();
        }

        if refresh_in == 0 {
            refresh_in = 128;
            let entries = manifest.entries();
            let s = state.lock().unwrap();
            for (i, r) in resolved.iter_mut().enumerate().take(s.params.len()) {
                let p = &s.params[i];
                r.trig = Some(match &p.trigger {
                    Trigger::Any => RTrig::AnyNote,
                    Trigger::Off => RTrig::Off,
                    Trigger::Source(a) => match routing::note_source_track(a) {
                        Some(t) => RTrig::Note(t),
                        None => match routing::resolve(&entries, a) {
                            Some(ch) => RTrig::Edge(ch),
                            None => RTrig::Off, // unresolvable: behave like off
                        },
                    },
                });
                r.mods = [
                    p.rise_src
                        .as_ref()
                        .and_then(|a| routing::resolve(&entries, a)),
                    p.fall_src
                        .as_ref()
                        .and_then(|a| routing::resolve(&entries, a)),
                    p.shape_src
                        .as_ref()
                        .and_then(|a| routing::resolve(&entries, a)),
                    p.atten_src
                        .as_ref()
                        .and_then(|a| routing::resolve(&entries, a)),
                    p.offset_src
                        .as_ref()
                        .and_then(|a| routing::resolve(&entries, a)),
                    p.pluck_src
                        .as_ref()
                        .and_then(|a| routing::resolve(&entries, a)),
                ];
                r.signal = p
                    .signal_src
                    .as_ref()
                    .and_then(|a| routing::resolve(&entries, a));
            }
            // publish consumed channels + note-track triggers for the
            // sequencer's who's-listening markers
            let mut channels = 0u64;
            let mut notes = 0u8;
            for r in resolved.iter().take(s.params.len()) {
                for ch in r.mods.iter().flatten().chain(r.signal.iter()) {
                    if *ch < 64 {
                        channels |= 1 << ch;
                    }
                }
                match r.trig {
                    Some(RTrig::Edge(ch)) if ch < 64 => channels |= 1 << ch,
                    Some(RTrig::Note(t)) if t < 8 => notes |= 1 << t,
                    _ => {}
                }
            }
            manifest.publish_consumes(channels, notes);
        }
        refresh_in -= 1;

        // Note events + manual TRIGGER events
        let mut triggers = [false; MAX_CHANNELS];
        // Per-track bitmaps: with several note tracks firing in the same
        // block, a single last-event-wins slot shadowed every channel
        // whose track wasn't the final writer (three note tracks = a
        // silent voice).
        let mut note_trigs: u64 = 0;
        let mut note_offs: u64 = 0;
        let mut event_count = 0u32;
        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                event_count += 1;
                match event.event_type {
                    0 => note_trigs |= 1 << (event.source & 63),
                    1 => note_offs |= 1 << (event.source & 63),
                    4 => {
                        let ch = (event.target as usize).min(MAX_CHANNELS - 1);
                        triggers[ch] = true;
                    }
                    _ => {}
                }
            }
        }

        let mut s = state.lock().unwrap();
        s.events_received += event_count as u64;
        let n = s.params.len();

        // Edge-triggered bindings fire on a 0.5 upward crossing; the
        // falling edge releases gate-mode channels (a high source sustains,
        // like holding a gate against the hardware's signal input)
        let mut edge_falls = [false; MAX_CHANNELS];
        for i in 0..n {
            if let Some(RTrig::Edge(chan)) = resolved[i].trig {
                let val = modbus.as_ref().map(|m| m.get(chan)).unwrap_or(0.0);
                if edge_prev[i] <= 0.5 && val > 0.5 {
                    triggers[i] = true;
                }
                if edge_prev[i] > 0.5 && val <= 0.5 {
                    edge_falls[i] = true;
                }
                edge_prev[i] = val;
            }
        }

        audio_buf.iter_mut().for_each(|v| *v = 0.0);
        let mut ch_final = [0.0f32; MAX_CHANNELS];
        let mut eoc_pulse = false;

        for i in 0..n {
            let params = s.params[i].clone();
            let r = resolved[i];
            let ch = &mut s.channels[i];

            let should_trigger = match r.trig {
                Some(RTrig::AnyNote) | None => note_trigs != 0 || triggers[i],
                Some(RTrig::Off) | Some(RTrig::Edge(_)) => triggers[i],
                Some(RTrig::Note(want)) => note_trigs & (1 << (want & 63)) != 0 || triggers[i],
            };
            // note_off only matters to a gate; a trig ignores it entirely.
            // Edge sources release on their falling edge — without this a
            // gate-mode channel on an edge trigger sustains forever.
            let should_release = params.gate_mode
                && match r.trig {
                    Some(RTrig::AnyNote) | None => note_offs != 0,
                    Some(RTrig::Off) => false,
                    Some(RTrig::Edge(_)) => edge_falls[i],
                    Some(RTrig::Note(want)) => note_offs & (1 << (want & 63)) != 0,
                };

            // Param modulation: bound + resolvable -> modbus value
            // (computed before trigger handling: a retrigger needs the
            // live shape to continue the rise from the current level)
            let chan_val = |c: Option<usize>| c.and_then(|c| modbus.as_ref().map(|m| m.get(c)));
            let rp = chan_val(r.mods[0])
                .map(|v| v.clamp(0.0, 1.0))
                .unwrap_or(params.rise_param);
            let fp = chan_val(r.mods[1])
                .map(|v| v.clamp(0.0, 1.0))
                .unwrap_or(params.fall_param);
            let sp = chan_val(r.mods[2])
                .map(|v| v.clamp(0.0, 1.0))
                .unwrap_or(params.shape_param);
            let att = chan_val(r.mods[3])
                .map(|v| v.clamp(-1.0, 1.0))
                .unwrap_or(params.attenuverter);
            #[allow(clippy::manual_clamp)] // NaN must die, clamp(NaN)=NaN
            let off = chan_val(r.mods[4])
                .map(|v| v.max(-1.0).min(1.0))
                .unwrap_or(params.offset);
            #[allow(clippy::manual_clamp)]
            let plk = chan_val(r.mods[5])
                .map(|v| v.max(0.0).min(1.0))
                .unwrap_or(params.pluck);

            if should_release && ch.stage != Stage::Off && ch.stage != Stage::Fall {
                // fall FROM the current level too: the non-pluck fall
                // curve is 1−vari(phase), so an early release mid-rise
                // used to jump up to 1.0 before falling
                ch.stage = Stage::Fall;
                ch.phase = if plk > 0.0 {
                    0.0 // pluck init captures ch.output itself
                } else {
                    vari_inverse(1.0 - ch.output, sp)
                };
            }
            if should_trigger {
                // continue the rise FROM the current output — a hard
                // phase=0 reset stepped a ringing tail to zero in one
                // sample, the click that got loud once shadowed triggers
                // started landing
                ch.stage = Stage::Rise;
                ch.phase = vari_inverse(ch.output, sp);
            }

            let rise_time = param_to_time(rp);
            let fall_time = param_to_time(fp);
            // Signal input: live value while the source runs; 0V when the
            // source dies (a pulled cable) so the channel slews down through
            // its fall time instead of snapping silent. Only unbinding
            // returns the channel to generator mode.
            let signal_target = if params.signal_src.is_some() {
                Some(
                    r.signal
                        .and_then(|c| modbus.as_ref().map(|m| m.get(c)))
                        .unwrap_or(0.0),
                )
            } else {
                None
            };

            let stage_params = StageParams {
                rise_time,
                fall_time,
                shape: sp,
                loop_mode: params.loop_mode,
                gate_mode: params.gate_mode,
                pluck: plk,
            };
            for frame in 0..BLOCK_SIZE {
                if let Some(target) = signal_target {
                    // Signal-input mode: slew limiter
                    ch.output = slew_step(ch.output, target, dt, rise_time, fall_time, sp);
                    ch.stage = Stage::Off;
                } else if advance_stage(ch, &stage_params, dt) && i == n - 1 {
                    eoc_pulse = true;
                }
                // audio path: attenuverted function (offsets excluded —
                // DC), one-shot strikes and all — the hardware function
                // jack carries everything, clicks included. Opt-in lives
                // at the MIXER: this source's fader defaults to 0 (the
                // software equivalent of an unpatched cable), so the raw
                // transients only reach the master when you push it up.
                let a = ch.output * att;
                audio_buf[frame * 2] += a;
                audio_buf[frame * 2 + 1] += a;
            }

            ch_final[i] = (ch.output * att + off).clamp(-1.0, 1.0);
        }

        // Buses + gates
        let sum = ch_final[..n].iter().sum::<f32>().clamp(-1.0, 1.0);
        let or_val = ch_final[..n]
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max);
        let and_val = ch_final[..n].iter().copied().fold(f32::INFINITY, f32::min);
        let invert = -ch_final[0];
        let eor = if matches!(s.channels[0].stage, Stage::Sustain | Stage::Fall) {
            1.0
        } else {
            0.0
        };
        let eoc = if s.channels[n - 1].stage == Stage::Off || eoc_pulse {
            1.0
        } else {
            0.0
        };

        let mod_base = s.mod_base;
        drop(s);

        // Modbus: base+0..5 channels, +6..9 sum/or/and/inv, +10 eor, +11 eoc
        if let (Some(ref mut bus), Some(base)) = (modbus.as_mut(), mod_base) {
            for (i, v) in ch_final.iter().enumerate().take(MAX_CHANNELS) {
                bus.set(base + i, if i < n { *v } else { 0.0 });
            }
            bus.set(base + MAX_CHANNELS, sum);
            bus.set(base + MAX_CHANNELS + 1, or_val);
            bus.set(base + MAX_CHANNELS + 2, and_val);
            bus.set(base + MAX_CHANNELS + 3, invert);
            bus.set(base + MAX_CHANNELS + 4, eor);
            bus.set(base + MAX_CHANNELS + 5, eoc);
        }

        // Audio out, DC-blocked so slow envelopes stay silent while
        // audio-rate cycling is heard (y = x - x1 + 0.995·y1)
        if let Some(ref mut rb) = audio {
            for f in 0..BLOCK_SIZE {
                let x = audio_buf[f * 2];
                let y = x - dc_x1 + 0.995 * dc_y1;
                dc_x1 = x;
                dc_y1 = y;
                let y = (y * 0.5).clamp(-1.0, 1.0);
                audio_buf[f * 2] = y;
                audio_buf[f * 2 + 1] = y;
            }
            let _ = rb.write(&audio_buf);
        }

        // Real-time pacing: exactly one 64-sample block per 1.333ms.
        // (The old loop slept a flat 1ms — envelopes ran 33% fast.)
        next_deadline += block_dur;
        let now = Instant::now();
        if next_deadline > now {
            std::thread::sleep(next_deadline - now);
        } else {
            next_deadline = now; // fell behind; don't spiral
        }
    }

    Ok(())
}

// ── UI ──────────────────────────────────────────────────────────────────────

fn meter(v: f32) -> String {
    let sign = if v < -0.005 { '-' } else { ' ' };
    format!("{}{}", sign, crate::theme::meter_char(v.abs()))
}

/// Bipolar value (−1..1) → unipolar gauge position.
fn bi(v: f32) -> f32 {
    (v.clamp(-1.0, 1.0) + 1.0) / 2.0
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &EnvelopeState,
    selected: usize,
    show_help: bool,
    overlay: Option<&str>,
    picker: Option<(Vec<String>, usize)>,
    ghosts: &[Option<f32>; 6],
    entries: &[crate::shm::ManifestEntry],
    picker_colors: &[Option<ratatui::style::Color>],
    instance: usize,
    bpm: f32,
    playing: bool,
) -> Result<()> {
    use crate::theme;
    use ratatui::text::Span;

    terminal.draw(|f| {
        let area = f.area();
        let w = area.width as usize;
        let n = state.params.len();
        let cur = state.current_channel.min(n - 1);
        let p = &state.params[cur];

        let mut lines: Vec<Line> = Vec::new();
        let ctx = format!("ch{}/{}", cur + 1, n);
        // the page wears its channel's identity color: title context, row
        // labels, its own (unbound) sliders — bound bars wear their cable
        let page = match state.mod_base {
            Some(base) => theme::channel_color(base + cur),
            None => theme::source_color(&format!("envelope/{}/ch{}", instance, cur + 1)),
        };
        let _ = (bpm, playing);
        let mut hdr = theme::header("MATHs", "", "", w);
        hdr.spans
            .insert(1, Span::styled(format!("·{}· ", ctx), theme::signal(page)));
        lines.push(hdr);

        // ── overview: every channel at a glance + the buses ─────────────
        let mut ov: Vec<Span> = Vec::new();
        for i in 0..n {
            let ch = &state.channels[i];
            let cp = &state.params[i];
            let arrow = match ch.stage {
                Stage::Rise => theme::RISE_ARROW,
                Stage::Fall => theme::FALL_ARROW,
                Stage::Sustain => theme::SUSTAIN_BAR,
                Stage::Off => ' ',
            };
            let cable = match state.mod_base {
                Some(base) => theme::channel_color(base + i),
                None => theme::source_color(&format!("envelope/{}/ch{}", instance, i + 1)),
            };
            let style = if i == cur {
                theme::signal(cable).add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                theme::signal(cable)
            };
            ov.push(Span::styled(format!(" C{}", i + 1), style));
            ov.push(Span::styled(
                theme::meter_char(ch.output.abs()).to_string(),
                theme::signal(theme::cv()),
            ));
            ov.push(Span::styled(
                format!("{}{}", arrow, if cp.loop_mode { "∞" } else { " " }),
                theme::signal(theme::clock()),
            ));
        }
        let outs: Vec<f32> = state
            .channels
            .iter()
            .zip(state.params.iter())
            .take(n)
            .map(|(c, q)| (c.output * q.attenuverter + q.offset).clamp(-1.0, 1.0))
            .collect();
        let sum: f32 = outs.iter().sum::<f32>().clamp(-1.0, 1.0);
        let or_v = outs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let eor = matches!(state.channels[0].stage, Stage::Sustain | Stage::Fall);
        let eoc = state.channels[n - 1].stage == Stage::Off;
        ov.push(Span::styled("  ∑", theme::chrome()));
        ov.push(Span::styled(meter(sum), theme::signal(theme::cv())));
        ov.push(Span::styled(" ∨", theme::chrome()));
        ov.push(Span::styled(meter(or_v), theme::signal(theme::cv())));
        ov.push(Span::styled(
            format!(
                " {}{}",
                if eor { theme::GATE_HI } else { theme::GATE_LO },
                if eoc { theme::GATE_HI } else { theme::GATE_LO }
            ),
            theme::signal(theme::cv()),
        ));
        lines.push(Line::from(ov));
        lines.push(theme::rule(w));

        // ── detail: the current channel with real sliders ───────────────
        let bar_w = theme::bar_width(w, 26);
        let trig_text = match &p.trigger {
            Trigger::Any => String::from("any note"),
            Trigger::Off => String::from("off"),
            Trigger::Source(a) => a.to_string(),
        };
        let row_label = |row: usize, name: &str| -> Span<'static> {
            if row == selected {
                Span::styled(format!(" {:<5}", name), theme::selected())
            } else {
                Span::styled(format!(" {:<5}", name), theme::signal(page))
            }
        };

        // trigger row (selectable, binding-only)
        lines.push(Line::from(vec![
            row_label(ROW_TRIGGER, "trig"),
            Span::styled(trig_text, theme::signal(theme::cv())),
            Span::styled(
                format!("·{}", if p.gate_mode { "gate" } else { "trig" }),
                theme::value(),
            ),
            Span::styled(
                if p.loop_mode { "  CYC∞" } else { "" }.to_string(),
                theme::signal(theme::clock()),
            ),
        ]));

        // value rows: (row, label, set 0..1 for gauge, display, ghost, hue tag)
        type ValueRow<'a> = (
            usize,
            &'a str,
            f32,
            String,
            Option<f32>,
            Option<&'a Option<SourceAddr>>,
        );
        let rows: [ValueRow; 6] = [
            (
                0,
                "rise",
                p.rise_param,
                format_time(param_to_time(p.rise_param)),
                ghosts[0],
                Some(&p.rise_src),
            ),
            (
                1,
                "fall",
                p.fall_param,
                format_time(param_to_time(p.fall_param)),
                ghosts[1],
                Some(&p.fall_src),
            ),
            (
                2,
                "shap",
                p.shape_param,
                format!("{:.2}", p.shape_param),
                ghosts[2],
                Some(&p.shape_src),
            ),
            (
                3,
                "attn",
                bi(p.attenuverter),
                format!("{:+.2}", p.attenuverter),
                ghosts[3].map(bi),
                Some(&p.atten_src),
            ),
            (
                4,
                "offs",
                bi(p.offset),
                format!("{:+.2}", p.offset),
                ghosts[4].map(bi),
                Some(&p.offset_src),
            ),
            (
                ROW_PLUCK,
                "plck",
                p.pluck,
                format!("{:.2}", p.pluck),
                ghosts[5],
                Some(&p.pluck_src),
            ),
        ];
        for (row, name, set, disp, ghost, src) in rows {
            let mut spans = vec![row_label(row, name)];
            let hue = match src {
                Some(Some(a)) => routing::cable_color(entries, a),
                _ => page, // the channel's own slider wears its identity
            };
            spans.extend(theme::bar(set, ghost, bar_w, hue));
            spans.push(Span::styled(format!(" {:>7}", disp), theme::value()));
            if let Some(Some(a)) = src {
                spans.push(Span::styled(
                    format!(" {}{}", theme::BIND, a.output),
                    theme::signal(routing::cable_color(entries, a)),
                ));
            }
            lines.push(Line::from(spans));
        }
        // signal row
        lines.push(Line::from(vec![
            row_label(ROW_SIGNAL, "sig"),
            match &p.signal_src {
                Some(a) => Span::styled(
                    format!("{}{} (slew)", theme::BIND, a),
                    theme::signal(theme::cv()),
                ),
                None => Span::styled("—".to_string(), theme::dim()),
            },
        ]));

        theme::anchor_bottom(&mut lines, area.height as usize, 2);
        lines.push(theme::rule(w));

        // ── status ──────────────────────────────────────────────────────
        let msg = overlay.map(|o| o.to_string()).unwrap_or_else(|| {
            format!(
                "a/x ±ch · c cyc · m {} · t fire · @ bind",
                if p.gate_mode { "→trig" } else { "→gate" }
            )
        });
        lines.push(theme::status("NORMAL", &msg, "", w));

        f.render_widget(Paragraph::new(lines), area);

        if show_help {
            let help_text = maths_help();
            let help = Paragraph::new(help_text)
                .style(Style::default().fg(theme::ink()).bg(theme::bg()))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(theme::chrome())
                        .title(Span::styled(" MATHs ", theme::chrome_hi())),
                );
            f.render_widget(help, area);
        }

        if let Some((rows, sel)) = picker {
            let h = (rows.len() as u16 + 2).min(area.height);
            let pw = rows.iter().map(|r| r.len()).max().unwrap_or(10).max(20) as u16 + 4;
            let r = ratatui::layout::Rect::new(
                (area.width.saturating_sub(pw)) / 2,
                (area.height.saturating_sub(h)) / 2,
                pw.min(area.width),
                h,
            );
            f.render_widget(ratatui::widgets::Clear, r);
            let items: Vec<ratatui::widgets::ListItem> = rows
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    let style = if i == sel {
                        theme::selected()
                    } else if let Some(Some(c)) = picker_colors.get(i) {
                        theme::signal(*c)
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
                    .title(Span::styled(" bind source ", theme::chrome_hi())),
            );
            f.render_widget(list, r);
        }
    })?;

    Ok(())
}

fn maths_help() -> Vec<Line<'static>> {
    vec![
        Line::from("━━━ MATHs ━━━"),
        Line::from(""),
        Line::from("  [/]  gg/G   Channel nav (counts)"),
        Line::from("  j/k  h/l    Row select / adjust (H/L ×10)"),
        Line::from("  a / x       Add / remove channel (≤6)"),
        Line::from("  c           Cycle (loop) — CYC∞"),
        Line::from("  m           Trig/gate per channel"),
        Line::from("  t / o       Manual trigger / gate"),
        Line::from("  @           Bind row (sig = slew input;"),
        Line::from("              trig: any/off/track/edge)"),
        Line::from("  u/^r        Undo/redo (counts)"),
        Line::from("  :set rise 0|100ms|2s|1.5m · pluck · mode"),
        Line::from("  :w/:e/:q    Patches / quit"),
        Line::from(""),
        Line::from("Rows: rise·fall 0→25min, shap log↔exp,"),
        Line::from("  attn, offs, plck (vactrol), sig, trig"),
        Line::from(""),
        Line::from("  ? closes help"),
    ]
}

// ── main loop ───────────────────────────────────────────────────────────────

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("envelope", instance);
    let shm_name = format!("/los_audio_envelope_{}", instance);
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    if let Err(e) = manifest.register("envelope", instance, Some(&shm_name), CLAIMED_OUTPUTS) {
        // a lost slot means every binding on our outputs goes dead —
        // say so where a pane capture can see it
        eprintln!("[envelope {}] manifest registration FAILED: {}", instance, e);
    }
    let claimed_base = manifest.claimed_base();

    for attempt in 0..20 {
        match enable_raw_mode() {
            Ok(()) => break,
            Err(e) => {
                if attempt < 19 {
                    std::thread::sleep(Duration::from_millis(200));
                } else {
                    return Err(anyhow::anyhow!(
                        "Failed to enable raw mode after 20 attempts: {}",
                        e
                    ));
                }
            }
        }
    }
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let state = Arc::new(Mutex::new(EnvelopeState::default()));
    state.lock().unwrap().mod_base = claimed_base;

    // Load saved state if available
    if let Ok(params) = state::load_module_state::<state::EnvelopeParams>("envelope", instance) {
        apply_params(&mut state.lock().unwrap(), &params);
    }

    let state_clone = Arc::clone(&state);
    let (_tx, rx) = std::sync::mpsc::channel();
    let _env_handle = std::thread::spawn(move || {
        if let Err(e) = env_thread(state_clone, rx, instance) {
            eprintln!("Envelope thread error: {}", e);
        }
    });

    let mut selected = 0usize;
    let mut show_help = false;
    let mut count = crate::keys::Count::default();
    let mut pending_g = false;
    let mut picker = crate::picker::Picker::default();
    let mut history = crate::undo::ParamHistory::default();
    let mut ex = crate::excmd::ExLine::default();
    let mut ex_msg: Option<String> = None;
    let mut patch_name: Option<String> = None;
    let mut baseline =
        state::to_toml_string(&snapshot_params(&state.lock().unwrap())).unwrap_or_default();
    let mut should_quit = false;
    // Global transport handle for Space = play/pause (lazily reopened)
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();
    // Live modbus reads for ghost markers on the current channel's gauges
    let mut ui_modbus = ModulationBus::open().ok();
    let mut ui_entries: Vec<crate::shm::ManifestEntry> = Vec::new();
    let mut ui_refresh = 0u32;

    loop {
        if state::check_save_signal() {
            let params = snapshot_params(&state.lock().unwrap());
            let _ = state::save_module_state("envelope", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(params) =
                state::load_module_state::<state::EnvelopeParams>("envelope", instance)
            {
                apply_params(&mut state.lock().unwrap(), &params);
            }
        }

        let current_state = state.lock().unwrap().clone();
        // ghosts: live values of the current channel's mod bindings (§5)
        if ui_refresh == 0 {
            ui_refresh = 40;
            ui_entries = Manifest::open().map(|m| m.entries()).unwrap_or_default();
            if ui_modbus.is_none() {
                ui_modbus = ModulationBus::open().ok();
            }
        }
        ui_refresh -= 1;
        let cur = current_state
            .current_channel
            .min(current_state.params.len() - 1);
        let cp = &current_state.params[cur];
        let live = |src: &Option<SourceAddr>| -> Option<f32> {
            src.as_ref()
                .and_then(|a| routing::resolve(&ui_entries, a))
                .and_then(|ch| ui_modbus.as_ref().map(|m| m.get(ch)))
        };
        let ghosts = [
            live(&cp.rise_src),
            live(&cp.fall_src),
            live(&cp.shape_src),
            live(&cp.atten_src),
            live(&cp.offset_src),
            live(&cp.pluck_src),
        ];
        let (bpm, playing) = transport_ui
            .as_ref()
            .map(|t| (t.bpm(), t.playing()))
            .unwrap_or((120.0, false));
        let overlay = if ex.is_active() {
            Some(ex.display())
        } else {
            ex_msg.clone()
        };
        let picker_rows = if picker.is_active() {
            Some(picker.rows())
        } else {
            None
        };
        let picker_colors: Vec<Option<ratatui::style::Color>> = if picker.is_active() {
            picker
                .row_sources()
                .iter()
                .map(|s| s.map(|a| routing::cable_color(&ui_entries, a)))
                .collect()
        } else {
            Vec::new()
        };
        draw_ui(
            &mut terminal,
            &current_state,
            selected,
            show_help,
            overlay.as_deref(),
            picker_rows,
            &ghosts,
            &ui_entries,
            &picker_colors,
            instance,
            bpm,
            playing,
        )?;

        if event::poll(Duration::from_millis(50))? {
            let ev = event::read()?;
            if let Event::Mouse(m) = ev {
                use crate::undo::{ParamUndo, ParamValue};
                use crossterm::event::{MouseButton, MouseEventKind};
                // y: 1 = overview (click selects channel); 3 = trig row;
                // 4..=9 = rise/fall/shap/attn/offs/plck; 10 = sig
                let row_at = |y: u16| -> Option<usize> {
                    match y {
                        3 => Some(ROW_TRIGGER),
                        4..=9 => Some(y as usize - 4),
                        10 => Some(ROW_SIGNAL),
                        _ => None,
                    }
                };
                match m.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let steps = if m.kind == MouseEventKind::ScrollUp {
                            1
                        } else {
                            -1
                        };
                        let mut s = state.lock().unwrap();
                        let slot = row_slot(s.current_channel, selected);
                        let old = s.get_param(slot);
                        adjust(&mut s, selected, steps, false);
                        let new = s.get_param(slot);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(slot, "Adjust", old, new);
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if m.row == 1 {
                            // overview strip: each channel cell is ~6 wide
                            let ch = (m.column as usize) / 6;
                            let mut s = state.lock().unwrap();
                            s.current_channel = ch.min(s.params.len() - 1);
                        } else if let Some(row) = row_at(m.row) {
                            selected = row;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(row) = row_at(m.row) {
                            if row <= 5 {
                                selected = row;
                                let w = terminal.size().map(|r| r.width as usize).unwrap_or(60);
                                let bar_w = crate::theme::bar_width(w, 26);
                                let x = (m.column as usize).saturating_sub(6);
                                let mut v = (x as f32 / bar_w.saturating_sub(1).max(1) as f32)
                                    .clamp(0.0, 1.0);
                                if matches!(row, 3 | 4) {
                                    v = v * 2.0 - 1.0; // attn/offs are bipolar
                                }
                                let mut s = state.lock().unwrap();
                                let slot = row_slot(s.current_channel, row);
                                let old = s.get_param(slot);
                                s.set_param(slot, ParamValue::F32(v));
                                if let Some(old) = old {
                                    history.record(slot, "Slide", old, ParamValue::F32(v));
                                }
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if let Event::Key(key) = ev {
                ex_msg = None;
                if picker.is_active() {
                    use crate::picker::PickerEvent;
                    let chosen: Option<Option<String>> = match picker.handle_key(key.code) {
                        PickerEvent::Chosen(addr) => Some(addr.map(|a| a.to_string())),
                        PickerEvent::ChosenSpecial(1) => Some(Some(String::from("off"))),
                        _ => None,
                    };
                    if let Some(value) = chosen {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = state.lock().unwrap();
                        let slot = match selected {
                            0..=3 => s.current_channel * CH_SLOT_STRIDE + BIND_OFF + selected,
                            4 => s.current_channel * CH_SLOT_STRIDE + BIND_OFF + 6,
                            ROW_PLUCK => s.current_channel * CH_SLOT_STRIDE + BIND_OFF + 7,
                            _ => row_slot(s.current_channel, selected),
                        };
                        let old = s.get_param(slot);
                        s.set_param(slot, ParamValue::Src(value));
                        let new = s.get_param(slot);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(slot, "Bind", old, new);
                        }
                    }
                    continue;
                }
                if ex.is_active() {
                    let completer = crate::excmd::standard_completer(crate::excmd::patch_names(
                        &state::patches_dir(),
                    ));
                    if let crate::excmd::ExEvent::Submit(cmd) = ex.handle_key(key.code, &completer)
                    {
                        use crate::excmd::ExCommand;
                        let params = snapshot_params(&state.lock().unwrap());
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
                            ExCommand::Edit(name) => {
                                match state::load_patch::<state::EnvelopeParams>(&name) {
                                    Ok(p) => {
                                        apply_params(&mut state.lock().unwrap(), &p);
                                        baseline = state::to_toml_string(&snapshot_params(
                                            &state.lock().unwrap(),
                                        ))
                                        .unwrap_or_default();
                                        patch_name = Some(name.clone());
                                        ex_msg = Some(format!("Loaded {}", name));
                                    }
                                    Err(e) => ex_msg = Some(e.to_string()),
                                }
                            }
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
                                match crate::excmd::ex_write(
                                    name,
                                    &mut patch_name,
                                    &mut baseline,
                                    &params,
                                ) {
                                    Ok(_) => should_quit = true,
                                    Err(m) => ex_msg = Some(m),
                                }
                            }
                            ExCommand::Set(k, v) => {
                                use crate::undo::{ParamUndo, ParamValue};
                                let mut s = state.lock().unwrap();
                                let ch = s.current_channel;
                                let parsed: Option<(usize, f32)> = match k.as_str() {
                                    "rise" => parse_time_param(&v).map(|p| (0, p)),
                                    "fall" => parse_time_param(&v).map(|p| (1, p)),
                                    "shape" => v.parse().ok().map(|p: f32| (2, p.clamp(0.0, 1.0))),
                                    "atten" => v.parse().ok().map(|p: f32| (3, p.clamp(-1.0, 1.0))),
                                    "offset" => {
                                        v.parse().ok().map(|p: f32| (4, p.clamp(-1.0, 1.0)))
                                    }
                                    "pluck" => v.parse().ok().map(|p: f32| (6, p.clamp(0.0, 1.0))),
                                    "mode" => match v.as_str() {
                                        "trig" => Some((7, 0.0)),
                                        "gate" => Some((7, 1.0)),
                                        _ => None,
                                    },
                                    _ => None,
                                };
                                match parsed {
                                    Some((row, val)) => {
                                        let slot = ch * CH_SLOT_STRIDE + row;
                                        let old = s.get_param(slot);
                                        let new_val = if row == 7 {
                                            ParamValue::Bool(val > 0.5)
                                        } else {
                                            ParamValue::F32(val)
                                        };
                                        s.set_param(slot, new_val);
                                        if let Some(old) = old {
                                            let new_val = if row == 7 {
                                                ParamValue::Bool(val > 0.5)
                                            } else {
                                                ParamValue::F32(val)
                                            };
                                            history.record(slot, "Set", old, new_val);
                                        }
                                        ex_msg = Some(match row {
                                            0 | 1 => format!(
                                                "{} = {}",
                                                k,
                                                format_time(param_to_time(val))
                                            ),
                                            7 => format!(
                                                "mode = {}",
                                                if val > 0.5 { "gate" } else { "trig" }
                                            ),
                                            _ => format!("{} = {:.2}", k, val),
                                        });
                                    }
                                    None => ex_msg = Some(format!("Can't set {} to {}", k, v)),
                                }
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
                    let mut s = state.lock().unwrap();
                    ex_msg = Some(crate::undo::history_status("Redo", n, || {
                        history.redo(&mut *s)
                    }));
                    continue;
                }
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let params = snapshot_params(&state.lock().unwrap());
                    let _ = state::save_module_state("envelope", instance, &params);
                    continue;
                }

                match key.code {
                    KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
                    KeyCode::Char('j') | KeyCode::Down => {
                        selected = crate::keys::cycle(selected, count.take() as i32, NUM_ROWS);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        selected = crate::keys::cycle(selected, -(count.take() as i32), NUM_ROWS);
                    }
                    KeyCode::Char('h' | 'l' | 'H' | 'L') | KeyCode::Left | KeyCode::Right => {
                        let c = match key.code {
                            KeyCode::Char(c) => c,
                            KeyCode::Left => 'h',
                            _ => 'l',
                        };
                        let n = count.take() as i32;
                        let (steps, coarse) = match c {
                            'h' => (-n, false),
                            'l' => (n, false),
                            'H' => (-n, true),
                            _ => (n, true),
                        };
                        use crate::undo::ParamUndo;
                        let mut s = state.lock().unwrap();
                        let slot = row_slot(s.current_channel, selected);
                        let old = s.get_param(slot);
                        adjust(&mut s, selected, steps, coarse);
                        let new = s.get_param(slot);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(slot, "Adjust", old, new);
                        }
                    }
                    KeyCode::Char('[') => {
                        let n = count.take();
                        let mut s = state.lock().unwrap();
                        s.current_channel = s.current_channel.saturating_sub(n);
                    }
                    KeyCode::Char(']') => {
                        let n = count.take();
                        let mut s = state.lock().unwrap();
                        s.current_channel = (s.current_channel + n).min(s.params.len() - 1);
                    }
                    KeyCode::Char('g') => {
                        count.clear();
                        if pending_g {
                            pending_g = false;
                            state.lock().unwrap().current_channel = 0;
                        } else {
                            pending_g = true;
                        }
                    }
                    KeyCode::Char('G') => {
                        count.clear();
                        let mut s = state.lock().unwrap();
                        s.current_channel = s.params.len() - 1;
                    }
                    KeyCode::Char('a') => {
                        count.clear();
                        let mut s = state.lock().unwrap();
                        ex_msg = Some(if add_channel(&mut s) {
                            format!("Channel {} added", s.params.len())
                        } else {
                            format!("Max {} channels", MAX_CHANNELS)
                        });
                    }
                    KeyCode::Char('x') => {
                        count.clear();
                        let mut s = state.lock().unwrap();
                        ex_msg = Some(if remove_channel(&mut s) {
                            format!("Channel removed ({} left)", s.params.len())
                        } else {
                            String::from("Can't remove the last channel")
                        });
                    }
                    KeyCode::Char('t') => {
                        count.clear();
                        let mut s = state.lock().unwrap();
                        let ch = s.current_channel;
                        s.channels[ch].stage = Stage::Rise;
                        s.channels[ch].phase = 0.0;
                    }
                    KeyCode::Char('m') => {
                        count.clear();
                        use crate::undo::ParamValue;
                        let mut s = state.lock().unwrap();
                        let ch = s.current_channel;
                        let was = s.params[ch].gate_mode;
                        s.params[ch].gate_mode = !was;
                        history.record(
                            ch * CH_SLOT_STRIDE + 7,
                            "Trig/gate mode",
                            ParamValue::Bool(was),
                            ParamValue::Bool(!was),
                        );
                        ex_msg = Some(format!(
                            "Ch{} note input: {}",
                            ch + 1,
                            if !was {
                                "gate (sustains)"
                            } else {
                                "trig (full AD)"
                            }
                        ));
                    }
                    KeyCode::Char('c') => {
                        count.clear();
                        use crate::undo::ParamValue;
                        let mut s = state.lock().unwrap();
                        let ch = s.current_channel;
                        let was = s.params[ch].loop_mode;
                        s.params[ch].loop_mode = !was;
                        history.record(
                            ch * CH_SLOT_STRIDE + 5,
                            "Cycle mode",
                            ParamValue::Bool(was),
                            ParamValue::Bool(!was),
                        );
                    }
                    KeyCode::Char('o') => {
                        count.clear();
                        let mut s = state.lock().unwrap();
                        s.gate = !s.gate;
                        if !s.gate {
                            for ch in s.channels.iter_mut() {
                                if ch.stage == Stage::Sustain {
                                    ch.stage = Stage::Fall;
                                    ch.phase = 0.0;
                                }
                            }
                        } else {
                            for ch in s.channels.iter_mut() {
                                if ch.stage == Stage::Off || ch.stage == Stage::Fall {
                                    ch.stage = Stage::Rise;
                                    ch.phase = 0.0;
                                }
                            }
                        }
                    }
                    KeyCode::Char('u') => {
                        let n = count.take();
                        let mut s = state.lock().unwrap();
                        ex_msg = Some(crate::undo::history_status("Undo", n, || {
                            history.undo(&mut *s)
                        }));
                    }
                    KeyCode::Char('@') => {
                        count.clear();
                        let sources = Manifest::open()
                            .map(|m| crate::routing::live_sources(&m.entries()))
                            .unwrap_or_default();
                        let s = state.lock().unwrap();
                        let ch = s.current_channel;
                        match selected {
                            ROW_TRIGGER => {
                                let (current, special) = match &s.params[ch].trigger {
                                    Trigger::Any => (None, 0),
                                    Trigger::Off => (None, 1),
                                    Trigger::Source(a) => (Some(a.clone()), 0),
                                };
                                drop(s);
                                picker.open_with(
                                    vec![String::from("— any note —"), String::from("— off —")],
                                    sources,
                                    current.as_ref(),
                                    special,
                                );
                            }
                            _ => {
                                let current = match selected {
                                    0 => s.params[ch].rise_src.clone(),
                                    1 => s.params[ch].fall_src.clone(),
                                    2 => s.params[ch].shape_src.clone(),
                                    3 => s.params[ch].atten_src.clone(),
                                    4 => s.params[ch].offset_src.clone(),
                                    ROW_PLUCK => s.params[ch].pluck_src.clone(),
                                    _ => s.params[ch].signal_src.clone(),
                                };
                                drop(s);
                                picker.open(sources, current.as_ref());
                            }
                        }
                    }
                    KeyCode::Char(' ') => {
                        count.clear();
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── engine math ─────────────────────────────────────────────────────

    #[test]
    fn time_taper_spans_instant_to_25_min() {
        assert_eq!(
            param_to_time(0.0),
            0.0,
            "zero attack is allowed — plucks need it"
        );
        assert!(
            (param_to_time(0.001) - 0.0005).abs() < 0.0001,
            "first step up is ~0.5ms"
        );
        assert!((param_to_time(1.0) - 1500.0).abs() < 1.0);
        // round trip
        for t in [0.001, 0.1, 2.5, 60.0, 900.0] {
            let p = time_to_param(t);
            assert!((param_to_time(p) - t).abs() / t < 0.001, "round trip {}", t);
        }
        // monotonic
        assert!(param_to_time(0.6) > param_to_time(0.4));
    }

    #[test]
    fn parse_time_param_units() {
        let close = |a: f32, t: f32| (param_to_time(a) - t).abs() / t < 0.01;
        assert!(close(parse_time_param("100ms").unwrap(), 0.1));
        assert!(close(parse_time_param("2s").unwrap(), 2.0));
        assert!(close(parse_time_param("1.5m").unwrap(), 90.0));
        assert!(
            (parse_time_param("0.42").unwrap() - 0.42).abs() < 1e-6,
            "bare value = param"
        );
        assert!(parse_time_param("nope").is_none());
        assert!(parse_time_param("-2s").is_none());
        assert!(parse_time_param("1.7").is_none(), "bare params are 0-1");
    }

    #[test]
    fn vari_response_endpoints_and_character() {
        for shape in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert!(vari_response(0.0, shape).abs() < 1e-6);
            assert!((vari_response(1.0, shape) - 1.0).abs() < 1e-6);
            // monotonic
            let mut prev = 0.0;
            for i in 1..=20 {
                let v = vari_response(i as f32 / 20.0, shape);
                assert!(v >= prev, "monotonic at shape {}", shape);
                prev = v;
            }
        }
        // linear at center
        assert!((vari_response(0.3, 0.5) - 0.3).abs() < 1e-3);
        // log: fast start (above linear); exp: slow start (below linear)
        assert!(vari_response(0.3, 0.0) > 0.5, "log charges fast");
        assert!(vari_response(0.3, 1.0) < 0.1, "exp starts slow");
    }

    #[test]
    fn slew_approaches_target_and_respects_rates() {
        let dt = 1.0 / 48000.0;
        // linear shape: full-scale in `time`
        let mut out = 0.0;
        for _ in 0..4800 {
            out = slew_step(out, 1.0, dt, 0.1, 0.1, 0.5); // 100ms rise
        }
        assert!(
            (out - 1.0).abs() < 1e-3,
            "linear slew completes in time, got {}",
            out
        );

        // never overshoots
        let mut out = 0.9999;
        out = slew_step(out, 1.0, dt, 0.0005, 0.0005, 0.5);
        assert!(out <= 1.0);

        // falls use fall time
        let mut out = 1.0;
        for _ in 0..2400 {
            out = slew_step(out, 0.0, dt, 10.0, 0.05, 0.5); // 50ms fall
        }
        assert!(out < 0.01, "fall rate from fall time, got {}", out);
    }

    // ── channels & rows ─────────────────────────────────────────────────

    #[test]
    fn add_remove_channels_bounded() {
        let mut s = EnvelopeState::default();
        assert_eq!(s.params.len(), DEFAULT_CHANNELS);
        while add_channel(&mut s) {}
        assert_eq!(s.params.len(), MAX_CHANNELS);
        assert_eq!(s.current_channel, MAX_CHANNELS - 1);
        while remove_channel(&mut s) {}
        assert_eq!(s.params.len(), 1, "last channel survives");
        assert_eq!(s.current_channel, 0);
    }

    #[test]
    fn adjust_steps_params_on_current_channel() {
        let mut s = EnvelopeState {
            current_channel: 1,
            ..Default::default()
        };
        let rise0 = s.params[1].rise_param;
        adjust(&mut s, 0, 2, false);
        assert!((s.params[1].rise_param - (rise0 + 0.01)).abs() < 1e-6);
        assert_eq!(s.params[0].rise_param, rise0, "other channels untouched");
        // offset row clamps
        adjust(&mut s, 4, -100, false);
        assert_eq!(s.params[1].offset, -1.0);
        // binding rows are not h/l adjustable
        let trig = s.params[1].trigger.clone();
        adjust(&mut s, ROW_TRIGGER, 5, true);
        assert_eq!(s.params[1].trigger, trig);
    }

    #[test]
    fn undo_slots_cover_values_and_bindings() {
        use crate::undo::{ParamUndo, ParamValue};
        let mut s = EnvelopeState::default();
        s.set_param(row_slot(2, 4), ParamValue::F32(0.5));
        assert_eq!(s.params[2].offset, 0.5);
        s.set_param(
            row_slot(1, ROW_SIGNAL),
            ParamValue::Src(Some("sequencer/0/t3".into())),
        );
        assert_eq!(
            s.params[1].signal_src.as_ref().unwrap().to_string(),
            "sequencer/0/t3"
        );
        s.set_param(
            row_slot(0, ROW_TRIGGER),
            ParamValue::Src(Some("off".into())),
        );
        assert_eq!(s.params[0].trigger, Trigger::Off);
        assert_eq!(
            s.get_param(row_slot(0, ROW_TRIGGER)),
            Some(ParamValue::Src(Some("off".into())))
        );
    }

    // ── trigger & state format ──────────────────────────────────────────

    #[test]
    fn trigger_param_roundtrip_includes_off() {
        assert_eq!(Trigger::Any.to_param(), None);
        assert_eq!(Trigger::Off.to_param().as_deref(), Some("off"));
        let a = SourceAddr::parse("sequencer/0/t2").unwrap();
        assert_eq!(
            Trigger::Source(a.clone()).to_param().as_deref(),
            Some("sequencer/0/t2")
        );

        assert_eq!(Trigger::from_param(None), Trigger::Any);
        assert_eq!(Trigger::from_param(Some("off")), Trigger::Off);
        assert_eq!(
            Trigger::from_param(Some("sequencer/0/t2")),
            Trigger::Source(a)
        );
    }

    #[test]
    fn old_format_envelope_state_keeps_default_trigger_and_count() {
        let mut s = EnvelopeState::default();
        assert!(matches!(s.params[0].trigger, Trigger::Source(_)));
        let old = state::EnvelopeParams {
            channels: vec![state::EnvelopeChannelParams::default(); 2],
            ..Default::default()
        };
        apply_params(&mut s, &old);
        assert!(
            matches!(s.params[0].trigger, Trigger::Source(_)),
            "old file must not reset the default trigger"
        );
        assert_eq!(
            s.params.len(),
            DEFAULT_CHANNELS,
            "old file must not resize channels"
        );
    }

    #[test]
    fn v2_state_owns_channel_count_and_bindings() {
        let mut s = EnvelopeState::default();
        let chp = state::EnvelopeChannelParams {
            signal_src: Some("sequencer/0/t4".into()),
            trigger_src: Some("off".into()),
            offset: 0.25,
            ..Default::default()
        };
        let p = state::EnvelopeParams {
            format: state::STATE_FORMAT,
            channels: vec![chp; 5],
            ..Default::default()
        };
        apply_params(&mut s, &p);
        assert_eq!(s.params.len(), 5);
        assert_eq!(s.params[4].offset, 0.25);
        assert_eq!(s.params[0].trigger, Trigger::Off);
        assert!(s.params[2].signal_src.is_some());
        // round trip
        let snap = snapshot_params(&s);
        assert_eq!(snap.channels.len(), 5);
        assert_eq!(snap.channels[0].trigger_src.as_deref(), Some("off"));
    }

    #[test]
    fn snapshot_roundtrip_via_toml() {
        let mut s = EnvelopeState::default();
        s.params[1].offset = -0.4;
        s.params[1].signal_src = SourceAddr::parse("sequencer/0/t2");
        let toml_str = state::to_toml_string(&snapshot_params(&s)).unwrap();
        let parsed: state::EnvelopeParams = toml::from_str(&toml_str).unwrap();
        let mut back = EnvelopeState::default();
        apply_params(&mut back, &parsed);
        assert_eq!(back.params[1].offset, -0.4);
        assert_eq!(
            back.params[1].signal_src.as_ref().unwrap().to_string(),
            "sequencer/0/t2"
        );
    }

    #[test]
    fn zero_times_parse_and_format() {
        assert_eq!(parse_time_param("0").unwrap(), 0.0);
        assert_eq!(parse_time_param("0ms").unwrap(), 0.0);
        assert_eq!(format_time(0.0), "0 (instant)");
    }

    #[test]
    fn vari_response_reaches_extreme_curvature() {
        // τ ±9: at full exp the curve stays under 2% through half the
        // segment — the staccato spike territory the hardware lives in
        assert!(
            vari_response(0.5, 1.0) < 0.02,
            "got {}",
            vari_response(0.5, 1.0)
        );
        // and the log mirror is correspondingly explosive at the start
        assert!(
            vari_response(0.05, 0.0) > 0.3,
            "got {}",
            vari_response(0.05, 0.0)
        );
        // mirror symmetry: f_log(x) == 1 - f_exp(1-x)
        for x in [0.1, 0.3, 0.7] {
            let log = vari_response(x, 0.0);
            let exp = vari_response(1.0 - x, 1.0);
            assert!((log - (1.0 - exp)).abs() < 1e-3, "mirror at {}", x);
        }
    }

    #[test]
    fn pluck_decay_snaps_then_rings() {
        let dt = 1.0 / 48000.0;
        let fall = 0.2; // 200ms
        let (mut f, mut s) = (1.0f32, 1.0f32);
        let mut out = 1.0f32;
        let mut t = 0.0f32;
        let mut t_half = None;
        let mut t_done = None;
        let mut prev = f32::MAX;
        while t < 20.0 {
            let (f2, s2, o) = pluck_decay(f, s, dt, fall, 1.0);
            f = f2;
            s = s2;
            out = o;
            assert!(out <= prev + 1e-6, "monotonic decay");
            prev = out;
            t += dt;
            if t_half.is_none() && out < 0.5 {
                t_half = Some(t);
            }
            if out < 0.01 {
                t_done = Some(t);
                break;
            }
        }
        let t_half = t_half.expect("decays through half");
        let t_done = t_done.expect("decays to silence");
        // the snap: drops through 50% well inside a quarter of the fall time
        assert!(t_half < fall * 0.25, "snap too slow: {}s", t_half);
        // the ring: full decay takes several times the fall time
        assert!(t_done > fall * 2.0, "tail too short: {}s", t_done);
        let _ = out;
    }

    #[test]
    fn pluck_zero_tail_is_much_shorter() {
        let dt = 1.0 / 48000.0;
        let fall = 0.2;
        let time_to_silence = |pluck: f32| -> f32 {
            let (mut f, mut s) = (1.0f32, 1.0f32);
            let mut t = 0.0f32;
            loop {
                let (f2, s2, o) = pluck_decay(f, s, dt, fall, pluck);
                f = f2;
                s = s2;
                t += dt;
                if o < 0.01 || t > 30.0 {
                    return t;
                }
            }
        };
        assert!(
            time_to_silence(1.0) > time_to_silence(0.1) * 2.0,
            "pluck stretches the ring"
        );
    }

    fn sp(rise: f32, fall: f32, gate: bool, pluck: f32) -> StageParams {
        StageParams {
            rise_time: rise,
            fall_time: fall,
            shape: 0.5,
            loop_mode: false,
            gate_mode: gate,
            pluck,
        }
    }

    #[test]
    fn fresh_session_channel_wiring_is_spikey_plucks() {
        let s = EnvelopeState::default();
        assert_eq!(s.params.len(), 4);
        // odd channels feed the voices, even ones stay free
        for (i, want) in [
            (0, Some("sequencer/0/t1")),
            (1, None),
            (2, Some("sequencer/0/t3")),
            (3, None),
        ] {
            match (&s.params[i].trigger, want) {
                (Trigger::Source(a), Some(w)) => assert_eq!(a.to_string(), w),
                (Trigger::Off, None) => {}
                (got, want) => panic!("ch{} trigger {:?}, wanted {:?}", i + 1, got, want),
            }
        }
        // the pluck character: instant strike, ~145ms exponential decay
        let p = ChannelParams::default();
        assert_eq!(p.rise_param, 0.0, "instant rise");
        assert_eq!(param_to_time(p.rise_param), 0.0);
        let fall = param_to_time(p.fall_param);
        assert!((0.1..0.25).contains(&fall), "short fall, got {fall}s");
        assert!(p.shape_param > 0.8, "strongly exponential");
        assert!(p.pluck > 0.5, "deep vactrol pluck");
        assert!(!p.gate_mode, "trig semantics");
    }

    #[test]
    fn trig_mode_fires_full_ad_without_note_off() {
        let dt = 1.0 / 48000.0;
        let p = sp(0.01, 0.01, false, 0.0); // 10ms / 10ms
        let mut ch = EnvelopeChannel {
            stage: Stage::Rise,
            ..Default::default()
        };
        let mut peaked = false;
        let mut finished = false;
        for _ in 0..48000 {
            advance_stage(&mut ch, &p, dt);
            if ch.output > 0.99 {
                peaked = true;
            }
            if peaked && ch.stage == Stage::Off {
                finished = true;
                break;
            }
        }
        assert!(peaked, "trig must reach the top");
        assert!(finished, "trig must fall back to Off with no release event");
    }

    #[test]
    fn gate_mode_sustains_at_top() {
        let dt = 1.0 / 48000.0;
        let p = sp(0.001, 0.01, true, 0.0);
        let mut ch = EnvelopeChannel {
            stage: Stage::Rise,
            ..Default::default()
        };
        for _ in 0..4800 {
            advance_stage(&mut ch, &p, dt);
        }
        assert_eq!(ch.stage, Stage::Sustain, "gate holds until note off");
        assert_eq!(ch.output, 1.0);
    }

    #[test]
    fn sustain_releases_when_gate_mode_flips_to_trig() {
        let dt = 1.0 / 48000.0;
        let mut ch = EnvelopeChannel {
            stage: Stage::Sustain,
            output: 1.0,
            ..Default::default()
        };
        // still a gate: sustain holds
        advance_stage(&mut ch, &sp(0.001, 0.005, true, 0.0), dt);
        assert_eq!(ch.stage, Stage::Sustain);
        // user flips the channel to trig mid-sustain: must fall, not hang
        let p = sp(0.001, 0.005, false, 0.0);
        advance_stage(&mut ch, &p, dt);
        assert_eq!(
            ch.stage,
            Stage::Fall,
            "sustain with no possible release falls"
        );
        for _ in 0..480 {
            advance_stage(&mut ch, &p, dt);
        }
        assert_eq!(ch.stage, Stage::Off, "and reaches silence");
        assert_eq!(ch.output, 0.0);
    }

    #[test]
    fn instant_rise_and_fall_produce_silence() {
        let dt = 1.0 / 48000.0;
        let p = sp(0.0, 0.0, false, 0.0);
        let mut ch = EnvelopeChannel {
            stage: Stage::Rise,
            ..Default::default()
        };
        let eoc = advance_stage(&mut ch, &p, dt);
        // sample 1: instant rise -> straight into fall
        let eoc2 = advance_stage(&mut ch, &p, dt);
        assert!(eoc || eoc2, "cycle completes immediately");
        assert_eq!(ch.output, 0.0, "hard drop: no residual level");
        assert_eq!(ch.stage, Stage::Off);
    }

    #[test]
    fn trig_mode_pluck_rings_after_instant_strike() {
        let dt = 1.0 / 48000.0;
        let p = sp(0.0, 0.15, false, 0.9);
        let mut ch = EnvelopeChannel {
            stage: Stage::Rise,
            ..Default::default()
        };
        advance_stage(&mut ch, &p, dt); // strike
        let mut t = 0.0;
        let mut above_tenth_at_100ms = false;
        while ch.stage != Stage::Off && t < 10.0 {
            advance_stage(&mut ch, &p, dt);
            t += dt;
            if (0.099..0.101).contains(&t) && ch.output > 0.05 {
                above_tenth_at_100ms = true;
            }
        }
        assert!(above_tenth_at_100ms, "the ring is still audible at 100ms");
        assert!(t > 0.15, "tail outlives the nominal fall time");
    }

    #[test]
    fn vari_inverse_roundtrips() {
        for shape in [0.0, 0.25, 0.5, 0.75, 1.0] {
            for i in 0..=20 {
                let x = i as f32 / 20.0;
                let y = vari_response(x, shape);
                let back = vari_inverse(y, shape);
                assert!((back - x).abs() < 1e-3, "shape {shape} x {x}: {back}");
            }
        }
    }

    /// The anti-click contract: however hard you retrigger, output never
    /// steps DOWN in a single sample (rising snaps are the instrument).
    #[test]
    fn retrigger_never_steps_down() {
        let dt = 1.0 / 48000.0;
        let p = StageParams {
            rise_time: 0.02,
            fall_time: 0.2,
            shape: 0.5,
            loop_mode: false,
            gate_mode: false,
            pluck: 0.8,
        };
        let mut ch = EnvelopeChannel {
            stage: Stage::Rise,
            ..Default::default()
        };
        let mut prev = ch.output;
        let mut max_down = 0.0f32;
        // a 2-second trigger storm: retrigger every 50ms, exactly the way
        // env_thread does it (Rise + vari_inverse of the live output)
        for n in 0..(48000 * 2) {
            if n % 2400 == 0 && n > 0 {
                ch.stage = Stage::Rise;
                ch.phase = vari_inverse(ch.output, p.shape);
            }
            advance_stage(&mut ch, &p, dt);
            max_down = max_down.max(prev - ch.output);
            prev = ch.output;
        }
        // steepest legitimate slope is the pluck fast pole; a hard reset
        // used to register ~0.3 here
        assert!(max_down < 0.02, "downward step {max_down}");
    }

    #[test]
    fn cycle_restart_is_continuous() {
        let dt = 1.0 / 48000.0;
        let p = StageParams {
            rise_time: 0.05,
            fall_time: 0.2,
            shape: 0.5,
            loop_mode: true,
            gate_mode: false,
            pluck: 0.8,
        };
        let mut ch = EnvelopeChannel {
            stage: Stage::Rise,
            ..Default::default()
        };
        let mut prev = ch.output;
        let mut max_down = 0.0f32;
        let mut cycles = 0;
        for _ in 0..(48000 * 5) {
            if advance_stage(&mut ch, &p, dt) {
                cycles += 1;
            }
            max_down = max_down.max(prev - ch.output);
            prev = ch.output;
        }
        assert!(cycles >= 10, "still cycles on time: {cycles}");
        assert!(max_down < 0.02, "restart cliff {max_down}");
    }

    #[test]
    fn cycling_with_pluck_restarts_on_time_not_tail() {
        let dt = 1.0 / 48000.0;
        // rise 50ms, fall 200ms, pluck well up: a cycle should take about
        // rise+fall, NOT the ~10s the -60dB vactrol tail needs
        let p = StageParams {
            rise_time: 0.05,
            fall_time: 0.2,
            shape: 0.5,
            loop_mode: true,
            gate_mode: false,
            pluck: 0.8,
        };
        let mut ch = EnvelopeChannel {
            stage: Stage::Rise,
            ..Default::default()
        };
        let mut cycles = 0;
        for _ in 0..(48000 * 5) {
            if advance_stage(&mut ch, &p, dt) {
                cycles += 1;
            }
        }
        assert!(
            cycles >= 12,
            "5s at ~250ms/cycle should loop plenty: {cycles}"
        );
        // and WITHOUT cycling, the tail still rings long (unchanged)
        let p2 = StageParams {
            loop_mode: false,
            ..p
        };
        let mut ch2 = EnvelopeChannel {
            stage: Stage::Fall,
            output: 1.0,
            ..Default::default()
        };
        let mut t = 0.0f32;
        while ch2.stage == Stage::Fall && t < 20.0 {
            advance_stage(&mut ch2, &p2, dt);
            t += dt;
        }
        assert!(
            t > 0.4,
            "non-cycling pluck tail outlives the fall time: {t}"
        );
    }

    #[test]
    fn cycling_ignores_gate_mode() {
        let dt = 1.0 / 48000.0;
        let p = StageParams {
            loop_mode: true,
            ..sp(0.005, 0.005, true, 0.0)
        };
        let mut ch = EnvelopeChannel {
            stage: Stage::Rise,
            ..Default::default()
        };
        let mut cycles = 0;
        for _ in 0..48000 {
            if advance_stage(&mut ch, &p, dt) {
                cycles += 1;
            }
        }
        assert!(
            cycles >= 90,
            "cycle mode keeps oscillating even in gate mode: {}",
            cycles
        );
    }

    #[test]
    fn gate_mode_param_persists_per_channel() {
        let mut s = EnvelopeState::default();
        s.params[2].gate_mode = true;
        let snap = snapshot_params(&s);
        assert!(!snap.channels[0].gate_mode);
        assert!(snap.channels[2].gate_mode, "per-channel, not global");
        let mut back = EnvelopeState::default();
        apply_params(&mut back, &snap);
        assert!(back.params[2].gate_mode && !back.params[1].gate_mode);
    }
}
