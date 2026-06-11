//! delay — a time domain processor, after the Buchla 288 / Verbos MDP
//! (docs/plans/delay-288.md).
//!
//! The first **fx module**: it consumes another module's audio ringbuffer
//! (claimed through the manifest's v3 `input_shm` field, which makes the
//! mixer release that strip — the cable leaves the console), runs it
//! through an 8-tap delay line, and produces its own stereo out that the
//! mixer adopts like any voice.
//!
//! Eight series taps with per-tap fader / pan / phase (the 288's phase
//! select), a swept time control that repitches the line, the MDP's
//! three feedback characters — regen, shimmer (+1 oct), wash (reverb;
//! both Faust, see tap8fx.dsp and docs/writing-dsp.md) — and an envelope
//! follower per tap published on the modbus (`delay/N/in`, `…/t1…t8`).
//! Every continuous param takes a mod input.

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
use crate::shm::{AudioRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

pub mod dsp;

/// The committed Faust codegen for the tap-8 feedback characters
/// (shimmer + wash). Regenerate with `just dsp`; never edit the _gen
/// file. The prelude types live in [`crate::faust`].
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
pub mod tap8fx {
    use crate::faust::*;
    include!("delay/tap8fx_gen.rs");
}

use dsp::MAX_TAPS;

/// Used only until the transport answers with the device's real rate.
const FALLBACK_RATE: f32 = 48_000.0;

// ── strips & rows ──────────────────────────────────────────────────────────
//
// The console layout: strips 0..=7 are taps T1…T8, strip 8 is GLOBAL.
// Like the mixer it's a 2D grid — h/l strips, j/k rows, -/= adjusts.

const GLOBAL_STRIP: usize = MAX_TAPS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapRow {
    Pan,
    /// The 288's phase select: normal / off / inverted. `off` is the
    /// tap's mute.
    Phase,
    Level,
}
const TAP_ROWS: [TapRow; 3] = [TapRow::Pan, TapRow::Phase, TapRow::Level];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalRow {
    /// Which audio source the delay consumes ("voice/0").
    Input,
    /// Per-stage time; tap i sits at i × time.
    Time,
    /// Plain tap-8 regeneration.
    Regen,
    /// Octave-up feedback (Faust shim channel).
    Shim,
    /// Reverb-washed feedback (Faust wash channel).
    Wash,
    /// Instantaneous input level in the mix.
    Dry,
    /// Active tap count 1–8 — the expandable-channels knob.
    Taps,
}
const GLOBAL_ROWS: [GlobalRow; 7] = [
    GlobalRow::Input,
    GlobalRow::Time,
    GlobalRow::Regen,
    GlobalRow::Shim,
    GlobalRow::Wash,
    GlobalRow::Dry,
    GlobalRow::Taps,
];

/// Phase states in `m` / h-l cycling order.
const PHASES: [&str; 3] = ["+", "·", "−"]; // normal, off, inverted
fn phase_sign(p: usize) -> f32 {
    match p {
        0 => 1.0,
        1 => 0.0,
        _ => -1.0,
    }
}

// ── state ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct TapState {
    level: f32,
    pan: f32,
    /// Index into PHASES.
    phase: usize,
    /// Bindings: [pan, level].
    srcs: [Option<SourceAddr>; 2],
    resolved: [Option<usize>; 2],
    /// Live effective [pan, level] (ghost display).
    eff: [f32; 2],
}

impl TapState {
    fn new() -> Self {
        Self {
            level: 0.6,
            pan: 0.0,
            phase: 0,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.0, 0.6],
        }
    }
}

/// Global bindable srcs order: time, regen, shim, wash, dry.
const GSRC: usize = 5;
fn gsrc_index(r: GlobalRow) -> Option<usize> {
    match r {
        GlobalRow::Time => Some(0),
        GlobalRow::Regen => Some(1),
        GlobalRow::Shim => Some(2),
        GlobalRow::Wash => Some(3),
        GlobalRow::Dry => Some(4),
        GlobalRow::Input | GlobalRow::Taps => None,
    }
}

struct DelayState {
    /// Per-stage time in seconds (knob position; the audio thread
    /// smooths it).
    time: f32,
    regen: f32,
    shim: f32,
    wash: f32,
    dry: f32,
    taps: usize,
    /// The consumed source as "module/instance", e.g. "voice/0".
    /// Resolved to a live SHM name through the manifest, so it survives
    /// the source restarting.
    input: Option<String>,
    /// True when `input` is unset or currently resolves to a live ring.
    input_live: bool,
    tap: [TapState; MAX_TAPS],
    gsrcs: [Option<SourceAddr>; GSRC],
    gresolved: [Option<usize>; GSRC],
    /// Live effective globals [time, regen, shim, wash, dry].
    geff: [f32; GSRC],
    /// Live follower values [in, t1…t8] for the meters.
    followers: [f32; MAX_TAPS + 1],
    selected: usize,
    sel_row: usize,
}

impl DelayState {
    fn new() -> Self {
        Self {
            time: 0.120,
            regen: 0.0,
            shim: 0.0,
            wash: 0.0,
            dry: 0.8,
            taps: MAX_TAPS,
            input: None,
            input_live: true,
            tap: std::array::from_fn(|_| TapState::new()),
            gsrcs: Default::default(),
            gresolved: Default::default(),
            geff: [0.120, 0.0, 0.0, 0.0, 0.8],
            followers: [0.0; MAX_TAPS + 1],
            selected: GLOBAL_STRIP, // start on the global strip: patch input first
            sel_row: 0,
        }
    }

    fn rows_in(&self, strip: usize) -> usize {
        if strip == GLOBAL_STRIP {
            GLOBAL_ROWS.len()
        } else {
            TAP_ROWS.len()
        }
    }

    /// Map a modbus value onto a global row's range.
    fn map_gmod(r: GlobalRow, v: f32) -> f32 {
        match r {
            GlobalRow::Time => dsp::time_from_norm(v),
            GlobalRow::Regen | GlobalRow::Shim | GlobalRow::Wash | GlobalRow::Dry => {
                v.clamp(0.0, 1.0)
            }
            GlobalRow::Input | GlobalRow::Taps => v,
        }
    }

    /// Effective global value (bound source replaces the knob).
    fn geffective(&self, r: GlobalRow, bus: Option<&ModulationBus>) -> f32 {
        let manual = match r {
            GlobalRow::Time => self.time,
            GlobalRow::Regen => self.regen,
            GlobalRow::Shim => self.shim,
            GlobalRow::Wash => self.wash,
            GlobalRow::Dry => self.dry,
            GlobalRow::Input | GlobalRow::Taps => 0.0,
        };
        match (gsrc_index(r).and_then(|i| self.gresolved[i]), bus) {
            (Some(ch), Some(bus)) => Self::map_gmod(r, bus.get(ch)),
            _ => manual,
        }
    }
}

// ── undo ───────────────────────────────────────────────────────────────────
//
// Slots: tap t × 16 + {0 pan, 1 phase, 2 level, 3 pan-src, 4 level-src};
// globals at 1000 + {0 time, 1 regen, 2 shim, 3 wash, 4 dry, 5 taps,
// 6 input}; global bindings at 1010 + gsrc index.

