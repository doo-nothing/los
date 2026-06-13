//! # Braids — the macro-oscillator voice shell
//!
//! A monophonic voice: a note source sets the pitch, an amp source (an
//! envelope) shapes the level. The `model` selects one of the analog
//! macro-oscillator voices; `timbre` and `color` are its two
//! parameters. The engine runs at braids' native 96 kHz and is
//! linearly resampled to the session rate. Publishes braids/N/level
//! (an output follower) on the bus.
//!
//! All 48 firmware models are available — the 13 analog-family
//! models and the 35 digital models (see `MODEL_NAMES`).

// max/min, not clamp, where modbus values land: clamp(NaN) is NaN and a
// stale channel must die at the boundary.
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

use super::dsp::{MacroOscillator, MODELS, MODEL_NAMES, NATIVE_SR};
use crate::ipc::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

const FALLBACK_RATE: f32 = 48_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Model,
    Timbre,
    Color,
    Level,
    Amp,
    Notes,
}

const ROWS: [Row; 6] = [
    Row::Model,
    Row::Timbre,
    Row::Color,
    Row::Level,
    Row::Amp,
    Row::Notes,
];

/// Bindable continuous knobs — timbre, color, level.
const BINDABLE: [Row; 3] = [Row::Timbre, Row::Color, Row::Level];
const N_SRC: usize = BINDABLE.len();
const SRC_SLOT_BASE: usize = 20;
const AMP_SLOT: usize = 4;
const NOTES_SLOT: usize = 5;

fn src_index(r: Row) -> Option<usize> {
    BINDABLE.iter().position(|b| *b == r)
}

struct BraidsState {
    model: usize,
    timbre: f32,
    color: f32,
    level: f32,
    freq: f32,
    gate: bool,
    velocity: f32,
    amp_src: Option<SourceAddr>,
    amp_resolved: Option<usize>,
    notes_src: Option<SourceAddr>,
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    out_now: f32,
    selected: usize,
}

impl BraidsState {
    fn new() -> Self {
        Self {
            model: 1, // morph — braids' signature default
            timbre: 0.5,
            color: 0.5,
            level: 0.8,
            freq: 220.0,
            gate: false,
            velocity: 0.0,
            amp_src: None,
            amp_resolved: None,
            notes_src: None,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.5, 0.5, 0.8],
            out_now: 0.0,
            selected: 0,
        }
    }

    fn get(&self, r: Row) -> f32 {
        match r {
            Row::Timbre => self.timbre,
            Row::Color => self.color,
            Row::Level => self.level,
            _ => 0.0,
        }
    }

    fn set(&mut self, r: Row, v: f32) {
        let v = v.max(0.0).min(1.0);
        match r {
            Row::Timbre => self.timbre = v,
            Row::Color => self.color = v,
            Row::Level => self.level = v,
            _ => {}
        }
    }
}

