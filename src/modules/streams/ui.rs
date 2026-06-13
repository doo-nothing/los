//! # Streams — the two-channel module shell
//!
//! Claims one audio input (ch1 = left, ch2 = right) and runs each
//! side through one of the six dynamics processors at the firmware's
//! 31.25 kHz control rate, applying the resulting VCA gain to the
//! audio in-line. Excitation per channel comes from a bound modbus
//! cable, a note-source gate, or the manual excite knob — in that
//! priority. Publishes streams/N/{g1,f1,g2,f2}: the gain and the
//! filter-frequency CV of each side (g at 0.5 = unity; patch f into
//! a wasp or filterbank to close the vactrol loop).

// max/min, not clamp, throughout: clamp(NaN) is NaN; a stale modbus
// or TOML value must die at the boundary (the swarm lesson).
#![allow(clippy::manual_clamp)]

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

use super::dsp::{
    Compressor, Envelope, FilterController, Follower, LorenzGenerator, Processor, Vactrol,
    NATIVE_SR,
};
use crate::ipc::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

const FALLBACK_RATE: f32 = 48_000.0;

pub const FUNCTION_NAMES: [&str; 6] = [
    "envelope",
    "vactrol",
    "follower",
    "compressor",
    "filter",
    "lorenz",
];

/// Knob labels, per function (the panel relabels like peaks).
fn param_labels(function: usize) -> [&'static str; 2] {
    match function {
        0 => ["shape", "→freq"],
        1 => ["shape", "→freq"],
        2 => ["resp", "→freq"],
        3 => ["thresh", "ratio"],
        4 => ["offset", "amount"],
        _ => ["rate", "balance"],
    }
}

/// What the alternate toggle means, per function.
fn alt_label(function: usize) -> &'static str {
    match function {
        0 => "AR (gated)",
        1 => "plucked",
        2 => "filter only",
        3 => "soft knee",
        4 => "—",
        _ => "swap x/z",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Input,
    Fn(usize),
    Alt(usize),
    P(usize, usize),
    Excite(usize),
    Notes(usize),
}

const ROWS: [Row; 13] = [
    Row::Input,
    Row::Fn(0),
    Row::Alt(0),
    Row::P(0, 0),
    Row::P(0, 1),
    Row::Excite(0),
    Row::Notes(0),
    Row::Fn(1),
    Row::Alt(1),
    Row::P(1, 0),
    Row::P(1, 1),
    Row::Excite(1),
    Row::Notes(1),
];

/// Bindable value rows, srcs[] order — every knob takes a cable.
const BINDABLE: [Row; 6] = [
    Row::P(0, 0),
    Row::P(0, 1),
    Row::Excite(0),
    Row::P(1, 0),
    Row::P(1, 1),
    Row::Excite(1),
];
const N_SRC: usize = BINDABLE.len();
const INPUT_SLOT: usize = 0;
const SRC_SLOT_BASE: usize = 20;

struct StreamsState {
    function: [usize; 2],
    alt: [bool; 2],
    params: [[f32; 2]; 2],
    excite: [f32; 2],
    input: Option<String>,
    input_live: bool,
    notes_src: [Option<SourceAddr>; 2],
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    gate: [f32; 2],
    out_g: [f32; 2],
    out_f: [f32; 2],
    selected: usize,
}

impl StreamsState {
    fn new() -> Self {
        Self {
            function: [1, 1], // the vactrol is streams' face
            alt: [false; 2],
            params: [[0.5, 0.5]; 2],
            excite: [0.0; 2],
            input: None,
            input_live: true,
            notes_src: [None, None],
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.5, 0.5, 0.0, 0.5, 0.5, 0.0],
            gate: [0.0; 2],
            out_g: [0.0; 2],
            out_f: [0.0; 2],
            selected: 0,
        }
    }

    fn get(&self, r: Row) -> f32 {
        match r {
            Row::P(c, k) => self.params[c][k],
            Row::Excite(c) => self.excite[c],
            Row::Input | Row::Fn(_) | Row::Alt(_) | Row::Notes(_) => 0.0,
        }
    }

    fn set(&mut self, r: Row, v: f32) {
        let v = v.max(0.0).min(1.0);
        match r {
            Row::P(c, k) => self.params[c][k] = v,
            Row::Excite(c) => self.excite[c] = v,
            Row::Input | Row::Fn(_) | Row::Alt(_) | Row::Notes(_) => {}
        }
    }
}