const GLOBAL_SLOT: usize = 1000;
const GSRC_SLOT: usize = 1010;
const TAP_STRIDE: usize = 16;

impl crate::undo::ParamUndo for DelayState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(GSRC_SLOT) {
            let s = self.gsrcs.get(i)?;
            return Some(V::Src(s.as_ref().map(|a| a.to_string())));
        }
        if let Some(k) = slot.checked_sub(GLOBAL_SLOT) {
            return match k {
                0 => Some(V::F32(self.time)),
                1 => Some(V::F32(self.regen)),
                2 => Some(V::F32(self.shim)),
                3 => Some(V::F32(self.wash)),
                4 => Some(V::F32(self.dry)),
                5 => Some(V::Usize(self.taps)),
                6 => Some(V::Src(self.input.clone())),
                _ => None,
            };
        }
        let (t, k) = (slot / TAP_STRIDE, slot % TAP_STRIDE);
        let tap = self.tap.get(t)?;
        match k {
            0 => Some(V::F32(tap.pan)),
            1 => Some(V::Usize(tap.phase)),
            2 => Some(V::F32(tap.level)),
            3 => Some(V::Src(tap.srcs[0].as_ref().map(|a| a.to_string()))),
            4 => Some(V::Src(tap.srcs[1].as_ref().map(|a| a.to_string()))),
            _ => None,
        }
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(GSRC_SLOT) {
            if let (Some(s), V::Src(v)) = (self.gsrcs.get_mut(i), value) {
                *s = v.as_deref().and_then(SourceAddr::parse);
                self.gresolved[i] = None;
            }
            return;
        }
        if let Some(k) = slot.checked_sub(GLOBAL_SLOT) {
            match (k, value) {
                (0, V::F32(v)) => self.time = v.clamp(dsp::TIME_MIN, dsp::TIME_MAX),
                (1, V::F32(v)) => self.regen = v.clamp(0.0, 1.0),
                (2, V::F32(v)) => self.shim = v.clamp(0.0, 1.0),
                (3, V::F32(v)) => self.wash = v.clamp(0.0, 1.0),
                (4, V::F32(v)) => self.dry = v.clamp(0.0, 1.0),
                (5, V::Usize(v)) => self.taps = v.clamp(1, MAX_TAPS),
                (6, V::Src(v)) => self.input = v,
                _ => {}
            }
            return;
        }
        let (t, k) = (slot / TAP_STRIDE, slot % TAP_STRIDE);
        let Some(tap) = self.tap.get_mut(t) else {
            return;
        };
        match (k, value) {
            (0, V::F32(v)) => tap.pan = v.clamp(-1.0, 1.0),
            (1, V::Usize(v)) => tap.phase = v.min(PHASES.len() - 1),
            (2, V::F32(v)) => tap.level = v.clamp(0.0, 1.0),
            (3, V::Src(v)) => {
                tap.srcs[0] = v.as_deref().and_then(SourceAddr::parse);
                tap.resolved[0] = None;
            }
            (4, V::Src(v)) => {
                tap.srcs[1] = v.as_deref().and_then(SourceAddr::parse);
                tap.resolved[1] = None;
            }
            _ => {}
        }
    }
}

/// Undo slot for the cursor position.
fn slot_at(s: &DelayState) -> usize {
    if s.selected == GLOBAL_STRIP {
        GLOBAL_SLOT + s.sel_row.min(GLOBAL_ROWS.len() - 1)
    } else {
        s.selected * TAP_STRIDE + s.sel_row.min(TAP_ROWS.len() - 1)
    }
}

/// Undo slot for the binding under the cursor (None on unbindable rows).
fn src_slot_at(s: &DelayState) -> Option<usize> {
    if s.selected == GLOBAL_STRIP {
        match GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)] {
            GlobalRow::Input => Some(GLOBAL_SLOT + 6),
            r => gsrc_index(r).map(|i| GSRC_SLOT + i),
        }
    } else {
        match TAP_ROWS[s.sel_row.min(TAP_ROWS.len() - 1)] {
            TapRow::Pan => Some(s.selected * TAP_STRIDE + 3),
            TapRow::Level => Some(s.selected * TAP_STRIDE + 4),
            TapRow::Phase => None,
        }
    }
}

// ── editing ────────────────────────────────────────────────────────────────

fn adjust(s: &mut DelayState, steps: i32, coarse: bool) -> Option<String> {
    use crate::keys::step_f32;
    // fine = 1% of the range, coarse = 5%
    let u = |fine: f32, coarse_u: f32| if coarse { coarse_u } else { fine };
    if s.selected == GLOBAL_STRIP {
        match GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)] {
            GlobalRow::Input => {
                return Some(String::from("@ picks the input source"));
            }
            GlobalRow::Time => {
                // adjust in the exponential norm domain so steps feel
                // even from 1 ms to 250 ms
                let n =
                    step_f32(dsp::norm_from_time(s.time), steps, u(0.01, 0.05), false, 0.0, 1.0);
                s.time = dsp::time_from_norm(n);
            }
            GlobalRow::Regen => s.regen = step_f32(s.regen, steps, u(0.01, 0.05), false, 0.0, 1.0),
            GlobalRow::Shim => s.shim = step_f32(s.shim, steps, u(0.01, 0.05), false, 0.0, 1.0),
            GlobalRow::Wash => s.wash = step_f32(s.wash, steps, u(0.01, 0.05), false, 0.0, 1.0),
            GlobalRow::Dry => s.dry = step_f32(s.dry, steps, u(0.01, 0.05), false, 0.0, 1.0),
            GlobalRow::Taps => {
                s.taps = (s.taps as i64 + steps as i64).clamp(1, MAX_TAPS as i64) as usize;
            }
        }
    } else {
        let t = &mut s.tap[s.selected];
        match TAP_ROWS[s.sel_row.min(TAP_ROWS.len() - 1)] {
            TapRow::Pan => t.pan = step_f32(t.pan, steps, u(0.02, 0.10), false, -1.0, 1.0),
            TapRow::Phase => t.phase = crate::keys::cycle(t.phase, steps, PHASES.len()),
            TapRow::Level => t.level = step_f32(t.level, steps, u(0.01, 0.05), false, 0.0, 1.0),
        }
    }
    None
}

fn reset_current(s: &mut DelayState) {
    if s.selected == GLOBAL_STRIP {
        match GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)] {
            GlobalRow::Input => s.input = None,
            GlobalRow::Time => s.time = 0.120,
            GlobalRow::Regen => s.regen = 0.0,
            GlobalRow::Shim => s.shim = 0.0,
            GlobalRow::Wash => s.wash = 0.0,
            GlobalRow::Dry => s.dry = 0.8,
            GlobalRow::Taps => s.taps = MAX_TAPS,
        }
    } else {
        let t = &mut s.tap[s.selected];
        match TAP_ROWS[s.sel_row.min(TAP_ROWS.len() - 1)] {
            TapRow::Pan => t.pan = 0.0,
            TapRow::Phase => t.phase = 0,
            TapRow::Level => t.level = 0.6,
        }
    }
}

// ── persistence ────────────────────────────────────────────────────────────

