//! The Frames module shell: the frame knob scans keyframes across
//! four modulation outputs; `a` drops a keyframe at the current
//! frame, `d` deletes the nearest. Poly-LFO mode turns the same
//! four outputs into coupled wavetable LFOs.
//!
//! (The DSP underneath is the MIT-licensed Mutable Instruments port —
//! see dsp.rs for the copyright and permission notice.)

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
    Keyframe, Keyframer, PolyLfo, EASING_CURVES, EASING_NAMES, NUM_CHANNELS,
};
#[cfg(test)]
use super::dsp::EasingCurve;
use crate::routing::{self, SourceAddr};
use crate::shm::{Manifest, ModulationBus, ShmTransport};
use crate::state;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Mode,
    Frame,
    Ch(usize),
    Easing(usize),
    Resp(usize),
    Shape,
    Spread,
    ShapeSpread,
    Coupling,
}

fn rows() -> Vec<Row> {
    let mut r = vec![Row::Mode, Row::Frame];
    for c in 0..NUM_CHANNELS {
        r.push(Row::Ch(c));
    }
    for c in 0..NUM_CHANNELS {
        r.push(Row::Easing(c));
    }
    for c in 0..NUM_CHANNELS {
        r.push(Row::Resp(c));
    }
    r.extend([Row::Shape, Row::Spread, Row::ShapeSpread, Row::Coupling]);
    r
}

/// CV bank: frame, the four channel values, the four responses, and
/// the LFO macros — every value row takes a cable.
fn bindable() -> Vec<Row> {
    let mut b = vec![Row::Frame];
    for c in 0..NUM_CHANNELS {
        b.push(Row::Ch(c));
    }
    for c in 0..NUM_CHANNELS {
        b.push(Row::Resp(c));
    }
    b.extend([Row::Shape, Row::Spread, Row::ShapeSpread, Row::Coupling]);
    b
}

const N_SRC: usize = 1 + NUM_CHANNELS * 2 + 4;

struct FramesState {
    lfo_mode: bool,
    frame: f32,
    keyframer: Keyframer,
    shape: f32,
    spread: f32,
    shape_spread: f32,
    coupling: f32,
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    out_now: [f32; NUM_CHANNELS],
    selected: usize,
}

impl FramesState {
    fn new() -> Self {
        let mut s = FramesState {
            lfo_mode: false,
            frame: 0.0,
            keyframer: Keyframer {
                immediate: [0.5; NUM_CHANNELS],
                ..Default::default()
            },
            shape: 0.0,
            spread: 0.5,
            shape_spread: 0.0,
            coupling: 0.0,
            srcs: [const { None }; N_SRC],
            resolved: [None; N_SRC],
            eff: [0.0; N_SRC],
            out_now: [0.0; NUM_CHANNELS],
            selected: 0,
        };
        for (k, r) in bindable().iter().enumerate() {
            s.eff[k] = s.get(*r);
        }
        s
    }

    fn get(&self, r: Row) -> f32 {
        match r {
            Row::Frame => self.frame,
            Row::Ch(c) => self.keyframer.immediate[c],
            Row::Resp(c) => self.keyframer.response[c],
            Row::Shape => self.shape,
            Row::Spread => self.spread,
            Row::ShapeSpread => self.shape_spread,
            Row::Coupling => self.coupling,
            _ => 0.0,
        }
    }

    fn set(&mut self, r: Row, v: f32) {
        let v = v.clamp(0.0, 1.0);
        match r {
            Row::Frame => self.frame = v,
            Row::Ch(c) => self.keyframer.immediate[c] = v,
            Row::Resp(c) => self.keyframer.response[c] = v,
            Row::Shape => self.shape = v,
            Row::Spread => self.spread = v,
            Row::ShapeSpread => self.shape_spread = v,
            Row::Coupling => self.coupling = v,
            _ => {}
        }
    }
}

const SRC_SLOT_BASE: usize = 50;

