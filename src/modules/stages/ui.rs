//! # Stages — the six-segment shell
//!
//! Six segments, each with a type (ramp/step/hold), a loop flag, the
//! two knobs, and an optional gate (a note track). Segments are wired
//! into groups exactly like the hardware: a segment with a gate
//! binding starts a new group, and the group's panel configuration
//! decides what it becomes (multi-stage envelope, sequencer, LFO,
//! S&H, pulse, delay …) via the firmware's Configure law. CV-only —
//! publishes stages/N/o1..o6 on the bus at the modbus rate while the
//! engine steps at the firmware's 31.25 kHz underneath.

// max/min, not clamp, where modbus values land: clamp(NaN) is NaN
// and a stale channel must die at the boundary.
#![allow(clippy::manual_clamp)]

use std::io;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
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
    extract_gate_flags, Configuration, Output, SegmentGenerator, SegmentType, GATE_LOW,
};
use crate::ipc::routing::{self, SourceAddr};
use crate::shm::{EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

pub const NUM_SEGMENTS: usize = 6;
pub const TYPE_NAMES: [&str; 4] = ["ramp", "step", "hold", "alt"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Type(usize),
    Loop(usize),
    P(usize),
    S(usize),
    Gate(usize),
}

const ROWS_PER_SEG: usize = 5;
const N_ROWS: usize = NUM_SEGMENTS * ROWS_PER_SEG;

fn row_at(i: usize) -> Row {
    let seg = i / ROWS_PER_SEG;
    match i % ROWS_PER_SEG {
        0 => Row::Type(seg),
        1 => Row::Loop(seg),
        2 => Row::P(seg),
        3 => Row::S(seg),
        _ => Row::Gate(seg),
    }
}

/// p1..p6 then s1..s6 — every knob takes a cable.
const N_SRC: usize = NUM_SEGMENTS * 2;
const SRC_SLOT_BASE: usize = 40;

fn src_index(r: Row) -> Option<usize> {
    match r {
        Row::P(i) => Some(i),
        Row::S(i) => Some(NUM_SEGMENTS + i),
        Row::Type(_) | Row::Loop(_) | Row::Gate(_) => None,
    }
}

struct StagesState {
    seg_type: [usize; NUM_SEGMENTS], // index into TYPE_NAMES
    loop_flag: [bool; NUM_SEGMENTS],
    p: [f32; NUM_SEGMENTS],
    s: [f32; NUM_SEGMENTS],
    gate_src: [Option<SourceAddr>; NUM_SEGMENTS],
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    gate: [bool; NUM_SEGMENTS],
    out_now: [f32; NUM_SEGMENTS],
    selected: usize,
}

impl StagesState {
    fn new() -> Self {
        Self {
            seg_type: [0; NUM_SEGMENTS],
            loop_flag: [false; NUM_SEGMENTS],
            p: [0.5; NUM_SEGMENTS],
            s: [0.5; NUM_SEGMENTS],
            gate_src: Default::default(),
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.5; N_SRC],
            gate: [false; NUM_SEGMENTS],
            out_now: [0.0; NUM_SEGMENTS],
            selected: 0,
        }
    }

    fn get(&self, r: Row) -> f32 {
        match r {
            Row::P(i) => self.p[i],
            Row::S(i) => self.s[i],
            Row::Type(_) | Row::Loop(_) | Row::Gate(_) => 0.0,
        }
    }

    fn set(&mut self, r: Row, v: f32) {
        let v = v.max(0.0).min(1.0);
        match r {
            Row::P(i) => self.p[i] = v,
            Row::S(i) => self.s[i] = v,
            Row::Type(_) | Row::Loop(_) | Row::Gate(_) => {}
        }
    }
}