impl crate::undo::ParamUndo for BraidsState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            let s = self.srcs.get(i)?;
            return Some(V::Src(s.as_ref().map(|a| a.to_string())));
        }
        let r = *ROWS.get(slot)?;
        Some(match r {
            Row::Model => V::Usize(self.model),
            Row::Amp => V::Src(self.amp_src.as_ref().map(|a| a.to_string())),
            Row::Notes => V::Src(self.notes_src.as_ref().map(|a| a.to_string())),
            _ => V::F32(self.get(r)),
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
            (Row::Model, V::Usize(v)) => self.model = v.min(MODEL_NAMES.len() - 1),
            (Row::Amp, V::Src(v)) => {
                self.amp_src = v.as_deref().and_then(SourceAddr::parse);
                self.amp_resolved = None;
            }
            (Row::Notes, V::Src(v)) => self.notes_src = v.as_deref().and_then(SourceAddr::parse),
            (_, V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &BraidsState) -> state::BraidsParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::BraidsParams {
        format: state::STATE_FORMAT,
        model: Some(MODEL_NAMES[s.model].to_string()),
        timbre: Some(s.timbre),
        color: Some(s.color),
        level: Some(s.level),
        timbre_src: src(0),
        color_src: src(1),
        level_src: src(2),
        amp_src: s.amp_src.as_ref().map(|a| a.to_string()),
        notes_src: s.notes_src.as_ref().map(|a| a.to_string()),
    }
}

fn apply_params(s: &mut BraidsState, p: &state::BraidsParams) {
    if let Some(m) = p.model.as_deref() {
        if let Some(i) = MODEL_NAMES.iter().position(|n| *n == m) {
            s.model = i;
        }
    }
    if let Some(v) = p.timbre {
        s.timbre = v.max(0.0).min(1.0);
    }
    if let Some(v) = p.color {
        s.color = v.max(0.0).min(1.0);
    }
    if let Some(v) = p.level {
        s.level = v.max(0.0).min(1.0);
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [parse(&p.timbre_src), parse(&p.color_src), parse(&p.level_src)];
    s.amp_src = parse(&p.amp_src);
    s.notes_src = parse(&p.notes_src);
    s.resolved = Default::default();
    s.amp_resolved = None;
}

/// Hz → braids pitch (MIDI × 128).
fn freq_to_pitch(freq: f32) -> i16 {
    let midi = 69.0 + 12.0 * (freq.max(1.0) / 440.0).log2();
    ((midi * 128.0).round() as i32).clamp(0, 127 * 128) as i16
}

// ── audio thread ─────────────────────────────────────────────────────────────

fn audio_thread(shared: Arc<Mutex<BraidsState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_braids_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating braids ring")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("braids", instance, Some(&shm_name), 1)?;
    let mod_base = manifest.claimed_base();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();
    let mut events = EventRingbuf::open_dynamic().ok();
    let transport = ShmTransport::open().ok();
    let mut sample_rate = transport
        .as_ref()
        .map(|t| t.sample_rate() as f32)
        .filter(|r| *r > 0.0)
        .unwrap_or(FALLBACK_RATE);

    let channels = ringbuf.channels() as usize;
    let slot_frames = ringbuf.slot_frames() as usize;
    let mut block = vec![0.0_f32; ringbuf.slot_len()];

    let mut osc = MacroOscillator::new();
    // 96 kHz render scratch + a fractional-position resampler
    let mut ratio = NATIVE_SR as f32 / sample_rate;
    let mut native_scratch = vec![0i16; (slot_frames as f32 * ratio) as usize + 4];
    let sync = vec![0u8; native_scratch.len()];
    let mut resample_pos = 0.0_f32;
    let mut prev_native = 0i16;

    let mut note_filter: Option<u8> = None;
    let mut gain_smooth = 0.0_f32;
    let mut follower = 0.0_f32;
    let mut blocks: u64 = 0;

    loop {
        if blocks.is_multiple_of(128) {
            let now_rate = transport
                .as_ref()
                .map(|t| t.sample_rate() as f32)
                .filter(|r| *r > 0.0)
                .unwrap_or(FALLBACK_RATE);
            if (now_rate - sample_rate).abs() > 0.5 {
                sample_rate = now_rate;
                ratio = NATIVE_SR as f32 / sample_rate;
                native_scratch = vec![0i16; (slot_frames as f32 * ratio) as usize + 4];
            }
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
            note_filter = s.notes_src.as_ref().and_then(routing::note_source_track);
            let mask = s
                .resolved
                .iter()
                .flatten()
                .chain(s.amp_resolved.iter())
                .filter(|&&c| c < 64)
                .fold(0u64, |m, &c| m | (1 << c));
            let notes = note_filter.filter(|&t| t < 8).map_or(0u8, |t| 1 << t);
            manifest.publish_consumes(mask, notes);
        }

        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                if let Some(t) = note_filter {
                    if event.source != t {
                        continue;
                    }
                }
                let mut s = shared.lock().unwrap();
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

        let (model, pitch, timbre, color, level, amp, gate) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            let cv = |k: usize, manual: f32, s: &BraidsState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let timbre = cv(0, s.timbre, &s);
            let color = cv(1, s.color, &s);
            let level = cv(2, s.level, &s);
            s.eff = [timbre, color, level];
            let amp = match (s.amp_src.is_some(), s.amp_resolved, bus) {
                (false, _, _) => 1.0,
                (true, Some(ch), Some(b)) => b.get(ch).clamp(0.0, 1.0),
                (true, _, _) => 0.0,
            };
            (
                MODELS[s.model.min(MODELS.len() - 1)],
                freq_to_pitch(s.freq),
                timbre,
                color,
                level,
                amp,
                s.gate,
            )
        };

        osc.set_model(model);
        osc.set_pitch(pitch);
        osc.set_parameters(
            (timbre.max(0.0).min(1.0) * 32767.0) as i16,
            (color.max(0.0).min(1.0) * 32767.0) as i16,
        );

        // render at 96k, then linearly resample to the session rate
        let need = (slot_frames as f32 * ratio).ceil() as usize + 2;
        if native_scratch.len() < need {
            native_scratch.resize(need, 0);
        }
        osc.render(&sync[..need.min(sync.len())], &mut native_scratch[..need], need);

        let g_alpha = 1.0 - (-1.0 / (0.0007 * sample_rate)).exp();
        // amplitude: when a note gates with no amp_src, fall back to velocity
        let strength = if amp < 0.999 {
            amp
        } else if gate {
            self_velocity_floor()
        } else {
            0.0
        };
        let gain_target = strength * level;
        for f in 0..slot_frames {
            let idx = resample_pos.floor() as usize;
            let frac = resample_pos - idx as f32;
            let a = if idx == 0 {
                prev_native
            } else {
                native_scratch[(idx - 1).min(need - 1)]
            } as f32;
            let b = native_scratch[idx.min(need - 1)] as f32;
            let s = (a + (b - a) * frac) / 32768.0;
            gain_smooth += (gain_target - gain_smooth) * g_alpha;
            let v = s * gain_smooth;
            follower = follower.max(v.abs()) * 0.9995;
            block[f * channels] = v;
            if channels > 1 {
                block[f * channels + 1] = v;
            }
            resample_pos += ratio;
        }
        // carry the fractional position into the next block
        let consumed = resample_pos.floor() as usize;
        prev_native = native_scratch[consumed.saturating_sub(1).min(need - 1)];
        resample_pos -= consumed as f32;

        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            osc = MacroOscillator::new();
            resample_pos = 0.0;
        }
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            bus.set(base, follower.min(1.0));
        }
        shared.lock().unwrap().out_now = follower.min(1.0);
        while ringbuf.write(&block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }
        blocks += 1;
    }
}