impl crate::undo::ParamUndo for FramesState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            if i < N_SRC {
                return Some(V::Src(self.srcs[i].as_ref().map(|a| a.to_string())));
            }
            return None;
        }
        let all = rows();
        let r = *all.get(slot)?;
        Some(match r {
            Row::Mode => V::Bool(self.lfo_mode),
            Row::Easing(c) => V::Usize(
                EASING_CURVES
                    .iter()
                    .position(|e| *e == self.keyframer.easing[c])
                    .unwrap_or(1),
            ),
            _ => V::F32(self.get(r)),
        })
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        let parse = |v: &Option<String>| v.as_deref().and_then(SourceAddr::parse);
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            if i < N_SRC {
                if let V::Src(v) = &value {
                    self.srcs[i] = parse(v);
                    self.resolved[i] = None;
                }
            }
            return;
        }
        let all = rows();
        match (all.get(slot).copied(), value) {
            (Some(Row::Mode), V::Bool(v)) => self.lfo_mode = v,
            (Some(Row::Easing(c)), V::Usize(v)) => {
                self.keyframer.easing[c] = EASING_CURVES[v.min(5)];
            }
            (Some(r), V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &FramesState) -> state::FramesParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::FramesParams {
        format: state::STATE_FORMAT,
        mode: Some(if s.lfo_mode { "polylfo" } else { "keyframer" }.into()),
        frame: Some(s.frame),
        ch: s.keyframer.immediate.to_vec(),
        easing: s
            .keyframer
            .easing
            .iter()
            .map(|e| {
                EASING_NAMES[EASING_CURVES.iter().position(|c| c == e).unwrap_or(1)].to_string()
            })
            .collect(),
        response: s.keyframer.response.to_vec(),
        shape: Some(s.shape),
        spread: Some(s.spread),
        shape_spread: Some(s.shape_spread),
        coupling: Some(s.coupling),
        keyframes: s
            .keyframer
            .keyframes
            .iter()
            .map(|k| state::FramesKeyframe {
                t: k.timestamp,
                values: k.values.to_vec(),
            })
            .collect(),
        frame_src: src(0),
        ch1_src: src(1),
        ch2_src: src(2),
        ch3_src: src(3),
        ch4_src: src(4),
        resp1_src: src(5),
        resp2_src: src(6),
        resp3_src: src(7),
        resp4_src: src(8),
        shape_src: src(9),
        spread_src: src(10),
        shape_spread_src: src(11),
        coupling_src: src(12),
    }
}

fn apply_params(s: &mut FramesState, p: &state::FramesParams) {
    if let Some(m) = p.mode.as_deref() {
        s.lfo_mode = m == "polylfo";
    }
    if let Some(v) = p.frame {
        s.frame = v.clamp(0.0, 1.0);
    }
    for (c, v) in p.ch.iter().take(4).enumerate() {
        s.keyframer.immediate[c] = v.clamp(0.0, 1.0);
    }
    for (c, name) in p.easing.iter().take(4).enumerate() {
        if let Some(i) = EASING_NAMES.iter().position(|n| n == name) {
            s.keyframer.easing[c] = EASING_CURVES[i];
        }
    }
    for (c, v) in p.response.iter().take(4).enumerate() {
        s.keyframer.response[c] = v.clamp(0.0, 1.0);
    }
    macro_rules! f {
        ($field:ident, $row:expr) => {
            if let Some(v) = p.$field {
                s.set($row, v);
            }
        };
    }
    f!(shape, Row::Shape);
    f!(spread, Row::Spread);
    f!(shape_spread, Row::ShapeSpread);
    f!(coupling, Row::Coupling);
    s.keyframer.keyframes = p
        .keyframes
        .iter()
        .map(|k| {
            let mut values = [0.0_f32; NUM_CHANNELS];
            for (i, v) in k.values.iter().take(4).enumerate() {
                values[i] = v.clamp(0.0, 1.0);
            }
            Keyframe {
                timestamp: k.t.clamp(0.0, 1.0),
                values,
            }
        })
        .collect();
    s.keyframer
        .keyframes
        .sort_by(|a, b| a.timestamp.total_cmp(&b.timestamp));
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.frame_src),
        parse(&p.ch1_src),
        parse(&p.ch2_src),
        parse(&p.ch3_src),
        parse(&p.ch4_src),
        parse(&p.resp1_src),
        parse(&p.resp2_src),
        parse(&p.resp3_src),
        parse(&p.resp4_src),
        parse(&p.shape_src),
        parse(&p.spread_src),
        parse(&p.shape_spread_src),
        parse(&p.coupling_src),
    ];
    s.resolved = [None; N_SRC];
}

// ── engine thread ──────────────────────────────────────────────────────────

/// Poly-LFO rate law: frame 0–1 → 0.0134 Hz … ~110 Hz (the
/// firmware's 13-octave increments table).
fn lfo_hz(frame: f32) -> f32 {
    0.0134 * 2.0_f32.powf(frame.clamp(0.0, 1.0) * 13.0)
}