impl crate::undo::ParamUndo for StagesState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            let s = self.srcs.get(i)?;
            return Some(V::Src(s.as_ref().map(|a| a.to_string())));
        }
        if slot >= N_ROWS {
            return None;
        }
        Some(match row_at(slot) {
            Row::Type(i) => V::Usize(self.seg_type[i]),
            Row::Loop(i) => V::Bool(self.loop_flag[i]),
            Row::Gate(i) => V::Src(self.gate_src[i].as_ref().map(|a| a.to_string())),
            r @ (Row::P(_) | Row::S(_)) => V::F32(self.get(r)),
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
        if slot >= N_ROWS {
            return;
        }
        match (row_at(slot), value) {
            (Row::Type(i), V::Usize(v)) => self.seg_type[i] = v.min(3),
            (Row::Loop(i), V::Bool(v)) => self.loop_flag[i] = v,
            (Row::Gate(i), V::Src(v)) => {
                self.gate_src[i] = v.as_deref().and_then(SourceAddr::parse)
            }
            (r @ (Row::P(_) | Row::S(_)), V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &StagesState) -> state::StagesParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    let gsrc = |i: usize| s.gate_src[i].as_ref().map(|a| a.to_string());
    state::StagesParams {
        format: state::STATE_FORMAT,
        types: s
            .seg_type
            .iter()
            .map(|&t| TYPE_NAMES[t].to_string())
            .collect(),
        loops: s.loop_flag.to_vec(),
        p: s.p.to_vec(),
        s: s.s.to_vec(),
        p1_src: src(0),
        p2_src: src(1),
        p3_src: src(2),
        p4_src: src(3),
        p5_src: src(4),
        p6_src: src(5),
        s1_src: src(6),
        s2_src: src(7),
        s3_src: src(8),
        s4_src: src(9),
        s5_src: src(10),
        s6_src: src(11),
        gate1_src: gsrc(0),
        gate2_src: gsrc(1),
        gate3_src: gsrc(2),
        gate4_src: gsrc(3),
        gate5_src: gsrc(4),
        gate6_src: gsrc(5),
    }
}

fn apply_params(s: &mut StagesState, p: &state::StagesParams) {
    for (i, name) in p.types.iter().take(NUM_SEGMENTS).enumerate() {
        if let Some(t) = TYPE_NAMES.iter().position(|n| n == name) {
            s.seg_type[i] = t;
        }
    }
    for (i, &l) in p.loops.iter().take(NUM_SEGMENTS).enumerate() {
        s.loop_flag[i] = l;
    }
    for (i, &v) in p.p.iter().take(NUM_SEGMENTS).enumerate() {
        s.p[i] = v.max(0.0).min(1.0);
    }
    for (i, &v) in p.s.iter().take(NUM_SEGMENTS).enumerate() {
        s.s[i] = v.max(0.0).min(1.0);
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.p1_src),
        parse(&p.p2_src),
        parse(&p.p3_src),
        parse(&p.p4_src),
        parse(&p.p5_src),
        parse(&p.p6_src),
        parse(&p.s1_src),
        parse(&p.s2_src),
        parse(&p.s3_src),
        parse(&p.s4_src),
        parse(&p.s5_src),
        parse(&p.s6_src),
    ];
    s.gate_src = [
        parse(&p.gate1_src),
        parse(&p.gate2_src),
        parse(&p.gate3_src),
        parse(&p.gate4_src),
        parse(&p.gate5_src),
        parse(&p.gate6_src),
    ];
    s.resolved = Default::default();
}

// ── grouping ───────────────────────────────────────────────────────────────

/// The hardware law: a segment with a patched gate starts a group;
/// leading ungated segments form a free-running group.
fn group_starts(gated: &[bool; NUM_SEGMENTS]) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, &g) in gated.iter().enumerate().skip(1) {
        if g {
            starts.push(i);
        }
    }
    starts
}

// ── control thread ─────────────────────────────────────────────────────────