fn snapshot_params(s: &DelayState) -> state::DelayParams {
    let src = |o: &Option<SourceAddr>| o.as_ref().map(|a| a.to_string());
    state::DelayParams {
        format: state::STATE_FORMAT,
        time: Some(s.time),
        regen: Some(s.regen),
        shim: Some(s.shim),
        wash: Some(s.wash),
        dry: Some(s.dry),
        taps: Some(s.taps),
        input: s.input.clone(),
        time_src: src(&s.gsrcs[0]),
        regen_src: src(&s.gsrcs[1]),
        shim_src: src(&s.gsrcs[2]),
        wash_src: src(&s.gsrcs[3]),
        dry_src: src(&s.gsrcs[4]),
        tap: s
            .tap
            .iter()
            .map(|t| state::DelayTapParam {
                level: t.level,
                pan: t.pan,
                phase: PHASES[t.phase.min(PHASES.len() - 1)].to_string(),
                pan_src: src(&t.srcs[0]),
                level_src: src(&t.srcs[1]),
            })
            .collect(),
    }
}

fn apply_params(s: &mut DelayState, p: &state::DelayParams) {
    if let Some(v) = p.time {
        s.time = v.clamp(dsp::TIME_MIN, dsp::TIME_MAX);
    }
    if let Some(v) = p.regen {
        s.regen = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.shim {
        s.shim = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.wash {
        s.wash = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.dry {
        s.dry = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.taps {
        s.taps = v.clamp(1, MAX_TAPS);
    }
    s.input = p.input.clone();
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.gsrcs = [
        parse(&p.time_src),
        parse(&p.regen_src),
        parse(&p.shim_src),
        parse(&p.wash_src),
        parse(&p.dry_src),
    ];
    s.gresolved = Default::default();
    for (i, tp) in p.tap.iter().enumerate().take(MAX_TAPS) {
        let t = &mut s.tap[i];
        t.level = tp.level.clamp(0.0, 1.0);
        t.pan = tp.pan.clamp(-1.0, 1.0);
        t.phase = PHASES.iter().position(|n| *n == tp.phase).unwrap_or(0);
        t.srcs = [parse(&tp.pan_src), parse(&tp.level_src)];
        t.resolved = Default::default();
    }
}

// ── the audio thread ───────────────────────────────────────────────────────
//
// Input-clocked when patched: producers write continuously while alive,
// so we block (briefly) on the input ring and emit one block per input
// block. On timeout — nothing patched, or the source died — we process
// silence at our own pace so the tails ring out.

fn audio_thread(shared: Arc<Mutex<DelayState>>, instance: usize) -> Result<()> {
    let out_name = format!("/los_audio_delay_{}", instance);
    let mut out_rb = AudioRingbuf::create(&out_name).context("creating output ringbuffer")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    // 9 modbus channels: the input follower + one per tap.
    manifest.register("delay", instance, Some(&out_name), (MAX_TAPS + 1) as u32)?;
    let mod_base = manifest.claimed_base();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();

    let slot_frames = out_rb.slot_frames() as usize;
    let slot_len = out_rb.slot_len();

    // The mixer owns the device and publishes its real rate on the
    // transport; if it changes (in practice: once, when the mixer comes
    // up on a non-48k device), rebuild the cores — their coefficients
    // and line lengths bake the rate in.
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

    let mut core = dsp::DelayCore::new(sample_rate, slot_frames);
    let mut fx = tap8fx::Tap8Fx::new();
    fx.init(sample_rate as i32);

    let mut input: Option<AudioRingbuf> = None;
    let mut input_shm: Option<String> = None;
    let mut block = vec![0.0_f32; slot_len];
    let mut scratch = vec![0.0_f32; slot_len];
    let mut blocks: u64 = 0;

    loop {
        // Slow path every ~85 ms: re-resolve bindings and the input claim.
        if blocks.is_multiple_of(64) {
            if transport.is_none() {
                transport = ShmTransport::open().ok();
            }
            let now_rate = rate_of(&transport);
            if (now_rate - sample_rate).abs() > 0.5 {
                sample_rate = now_rate;
                slot_duration =
                    Duration::from_nanos((slot_frames as f64 / sample_rate as f64 * 1e9) as u64);
                core = dsp::DelayCore::new(sample_rate, slot_frames);
                fx = tap8fx::Tap8Fx::new();
                fx.init(sample_rate as i32);
                let t = { shared.lock().unwrap().time };
                core.snap_time(t);
            }
            let entries = manifest.entries();
            let desired: Option<String> = {
                let mut s = shared.lock().unwrap();
                for i in 0..GSRC {
                    s.gresolved[i] = s.gsrcs[i]
                        .as_ref()
                        .and_then(|a| routing::resolve(&entries, a));
                }
                for t in s.tap.iter_mut() {
                    for k in 0..2 {
                        t.resolved[k] = t.srcs[k]
                            .as_ref()
                            .and_then(|a| routing::resolve(&entries, a));
                    }
                }
                let desired = s.input.as_deref().and_then(|sel| {
                    let (m, i) = sel.split_once('/')?;
                    let i: usize = i.parse().ok()?;
                    entries
                        .iter()
                        .find(|e| e.module_name == m && e.instance == i)
                        .and_then(|e| e.audio_shm.clone())
                });
                s.input_live = s.input.is_none() || desired.is_some();
                desired
            };
            if desired != input_shm {
                input = desired.as_deref().and_then(|n| AudioRingbuf::open(n).ok());
                // Fast-forward the backlog the mixer left behind so we
                // don't burst 20 ms of stale audio through the line.
                if let Some(rb) = input.as_mut() {
                    while rb.available() > 1 {
                        let _ = rb.read(&mut scratch);
                    }
                }
                manifest.publish_input(desired.as_deref());
                input_shm = desired;
            }
        }

        // Acquire one input block (or time out to silence).
        let tick = Instant::now();
        let mut got = false;
        if let Some(rb) = input.as_mut() {
            loop {
                if rb.read(&mut block).unwrap_or(false) {
                    got = true;
                    break;
                }
                if tick.elapsed() > Duration::from_millis(4) {
                    break;
                }
                thread::sleep(Duration::from_micros(200));
            }
        }
        if !got {
            block.iter_mut().for_each(|v| *v = 0.0);
        }

        // Snapshot effective params under one short lock.
        let p = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            let time = s.geffective(GlobalRow::Time, bus);
            let regen = s.geffective(GlobalRow::Regen, bus);
            let shim = s.geffective(GlobalRow::Shim, bus);
            let wash = s.geffective(GlobalRow::Wash, bus);
            let dry = s.geffective(GlobalRow::Dry, bus);
            s.geff = [time, regen, shim, wash, dry];
            let mut level = [0.0; MAX_TAPS];
            let mut pan = [0.0; MAX_TAPS];
            let mut phase = [0.0; MAX_TAPS];
            for (i, t) in s.tap.iter_mut().enumerate() {
                pan[i] = match (t.resolved[0], bus) {
                    (Some(ch), Some(bus)) => bus.get(ch).clamp(-1.0, 1.0),
                    _ => t.pan,
                };
                level[i] = match (t.resolved[1], bus) {
                    (Some(ch), Some(bus)) => bus.get(ch).clamp(0.0, 1.0),
                    _ => t.level,
                };
                phase[i] = phase_sign(t.phase);
                t.eff = [pan[i], level[i]];
            }
            dsp::BlockParams {
                time,
                regen,
                shim,
                wash,
                dry,
                taps: s.taps,
                level,
                pan,
                phase,
            }
        };

        core.process_block(&mut block, &p, &mut fx);

        // Publish the followers — the MDP's rhythm-section-of-CVs trick.
        let f = core.followers();
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            for (i, v) in f.iter().enumerate() {
                bus.set(base + i, *v);
            }
        }
        if let Ok(mut s) = shared.lock() {
            s.followers = f;
        }

        while out_rb.write(&block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }

        blocks += 1;
        // Self-pace only when the input didn't (silence path).
        if !got {
            let elapsed = tick.elapsed();
            if elapsed < slot_duration {
                thread::sleep(slot_duration - elapsed);
            }
        }
    }
}

// ── rendering ──────────────────────────────────────────────────────────────

const TAP_W: usize = 6;
const PANEL_W: usize = 26;
const CONSOLE_MIN_H: usize = 12;
/// First fader row in console mode: header + names + pan + phase.
const FADER_TOP: usize = 4;

/// Fader geometry from the pane height — one function for the renderer
/// and the mouse hit-test.
fn fader_rows_for(h: usize) -> usize {
    h.saturating_sub(9).clamp(3, 8)
}

fn time_text(t: f32) -> String {
    let ms = t * 1000.0;
    if ms >= 100.0 {
        format!("{:.0}ms", ms)
    } else if ms >= 10.0 {
        format!("{:.1}ms", ms)
    } else {
        format!("{:.2}ms", ms)
    }
}

fn pan_text(v: f32) -> String {
    if v.abs() < 0.05 {
        String::from("·")
    } else if v < 0.0 {
        format!("‹{:.0}", v.abs() * 100.0)
    } else {
        format!("{:.0}›", v * 100.0)
    }
}

fn global_text(s: &DelayState, r: GlobalRow) -> String {
    let bound = gsrc_index(r).is_some_and(|i| s.gsrcs[i].is_some());
    let shown = |i: usize, manual: f32| if bound { s.geff[i] } else { manual };
    match r {
        GlobalRow::Input => match (&s.input, s.input_live) {
            (None, _) => String::from("— none —"),
            (Some(sel), true) => sel.replace('/', " "),
            (Some(sel), false) => format!("{} ✗", sel.replace('/', " ")),
        },
        GlobalRow::Time => time_text(shown(0, s.time)),
        GlobalRow::Regen => format!("{:.0}%", shown(1, s.regen) * 100.0),
        GlobalRow::Shim => format!("{:.0}%", shown(2, s.shim) * 100.0),
        GlobalRow::Wash => format!("{:.0}%", shown(3, s.wash) * 100.0),
        GlobalRow::Dry => format!("{:.0}%", shown(4, s.dry) * 100.0),
        GlobalRow::Taps => format!("{}", s.taps),
    }
}

fn global_label(r: GlobalRow) -> &'static str {
    match r {
        GlobalRow::Input => "input",
        GlobalRow::Time => "time",
        GlobalRow::Regen => "fdbk",
        GlobalRow::Shim => "shim",
        GlobalRow::Wash => "wash",
        GlobalRow::Dry => "dry",
        GlobalRow::Taps => "taps",
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &DelayState,
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

        let ctx = format!(
            "{} · {} taps · {}",
            instance,
            s.taps,
            global_text(s, GlobalRow::Input)
        );
        lines.push(theme::header("DELAY", &ctx, "", w));

        // One global-panel line, reused by both layouts.
        let panel_line = |row: usize| -> Vec<Span<'static>> {
            let Some(r) = GLOBAL_ROWS.get(row).copied() else {
                return vec![Span::raw(" ".repeat(PANEL_W))];
            };
            let cursor = s.selected == GLOBAL_STRIP && s.sel_row.min(GLOBAL_ROWS.len() - 1) == row;
            let bound = gsrc_index(r).is_some_and(|i| s.gsrcs[i].is_some());
            let mark = if bound { theme::BIND } else { ' ' };
            let mut txt = format!(" {:<5}{}{}", global_label(r), mark, global_text(s, r));
            txt.truncate(PANEL_W);
            while txt.chars().count() < PANEL_W {
                txt.push(' ');
            }
            let style = if cursor {
                theme::selected()
            } else if bound {
                let cable = gsrc_index(r)
                    .and_then(|i| s.gsrcs[i].as_ref())
                    .map(|a| routing::cable_color(entries, a))
                    .unwrap_or_else(theme::clock);
                theme::signal(cable)
            } else if r == GlobalRow::Input && s.input.is_some() && !s.input_live {
                theme::signal(theme::alert())
            } else {
                theme::value()
            };
            vec![Span::styled(txt, style)]
        };

        let console = h >= CONSOLE_MIN_H && w >= MAX_TAPS * TAP_W + 3 + PANEL_W;
        if console {
            let fader_rows = fader_rows_for(h);
            let sep = || Span::styled(" │", theme::chrome());
            // names
            let mut spans: Vec<Span> = Vec::new();
            for i in 0..MAX_TAPS {
                let mut nm = format!("  T{}", i + 1);
                while nm.chars().count() < TAP_W {
                    nm.push(' ');
                }
                let style = if i == s.selected {
                    theme::selected()
                } else if i < s.taps {
                    // the CV teal ramp doubles as echo depth: T1 bright,
                    // T8 deep in the tail
                    theme::signal(theme::cv_ramp(1.0 - i as f32 / (MAX_TAPS - 1) as f32))
                } else {
                    theme::dim()
                };
                spans.push(Span::styled(nm, style));
            }
            spans.push(sep());
            spans.push(Span::styled(
                if s.selected == GLOBAL_STRIP {
                    " GLOBAL"
                } else {
                    " global"
                }
                .to_string(),
                if s.selected == GLOBAL_STRIP {
                    theme::selected()
                } else {
                    theme::chrome_hi()
                },
            ));
            lines.push(Line::from(spans));

            let mut panel_row = 0usize;
            // pan + phase rows
            for (row, tr) in [TapRow::Pan, TapRow::Phase].iter().enumerate() {
                let mut spans: Vec<Span> = Vec::new();
                for (i, t) in s.tap.iter().enumerate() {
                    let cursor = i == s.selected && s.sel_row.min(TAP_ROWS.len() - 1) == row;
                    let (txt, bound) = match tr {
                        TapRow::Pan => {
                            let bound = t.srcs[0].is_some();
                            (pan_text(if bound { t.eff[0] } else { t.pan }), bound)
                        }
                        TapRow::Phase => (PHASES[t.phase].to_string(), false),
                        TapRow::Level => unreachable!(),
                    };
                    let mut cell = format!(" {:>4}", txt);
                    while cell.chars().count() < TAP_W {
                        cell.push(' ');
                    }
                    let style = if cursor {
                        theme::selected()
                    } else if bound {
                        let cable = t.srcs[0]
                            .as_ref()
                            .map(|a| routing::cable_color(entries, a))
                            .unwrap_or_else(theme::clock);
                        theme::signal(cable)
                    } else if i >= s.taps || (*tr == TapRow::Phase && t.phase == 1) {
                        theme::dim()
                    } else {
                        theme::value()
                    };
                    spans.push(Span::styled(cell, style));
                }
                spans.push(sep());
                spans.extend(panel_line(panel_row));
                panel_row += 1;
                lines.push(Line::from(spans));
            }

            // the tap faders: a knob on a rail beside that tap's
            // envelope-follower ladder — the echo pattern as a row of
            // breathing LED meters
            let row_of =
                |v: f32| ((1.0 - v.clamp(0.0, 1.0)) * (fader_rows - 1) as f32).round() as usize;
            for fr in 0..fader_rows {
                let mut spans: Vec<Span> = Vec::new();
                for (i, t) in s.tap.iter().enumerate() {
                    let bound = t.srcs[1].is_some();
                    let live = t.eff[1];
                    let meter =
                        if i < s.taps { theme::meter_frac(s.followers[i + 1]) } else { 0.0 };
                    spans.push(Span::raw("  "));
                    if bound && fr == row_of(live) {
                        let cable = t.srcs[1]
                            .as_ref()
                            .map(|a| routing::cable_color(entries, a))
                            .unwrap_or_else(theme::clock);
                        spans.push(Span::styled(theme::GHOST.to_string(), theme::signal(cable)));
                    } else if let Some(knob) = theme::knob_cell(t.level, fr, fader_rows) {
                        let style = if i == s.selected {
                            theme::selected()
                        } else if i < s.taps {
                            theme::value()
                        } else {
                            theme::dim()
                        };
                        spans.push(Span::styled(knob.to_string(), style));
                    } else {
                        spans.push(Span::styled(theme::RAIL.to_string(), theme::chrome()));
                    }
                    spans.push(Span::raw(" "));
                    let (mc, mstyle) = theme::meter_cell(meter, fr, fader_rows);
                    spans.push(Span::styled(mc.to_string(), mstyle));
                    spans.push(Span::raw(" "));
                }
                spans.push(sep());
                spans.extend(panel_line(panel_row));
                panel_row += 1;
                lines.push(Line::from(spans));
            }

            // level % row
            let mut spans: Vec<Span> = Vec::new();
            for (i, t) in s.tap.iter().enumerate() {
                let bound = t.srcs[1].is_some();
                let shown = if bound { t.eff[1] } else { t.level };
                let cursor =
                    i == s.selected && TAP_ROWS[s.sel_row.min(TAP_ROWS.len() - 1)] == TapRow::Level;
                let mut cell = format!(" {:>3.0}%", shown * 100.0);
                while cell.chars().count() < TAP_W {
                    cell.push(' ');
                }
                let style = if cursor {
                    theme::selected()
                } else if bound {
                    let cable = t.srcs[1]
                        .as_ref()
                        .map(|a| routing::cable_color(entries, a))
                        .unwrap_or_else(theme::clock);
                    theme::signal(cable)
                } else if i >= s.taps {
                    theme::dim()
                } else {
                    theme::value()
                };
                spans.push(Span::styled(cell, style));
            }
            spans.push(sep());
            spans.extend(panel_line(panel_row));
            lines.push(Line::from(spans));
        } else {
            // compact: a globals line, a meters line, a selected detail
            let mut g: Vec<Span> = vec![Span::raw(" ")];
            for (row, r) in GLOBAL_ROWS.iter().enumerate() {
                let cursor =
                    s.selected == GLOBAL_STRIP && s.sel_row.min(GLOBAL_ROWS.len() - 1) == row;
                let style = if cursor {
                    theme::selected()
                } else {
                    theme::value()
                };
                g.push(Span::styled(
                    format!("{} {}", global_label(*r), global_text(s, *r)),
                    style,
                ));
                g.push(Span::styled(theme::SEP.to_string(), theme::chrome()));
            }
            lines.push(Line::from(g));
            let mut m: Vec<Span> = vec![Span::raw(" ")];
            for (i, t) in s.tap.iter().enumerate() {
                let style = if i == s.selected {
                    theme::selected()
                } else if i < s.taps {
                    theme::signal(theme::audio())
                } else {
                    theme::dim()
                };
                m.push(Span::styled(
                    format!(
                        "t{} {} ",
                        i + 1,
                        theme::meter_char(theme::meter_frac(s.followers[i + 1]))
                    ),
                    style,
                ));
                let _ = t;
            }
            lines.push(Line::from(m));
            if s.selected < MAX_TAPS {
                let t = &s.tap[s.selected];
                let row = s.sel_row.min(TAP_ROWS.len() - 1);
                let cell = |r: usize, txt: String| {
                    Span::styled(
                        txt,
                        if r == row {
                            theme::selected()
                        } else {
                            theme::value()
                        },
                    )
                };
                lines.push(Line::from(vec![
                    Span::styled(format!(" › t{}: ", s.selected + 1), theme::chrome_hi()),
                    cell(0, format!("pan {}  ", pan_text(t.pan))),
                    cell(1, format!("phs {}  ", PHASES[t.phase])),
                    cell(2, format!("lvl {:.0}%", t.level * 100.0)),
                ]));
            }
        }

        theme::anchor_bottom(&mut lines, h, 2);
        lines.push(theme::rule(w));
        lines.push(theme::status("NORMAL", overlay.unwrap_or(""), "", w));
        f.render_widget(Paragraph::new(lines), area);

        if show_help {
            let help = Paragraph::new(vec![
                Line::from("━━━ DELAY · time domain processor (288) ━━━"),
                Line::from(""),
                Line::from("  h/l        Select strip (taps T1–T8, then GLOBAL)"),
                Line::from("  j/k        Select row (taps: pan·phs·lvl)"),
                Line::from("  K/J or =/- Adjust 1% (up/down; _/+ = 5%, counts)"),
                Line::from("  0          Reset row · m cycle tap phase (+ · −)"),
                Line::from("  @          Bind a mod source (input row: pick the"),
                Line::from("             audio source to consume) · x unbinds"),
                Line::from("  gg / G     First tap / GLOBAL"),
                Line::from("  u/^r  :    Undo / redo · patches/:set · Space"),
                Line::from(""),
                Line::from("8 series taps at 1×–8× the stage time; sweeping"),
                Line::from("time repitches the line. fdbk/shim/wash are the"),
                Line::from("feedback characters (plain · +1 oct · reverb)."),
                Line::from("Followers publish as delay/N/in + t1…t8."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" DELAY ", theme::chrome_hi())),
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
                    .title(Span::styled(" patch ", theme::chrome_hi())),
            );
            f.render_widget(list, r);
        }
    })?;
    Ok(())
}