impl crate::undo::ParamUndo for StreamsState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            let s = self.srcs.get(i)?;
            return Some(V::Src(s.as_ref().map(|a| a.to_string())));
        }
        let r = *ROWS.get(slot)?;
        Some(match r {
            Row::Input => V::Src(self.input.clone()),
            Row::Fn(c) => V::Usize(self.function[c]),
            Row::Alt(c) => V::Bool(self.alt[c]),
            Row::Notes(c) => V::Src(self.notes_src[c].as_ref().map(|a| a.to_string())),
            Row::P(..) | Row::Excite(_) => V::F32(self.get(r)),
        })
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            if let (Some(s), V::Src(v)) = (self.srcs.get_mut(i), value) {
                *s = v.as_deref().and_then(SourceAddr::parse);
                self.resolved[i] = None;
            }
            return;
        }
        let Some(r) = ROWS.get(slot).copied() else {
            return;
        };
        match (r, value) {
            (Row::Input, V::Src(v)) => self.input = v,
            (Row::Fn(c), V::Usize(v)) => self.function[c] = v.min(FUNCTION_NAMES.len() - 1),
            (Row::Alt(c), V::Bool(v)) => self.alt[c] = v,
            (Row::Notes(c), V::Src(v)) => {
                self.notes_src[c] = v.as_deref().and_then(SourceAddr::parse)
            }
            (Row::P(..), V::F32(v)) | (Row::Excite(_), V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &StreamsState) -> state::StreamsParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::StreamsParams {
        format: state::STATE_FORMAT,
        fn1: Some(FUNCTION_NAMES[s.function[0]].to_string()),
        fn2: Some(FUNCTION_NAMES[s.function[1]].to_string()),
        alt1: Some(s.alt[0]),
        alt2: Some(s.alt[1]),
        p1: s.params[0].to_vec(),
        p2: s.params[1].to_vec(),
        excite1: Some(s.excite[0]),
        excite2: Some(s.excite[1]),
        input: s.input.clone(),
        p1a_src: src(0),
        p1b_src: src(1),
        excite1_src: src(2),
        p2a_src: src(3),
        p2b_src: src(4),
        excite2_src: src(5),
        notes1_src: s.notes_src[0].as_ref().map(|a| a.to_string()),
        notes2_src: s.notes_src[1].as_ref().map(|a| a.to_string()),
    }
}

fn apply_params(s: &mut StreamsState, p: &state::StreamsParams) {
    for (c, f) in [(0, &p.fn1), (1, &p.fn2)] {
        if let Some(name) = f.as_deref() {
            if let Some(i) = FUNCTION_NAMES.iter().position(|n| *n == name) {
                s.function[c] = i;
            }
        }
    }
    if let Some(v) = p.alt1 {
        s.alt[0] = v;
    }
    if let Some(v) = p.alt2 {
        s.alt[1] = v;
    }
    for (c, knobs) in [(0, &p.p1), (1, &p.p2)] {
        for (k, v) in knobs.iter().take(2).enumerate() {
            s.params[c][k] = v.max(0.0).min(1.0);
        }
    }
    if let Some(v) = p.excite1 {
        s.excite[0] = v.max(0.0).min(1.0);
    }
    if let Some(v) = p.excite2 {
        s.excite[1] = v.max(0.0).min(1.0);
    }
    if p.input.is_some() {
        s.input = p.input.clone();
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.p1a_src),
        parse(&p.p1b_src),
        parse(&p.excite1_src),
        parse(&p.p2a_src),
        parse(&p.p2b_src),
        parse(&p.excite2_src),
    ];
    s.notes_src = [parse(&p.notes1_src), parse(&p.notes2_src)];
    s.resolved = Default::default();
}

// ── engine ─────────────────────────────────────────────────────────────────

/// One side's processor bank; only the selected function runs.
struct ChannelEngine {
    envelope: Envelope,
    vactrol: Vactrol,
    follower: Follower,
    compressor: Compressor,
    filter: FilterController,
    lorenz: LorenzGenerator,
    last_cfg: Option<(usize, bool, i32, i32)>,
}