fn control_thread(shared: Arc<Mutex<StagesState>>, instance: usize) -> Result<()> {
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("stages", instance, None, NUM_SEGMENTS as u32)?;
    let mod_base = manifest.claimed_base();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();
    let mut events = EventRingbuf::open_dynamic().ok();
    let _transport = ShmTransport::open().ok();

    let mut generators: Vec<SegmentGenerator> = (0..NUM_SEGMENTS)
        .map(|i| SegmentGenerator::new(0x57a6 ^ (instance as u32) << 8 ^ i as u32))
        .collect();
    let mut note_filter: [Option<u8>; NUM_SEGMENTS] = [None; NUM_SEGMENTS];
    let mut prev_flag = [GATE_LOW; NUM_SEGMENTS];
    let mut last_cfg: Option<([usize; NUM_SEGMENTS], [bool; NUM_SEGMENTS], [bool; NUM_SEGMENTS])> =
        None;
    let mut starts: Vec<usize> = vec![0];

    // ~750 Hz control ticks; the engine steps 31250/750 ≈ 42 native
    // samples per tick through a fractional accumulator
    let tick_dur = Duration::from_micros(1333);
    let native_per_tick = super::dsp::NATIVE_SR * tick_dur.as_secs_f64();
    let mut acc = 0.0_f64;
    let mut ticks: u64 = 0;
    let mut block: Vec<Output> = Vec::with_capacity(64);
    let mut gflags: Vec<u8> = Vec::with_capacity(64);

    loop {
        let t0 = Instant::now();
        if ticks.is_multiple_of(128) {
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
            #[allow(clippy::needless_range_loop)] // i strides note_filter + gate_src
            for i in 0..NUM_SEGMENTS {
                note_filter[i] = s.gate_src[i].as_ref().and_then(routing::note_source_track);
            }
            let mask = s
                .resolved
                .iter()
                .flatten()
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
                #[allow(clippy::needless_range_loop)] // i strides note_filter + gate
                for i in 0..NUM_SEGMENTS {
                    let Some(t) = note_filter[i] else { continue };
                    if event.source != t {
                        continue;
                    }
                    if event.is_note_on() {
                        s.gate[i] = true;
                    } else if event.is_note_off() {
                        s.gate[i] = false;
                    }
                }
            }
        }

        // reconfigure when the panel moved
        let (types, loops, p, sv, gates, gated) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            let cv = |k: usize, manual: f32, s: &StagesState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let mut p = [0.0_f32; NUM_SEGMENTS];
            let mut sv = [0.0_f32; NUM_SEGMENTS];
            for i in 0..NUM_SEGMENTS {
                p[i] = cv(i, s.p[i], &s);
                sv[i] = cv(NUM_SEGMENTS + i, s.s[i], &s);
                s.eff[i] = p[i];
                s.eff[NUM_SEGMENTS + i] = sv[i];
            }
            let mut gated = [false; NUM_SEGMENTS];
            #[allow(clippy::needless_range_loop)] // i strides gated + gate_src
            for i in 0..NUM_SEGMENTS {
                gated[i] = s.gate_src[i].is_some();
            }
            (s.seg_type, s.loop_flag, p, sv, s.gate, gated)
        };
        let cfg_key = (types, loops, gated);
        if last_cfg != Some(cfg_key) {
            last_cfg = Some(cfg_key);
            starts = group_starts(&gated);
            for (gi, &start) in starts.iter().enumerate() {
                let end = starts.get(gi + 1).copied().unwrap_or(NUM_SEGMENTS);
                let cfg: Vec<Configuration> = (start..end)
                    .map(|i| Configuration {
                        segment_type: match types[i] {
                            0 => SegmentType::Ramp,
                            1 => SegmentType::Step,
                            2 => SegmentType::Hold,
                            _ => SegmentType::Alt,
                        },
                        loop_flag: loops[i],
                    })
                    .collect();
                generators[start].configure(gated[start], &cfg);
            }
        }

        // knob values flow into the leaders every tick
        for (gi, &start) in starts.iter().enumerate() {
            let end = starts.get(gi + 1).copied().unwrap_or(NUM_SEGMENTS);
            for i in start..end {
                generators[start].set_segment_parameters(i - start, p[i], sv[i]);
            }
        }

        acc += native_per_tick;
        let n = acc as usize;
        acc -= n as f64;
        let n = n.clamp(1, 64);
        let mut out_values = [0.0_f32; NUM_SEGMENTS];
        for (gi, &start) in starts.iter().enumerate() {
            let end = starts.get(gi + 1).copied().unwrap_or(NUM_SEGMENTS);
            // the group's gate comes from its leader's binding
            gflags.clear();
            let g = gates[start] && gated[start];
            for k in 0..n {
                let _ = k;
                prev_flag[start] = extract_gate_flags(prev_flag[start], g);
                gflags.push(prev_flag[start]);
            }
            block.clear();
            block.resize(n, Output::default());
            generators[start].process(&gflags, &mut block);
            let last = block[n - 1];
            out_values[start] = last.value.max(0.0).min(1.0);
            // slaves: 1 - phase while their segment is active
            #[allow(clippy::needless_range_loop)] // i is both index and relative segment
            for i in start + 1..end {
                let rel = (i - start) as i32;
                out_values[i] = if last.segment == rel {
                    (1.0 - last.phase).max(0.0).min(1.0)
                } else {
                    0.0
                };
            }
        }

        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            let mut s = shared.lock().unwrap();
            for (i, v) in out_values.iter().enumerate() {
                let v = if v.is_finite() { *v } else { 0.0 };
                bus.set(base + i, v);
                s.out_now[i] = v;
            }
        }

        ticks += 1;
        let elapsed = t0.elapsed();
        if elapsed < tick_dur {
            thread::sleep(tick_dur - elapsed);
        }
    }
}

