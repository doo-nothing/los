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
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
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
}

impl Default for EnvelopeChannel {
    fn default() -> Self {
        Self { stage: Stage::Off, phase: 0.0, output: 0.0 }
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
            Some(a) => SourceAddr::parse(a).map(Trigger::Source).unwrap_or(Trigger::Any),
        }
    }
}

#[derive(Clone)]
struct ChannelParams {
    rise_param: f32,   // 0.0-1.0 → 0.5ms to 25min (exponential taper)
    fall_param: f32,
    shape_param: f32,  // 0.0 log (RC) … 0.5 linear … 1.0 exponential
    loop_mode: bool,
    attenuverter: f32, // -1.0 to 1.0
    offset: f32,       // -1.0 to 1.0, post-attenuverter DC offset
    // Receiver-side bindings (routing.rs source addresses).
    trigger: Trigger,
    /// When bound, the channel stops generating and slews this source
    /// (Maths' signal input: portamento / lag / CV smoothing).
    signal_src: Option<SourceAddr>,
    rise_src: Option<SourceAddr>,
    fall_src: Option<SourceAddr>,
    shape_src: Option<SourceAddr>,
    atten_src: Option<SourceAddr>,
}

impl Default for ChannelParams {
    fn default() -> Self {
        Self {
            rise_param: 0.42, // ~100ms on the new taper
            fall_param: 0.42,
            shape_param: 0.5, // linear
            loop_mode: false,
            attenuverter: 1.0,
            offset: 0.0,
            trigger: Trigger::Any,
            signal_src: None,
            rise_src: None,
            fall_src: None,
            shape_src: None,
            atten_src: None,
        }
    }
}

// ── engine math ─────────────────────────────────────────────────────────────

/// Time-range constants: 0.5 ms to 25 minutes (Maths-spec).
const TIME_MIN: f32 = 0.0005;
const TIME_RANGE: f32 = 3_000_000.0; // TIME_MIN * RANGE = 1500s = 25min

/// Exponential parameter → seconds. 0.0 → 0.5ms, ~0.42 → 100ms, 1.0 → 25min.
fn param_to_time(param: f32) -> f32 {
    TIME_MIN * TIME_RANGE.powf(param.clamp(0.0, 1.0))
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
    (t > 0.0).then(|| time_to_param(t * mult))
}