fn engine_thread(shared: Arc<Mutex<FramesState>>, instance: usize) -> Result<()> {
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    // four claims: ch1-ch4 on the modulation bus
    manifest.register("frames", instance, None, NUM_CHANNELS as u32)?;
    let mod_base = manifest.claimed_base();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();
    let mut lfo = PolyLfo::default();
    let mut ticks: u64 = 0;
    const TICK: Duration = Duration::from_millis(1);

    loop {
        if ticks.is_multiple_of(64) {
            let entries = manifest.entries();
            let mut s = shared.lock().unwrap();
            for k in 0..N_SRC {
                s.resolved[k] = s.srcs[k]
                    .as_ref()
                    .and_then(|a| routing::resolve(&entries, a));
            }
            let mask = s
                .resolved
                .iter()
                .flatten()
                .filter(|&&c| c < 64)
                .fold(0u64, |m, &c| m | (1 << c));
            manifest.publish_consumes(mask, 0);
        }

        let levels = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            // max/min, not clamp: NaN from a stale channel dies here
            #[allow(clippy::manual_clamp)]
            let cv = |k: usize, manual: f32, s: &FramesState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let bank = bindable();
            let mut vals = [0.0_f32; N_SRC];
            for (k, r) in bank.iter().enumerate() {
                vals[k] = cv(k, s.get(*r), &s);
                s.eff[k] = vals[k];
            }
            let frame = vals[0];
            let levels = if s.lfo_mode {
                lfo.shape = vals[9];
                lfo.spread = vals[10] * 2.0 - 1.0;
                lfo.shape_spread = vals[11] * 2.0 - 1.0;
                lfo.coupling = vals[12] * 2.0 - 1.0;
                lfo.render(lfo_hz(frame), TICK.as_secs_f32())
            } else {
                // CV-driven channel values ride on top of the stored
                // immediates while editing
                let mut k = s.keyframer.clone();
                k.immediate.copy_from_slice(&vals[1..1 + NUM_CHANNELS]);
                k.response.copy_from_slice(&vals[5..5 + NUM_CHANNELS]);
                k.evaluate(frame)
            };
            s.out_now = levels;
            levels
        };

        if let (Some(b), Some(bus)) = (mod_base, modbus.as_mut()) {
            for (i, v) in levels.iter().enumerate() {
                bus.set(b + i, v.clamp(0.0, 1.0));
            }
        }

        ticks += 1;
        thread::sleep(TICK);
    }
}

// ── ui ─────────────────────────────────────────────────────────────────────

fn row_label(r: Row) -> String {
    match r {
        Row::Mode => "mode".into(),
        Row::Frame => "frame".into(),
        Row::Ch(c) => format!("ch {}", c + 1),
        Row::Easing(c) => format!("ease {}", c + 1),
        Row::Resp(c) => format!("resp {}", c + 1),
        Row::Shape => "shape".into(),
        Row::Spread => "spread".into(),
        Row::ShapeSpread => "sspread".into(),
        Row::Coupling => "coupling".into(),
    }
}

fn src_index(r: Row) -> Option<usize> {
    bindable().iter().position(|b| *b == r)
}

fn binding_slot(r: Row) -> Option<usize> {
    src_index(r).map(|i| SRC_SLOT_BASE + i)
}