// ── entry point ────────────────────────────────────────────────────────────

/// What the `@` picker is currently picking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Picking {
    /// A mod source for the row under the cursor.
    ModSource,
    /// The audio input (specials list of live audio producers).
    Input,
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("delay", instance);

    let shared = Arc::new(Mutex::new(DelayState::new()));
    if let Ok(p) = state::load_module_state::<state::DelayParams>("delay", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    // The audio thread owns every RT resource. Its setup races the
    // session's boot storm (a dozen modules registering into the CAS
    // manifest at once), so a failure retries instead of dying silent —
    // and persistent errors land in a tmp file the user can find.
    let audio_state = Arc::clone(&shared);
    // A named builder with a roomy stack: generated Faust cores hold
    // their delay lines as big inline arrays (tap8fx ≈ 800 KB), and a
    // debug build materializes extra copies constructing them — the
    // default 2 MB thread stack overflowed and took the whole process
    // (and its pane) down.
    let audio_builder = thread::Builder::new()
        .name(String::from("delay-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = audio_builder.spawn(move || {
        let err_path = state::tmp_dir().join(format!("delay_{}.err", instance));
        let _ = std::fs::remove_file(&err_path);
        loop {
            match audio_thread(Arc::clone(&audio_state), instance) {
                Ok(()) => break,
                Err(e) => {
                    let _ = std::fs::write(&err_path, format!("{}", e));
                    eprintln!("[delay {}] audio thread error (retrying): {}", instance, e);
                    thread::sleep(Duration::from_millis(500));
                }
            }
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
    let mut picking = Picking::ModSource;
    // the input picker's option list, parallel to its special rows
    let mut input_options: Vec<String> = Vec::new();
    let mut ex_msg: Option<String> = None;
    let mut patch_name: Option<String> = None;
    let mut should_quit = false;
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();
    let manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let mut ui_entries: Vec<crate::shm::ManifestEntry> = Vec::new();
    let mut ui_entries_at: Option<Instant> = None;
    // the tap fader currently held by the mouse
    let mut grabbed: Option<usize> = None;
    let mut baseline =
        state::to_toml_string(&snapshot_params(&shared.lock().unwrap())).unwrap_or_default();

    loop {
        if state::check_save_signal() {
            let params = snapshot_params(&shared.lock().unwrap());
            let _ = state::save_module_state("delay", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::DelayParams>("delay", instance) {
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
                    let slot = slot_at(&s);
                    let old = s.get_param(slot);
                    let msg = adjust(&mut s, steps, false);
                    let new = s.get_param(slot);
                    if let (Some(old), Some(new)) = (old, new) {
                        history.record(slot, "Adjust", old, new);
                    }
                    if msg.is_some() {
                        ex_msg = msg;
                    }
                }
                MouseEventKind::Down(_) => {
                    // click selects; clicking a tap's fader grabs it
                    let h = terminal.size().map(|r| r.height as usize).unwrap_or(0);
                    let mut s = shared.lock().unwrap();
                    let col = m.column as usize;
                    let strip = if col < MAX_TAPS * TAP_W { col / TAP_W } else { GLOBAL_STRIP };
                    s.selected = strip;
                    s.sel_row = s.sel_row.min(s.rows_in(s.selected) - 1);
                    let rows = fader_rows_for(h);
                    if strip < MAX_TAPS
                        && h >= CONSOLE_MIN_H
                        && (FADER_TOP..FADER_TOP + rows).contains(&(m.row as usize))
                    {
                        s.sel_row = TAP_ROWS.len() - 1; // the fader row
                        grabbed = Some(strip);
                    }
                }
                MouseEventKind::Drag(_) => {
                    // only the grabbed fader follows, only vertically —
                    // crossing other taps must never throw THEIR faders
                    let Some(strip) = grabbed else { continue };
                    use crate::undo::{ParamUndo, ParamValue};
                    let h = terminal.size().map(|r| r.height as usize).unwrap_or(0);
                    let rows = fader_rows_for(h);
                    if h < CONSOLE_MIN_H || rows < 2 {
                        continue;
                    }
                    let row = (m.row as usize).clamp(FADER_TOP, FADER_TOP + rows - 1);
                    let value = 1.0 - (row - FADER_TOP) as f32 / (rows - 1) as f32;
                    let mut s = shared.lock().unwrap();
                    let slot = strip * TAP_STRIDE + 2;
                    let old = s.get_param(slot);
                    s.tap[strip].level = value.clamp(0.0, 1.0);
                    if let Some(old) = old {
                        history.record(slot, "Fader", old, ParamValue::F32(value));
                    }
                }
                MouseEventKind::Up(_) => {
                    grabbed = None;
                }
                _ => {}
            }
            continue;
        }
        let Event::Key(key) = ev else { continue };
        ex_msg = None;

        if picker.is_active() {
            match picker.handle_key(key.code) {
                crate::picker::PickerEvent::Chosen(addr) => match picking {
                    Picking::ModSource => {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = shared.lock().unwrap();
                        if let Some(slot) = src_slot_at(&s) {
                            let old = s.get_param(slot);
                            s.set_param(
                                slot,
                                ParamValue::Src(addr.as_ref().map(|a| a.to_string())),
                            );
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
                    // Chosen(None) on the input picker = the "— none —"
                    // row: unpatch.
                    Picking::Input => {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = shared.lock().unwrap();
                        let slot = GLOBAL_SLOT + 6;
                        let old = s.get_param(slot);
                        s.input = None;
                        if let Some(old) = old {
                            history.record(slot, "Unpatch", old, ParamValue::Src(None));
                        }
                    }
                },
                crate::picker::PickerEvent::ChosenSpecial(i) if picking == Picking::Input => {
                    use crate::undo::{ParamUndo, ParamValue};
                    if let Some(sel) = input_options.get(i.saturating_sub(1)).cloned() {
                        let mut s = shared.lock().unwrap();
                        let slot = GLOBAL_SLOT + 6;
                        let old = s.get_param(slot);
                        s.input = Some(sel.clone());
                        s.input_live = true; // optimistic until the next resolve
                        if let Some(old) = old {
                            history.record(slot, "Patch", old, ParamValue::Src(Some(sel)));
                        }
                    }
                }
                _ => {}
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
                    ExCommand::Edit(name) => match state::load_patch::<state::DelayParams>(&name) {
                        Ok(p) => {
                            apply_params(&mut shared.lock().unwrap(), &p);
                            baseline =
                                state::to_toml_string(&snapshot_params(&shared.lock().unwrap()))
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
            let _ = state::save_module_state("delay", instance, &params);
            ex_msg = Some(String::from("Saved"));
            continue;
        }

        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
            KeyCode::Char('h') | KeyCode::Left => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, -n, MAX_TAPS + 1);
                s.sel_row = s.sel_row.min(s.rows_in(s.selected) - 1);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, n, MAX_TAPS + 1);
                s.sel_row = s.sel_row.min(s.rows_in(s.selected) - 1);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                let rows = s.rows_in(s.selected);
                s.sel_row = crate::keys::cycle(s.sel_row.min(rows - 1), n, rows);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                let rows = s.rows_in(s.selected);
                s.sel_row = crate::keys::cycle(s.sel_row.min(rows - 1), -n, rows);
            }
            KeyCode::Char(c @ ('-' | '=' | '_' | '+' | 'J' | 'K')) => {
                let n = count.take() as i32;
                let (steps, coarse) = match c {
                    '-' | 'J' => (-n, false),
                    '=' | 'K' => (n, false),
                    '_' => (-n, true),
                    _ => (n, true),
                };
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                let slot = slot_at(&s);
                let old = s.get_param(slot);
                let msg = adjust(&mut s, steps, coarse);
                let new = s.get_param(slot);
                if let (Some(old), Some(new)) = (old, new) {
                    history.record(slot, "Adjust", old, new);
                }
                if msg.is_some() {
                    ex_msg = msg;
                }
            }
            KeyCode::Char('0') => {
                count.clear();
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                let slot = slot_at(&s);
                let old = s.get_param(slot);
                reset_current(&mut s);
                let new = s.get_param(slot);
                if let (Some(old), Some(new)) = (old, new) {
                    history.record(slot, "Reset", old, new);
                }
            }
            // m cycles the tap's phase: + → · → − (off is the mute).
            KeyCode::Char('m') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                if s.selected < MAX_TAPS {
                    let slot = s.selected * TAP_STRIDE + 1;
                    let old = s.get_param(slot);
                    let sel = s.selected;
                    s.tap[sel].phase = crate::keys::cycle(s.tap[sel].phase, 1, PHASES.len());
                    if let Some(old) = old {
                        history.record(slot, "Phase", old, ParamValue::Usize(s.tap[sel].phase));
                    }
                }
            }
            KeyCode::Char('@') | KeyCode::Enter => {
                count.clear();
                let s = shared.lock().unwrap();
                let on_input = s.selected == GLOBAL_STRIP
                    && GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)] == GlobalRow::Input;
                if on_input {
                    // pick an audio source: every live entry with an
                    // audio ring except our own output
                    let current = s.input.clone();
                    drop(s);
                    let entries = Manifest::open().map(|m| m.entries()).unwrap_or_default();
                    input_options = entries
                        .iter()
                        .filter(|e| e.audio_shm.is_some())
                        .filter(|e| !(e.module_name == "delay" && e.instance == instance))
                        .map(|e| format!("{}/{}", e.module_name, e.instance))
                        .collect();
                    input_options.sort();
                    let mut specials = vec![String::from("— none —")];
                    specials.extend(input_options.iter().map(|o| o.replace('/', " ")));
                    let cur_special = current
                        .as_ref()
                        .and_then(|c| input_options.iter().position(|o| o == c))
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    picking = Picking::Input;
                    picker.open_with(specials, Vec::new(), None, cur_special);
                } else {
                    let bindable = src_slot_at(&s).is_some()
                        && !(s.selected == GLOBAL_STRIP
                            && GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)]
                                == GlobalRow::Taps);
                    if bindable {
                        let current: Option<SourceAddr> = if s.selected == GLOBAL_STRIP {
                            gsrc_index(GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)])
                                .and_then(|i| s.gsrcs[i].clone())
                        } else {
                            match TAP_ROWS[s.sel_row.min(TAP_ROWS.len() - 1)] {
                                TapRow::Pan => s.tap[s.selected].srcs[0].clone(),
                                TapRow::Level => s.tap[s.selected].srcs[1].clone(),
                                TapRow::Phase => None,
                            }
                        };
                        drop(s);
                        let sources = Manifest::open()
                            .map(|m| routing::live_sources(&m.entries()))
                            .unwrap_or_default();
                        picking = Picking::ModSource;
                        picker.open(sources, current.as_ref());
                    } else {
                        ex_msg = Some(String::from("not bindable"));
                    }
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let on_input = s.selected == GLOBAL_STRIP
                    && GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)] == GlobalRow::Input;
                let slot = if on_input {
                    Some(GLOBAL_SLOT + 6)
                } else {
                    src_slot_at(&s)
                };
                if let Some(slot) = slot {
                    let old = s.get_param(slot);
                    if !matches!(old, Some(ParamValue::Src(None))) {
                        s.set_param(slot, ParamValue::Src(None));
                        if let Some(old) = old {
                            let desc = if on_input { "Unpatch" } else { "Unbind" };
                            history.record(slot, desc, old, ParamValue::Src(None));
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
                    let mut s = shared.lock().unwrap();
                    s.selected = 0;
                    s.sel_row = s.sel_row.min(TAP_ROWS.len() - 1);
                } else {
                    pending_g = true;
                }
            }
            KeyCode::Char('G') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                s.selected = GLOBAL_STRIP;
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

/// `:set time 120ms · :set regen 35 · :set taps 4 · :set input voice/0`.
fn ex_set(
    s: &mut DelayState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    let record = |s: &mut DelayState, h: &mut crate::undo::ParamHistory, slot: usize, old| {
        if let (Some(old), Some(new)) = (old, s.get_param(slot)) {
            h.record(slot, "Set", old, new);
        }
    };
    match key {
        "time" => {
            // accept "120ms", "0.12s", or a bare number in ms
            let v = value.trim();
            let secs = if let Some(ms) = v.strip_suffix("ms") {
                ms.trim().parse::<f32>().ok().map(|m| m / 1000.0)
            } else if let Some(sec) = v.strip_suffix('s') {
                sec.trim().parse::<f32>().ok()
            } else {
                v.parse::<f32>().ok().map(|m| m / 1000.0)
            };
            let Some(secs) = secs else {
                return format!("time: not a duration: {}", value);
            };
            let slot = GLOBAL_SLOT;
            let old = s.get_param(slot);
            s.time = secs.clamp(dsp::TIME_MIN, dsp::TIME_MAX);
            record(s, history, slot, old);
            format!("time = {}", time_text(s.time))
        }
        "regen" | "shim" | "wash" | "dry" => {
            // bare number reads as a percentage
            let Ok(pct) = value.trim_end_matches('%').parse::<f32>() else {
                return format!("{}: not a number: {}", key, value);
            };
            let v = (pct / 100.0).clamp(0.0, 1.0);
            let slot = match key {
                "regen" => GLOBAL_SLOT + 1,
                "shim" => GLOBAL_SLOT + 2,
                "wash" => GLOBAL_SLOT + 3,
                _ => GLOBAL_SLOT + 4,
            };
            let old = s.get_param(slot);
            match key {
                "regen" => s.regen = v,
                "shim" => s.shim = v,
                "wash" => s.wash = v,
                _ => s.dry = v,
            }
            record(s, history, slot, old);
            format!("{} = {:.0}%", key, v * 100.0)
        }
        "taps" => match value.parse::<usize>() {
            Ok(n) if (1..=MAX_TAPS).contains(&n) => {
                let slot = GLOBAL_SLOT + 5;
                let old = s.get_param(slot);
                s.taps = n;
                record(s, history, slot, old);
                format!("taps = {}", n)
            }
            _ => format!("taps: 1–{}", MAX_TAPS),
        },
        "input" => {
            let slot = GLOBAL_SLOT + 6;
            let old = s.get_param(slot);
            if value == "none" || value.is_empty() {
                s.input = None;
            } else if value.contains('/') {
                s.input = Some(value.to_string());
            } else {
                return String::from("input: module/instance (e.g. voice/0) or none");
            }
            let new = s.get_param(slot);
            if let (Some(old), Some(new)) = (old, new) {
                history.record(slot, "Patch", old, new);
            }
            format!("input = {}", s.input.as_deref().unwrap_or("none"))
        }
        _ => format!(
            "Unknown setting: {} (time regen shim wash dry taps input)",
            key
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap8fx_core_shape_and_tail() {
        // the generated Faust core: 1 in (tap 8) → 2 out (shim, wash)
        assert_eq!(tap8fx::FAUST_INPUTS, 1);
        assert_eq!(tap8fx::FAUST_OUTPUTS, 2);
        let mut fx = tap8fx::Tap8Fx::new();
        fx.init(48_000);
        // zero params on purpose (design doc §5): the host owns amounts
        let mut map = crate::faust::ParamMap::default();
        tap8fx::Tap8Fx::build_user_interface_static(&mut map);
        assert!(
            map.params.is_empty(),
            "tap8fx declares no widgets: {:?}",
            map.params
        );
        // an impulse must come out somewhere: run 100 blocks and look
        // for energy on either output (transposer grains + reverb tail)
        let mut impulse = vec![0.0_f32; 64];
        impulse[0] = 1.0;
        let mut energy = 0.0_f32;
        let mut shim = vec![0.0_f32; 64];
        let mut wash = vec![0.0_f32; 64];
        for i in 0..100 {
            let input = if i == 0 {
                impulse.clone()
            } else {
                vec![0.0; 64]
            };
            let ins = [&input[..]];
            let mut outs = [&mut shim[..], &mut wash[..]];
            fx.compute(64, &ins, &mut outs);
            energy += shim.iter().chain(wash.iter()).map(|s| s * s).sum::<f32>();
        }
        assert!(energy > 1e-4, "impulse response has energy, got {}", energy);
        assert!(energy.is_finite());
    }

    #[test]
    fn adjust_covers_every_row() {
        let mut s = DelayState::new();
        // global rows
        s.selected = GLOBAL_STRIP;
        s.sel_row = 1; // time
        let t0 = s.time;
        adjust(&mut s, 5, false);
        assert!(s.time > t0, "time steps up in the norm domain");
        adjust(&mut s, -1000, true);
        assert!((s.time - dsp::TIME_MIN).abs() < 1e-6, "clamps at 1 ms");
        s.sel_row = 6; // taps
        adjust(&mut s, -3, false);
        assert_eq!(s.taps, 5);
        adjust(&mut s, -100, false);
        assert_eq!(s.taps, 1, "taps clamp, not wrap");
        s.sel_row = 0; // input row: -/= is a no-op with a hint
        assert!(adjust(&mut s, 1, false).is_some());
        // tap rows
        s.selected = 2;
        s.sel_row = 0;
        adjust(&mut s, -4, false);
        assert!((s.tap[2].pan + 0.08).abs() < 1e-6, "pan fine = 0.02");
        adjust(&mut s, -2, true);
        assert!((s.tap[2].pan + 0.28).abs() < 1e-6, "pan coarse = 0.10");
        s.sel_row = 1;
        adjust(&mut s, 1, false);
        assert_eq!(s.tap[2].phase, 1, "phase cycles to off");
        s.sel_row = 2;
        adjust(&mut s, 100, true);
        assert_eq!(s.tap[2].level, 1.0);
    }

    #[test]
    fn undo_slots_round_trip() {
        use crate::undo::{ParamUndo, ParamValue as V};
        let mut s = DelayState::new();
        s.set_param(GLOBAL_SLOT, V::F32(0.2));
        assert_eq!(s.get_param(GLOBAL_SLOT), Some(V::F32(0.2)), "time");
        s.set_param(GLOBAL_SLOT + 5, V::Usize(3));
        assert_eq!(s.taps, 3);
        s.set_param(GLOBAL_SLOT + 6, V::Src(Some("voice/0".into())));
        assert_eq!(s.input.as_deref(), Some("voice/0"));
        s.set_param(2 * TAP_STRIDE + 1, V::Usize(2));
        assert_eq!(s.tap[2].phase, 2, "tap 3 inverted");
        s.set_param(7 * TAP_STRIDE + 4, V::Src(Some("envelope/0/ch1".into())));
        assert_eq!(
            s.tap[7].srcs[1].as_ref().map(|a| a.to_string()),
            Some("envelope/0/ch1".into())
        );
        s.set_param(GSRC_SLOT + 1, V::Src(Some("sequencer/0/t2".into())));
        assert_eq!(
            s.gsrcs[1].as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t2".into())
        );
        // cursor slot mapping
        s.selected = GLOBAL_STRIP;
        s.sel_row = 4; // wash (GLOBAL_ROWS: input time regen shim wash …)
        assert_eq!(slot_at(&s), GLOBAL_SLOT + 4);
        assert_eq!(src_slot_at(&s), Some(GSRC_SLOT + 3), "wash is gsrc 3");
        s.selected = 4;
        s.sel_row = 1; // phase: not bindable
        assert_eq!(src_slot_at(&s), None);
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = DelayState::new();
        s.time = 0.033;
        s.taps = 5;
        s.input = Some("voice/1".into());
        s.tap[0].phase = 2;
        s.tap[0].pan = -0.5;
        s.gsrcs[0] = SourceAddr::parse("envelope/0/sum");
        s.tap[3].srcs[1] = SourceAddr::parse("sequencer/0/t1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::DelayParams = toml::from_str(&toml).expect("parses");
        let mut s2 = DelayState::new();
        apply_params(&mut s2, &back);
        assert!((s2.time - 0.033).abs() < 1e-6);
        assert_eq!(s2.taps, 5);
        assert_eq!(s2.input.as_deref(), Some("voice/1"));
        assert_eq!(s2.tap[0].phase, 2);
        assert!((s2.tap[0].pan + 0.5).abs() < 1e-6);
        assert_eq!(
            s2.gsrcs[0].as_ref().map(|a| a.to_string()),
            Some("envelope/0/sum".into())
        );
        assert_eq!(
            s2.tap[3].srcs[1].as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t1".into())
        );
        // an empty save leaves defaults standing
        let empty: state::DelayParams = toml::from_str("").expect("parses");
        let mut s3 = DelayState::new();
        apply_params(&mut s3, &empty);
        assert_eq!(s3.taps, MAX_TAPS);
        assert!(s3.input.is_none());
    }

    #[test]
    fn ex_set_parses_durations_and_rejects() {
        let mut s = DelayState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert_eq!(ex_set(&mut s, &mut h, "time", "33ms"), "time = 33.0ms");
        assert_eq!(ex_set(&mut s, &mut h, "time", "0.2s"), "time = 200ms");
        assert_eq!(ex_set(&mut s, &mut h, "time", "50"), "time = 50.0ms");
        assert!(ex_set(&mut s, &mut h, "time", "fast").contains("not a duration"));
        assert_eq!(ex_set(&mut s, &mut h, "regen", "35"), "regen = 35%");
        assert_eq!(ex_set(&mut s, &mut h, "taps", "4"), "taps = 4");
        assert!(ex_set(&mut s, &mut h, "taps", "12").contains("1–8"));
        assert_eq!(
            ex_set(&mut s, &mut h, "input", "voice/0"),
            "input = voice/0"
        );
        assert!(ex_set(&mut s, &mut h, "input", "voice").contains("module/instance"));
        assert_eq!(ex_set(&mut s, &mut h, "input", "none"), "input = none");
        assert!(ex_set(&mut s, &mut h, "zoom", "1").contains("Unknown setting"));
    }

    #[test]
    fn mod_mapping_and_phase_signs() {
        assert!((DelayState::map_gmod(GlobalRow::Time, 0.0) - dsp::TIME_MIN).abs() < 1e-7);
        assert!((DelayState::map_gmod(GlobalRow::Time, 1.0) - dsp::TIME_MAX).abs() < 1e-6);
        assert_eq!(DelayState::map_gmod(GlobalRow::Regen, 7.0), 1.0, "clamped");
        assert_eq!(phase_sign(0), 1.0);
        assert_eq!(phase_sign(1), 0.0);
        assert_eq!(phase_sign(2), -1.0);
    }
}