/// Display a time with auto units.
fn format_time(t: f32) -> String {
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
/// the analog curves: f(x) = (e^(τx) − 1) / (e^τ − 1), τ ∈ [−6, +6].
fn vari_response(x: f32, shape: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    let tau = (shape.clamp(0.0, 1.0) - 0.5) * 12.0;
    if tau.abs() < 0.01 {
        x
    } else {
        (tau * x).exp_m1() / tau.exp_m1()
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
    let time = if diff > 0.0 { rise_t } else { fall_t }.max(TIME_MIN);
    let lin = dt / time; // full-scale per `time`
    let rc = (dt / (time * 0.2)) * diff.abs(); // τ ≈ time/5
    let rc_weight = ((0.5 - shape.clamp(0.0, 1.0)) * 2.0).max(0.0);
    let step = (lin + (rc - lin) * rc_weight).min(diff.abs());
    out + diff.signum() * step
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
        // Channel 1 defaults to sequencer track 1; the rest to any-note
        if let Some(a) = SourceAddr::parse("sequencer/0/t1") {
            params[0].trigger = Trigger::Source(a);
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

// Rows: 0 rise, 1 fall, 2 shape, 3 atten, 4 offset, 5 signal, 6 trigger.
const NUM_ROWS: usize = 7;
const ROW_SIGNAL: usize = 5;
const ROW_TRIGGER: usize = 6;

/// Undo slots: ch*32 + row for values (0–4) and loop (5); ch*32 + 8 + n for
/// the six bindings (rise/fall/shape/atten/signal/trigger).
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
            r if (BIND_OFF..BIND_OFF + 6).contains(&r) => {
                let b = match r - BIND_OFF {
                    0 => p.rise_src.as_ref().map(|a| a.to_string()),
                    1 => p.fall_src.as_ref().map(|a| a.to_string()),
                    2 => p.shape_src.as_ref().map(|a| a.to_string()),
                    3 => p.atten_src.as_ref().map(|a| a.to_string()),
                    4 => p.signal_src.as_ref().map(|a| a.to_string()),
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
        let Some(p) = self.params.get_mut(ch) else { return };
        match (row, value) {
            (0, V::F32(v)) => p.rise_param = v,
            (1, V::F32(v)) => p.fall_param = v,
            (2, V::F32(v)) => p.shape_param = v,
            (3, V::F32(v)) => p.attenuverter = v,
            (4, V::F32(v)) => p.offset = v,
            (5, V::Bool(v)) => p.loop_mode = v,
            (r, V::Src(a)) if (BIND_OFF..BIND_OFF + 6).contains(&r) => {
                if r - BIND_OFF == 5 {
                    p.trigger = Trigger::from_param(a.as_deref());
                } else {
                    let addr = a.as_deref().and_then(SourceAddr::parse);
                    match r - BIND_OFF {
                        0 => p.rise_src = addr,
                        1 => p.fall_src = addr,
                        2 => p.shape_src = addr,
                        3 => p.atten_src = addr,
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
        _ => {} // signal/trigger rows are binding-only (@ opens the picker)
    }
}

// ── params snapshot/apply ───────────────────────────────────────────────────

fn snapshot_params(s: &EnvelopeState) -> state::EnvelopeParams {
    state::EnvelopeParams {
        format: state::STATE_FORMAT,
        channels: s.params.iter().map(|p| state::EnvelopeChannelParams {
            rise: p.rise_param,
            fall: p.fall_param,
            shape: p.shape_param,
            loop_mode: p.loop_mode,
            attenuverter: p.attenuverter,
            offset: p.offset,
            signal_src: p.signal_src.as_ref().map(|a| a.to_string()),
            trigger_src: p.trigger.to_param(),
            rise_src: p.rise_src.as_ref().map(|a| a.to_string()),
            fall_src: p.fall_src.as_ref().map(|a| a.to_string()),
            shape_src: p.shape_src.as_ref().map(|a| a.to_string()),
            atten_src: p.atten_src.as_ref().map(|a| a.to_string()),
        }).collect(),
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
            s.params[i].signal_src = ch.signal_src.as_deref().and_then(SourceAddr::parse);
            s.params[i].trigger = Trigger::from_param(ch.trigger_src.as_deref());
            s.params[i].rise_src = ch.rise_src.as_deref().and_then(SourceAddr::parse);
            s.params[i].fall_src = ch.fall_src.as_deref().and_then(SourceAddr::parse);
            s.params[i].shape_src = ch.shape_src.as_deref().and_then(SourceAddr::parse);
            s.params[i].atten_src = ch.atten_src.as_deref().and_then(SourceAddr::parse);
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
    mods: [Option<usize>; 4],
    signal: Option<usize>,
}

fn env_thread(
    state: Arc<Mutex<EnvelopeState>>,
    shutdown: std::sync::mpsc::Receiver<()>,
    instance: usize,
) -> Result<()> {
    let consumer_id = crate::shm::consumer_id("envelope", instance);
    let mut events = EventRingbuf::open(consumer_id).ok();
    let mut modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();
    let manifest = Manifest::open().or_else(|_| Manifest::create())?;
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
            events = EventRingbuf::open(consumer_id).ok();
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
                    p.rise_src.as_ref().and_then(|a| routing::resolve(&entries, a)),
                    p.fall_src.as_ref().and_then(|a| routing::resolve(&entries, a)),
                    p.shape_src.as_ref().and_then(|a| routing::resolve(&entries, a)),
                    p.atten_src.as_ref().and_then(|a| routing::resolve(&entries, a)),
                ];
                r.signal = p.signal_src.as_ref().and_then(|a| routing::resolve(&entries, a));
            }
        }
        refresh_in -= 1;

        // Note events + manual TRIGGER events
        let mut triggers = [false; MAX_CHANNELS];
        let mut track_trigger: Option<u8> = None;
        let mut release_track: Option<u8> = None;
        let mut event_count = 0u32;
        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                event_count += 1;
                match event.event_type {
                    0 => track_trigger = Some(event.source),
                    1 => release_track = Some(event.source),
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

        // Edge-triggered bindings fire on a 0.5 upward crossing
        for i in 0..n {
            if let Some(RTrig::Edge(chan)) = resolved[i].trig {
                let val = modbus.as_ref().map(|m| m.get(chan)).unwrap_or(0.0);
                if edge_prev[i] <= 0.5 && val > 0.5 {
                    triggers[i] = true;
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
                Some(RTrig::AnyNote) | None => track_trigger.is_some() || triggers[i],
                Some(RTrig::Off) | Some(RTrig::Edge(_)) => triggers[i],
                Some(RTrig::Note(want)) => track_trigger == Some(want) || triggers[i],
            };
            let should_release = match r.trig {
                Some(RTrig::AnyNote) | None => release_track.is_some(),
                Some(RTrig::Off) | Some(RTrig::Edge(_)) => false,
                Some(RTrig::Note(want)) => release_track == Some(want),
            };

            if should_release && ch.stage != Stage::Off && ch.stage != Stage::Fall {
                ch.stage = Stage::Fall;
                ch.phase = 0.0;
            }
            if should_trigger {
                ch.stage = Stage::Rise;
                ch.phase = 0.0;
            }

            // Param modulation: bound + resolvable -> modbus value
            let chan_val = |c: Option<usize>| c.and_then(|c| modbus.as_ref().map(|m| m.get(c)));
            let rp = chan_val(r.mods[0]).map(|v| v.clamp(0.0, 1.0)).unwrap_or(params.rise_param);
            let fp = chan_val(r.mods[1]).map(|v| v.clamp(0.0, 1.0)).unwrap_or(params.fall_param);
            let sp = chan_val(r.mods[2]).map(|v| v.clamp(0.0, 1.0)).unwrap_or(params.shape_param);
            let att = chan_val(r.mods[3]).map(|v| v.clamp(-1.0, 1.0)).unwrap_or(params.attenuverter);

            let rise_time = param_to_time(rp);
            let fall_time = param_to_time(fp);
            let signal_target = r.signal.and_then(|c| modbus.as_ref().map(|m| m.get(c)));

            for frame in 0..BLOCK_SIZE {
                if let Some(target) = signal_target {
                    // Signal-input mode: slew limiter
                    ch.output = slew_step(ch.output, target, dt, rise_time, fall_time, sp);
                    ch.stage = Stage::Off;
                } else {
                    match ch.stage {
                        Stage::Off => {
                            ch.output = 0.0;
                            if params.loop_mode {
                                ch.stage = Stage::Rise;
                                ch.phase = 0.0;
                            }
                        }
                        Stage::Rise => {
                            ch.phase += dt / rise_time.max(TIME_MIN);
                            if ch.phase >= 1.0 {
                                ch.phase = 1.0;
                                ch.output = 1.0;
                                if params.loop_mode {
                                    ch.stage = Stage::Fall;
                                    ch.phase = 0.0;
                                } else {
                                    ch.stage = Stage::Sustain;
                                }
                            } else {
                                ch.output = vari_response(ch.phase, sp);
                            }
                        }
                        Stage::Sustain => {
                            ch.output = 1.0;
                        }
                        Stage::Fall => {
                            ch.phase += dt / fall_time.max(TIME_MIN);
                            if ch.phase >= 1.0 {
                                ch.phase = 1.0;
                                ch.output = 0.0;
                                if i == n - 1 {
                                    eoc_pulse = true;
                                }
                                if params.loop_mode {
                                    ch.stage = Stage::Rise;
                                    ch.phase = 0.0;
                                } else {
                                    ch.stage = Stage::Off;
                                }
                            } else {
                                ch.output = 1.0 - vari_response(ch.phase, sp);
                            }
                        }
                    }
                }
                // audio path: attenuverted function (offsets excluded — DC)
                let a = ch.output * att;
                audio_buf[frame * 2] += a;
                audio_buf[frame * 2 + 1] += a;
            }

            ch_final[i] = (ch.output * att + params.offset).clamp(-1.0, 1.0);
        }

        // Buses + gates
        let sum = ch_final[..n].iter().sum::<f32>().clamp(-1.0, 1.0);
        let or_val = ch_final[..n].iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let and_val = ch_final[..n].iter().copied().fold(f32::INFINITY, f32::min);
        let invert = -ch_final[0];
        let eor = if matches!(s.channels[0].stage, Stage::Sustain | Stage::Fall) { 1.0 } else { 0.0 };
        let eoc = if s.channels[n - 1].stage == Stage::Off || eoc_pulse { 1.0 } else { 0.0 };

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
    let blocks = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let idx = ((v.abs().clamp(0.0, 1.0)) * 7.0).round() as usize;
    let c = blocks[idx.min(7)];
    let sign = if v < -0.005 { '-' } else { ' ' };
    format!("{}{}", sign, c)
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &EnvelopeState,
    selected: usize,
    show_help: bool,
    overlay: Option<&str>,
    picker: Option<(Vec<String>, usize)>,
) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        let n = state.params.len();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);

        // Maths panel: one column per channel + a logic column
        let col_constraints: Vec<Constraint> =
            (0..=n).map(|_| Constraint::Ratio(1, (n + 1) as u32)).collect();
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(col_constraints)
            .split(chunks[0]);

        let row_names = ["Rise", "Fall", "Shap", "Attn", "Offs", "Sig ", "Trig"];
        for i in 0..n {
            let p = &state.params[i];
            let ch = &state.channels[i];
            let is_cur = i == state.current_channel;

            let bound = |a: &Option<SourceAddr>| if a.is_some() { "@" } else { " " };
            let trig_short = match &p.trigger {
                Trigger::Any => String::from("any"),
                Trigger::Off => String::from("off"),
                Trigger::Source(a) => a.output.clone(),
            };
            let values = [
                format!("{}{}", format_time(param_to_time(p.rise_param)), bound(&p.rise_src)),
                format!("{}{}", format_time(param_to_time(p.fall_param)), bound(&p.fall_src)),
                format!("{:.2}{}", p.shape_param, bound(&p.shape_src)),
                format!("{:+.2}{}", p.attenuverter, bound(&p.atten_src)),
                format!("{:+.2}", p.offset),
                p.signal_src.as_ref().map(|a| a.output.clone()).unwrap_or_else(|| "—".into()),
                trig_short,
            ];

            let mut lines: Vec<Line> = Vec::with_capacity(NUM_ROWS + 2);
            let flags = format!(
                "{}{}",
                if p.loop_mode { " CYC" } else { "" },
                match ch.stage {
                    Stage::Rise => " ↗",
                    Stage::Fall => " ↘",
                    Stage::Sustain => " ―",
                    Stage::Off => "",
                }
            );
            lines.push(Line::from(format!("Ch{}{}", i + 1, flags)));
            for (row, (name, val)) in row_names.iter().zip(values.iter()).enumerate() {
                let text = format!("{} {}", name, val);
                let style = if is_cur && row == selected {
                    Style::default().fg(Color::Black).bg(Color::Yellow)
                } else if is_cur {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                lines.push(Line::styled(text, style));
            }
            lines.push(Line::from(format!("out {} {:+.2}", meter(ch.output), ch.output)));

            let border_style = if is_cur {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let block = Block::default().borders(Borders::ALL).border_style(border_style);
            f.render_widget(Paragraph::new(lines).block(block), cols[i]);
        }

        // Logic column
        {
            let outs: Vec<f32> = state
                .channels
                .iter()
                .zip(state.params.iter())
                .map(|(c, p)| (c.output * p.attenuverter + p.offset).clamp(-1.0, 1.0))
                .collect();
            let sum = outs.iter().sum::<f32>().clamp(-1.0, 1.0);
            let or_v = outs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let and_v = outs.iter().copied().fold(f32::INFINITY, f32::min);
            let eor = matches!(state.channels[0].stage, Stage::Sustain | Stage::Fall);
            let eoc = state.channels[n - 1].stage == Stage::Off;
            let lines = vec![
                Line::from("Logic"),
                Line::from(format!("SUM {} {:+.2}", meter(sum), sum)),
                Line::from(format!("OR  {} {:+.2}", meter(or_v), or_v)),
                Line::from(format!("AND {} {:+.2}", meter(and_v), and_v)),
                Line::from(format!("INV {} {:+.2}", meter(-outs[0]), -outs[0])),
                Line::from(format!("EOR {}", if eor { "●" } else { "○" })),
                Line::from(format!("EOC {}", if eoc { "●" } else { "○" })),
                Line::from(format!("gate {}", if state.gate { "●" } else { "○" })),
            ];
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan));
            f.render_widget(Paragraph::new(lines).block(block), cols[n]);
        }

        // Status line: full binding detail for the selected row
        let p = &state.params[state.current_channel.min(n - 1)];
        let detail = match selected {
            0 => p.rise_src.as_ref().map(|a| format!("rise @{}", a)),
            1 => p.fall_src.as_ref().map(|a| format!("fall @{}", a)),
            2 => p.shape_src.as_ref().map(|a| format!("shape @{}", a)),
            3 => p.atten_src.as_ref().map(|a| format!("atten @{}", a)),
            ROW_SIGNAL => p.signal_src.as_ref().map(|a| format!("signal @{} (slew mode)", a)),
            ROW_TRIGGER => Some(match &p.trigger {
                Trigger::Any => String::from("trigger: any note"),
                Trigger::Off => String::from("trigger: off (manual t / cycle only)"),
                Trigger::Source(a) => format!("trigger @{}", a),
            }),
            _ => None,
        };
        let status = match overlay {
            Some(text) => text.to_string(),
            None => detail.unwrap_or_else(|| {
                format!(
                    "Ch{}/{} | a/x add/remove ch | c cycle | t trig | o gate | @ bind | ? help",
                    state.current_channel + 1,
                    n
                )
            }),
        };
        let style = if overlay.is_some() {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Cyan)
        };
        f.render_widget(Paragraph::new(status).style(style), chunks[1]);

        // Help overlay
        if show_help {
            let help_text = vec![
                Line::from("━━━ Maths (envelope) Help ━━━"),
                Line::from(""),
                Line::from("Navigation:"),
                Line::from("  [ / ]      Prev/next channel (counts)"),
                Line::from("  gg / G     First / last channel"),
                Line::from("  j/k        Select row   h/l adjust (H/L coarse)"),
                Line::from(""),
                Line::from("Rows: Rise, Fall (0.5ms–25min), Shape"),
                Line::from("  (log↔lin↔exp), Atten, Offset, Sig, Trig"),
                Line::from(""),
                Line::from("Actions:"),
                Line::from("  a / x      Add / remove channel"),
                Line::from("  c          Toggle cycle (loop) mode"),
                Line::from("  t          Trigger channel manually"),
                Line::from("  o          Toggle gate on/off (sustain)"),
                Line::from("  @          Bind row to a source (picker)"),
                Line::from("             Sig row = slew input; Trig row"),
                Line::from("             offers any-note / off / sources"),
                Line::from("             (non-note source = edge trigger)"),
                Line::from("  u / ^r     Undo / redo (counts)"),
                Line::from("  :set rise 100ms | 2s | 1.5m | 0.42"),
                Line::from("  :w/:e/:q   Patch save/load, quit"),
                Line::from(""),
                Line::from("Outputs: ch1..ch6, sum, or, and, inv,"),
                Line::from("  eor (ch1), eoc (last ch) + audio out"),
                Line::from(""),
                Line::from("  space      Play/pause (global)"),
                Line::from("  ?          Toggle this help"),
            ];
            let help = Paragraph::new(help_text)
                .style(Style::default().fg(Color::White).bg(Color::Black))
                .block(Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title("Help"));
            f.render_widget(help, area);
        }

        // Source picker overlay (@)
        if let Some((rows, sel)) = picker {
            let h = (rows.len() as u16 + 2).min(area.height);
            let w = rows.iter().map(|r| r.len()).max().unwrap_or(10).max(20) as u16 + 4;
            let r = ratatui::layout::Rect::new(
                (area.width.saturating_sub(w)) / 2,
                (area.height.saturating_sub(h)) / 2,
                w.min(area.width),
                h,
            );
            f.render_widget(ratatui::widgets::Clear, r);
            let items: Vec<ratatui::widgets::ListItem> = rows
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    let style = if i == sel {
                        Style::default().fg(Color::Black).bg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    ratatui::widgets::ListItem::new(row.clone()).style(style)
                })
                .collect();
            let list = ratatui::widgets::List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title("Bind source (Enter binds, x unbinds, Esc cancels)"),
            );
            f.render_widget(list, r);
        }
    })?;

    Ok(())
}

// ── main loop ───────────────────────────────────────────────────────────────

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("envelope", instance);
    let shm_name = format!("/los_audio_envelope_{}", instance);
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let _ = manifest.register("envelope", instance, Some(&shm_name), CLAIMED_OUTPUTS);
    let claimed_base = manifest.claimed_base();

    for attempt in 0..20 {
        match enable_raw_mode() {
            Ok(()) => break,
            Err(e) => {
                if attempt < 19 {
                    std::thread::sleep(Duration::from_millis(200));
                } else {
                    return Err(anyhow::anyhow!("Failed to enable raw mode after 20 attempts: {}", e));
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
    let mut baseline = state::to_toml_string(&snapshot_params(&state.lock().unwrap())).unwrap_or_default();
    let mut should_quit = false;
    // Global transport handle for Space = play/pause (lazily reopened)
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();

    loop {
        if state::check_save_signal() {
            let params = snapshot_params(&state.lock().unwrap());
            let _ = state::save_module_state("envelope", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(params) = state::load_module_state::<state::EnvelopeParams>("envelope", instance) {
                apply_params(&mut state.lock().unwrap(), &params);
            }
        }

        let current_state = state.lock().unwrap().clone();
        let overlay = if ex.is_active() {
            Some(ex.display())
        } else {
            ex_msg.clone()
        };
        let picker_rows = if picker.is_active() { Some(picker.rows()) } else { None };
        draw_ui(&mut terminal, &current_state, selected, show_help, overlay.as_deref(), picker_rows)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
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
                        let slot = if selected <= 3 {
                            s.current_channel * CH_SLOT_STRIDE + BIND_OFF + selected
                        } else {
                            row_slot(s.current_channel, selected)
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
                    let candidates = crate::excmd::patch_names(&state::patches_dir());
                    if let crate::excmd::ExEvent::Submit(cmd) = ex.handle_key(key.code, &candidates) {
                        use crate::excmd::ExCommand;
                        let params = snapshot_params(&state.lock().unwrap());
                        match cmd {
                            ExCommand::Write(name) => {
                                ex_msg = Some(match crate::excmd::ex_write(name, &mut patch_name, &mut baseline, &params) {
                                    Ok(m) | Err(m) => m,
                                });
                            }
                            ExCommand::Edit(name) => match state::load_patch::<state::EnvelopeParams>(&name) {
                                Ok(p) => {
                                    apply_params(&mut state.lock().unwrap(), &p);
                                    baseline = state::to_toml_string(&snapshot_params(&state.lock().unwrap())).unwrap_or_default();
                                    patch_name = Some(name.clone());
                                    ex_msg = Some(format!("Loaded {}", name));
                                }
                                Err(e) => ex_msg = Some(e.to_string()),
                            },
                            ExCommand::Quit { force } => {
                                if !force && crate::excmd::is_dirty(&params, &baseline) {
                                    ex_msg = Some(String::from("Unsaved changes (:q! to discard, :w <name> to save)"));
                                } else {
                                    should_quit = true;
                                }
                            }
                            ExCommand::WriteQuit(name) => {
                                match crate::excmd::ex_write(name, &mut patch_name, &mut baseline, &params) {
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
                                    "offset" => v.parse().ok().map(|p: f32| (4, p.clamp(-1.0, 1.0))),
                                    _ => None,
                                };
                                match parsed {
                                    Some((row, val)) => {
                                        let slot = ch * CH_SLOT_STRIDE + row;
                                        let old = s.get_param(slot);
                                        s.set_param(slot, ParamValue::F32(val));
                                        if let Some(old) = old {
                                            history.record(slot, "Set", old, ParamValue::F32(val));
                                        }
                                        ex_msg = Some(match row {
                                            0 | 1 => format!("{} = {}", k, format_time(param_to_time(val))),
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
                    ex_msg = Some(crate::undo::history_status("Redo", n, || history.redo(&mut *s)));
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
                        ex_msg = Some(crate::undo::history_status("Undo", n, || history.undo(&mut *s)));
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
                            4 => {} // offset has no binding
                            _ => {
                                let current = match selected {
                                    0 => s.params[ch].rise_src.clone(),
                                    1 => s.params[ch].fall_src.clone(),
                                    2 => s.params[ch].shape_src.clone(),
                                    3 => s.params[ch].atten_src.clone(),
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
    fn time_taper_spans_half_ms_to_25_min() {
        assert!((param_to_time(0.0) - 0.0005).abs() < 1e-6);
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
        assert!((parse_time_param("0.42").unwrap() - 0.42).abs() < 1e-6, "bare value = param");
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
        assert!((out - 1.0).abs() < 1e-3, "linear slew completes in time, got {}", out);

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
        let mut s = EnvelopeState { current_channel: 1, ..Default::default() };
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
        s.set_param(row_slot(1, ROW_SIGNAL), ParamValue::Src(Some("sequencer/0/t3".into())));
        assert_eq!(s.params[1].signal_src.as_ref().unwrap().to_string(), "sequencer/0/t3");
        s.set_param(row_slot(0, ROW_TRIGGER), ParamValue::Src(Some("off".into())));
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
        assert_eq!(Trigger::Source(a.clone()).to_param().as_deref(), Some("sequencer/0/t2"));

        assert_eq!(Trigger::from_param(None), Trigger::Any);
        assert_eq!(Trigger::from_param(Some("off")), Trigger::Off);
        assert_eq!(Trigger::from_param(Some("sequencer/0/t2")), Trigger::Source(a));
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
        assert_eq!(s.params.len(), DEFAULT_CHANNELS, "old file must not resize channels");
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
        assert_eq!(back.params[1].signal_src.as_ref().unwrap().to_string(), "sequencer/0/t2");
    }
}