/// Default strength for a gated note with no amp envelope bound.
fn self_velocity_floor() -> f32 {
    0.8
}

// ── ui ───────────────────────────────────────────────────────────────────────

fn row_label(r: Row) -> &'static str {
    match r {
        Row::Model => "model",
        Row::Timbre => "timbre",
        Row::Color => "color",
        Row::Level => "level",
        Row::Amp => "amp",
        Row::Notes => "notes",
    }
}

fn row_text(s: &BraidsState, r: Row) -> String {
    match r {
        Row::Model => MODEL_NAMES[s.model].to_string(),
        Row::Timbre | Row::Color | Row::Level => format!("{:.0}%", s.get(r) * 100.0),
        Row::Amp => s
            .amp_src
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(gate · velocity)".into()),
        Row::Notes => s
            .notes_src
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(none)".into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &BraidsState,
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
        lines.push(theme::header("BRAIDS", &format!("macro {}", instance), "", w));
        lines.push(Line::from(vec![
            Span::styled("  out ".to_string(), theme::chrome()),
            Span::styled(
                theme::meter_char(s.out_now).to_string(),
                theme::signal(theme::cv_ramp(s.out_now)),
            ),
        ]));
        lines.push(Line::from(""));

        let bar_w = theme::bar_width(w, 28);
        for (i, r) in ROWS.iter().enumerate() {
            let selected = i == s.selected;
            let label_style = if selected {
                theme::selected()
            } else {
                theme::chrome()
            };
            let mut spans: Vec<Span> =
                vec![Span::styled(format!(" {:<8}", row_label(*r)), label_style)];
            let bound = src_index(*r).is_some_and(|k| s.srcs[k].is_some())
                || (*r == Row::Amp && s.amp_src.is_some())
                || (*r == Row::Notes && s.notes_src.is_some());
            let hue = src_index(*r)
                .and_then(|k| s.srcs[k].as_ref())
                .or(match r {
                    Row::Amp => s.amp_src.as_ref(),
                    Row::Notes => s.notes_src.as_ref(),
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
            spans.push(Span::styled(format!(" {}{}", mark, row_text(s, *r)), vstyle));
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
                Line::from("━━━ BRAIDS · macro-oscillator (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  model    csaw·morph·saw_square·sine_triangle"),
                Line::from("           buzz·square/saw_sub·square/saw_sync"),
                Line::from("           triple saw/square/triangle/sine"),
                Line::from("  timbre   the model's first parameter"),
                Line::from("  color    the model's second parameter"),
                Line::from("  notes    a note track sets the pitch"),
                Line::from("  amp      an envelope channel shapes the level"),
                Line::from(""),
                Line::from("Renders at 96 kHz, resampled to the session rate."),
                Line::from("(all 48 models: analog + digital)"),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" BRAIDS ", theme::chrome_hi())),
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
    Amp,
    Notes,
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("braids", instance);

    let shared = Arc::new(Mutex::new(BraidsState::new()));
    if let Ok(p) = state::load_module_state::<state::BraidsParams>("braids", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let builder = thread::Builder::new()
        .name(String::from("braids-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = builder.spawn(move || {
        if let Err(e) = audio_thread(audio_state, instance) {
            eprintln!("[braids {}] audio thread error: {}", instance, e);
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
            let _ = state::save_module_state("braids", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::BraidsParams>("braids", instance) {
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
                    if src_index(r).is_none() {
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
            if let crate::picker::PickerEvent::Chosen(addr) = picker.handle_key(key.code) {
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                match picking {
                    Picking::ModSource => {
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
                    Picking::Amp => {
                        let old = s.get_param(AMP_SLOT);
                        s.amp_src = addr.clone();
                        s.amp_resolved = None;
                        if let Some(old) = old {
                            history.record(
                                AMP_SLOT,
                                "Bind",
                                old,
                                ParamValue::Src(addr.map(|a| a.to_string())),
                            );
                        }
                    }
                    Picking::Notes => {
                        let old = s.get_param(NOTES_SLOT);
                        s.notes_src = addr.clone();
                        if let Some(old) = old {
                            history.record(
                                NOTES_SLOT,
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
                    ExCommand::Edit(name) => match state::load_patch::<state::BraidsParams>(&name) {
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
            let _ = state::save_module_state("braids", instance, &params);
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
                    Row::Model => {
                        let old = s.get_param(slot);
                        let v = (s.model as i32 + steps).rem_euclid(MODEL_NAMES.len() as i32) as usize;
                        s.model = v;
                        if let Some(old) = old {
                            history.record(slot, "Model", old, ParamValue::Usize(v));
                        }
                    }
                    Row::Amp | Row::Notes => {
                        ex_msg = Some("source row: @ binds, x clears".into());
                    }
                    _ => {
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
                let def = BraidsState::new();
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
                let (pick, current) = match r {
                    Row::Amp => (Some(Picking::Amp), s.amp_src.clone()),
                    Row::Notes => (Some(Picking::Notes), s.notes_src.clone()),
                    _ => match src_index(r) {
                        Some(k) => (Some(Picking::ModSource), s.srcs[k].clone()),
                        None => (None, None),
                    },
                };
                drop(s);
                if let Some(p) = pick {
                    let sources = Manifest::open()
                        .map(|m| routing::live_sources(&m.entries()))
                        .unwrap_or_default();
                    picking = p;
                    picker.open(sources, current.as_ref());
                } else {
                    ex_msg = Some("model row: h/l cycles".into());
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                match r {
                    Row::Amp => {
                        if s.amp_src.is_some() {
                            let old = s.get_param(AMP_SLOT);
                            s.amp_src = None;
                            s.amp_resolved = None;
                            if let Some(old) = old {
                                history.record(AMP_SLOT, "Unbind", old, ParamValue::Src(None));
                            }
                        }
                    }
                    Row::Notes => {
                        if s.notes_src.is_some() {
                            let old = s.get_param(NOTES_SLOT);
                            s.notes_src = None;
                            if let Some(old) = old {
                                history.record(NOTES_SLOT, "Unbind", old, ParamValue::Src(None));
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
    s: &mut BraidsState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    let Some(slot) = ROWS.iter().position(|r| row_label(*r) == key) else {
        return format!("Unknown setting: {key} (model timbre color level amp notes)");
    };
    let r = ROWS[slot];
    let parsed: Result<V, String> = match r {
        Row::Model => MODEL_NAMES
            .iter()
            .position(|n| *n == value)
            .map(V::Usize)
            .ok_or_else(|| format!("{key}: one of {}", MODEL_NAMES.join(" "))),
        Row::Amp | Row::Notes => {
            if value == "-" {
                Ok(V::Src(None))
            } else {
                Ok(V::Src(Some(value.to_string())))
            }
        }
        _ => {
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
    fn every_continuous_knob_takes_a_cable() {
        for r in ROWS {
            if matches!(r, Row::Timbre | Row::Color | Row::Level) {
                assert!(src_index(r).is_some(), "{r:?} must be bindable");
            }
        }
        assert_eq!(N_SRC, 3);
    }

    #[test]
    fn freq_to_pitch_tracks_octaves() {
        let a4 = freq_to_pitch(440.0);
        let a5 = freq_to_pitch(880.0);
        // 69*128 = 8832 for A4; an octave is 12*128 = 1536
        assert_eq!(a4, 69 * 128);
        assert_eq!(a5 - a4, 12 * 128);
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = BraidsState::new();
        s.model = 9; // triple_saw
        s.timbre = 0.7;
        s.color = 0.3;
        s.srcs[0] = SourceAddr::parse("lfo/0/s1");
        s.amp_src = SourceAddr::parse("envelope/0/ch1");
        s.notes_src = SourceAddr::parse("sequencer/0/t1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::BraidsParams = toml::from_str(&toml).expect("parses");
        let mut s2 = BraidsState::new();
        apply_params(&mut s2, &back);
        assert_eq!(s2.model, 9);
        assert!((s2.timbre - 0.7).abs() < 1e-6);
        assert_eq!(s2.amp_src.as_ref().map(|a| a.to_string()), Some("envelope/0/ch1".into()));
        assert_eq!(s2.notes_src.as_ref().map(|a| a.to_string()), Some("sequencer/0/t1".into()));
    }

    #[test]
    fn ex_set_parses() {
        let mut s = BraidsState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "model", "buzz").contains("buzz"));
        assert!(ex_set(&mut s, &mut h, "timbre", "0.6").contains('%'));
        assert!(ex_set(&mut s, &mut h, "color", "lfo/0/s2").contains("lfo/0/s2"));
        assert!(ex_set(&mut s, &mut h, "notes", "sequencer/0/t1").contains("sequencer/0/t1"));
        assert_eq!(s.model, 4);
        assert!(s.srcs[1].is_some());
    }
}