impl ChannelEngine {
    fn new() -> Self {
        Self {
            envelope: Envelope::new(),
            vactrol: Vactrol::new(),
            follower: Follower::new(),
            compressor: Compressor::new(),
            filter: FilterController::default(),
            lorenz: LorenzGenerator::new(),
            last_cfg: None,
        }
    }

    /// Reconfigure only when the panel moved — the firmware calls
    /// Configure on knob change, and the envelope's hard_reset flag
    /// must not be re-armed every block.
    fn configure(&mut self, function: usize, alt: bool, p1: f32, p2: f32) {
        let p1u = (p1.max(0.0).min(1.0) * 65535.0) as i32;
        let p2u = (p2.max(0.0).min(1.0) * 65535.0) as i32;
        let cfg = (function, alt, p1u, p2u);
        if self.last_cfg == Some(cfg) {
            return;
        }
        self.last_cfg = Some(cfg);
        match function {
            0 => self.envelope.configure(alt, p1u, p2u),
            1 => self.vactrol.configure(alt, p1u, p2u),
            2 => self.follower.configure(alt, p1u, p2u),
            3 => self.compressor.configure(alt, p1u, p2u),
            4 => self.filter.configure(p1u, p2u),
            _ => {
                self.lorenz.index = alt;
                self.lorenz.configure(p1u, p2u);
            }
        }
    }

    fn process(&mut self, function: usize, audio: i16, excite: i16) -> (u16, u16) {
        match function {
            0 => self.envelope.process(audio, excite),
            1 => self.vactrol.process(audio, excite),
            2 => self.follower.process(audio, excite),
            3 => self.compressor.process(audio, excite),
            4 => self.filter.process(audio, excite),
            _ => self.lorenz.process(audio, excite),
        }
    }

    /// The filter controller emits gain 0 (it is a CV processor, not
    /// a VCA) — audio passes at unity for that function.
    fn passes_audio(function: usize) -> bool {
        function != 4
    }
}

// ── audio thread ───────────────────────────────────────────────────────────