// ── ui ─────────────────────────────────────────────────────────────────────

fn row_label(s: &StagesState, r: Row) -> String {
    match r {
        Row::Type(i) => format!("seg {} type", i + 1),
        Row::Loop(_) => "· loop".into(),
        Row::P(i) => match s.seg_type[i] {
            0 | 3 => "· time".into(),  // ramp/alt: time
            _ => "· level".into(),     // step/hold: level
        },
        Row::S(i) => match s.seg_type[i] {
            0 | 3 => "· shape".into(), // ramp/alt: shape
            1 => "· porta".into(),     // step: portamento
            _ => "· time".into(),      // hold: time
        },
        Row::Gate(_) => "· gate".into(),
    }
}

fn row_text(s: &StagesState, r: Row) -> String {
    match r {
        Row::Type(i) => TYPE_NAMES[s.seg_type[i]].to_string(),
        Row::Loop(i) => if s.loop_flag[i] { "on" } else { "off" }.to_string(),
        Row::P(i) => format!("{:.0}%", s.p[i] * 100.0),
        Row::S(i) => format!("{:.0}%", s.s[i] * 100.0),
        Row::Gate(i) => s.gate_src[i]
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(none)".into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &StagesState,
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
            "STAGES",
            &format!("segments {}", instance),
            "",
            w,
        ));
        let mut meter = vec![Span::styled("  out ".to_string(), theme::chrome())];
        for v in s.out_now.iter() {
            meter.push(Span::styled(
                theme::meter_char(*v).to_string(),
                theme::signal(theme::cv_ramp(*v)),
            ));
            meter.push(Span::raw(" "));
        }
        lines.push(Line::from(meter));
        lines.push(Line::from(""));

        let bar_w = theme::bar_width(w, 30);
        for i in 0..N_ROWS {
            let r = row_at(i);
            let selected = i == s.selected;
            let label_style = if selected {
                theme::selected()
            } else {
                theme::chrome()
            };
            let mut spans: Vec<Span> = vec![Span::styled(
                format!(" {:<11}", row_label(s, r)),
                label_style,
            )];
            let bound = src_index(r).is_some_and(|k| s.srcs[k].is_some())
                || matches!(r, Row::Gate(g) if s.gate_src[g].is_some());
            let hue = src_index(r)
                .and_then(|k| s.srcs[k].as_ref())
                .or(match r {
                    Row::Gate(g) => s.gate_src[g].as_ref(),
                    _ => None,
                })
                .map(|a| routing::cable_color(entries, a));
            if src_index(r).is_some() {
                let shown = match src_index(r) {
                    Some(k) if s.srcs[k].is_some() => s.eff[k],
                    _ => s.get(r),
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
            spans.push(Span::styled(format!(" {}{}", mark, row_text(s, r)), vstyle));
            lines.push(Line::from(spans));
        }

        theme::anchor_bottom(&mut lines, h, 2);
        lines.push(theme::rule(w));
        lines.push(theme::status("NORMAL", overlay.unwrap_or(""), "", w));
        f.render_widget(Paragraph::new(lines), area);

        if show_help {
            let help = Paragraph::new(vec![
                Line::from("━━━ STAGES · segment generator (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  6 segments; a gate binding starts a new group."),
                Line::from("  type+loop per group decide what it becomes:"),
                Line::from("  ramp = env/LFO segments · step = S&H/seq steps"),
                Line::from("  hold = pulses/delays. One looping ramp alone is"),
                Line::from("  an LFO; ramps in a gated group are an envelope;"),
                Line::from("  hold + steps after it is the sequencer."),
                Line::from("  knobs      time/level + shape/porta per type"),
                Line::from(""),
                Line::from("Publishes stages/N/o1..o6 (CV only, no audio)."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" STAGES ", theme::chrome_hi())),
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
    Gate(usize),
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("stages", instance);

    let shared = Arc::new(Mutex::new(StagesState::new()));
    if let Ok(p) = state::load_module_state::<state::StagesParams>("stages", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let ctl_state = Arc::clone(&shared);
    let builder = thread::Builder::new()
        .name(String::from("stages-ctl"))
        .stack_size(4 * 1024 * 1024);
    let _ = builder.spawn(move || {
        if let Err(e) = control_thread(ctl_state, instance) {
            eprintln!("[stages {}] control thread error: {}", instance, e);
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
            let _ = state::save_module_state("stages", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::StagesParams>("stages", instance) {
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
                    let r = row_at(slot.min(N_ROWS - 1));
                    if !matches!(r, Row::P(_) | Row::S(_)) {
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
                    if row < N_ROWS {
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
                match picking {
                    Picking::ModSource => {
                        if let Some(k) = src_index(row_at(s.selected.min(N_ROWS - 1))) {
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
                    Picking::Gate(i) => {
                        let slot = i * ROWS_PER_SEG + 4;
                        let old = s.get_param(slot);
                        s.gate_src[i] = addr.clone();
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
                        match state::load_patch::<state::StagesParams>(&name) {
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
            let _ = state::save_module_state("stages", instance, &params);
            ex_msg = Some(String::from("Saved"));
            continue;
        }

        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
            KeyCode::Char('j') | KeyCode::Down => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, n, N_ROWS);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, -n, N_ROWS);
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
                match row_at(slot.min(N_ROWS - 1)) {
                    Row::Type(i) => {
                        let old = s.get_param(slot);
                        let next = (s.seg_type[i] as i32 + steps).rem_euclid(4) as usize;
                        s.seg_type[i] = next;
                        if let Some(old) = old {
                            history.record(slot, "Type", old, ParamValue::Usize(next));
                        }
                    }
                    Row::Loop(i) => {
                        let old = s.get_param(slot);
                        s.loop_flag[i] = !s.loop_flag[i];
                        let new = ParamValue::Bool(s.loop_flag[i]);
                        if let Some(old) = old {
                            history.record(slot, "Toggle", old, new);
                        }
                    }
                    Row::Gate(_) => {
                        ex_msg = Some("gate: @ binds a track, x unbinds".into());
                    }
                    r @ (Row::P(_) | Row::S(_)) => {
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
                let def = StagesState::new();
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
                let r = row_at(s.selected.min(N_ROWS - 1));
                match r {
                    Row::Gate(i) => {
                        let current = s.gate_src[i].clone();
                        drop(s);
                        let sources = Manifest::open()
                            .map(|m| routing::live_sources(&m.entries()))
                            .unwrap_or_default();
                        picking = Picking::Gate(i);
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
                            ex_msg = Some("type/loop rows: h/l cycles".into());
                        }
                    }
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let r = row_at(s.selected.min(N_ROWS - 1));
                match r {
                    Row::Gate(i) => {
                        if s.gate_src[i].is_some() {
                            let slot = s.selected;
                            let old = s.get_param(slot);
                            s.gate_src[i] = None;
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
                shared.lock().unwrap().selected = N_ROWS - 1;
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
    s: &mut StagesState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    // keys: type1..6, loop1..6, p1..6, s1..6, gate1..6
    let parse_seg = |k: &str, prefix: &str| -> Option<usize> {
        k.strip_prefix(prefix)
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| (1..=NUM_SEGMENTS).contains(n))
            .map(|n| n - 1)
    };
    let (r, slot) = if let Some(i) = parse_seg(key, "type") {
        (Row::Type(i), i * ROWS_PER_SEG)
    } else if let Some(i) = parse_seg(key, "loop") {
        (Row::Loop(i), i * ROWS_PER_SEG + 1)
    } else if let Some(i) = parse_seg(key, "gate") {
        (Row::Gate(i), i * ROWS_PER_SEG + 4)
    } else if let Some(i) = parse_seg(key, "p") {
        (Row::P(i), i * ROWS_PER_SEG + 2)
    } else if let Some(i) = parse_seg(key, "s") {
        (Row::S(i), i * ROWS_PER_SEG + 3)
    } else {
        return format!("Unknown setting: {key} (type1-6 loop1-6 p1-6 s1-6 gate1-6)");
    };
    let parsed: Result<V, String> = match r {
        Row::Type(_) => TYPE_NAMES
            .iter()
            .position(|n| *n == value)
            .map(V::Usize)
            .ok_or_else(|| format!("{key}: one of {}", TYPE_NAMES.join(" "))),
        Row::Loop(_) => match value {
            "on" | "true" | "1" => Ok(V::Bool(true)),
            "off" | "false" | "0" => Ok(V::Bool(false)),
            _ => Err(format!("{key}: on/off")),
        },
        Row::Gate(_) => {
            if value == "-" {
                Ok(V::Src(None))
            } else {
                Ok(V::Src(Some(value.to_string())))
            }
        }
        Row::P(_) | Row::S(_) => {
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
    fn every_knob_row_takes_a_cable() {
        for i in 0..N_ROWS {
            let r = row_at(i);
            match r {
                Row::P(_) | Row::S(_) => {
                    assert!(src_index(r).is_some(), "{r:?} must be bindable");
                }
                Row::Type(_) | Row::Loop(_) | Row::Gate(_) => {}
            }
        }
        assert_eq!(N_SRC, 12);
    }

    #[test]
    fn grouping_follows_gate_bindings() {
        let mut gated = [false; NUM_SEGMENTS];
        assert_eq!(group_starts(&gated), vec![0]);
        gated[2] = true;
        gated[4] = true;
        assert_eq!(group_starts(&gated), vec![0, 2, 4]);
        let mut all = [true; NUM_SEGMENTS];
        all[0] = false;
        assert_eq!(group_starts(&all), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = StagesState::new();
        s.seg_type = [0, 0, 1, 2, 1, 0];
        s.loop_flag[1] = true;
        s.p = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        s.srcs[0] = SourceAddr::parse("lfo/0/s1");
        s.srcs[7] = SourceAddr::parse("envelope/0/ch2");
        s.gate_src[0] = SourceAddr::parse("sequencer/0/t1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::StagesParams = toml::from_str(&toml).expect("parses");
        let mut s2 = StagesState::new();
        apply_params(&mut s2, &back);
        assert_eq!(s2.seg_type, [0, 0, 1, 2, 1, 0]);
        assert!(s2.loop_flag[1]);
        assert!((s2.p[3] - 0.4).abs() < 1e-6);
        assert_eq!(
            s2.srcs[7].as_ref().map(|a| a.to_string()),
            Some("envelope/0/ch2".into())
        );
        assert_eq!(
            s2.gate_src[0].as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t1".into())
        );
    }

    #[test]
    fn ex_set_parses() {
        let mut s = StagesState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "type1", "step").contains("step"));
        assert!(ex_set(&mut s, &mut h, "loop2", "on").contains("on"));
        assert!(ex_set(&mut s, &mut h, "p3", "0.7").contains('%'));
        assert!(ex_set(&mut s, &mut h, "s4", "lfo/0/s2").contains("lfo/0/s2"));
        assert!(ex_set(&mut s, &mut h, "gate1", "sequencer/0/t1").contains("t1"));
        assert!(ex_set(&mut s, &mut h, "gate1", "-").contains("(none)"));
        assert!(ex_set(&mut s, &mut h, "bogus", "1").contains("Unknown"));
        assert_eq!(s.seg_type[0], 1);
        assert!(s.loop_flag[1]);
        assert!(s.srcs[NUM_SEGMENTS + 3].is_some());
    }
}