fn row_text(s: &FramesState, r: Row) -> String {
    match r {
        Row::Mode => if s.lfo_mode { "poly lfo" } else { "keyframer" }.into(),
        Row::Frame => format!(
            "{:.0}% · {} keyframes",
            s.frame * 100.0,
            s.keyframer.keyframes.len()
        ),
        Row::Easing(c) => EASING_NAMES[EASING_CURVES
            .iter()
            .position(|e| *e == s.keyframer.easing[c])
            .unwrap_or(1)]
        .into(),
        _ => format!("{:.0}%", s.get(r) * 100.0),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &FramesState,
    instance: usize,
    show_help: bool,
    overlay: Option<&str>,
    picker: Option<(Vec<String>, usize)>,
    entries: &[crate::shm::ManifestEntry],
) -> Result<()> {
    use crate::theme;
    let all = rows();
    terminal.draw(|f| {
        let area = f.area();
        let w = area.width as usize;
        let h = area.height as usize;
        let mut lines: Vec<Line> = Vec::new();
        lines.push(theme::header(
            "FRAMES",
            &format!("keyframer {}", instance),
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
        for (i, r) in all.iter().enumerate() {
            let selected = i == s.selected;
            let label_style = if selected {
                theme::selected()
            } else {
                theme::chrome()
            };
            let mut spans: Vec<Span> =
                vec![Span::styled(format!(" {:<8}", row_label(*r)), label_style)];
            let bound = src_index(*r).is_some_and(|k| s.srcs[k].is_some());
            let hue = src_index(*r)
                .and_then(|k| s.srcs[k].as_ref())
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
                Line::from("━━━ FRAMES · keyframer (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  frame    THE knob — scans the keyframes; bind it"),
                Line::from("           to an LFO and the snapshot morphs itself"),
                Line::from("  ch 1–4   channel values (recorded into keyframes)"),
                Line::from("  ease     per-channel interpolation curve"),
                Line::from("  resp     linear ↔ exponential response"),
                Line::from(""),
                Line::from("  a        add/replace keyframe at the current frame"),
                Line::from("  d        delete the nearest keyframe"),
                Line::from(""),
                Line::from("Poly-LFO mode: frame = rate, shape/spread/sspread/"),
                Line::from("coupling sculpt four entangled wavetable LFOs."),
                Line::from("Outputs frames/N/ch1–ch4 on the bus."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" FRAMES ", theme::chrome_hi())),
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
    state::write_pid_file("frames", instance);

    let shared = Arc::new(Mutex::new(FramesState::new()));
    if let Ok(p) = state::load_module_state::<state::FramesParams>("frames", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let engine_state = Arc::clone(&shared);
    let builder = thread::Builder::new().name(String::from("frames-engine"));
    let _ = builder.spawn(move || {
        // black box: a dead engine thread must leave a trace
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine_thread(engine_state, instance)
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
        eprintln!("[frames {instance}] engine thread died: {msg}");
        let path = crate::state::tmp_dir().join(format!("frames_{instance}.crash"));
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

    let all_rows = rows();
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
            let _ = state::save_module_state("frames", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::FramesParams>("frames", instance) {
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
                    let r = all_rows[slot.min(all_rows.len() - 1)];
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
                    if row < all_rows.len() {
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
                let r = all_rows[s.selected.min(all_rows.len() - 1)];
                if let Some(slot) = binding_slot(r) {
                    let old = s.get_param(slot);
                    let text = addr.as_ref().map(|a| a.to_string());
                    if let Some(k) = src_index(r) {
                        s.srcs[k] = addr.clone();
                        s.resolved[k] = None;
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
                    ExCommand::Edit(name) => {
                        match state::load_patch::<state::FramesParams>(&name) {
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
            let _ = state::save_module_state("frames", instance, &params);
            ex_msg = Some(String::from("Saved"));
            continue;
        }

        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
            KeyCode::Char('j') | KeyCode::Down => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, n, all_rows.len());
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, -n, all_rows.len());
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
                let r = all_rows[slot.min(all_rows.len() - 1)];
                let old = s.get_param(slot);
                match r {
                    Row::Mode => s.lfo_mode = !s.lfo_mode,
                    Row::Easing(c) => {
                        let cur = EASING_CURVES
                            .iter()
                            .position(|e| *e == s.keyframer.easing[c])
                            .unwrap_or(1);
                        let next = (cur as i32 + steps.signum()).rem_euclid(6) as usize;
                        s.keyframer.easing[c] = EASING_CURVES[next];
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
            KeyCode::Char('a') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                let frame = s.frame;
                let values = s.keyframer.immediate;
                if s.keyframer.add(frame, values) {
                    ex_msg = Some(format!(
                        "Keyframe at {:.0}% ({} total)",
                        frame * 100.0,
                        s.keyframer.keyframes.len()
                    ));
                } else {
                    ex_msg = Some("Keyframe store full".into());
                }
            }
            KeyCode::Char('d') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                let frame = s.frame;
                if s.keyframer.remove_nearest(frame, 0.05) {
                    ex_msg = Some(format!(
                        "Keyframe removed ({} left)",
                        s.keyframer.keyframes.len()
                    ));
                } else {
                    ex_msg = Some("No keyframe within reach".into());
                }
            }
            KeyCode::Char('0') => {
                count.clear();
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                let slot = s.selected;
                let old = s.get_param(slot);
                let def = FramesState::new();
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
                let r = all_rows[s.selected.min(all_rows.len() - 1)];
                if binding_slot(r).is_some() {
                    let current = src_index(r).and_then(|k| s.srcs[k].clone());
                    drop(s);
                    let sources = Manifest::open()
                        .map(|m| routing::live_sources(&m.entries()))
                        .unwrap_or_default();
                    picker.open(sources, current.as_ref());
                } else {
                    ex_msg = Some(format!("{} is not bindable", row_label(r)));
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let r = all_rows[s.selected.min(all_rows.len() - 1)];
                if let Some(slot) = binding_slot(r) {
                    if let Some(k) = src_index(r) {
                        if s.srcs[k].is_some() {
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
                shared.lock().unwrap().selected = all_rows.len() - 1;
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
    s: &mut FramesState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    if key == "mode" {
        s.lfo_mode = matches!(value, "polylfo" | "lfo");
        return format!("mode = {}", if s.lfo_mode { "polylfo" } else { "keyframer" });
    }
    if let Some(rest) = key.strip_prefix("ease") {
        if let Ok(c) = rest.parse::<usize>() {
            if (1..=4).contains(&c) {
                if let Some(i) = EASING_NAMES.iter().position(|n| *n == value) {
                    s.keyframer.easing[c - 1] = EASING_CURVES[i];
                    return format!("{key} = {value}");
                }
                return format!("{key}: one of {}", EASING_NAMES.join(" "));
            }
        }
    }
    let chan = |k: &str, prefix: &str| -> Option<usize> {
        k.strip_prefix(prefix)
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| (1..=4).contains(&n).then_some(n - 1))
    };
    let row = if key == "frame" {
        Row::Frame
    } else if let Some(c) = chan(key, "ch") {
        Row::Ch(c)
    } else if let Some(c) = chan(key, "resp") {
        Row::Resp(c)
    } else if key == "shape" {
        Row::Shape
    } else if key == "spread" {
        Row::Spread
    } else if key == "sspread" {
        Row::ShapeSpread
    } else if key == "coupling" {
        Row::Coupling
    } else {
        return format!("Unknown setting: {key}");
    };
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
    fn params_round_trip_through_toml_with_keyframes() {
        let mut s = FramesState::new();
        s.frame = 0.4;
        s.keyframer.add(0.0, [0.0, 1.0, 0.5, 0.2]);
        s.keyframer.add(1.0, [1.0, 0.0, 0.5, 0.9]);
        s.keyframer.easing[1] = EasingCurve::Bounce;
        s.srcs[0] = SourceAddr::parse("lfo/0/s1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::FramesParams = toml::from_str(&toml).expect("parses");
        let mut s2 = FramesState::new();
        apply_params(&mut s2, &back);
        assert!((s2.frame - 0.4).abs() < 1e-6);
        assert_eq!(s2.keyframer.keyframes.len(), 2);
        assert_eq!(s2.keyframer.easing[1], EasingCurve::Bounce);
        assert!(s2.srcs[0].is_some());
    }

    #[test]
    fn every_value_row_is_bindable() {
        use crate::undo::ParamUndo;
        use crate::undo::ParamValue as V;
        for r in rows() {
            if matches!(r, Row::Mode | Row::Easing(_)) {
                continue;
            }
            let i = src_index(r).unwrap_or_else(|| panic!("{r:?} must be bindable"));
            let mut s = FramesState::new();
            s.set_param(SRC_SLOT_BASE + i, V::Src(Some("lfo/0/a1".into())));
            assert!(s.srcs[i].is_some(), "{r:?} binds");
        }
    }

    #[test]
    fn ex_set_handles_modes_eases_and_cables() {
        let mut s = FramesState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "mode", "polylfo").contains("polylfo"));
        assert!(ex_set(&mut s, &mut h, "ease2", "bounce").contains("bounce"));
        assert_eq!(s.keyframer.easing[1], EasingCurve::Bounce);
        assert!(ex_set(&mut s, &mut h, "frame", "0.6").contains("60%"));
        assert!(ex_set(&mut s, &mut h, "frame", "lfo/0/s1").contains("cable"));
        assert!(s.srcs[0].is_some());
    }

    #[test]
    fn lfo_rate_law_spans_thirteen_octaves() {
        assert!((lfo_hz(0.0) - 0.0134).abs() < 1e-3);
        assert!((lfo_hz(1.0) / 110.0 - 0.0134 * 8192.0 / 110.0).abs() < 0.2);
        assert!(lfo_hz(1.0) > 100.0);
    }
}