fn audio_thread(shared: Arc<Mutex<StreamsState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_streams_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating streams ring")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    // four claims: each side's gain and frequency CV go on the bus
    manifest.register("streams", instance, Some(&shm_name), 4)?;
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
        .unwrap_or(FALLBACK_RATE);

    let channels = ringbuf.channels() as usize;
    let slot_frames = ringbuf.slot_frames() as usize;
    let mut block = vec![0.0_f32; ringbuf.slot_len()];
    let mut scratch = vec![0.0_f32; ringbuf.slot_len()];

    let mut engine = [ChannelEngine::new(), ChannelEngine::new()];
    let mut note_filter: [Option<u8>; 2] = [None, None];
    // the engines step at the firmware's 31.25 kHz regardless of the
    // session rate; gain/frequency hold between steps (the hardware
    // DAC updates at the same rate)
    let step_per_frame = NATIVE_SR / sample_rate as f64;
    let mut acc = [0.0_f64; 2];
    let mut held: [(u16, u16); 2] = [(0, 65535); 2];
    let mut gain_smooth = [0.0_f32; 2];
    let g_alpha = 1.0 - (-1.0 / (0.0007 * sample_rate)).exp();

    let mut input: Option<AudioRingbuf> = None;
    let mut input_shm: Option<String> = None;
    let mut blocks: u64 = 0;

    loop {
        if blocks.is_multiple_of(64) {
            if events.is_none() {
                events = EventRingbuf::open_dynamic().ok();
            }
            let entries = manifest.entries();
            let desired: Option<String> = {
                let mut s = shared.lock().unwrap();
                for k in 0..N_SRC {
                    s.resolved[k] = s.srcs[k]
                        .as_ref()
                        .and_then(|a| routing::resolve(&entries, a));
                }
                #[allow(clippy::needless_range_loop)] // c strides two arrays
                for c in 0..2 {
                    note_filter[c] = s.notes_src[c].as_ref().and_then(routing::note_source_track);
                }
                let mask = s
                    .resolved
                    .iter()
                    .flatten()
                    .filter(|&&ch| ch < 64)
                    .fold(0u64, |m, &ch| m | (1 << ch));
                let notes = note_filter
                    .iter()
                    .flatten()
                    .filter(|&&t| t < 8)
                    .fold(0u8, |m, &t| m | (1 << t));
                manifest.publish_consumes(mask, notes);
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
                if let Some(rb) = input.as_mut() {
                    while rb.available() > 1 {
                        let _ = rb.read(&mut scratch);
                    }
                }
                manifest.publish_input(desired.as_deref());
                input_shm = desired;
            }
        }

        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                let mut s = shared.lock().unwrap();
                #[allow(clippy::needless_range_loop)] // c strides two arrays
                for c in 0..2 {
                    // unbound channels ignore the note bus: streams'
                    // excite defaults to its knob, not to every track
                    let Some(t) = note_filter[c] else { continue };
                    if event.source != t {
                        continue;
                    }
                    if event.is_note_on() {
                        s.gate[c] = (event.param as f32 / 127.0).max(0.0).min(1.0);
                    } else if event.is_note_off() {
                        s.gate[c] = 0.0;
                    }
                }
            }
        }

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

        let (functions, excite_eff) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            // max/min, not clamp: clamp(NaN) is NaN and a stale modbus
            // value must die here
            #[allow(clippy::manual_clamp)]
            let cv = |k: usize, manual: f32, s: &StreamsState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let mut p = [[0.0_f32; 2]; 2];
            let mut excite_eff = [0.0_f32; 2];
            for (k, r) in BINDABLE.iter().enumerate() {
                let v = cv(k, s.get(*r), &s);
                match *r {
                    Row::P(c, j) => p[c][j] = v,
                    Row::Excite(c) => {
                        // cable beats gate beats knob
                        excite_eff[c] = if s.srcs[k].is_some() {
                            v
                        } else if s.notes_src[c].is_some() {
                            s.gate[c]
                        } else {
                            s.excite[c]
                        };
                    }
                    Row::Input | Row::Fn(_) | Row::Alt(_) | Row::Notes(_) => {}
                }
                s.eff[k] = v;
            }
            for c in 0..2 {
                engine[c].configure(s.function[c], s.alt[c], p[c][0], p[c][1]);
            }
            (s.function, excite_eff)
        };

        for f in 0..slot_frames {
            #[allow(clippy::needless_range_loop)] // c strides engines + holds
            for c in 0..2 {
                let ch = if channels > 1 { c.min(channels - 1) } else { 0 };
                let audio = (block[f * channels + ch].max(-1.0).min(1.0) * 32767.0) as i16;
                let excite = (excite_eff[c] * 32767.0) as i16;
                acc[c] += step_per_frame;
                while acc[c] >= 1.0 {
                    acc[c] -= 1.0;
                    held[c] = engine[c].process(functions[c], audio, excite);
                }
                let target = if ChannelEngine::passes_audio(functions[c]) {
                    held[c].0 as f32 / 32767.0
                } else {
                    1.0
                };
                gain_smooth[c] += (target - gain_smooth[c]) * g_alpha;
                // on a mono ring both engines hear ch0 (their CVs
                // still publish) but only side 1 owns the sample
                if c < channels {
                    block[f * channels + ch] *= gain_smooth[c];
                }
            }
        }

        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            engine = [ChannelEngine::new(), ChannelEngine::new()];
            gain_smooth = [0.0; 2];
        }

        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            let mut s = shared.lock().unwrap();
            #[allow(clippy::needless_range_loop)] // c strides holds + meters
            for c in 0..2 {
                let g = held[c].0 as f32 / 65535.0;
                let fr = held[c].1 as f32 / 65535.0;
                bus.set(base + c * 2, g.max(0.0).min(1.0));
                bus.set(base + c * 2 + 1, fr.max(0.0).min(1.0));
                s.out_g[c] = g.max(0.0).min(1.0);
                s.out_f[c] = fr.max(0.0).min(1.0);
            }
        }

        while ringbuf.write(&block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }
        blocks += 1;
    }
}

// ── ui ─────────────────────────────────────────────────────────────────────

fn row_label(s: &StreamsState, r: Row) -> String {
    match r {
        Row::Input => "input".into(),
        Row::Fn(c) => format!("fn {}", c + 1),
        Row::Alt(c) => format!("· {}", alt_label(s.function[c])),
        Row::P(c, k) => format!("· {}", param_labels(s.function[c])[k]),
        Row::Excite(c) => format!("excite {}", c + 1),
        Row::Notes(c) => format!("notes {}", c + 1),
    }
}

fn src_index(r: Row) -> Option<usize> {
    BINDABLE.iter().position(|b| *b == r)
}

fn row_text(s: &StreamsState, r: Row) -> String {
    match r {
        Row::Input => s
            .input
            .clone()
            .map(|i| {
                if s.input_live {
                    i
                } else {
                    format!("{i} ✗ offline")
                }
            })
            .unwrap_or_else(|| "(unpatched · silent)".into()),
        Row::Fn(c) => FUNCTION_NAMES[s.function[c]].to_string(),
        Row::Alt(c) => if s.alt[c] { "on" } else { "off" }.to_string(),
        Row::P(c, k) => format!("{:.0}%", s.params[c][k] * 100.0),
        Row::Excite(c) => {
            if s.notes_src[c].is_some() && src_index(r).is_none_or(|k| s.srcs[k].is_none()) {
                format!("gate {:.0}%", s.gate[c] * 100.0)
            } else {
                format!("{:.0}%", s.excite[c] * 100.0)
            }
        }
        Row::Notes(c) => s.notes_src[c]
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(none)".into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &StreamsState,
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
            "STREAMS",
            &format!("dynamics {}", instance),
            "",
            w,
        ));
        let mut meter = vec![Span::styled("  g·f ".to_string(), theme::chrome())];
        for c in 0..2 {
            meter.push(Span::styled(
                theme::meter_char(s.out_g[c]).to_string(),
                theme::signal(theme::cv_ramp(s.out_g[c])),
            ));
            meter.push(Span::styled(
                theme::meter_char(s.out_f[c]).to_string(),
                theme::signal(theme::cv_ramp(s.out_f[c])),
            ));
            meter.push(Span::raw(" "));
        }
        lines.push(Line::from(meter));
        lines.push(Line::from(""));

        let bar_w = theme::bar_width(w, 30);
        for (i, r) in ROWS.iter().enumerate() {
            let selected = i == s.selected;
            let label_style = if selected {
                theme::selected()
            } else {
                theme::chrome()
            };
            let mut spans: Vec<Span> = vec![Span::styled(
                format!(" {:<13}", row_label(s, *r)),
                label_style,
            )];
            let bound = src_index(*r).is_some_and(|k| s.srcs[k].is_some())
                || matches!(*r, Row::Notes(c) if s.notes_src[c].is_some());
            let hue = src_index(*r)
                .and_then(|k| s.srcs[k].as_ref())
                .or(match r {
                    Row::Notes(c) => s.notes_src[*c].as_ref(),
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
            if let Some(addr) = src_index(*r).and_then(|k| s.srcs[k].as_ref()) {
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
                Line::from("━━━ STREAMS · dual dynamics gate (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  fn 1/2      envelope · vactrol · follower"),
                Line::from("              compressor · filter · lorenz"),
                Line::from("  ·alt        AR / plucked / filter-only / soft knee / swap"),
                Line::from("  knobs       relabel per function (shape · →freq · …)"),
                Line::from("  excite      cable beats note gate beats knob"),
                Line::from("  notes 1/2   a note track gates the channel"),
                Line::from(""),
                Line::from("Ch1 = left, ch2 = right of the claimed input; gain is"),
                Line::from("applied in-line. Publishes streams/N/g1·f1·g2·f2"),
                Line::from("(g 0.5 = unity; patch f# into a wasp's freq)."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" STREAMS ", theme::chrome_hi())),
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

#[derive(PartialEq)]
enum Picking {
    ModSource,
    Input,
    Notes(usize),
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("streams", instance);

    let shared = Arc::new(Mutex::new(StreamsState::new()));
    if let Ok(p) = state::load_module_state::<state::StreamsParams>("streams", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let audio_builder = thread::Builder::new()
        .name(String::from("streams-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = audio_builder.spawn(move || {
        if let Err(e) = audio_thread(audio_state, instance) {
            eprintln!("[streams {}] audio thread error: {}", instance, e);
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
    let mut input_options: Vec<String> = Vec::new();
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
            let _ = state::save_module_state("streams", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::StreamsParams>("streams", instance) {
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
                    let steps: i32 = if m.kind == MouseEventKind::ScrollUp { 1 } else { -1 };
                    use crate::undo::ParamUndo;
                    let mut s = shared.lock().unwrap();
                    let slot = s.selected;
                    let r = ROWS[slot.min(ROWS.len() - 1)];
                    if !matches!(r, Row::P(..) | Row::Excite(_)) {
                        continue;
                    }
                    let old = s.get_param(slot);
                    let v = s.get(r) + steps as f32 * 0.01;
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
            match picker.handle_key(key.code) {
                crate::picker::PickerEvent::Chosen(addr) => match picking {
                    Picking::ModSource => {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = shared.lock().unwrap();
                        if let Some(k) = src_index(ROWS[s.selected.min(ROWS.len() - 1)]) {
                            let slot = SRC_SLOT_BASE + k;
                            let old = s.get_param(slot);
                            s.srcs[k] = addr.clone();
                            s.resolved[k] = None;
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
                    Picking::Notes(c) => {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = shared.lock().unwrap();
                        let slot = ROWS
                            .iter()
                            .position(|r| *r == Row::Notes(c))
                            .unwrap_or(0);
                        let old = s.get_param(slot);
                        s.notes_src[c] = addr.clone();
                        if let Some(old) = old {
                            history.record(
                                slot,
                                "Bind",
                                old,
                                ParamValue::Src(addr.map(|a| a.to_string())),
                            );
                        }
                    }
                    Picking::Input => {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = shared.lock().unwrap();
                        let old = s.get_param(INPUT_SLOT);
                        s.input = None;
                        if let Some(old) = old {
                            history.record(INPUT_SLOT, "Unpatch", old, ParamValue::Src(None));
                        }
                    }
                },
                crate::picker::PickerEvent::ChosenSpecial(i) if picking == Picking::Input => {
                    use crate::undo::{ParamUndo, ParamValue};
                    if let Some(sel) = input_options.get(i.saturating_sub(1)).cloned() {
                        let mut s = shared.lock().unwrap();
                        let old = s.get_param(INPUT_SLOT);
                        s.input = Some(sel.clone());
                        s.input_live = true;
                        if let Some(old) = old {
                            history.record(INPUT_SLOT, "Patch", old, ParamValue::Src(Some(sel)));
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
                    ExCommand::Edit(name) => {
                        match state::load_patch::<state::StreamsParams>(&name) {
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
            let _ = state::save_module_state("streams", instance, &params);
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
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let slot = s.selected;
                let r = ROWS[slot.min(ROWS.len() - 1)];
                match r {
                    Row::Input => {
                        ex_msg = Some("input: @ patches, x unpatches".into());
                    }
                    Row::Notes(_) => {
                        ex_msg = Some("notes: @ binds a track, x unbinds".into());
                    }
                    Row::Fn(ch) => {
                        let old = s.get_param(slot);
                        let cur = s.function[ch] as i32;
                        let next = (cur + steps).rem_euclid(FUNCTION_NAMES.len() as i32) as usize;
                        s.function[ch] = next;
                        if let Some(old) = old {
                            history.record(slot, "Function", old, ParamValue::Usize(next));
                        }
                    }
                    Row::Alt(ch) => {
                        let old = s.get_param(slot);
                        s.alt[ch] = !s.alt[ch];
                        let new = ParamValue::Bool(s.alt[ch]);
                        if let Some(old) = old {
                            history.record(slot, "Toggle", old, new);
                        }
                    }
                    Row::P(..) | Row::Excite(_) => {
                        let old = s.get_param(slot);
                        let v = step_f32(s.get(r), steps, 0.01, coarse, 0.0, 1.0);
                        s.set(r, v);
                        let new = s.get_param(slot);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(slot, "Adjust", old, new);
                        }
                    }
                }
            }
            KeyCode::Char('0') => {
                count.clear();
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                let slot = s.selected;
                let old = s.get_param(slot);
                let def = StreamsState::new();
                if let Some(v) = def.get_param(slot) {
                    s.set_param(slot, v.clone());
                    if let Some(old) = old {
                        history.record(slot, "Reset", old, v);
                    }
                }
            }
            KeyCode::Char('@') | KeyCode::Enter => {
                count.clear();
                let s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                match r {
                    Row::Input => {
                        let current = s.input.clone();
                        drop(s);
                        let entries = Manifest::open().map(|m| m.entries()).unwrap_or_default();
                        input_options = entries
                            .iter()
                            .filter(|e| e.audio_shm.is_some())
                            .filter(|e| {
                                !(e.module_name == "streams" && e.instance == instance)
                            })
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
                    }
                    Row::Notes(c) => {
                        let current = s.notes_src[c].clone();
                        drop(s);
                        let sources = Manifest::open()
                            .map(|m| routing::live_sources(&m.entries()))
                            .unwrap_or_default();
                        picking = Picking::Notes(c);
                        picker.open(sources, current.as_ref());
                    }
                    _ => {
                        if let Some(k) = src_index(r) {
                            let current = s.srcs[k].clone();
                            drop(s);
                            let sources = Manifest::open()
                                .map(|m| routing::live_sources(&m.entries()))
                                .unwrap_or_default();
                            picking = Picking::ModSource;
                            picker.open(sources, current.as_ref());
                        } else {
                            ex_msg = Some("fn/alt rows are not bindable — h/l cycles".into());
                        }
                    }
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                match r {
                    Row::Input => {
                        let old = s.get_param(INPUT_SLOT);
                        s.input = None;
                        if let Some(old) = old {
                            history.record(INPUT_SLOT, "Unpatch", old, ParamValue::Src(None));
                        }
                    }
                    Row::Notes(c) => {
                        if s.notes_src[c].is_some() {
                            let slot = s.selected;
                            let old = s.get_param(slot);
                            s.notes_src[c] = None;
                            if let Some(old) = old {
                                history.record(slot, "Unbind", old, ParamValue::Src(None));
                            }
                        }
                    }
                    _ => {
                        if let Some(k) = src_index(r) {
                            if s.srcs[k].is_some() {
                                let slot = SRC_SLOT_BASE + k;
                                let old = s.get_param(slot);
                                s.srcs[k] = None;
                                s.resolved[k] = None;
                                if let Some(old) = old {
                                    history.record(slot, "Unbind", old, ParamValue::Src(None));
                                }
                            }
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
    s: &mut StreamsState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    let keys: [(&str, Row); 13] = [
        ("input", Row::Input),
        ("fn1", Row::Fn(0)),
        ("alt1", Row::Alt(0)),
        ("p1a", Row::P(0, 0)),
        ("p1b", Row::P(0, 1)),
        ("excite1", Row::Excite(0)),
        ("notes1", Row::Notes(0)),
        ("fn2", Row::Fn(1)),
        ("alt2", Row::Alt(1)),
        ("p2a", Row::P(1, 0)),
        ("p2b", Row::P(1, 1)),
        ("excite2", Row::Excite(1)),
        ("notes2", Row::Notes(1)),
    ];
    let Some((_, r)) = keys.iter().find(|(k, _)| *k == key) else {
        let names: Vec<&str> = keys.iter().map(|(k, _)| *k).collect();
        return format!("Unknown setting: {key} ({})", names.join(" "));
    };
    let r = *r;
    let slot = ROWS.iter().position(|row| *row == r).unwrap_or(0);
    let parsed: Result<V, String> = match r {
        Row::Input | Row::Notes(_) => {
            if value == "-" {
                Ok(V::Src(None))
            } else {
                Ok(V::Src(Some(value.to_string())))
            }
        }
        Row::Fn(_) => FUNCTION_NAMES
            .iter()
            .position(|n| *n == value)
            .map(V::Usize)
            .or_else(|| value.parse::<usize>().ok().map(V::Usize))
            .ok_or_else(|| format!("{key}: one of {}", FUNCTION_NAMES.join(" "))),
        Row::Alt(_) => match value {
            "on" | "true" | "1" => Ok(V::Bool(true)),
            "off" | "false" | "0" => Ok(V::Bool(false)),
            _ => Err(format!("{key}: on/off")),
        },
        Row::P(..) | Row::Excite(_) => {
            // numbers set the knob; anything else binds a source
            if let Ok(v) = value.parse::<f32>() {
                Ok(V::F32(v))
            } else if let Some(k) = src_index(r) {
                let slot = SRC_SLOT_BASE + k;
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
                return format!("{} ◂ {}", key, value);
            } else {
                Err(format!("{key}: not a number: {value}"))
            }
        }
    };
    match parsed {
        Ok(v) => {
            let old = s.get_param(slot);
            s.set_param(slot, v.clone());
            if let Some(old) = old {
                history.record(slot, "Set", old, v);
            }
            format!("{} = {}", key, row_text(s, r))
        }
        Err(m) => m,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_value_row_takes_a_cable() {
        // the all-params-modulatable rule: each knob row appears in
        // BINDABLE exactly once
        for r in ROWS {
            match r {
                Row::P(..) | Row::Excite(_) => {
                    assert!(src_index(r).is_some(), "{r:?} must be bindable");
                }
                Row::Input | Row::Fn(_) | Row::Alt(_) | Row::Notes(_) => {}
            }
        }
        assert_eq!(N_SRC, 6);
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = StreamsState::new();
        s.function = [3, 5];
        s.alt = [true, false];
        s.params = [[0.7, 0.2], [0.9, 0.4]];
        s.excite[1] = 0.3;
        s.input = Some("voice/0".into());
        s.srcs[2] = SourceAddr::parse("envelope/0/ch1");
        s.notes_src[0] = SourceAddr::parse("sequencer/0/t1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::StreamsParams = toml::from_str(&toml).expect("parses");
        let mut s2 = StreamsState::new();
        apply_params(&mut s2, &back);
        assert_eq!(s2.function, [3, 5]);
        assert_eq!(s2.alt, [true, false]);
        assert!((s2.params[0][0] - 0.7).abs() < 1e-6);
        assert!((s2.excite[1] - 0.3).abs() < 1e-6);
        assert_eq!(s2.input.as_deref(), Some("voice/0"));
        assert_eq!(
            s2.srcs[2].as_ref().map(|a| a.to_string()),
            Some("envelope/0/ch1".into())
        );
        assert_eq!(
            s2.notes_src[0].as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t1".into())
        );
    }

    #[test]
    fn ex_set_parses() {
        let mut s = StreamsState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "fn1", "compressor").contains("compressor"));
        assert!(ex_set(&mut s, &mut h, "alt2", "on").contains("on"));
        assert!(ex_set(&mut s, &mut h, "p1a", "0.8").contains('%'));
        assert!(ex_set(&mut s, &mut h, "excite1", "lfo/0/out").contains("lfo/0/out"));
        assert!(ex_set(&mut s, &mut h, "notes1", "sequencer/0/t2").contains("sequencer/0/t2"));
        assert!(ex_set(&mut s, &mut h, "input", "voice/0").contains("voice/0"));
        assert!(ex_set(&mut s, &mut h, "wow", "1").contains("Unknown"));
        assert_eq!(s.function[0], 3);
        assert!(s.alt[1]);
        assert!(s.srcs[2].is_some(), "excite1 took a cable");
    }

    #[test]
    fn engine_gates_audio_through_the_vactrol() {
        // a pluck on the vactrol must pass audio, then close
        let mut e = ChannelEngine::new();
        e.configure(1, true, 0.3, 0.6);
        let mut peak = 0u16;
        let mut last = 0u16;
        for i in 0..62_500 {
            let excite = if i < 300 { 28_000 } else { 0 };
            let (g, _) = e.process(1, 12_000, excite);
            peak = peak.max(g);
            last = g;
        }
        assert!(peak > 16_000, "vactrol opened: {peak}");
        assert!(last < peak / 2, "and closed again: {peak} -> {last}");
    }

    #[test]
    fn function_switch_reconfigures() {
        let mut e = ChannelEngine::new();
        e.configure(0, false, 0.5, 0.5);
        let first = e.last_cfg;
        e.configure(0, false, 0.5, 0.5);
        assert_eq!(e.last_cfg, first, "no-op reconfigure is skipped");
        e.configure(3, false, 0.5, 0.5);
        assert_ne!(e.last_cfg, first, "function change reconfigures");
    }

    #[test]
    fn native_rate_stepping_covers_every_frame() {
        // at 48 kHz the engine must step ~31250 times per second
        let step = NATIVE_SR / 48_000.0;
        let mut acc = 0.0_f64;
        let mut steps = 0u32;
        for _ in 0..48_000 {
            acc += step;
            while acc >= 1.0 {
                acc -= 1.0;
                steps += 1;
            }
        }
        assert!((steps as i64 - 31_250).abs() <= 1, "steps: {steps}");
    }
}
