use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    symbols::Marker,
    text::Line,
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph},
    Terminal,
};

use crate::shm::{AudioRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

const BUFFER_SIZE: usize = 4096; // ~85ms of 48kHz audio
const SHM_NAME: &str = "/los_mix_in";

#[derive(Clone)]
struct ScopeState {
    buffer: Vec<f32>,
    mode: usize,
    channel: usize,
    zoom: f32,
    gain: f32,
    trigger_level: f32,
    source: usize,        // 0=audio, 1=modbus
    modbus_channel: usize,
    selected: usize,      // selected param row (not persisted)
    modbus_label: Option<String>, // live source label for modbus_channel
}

impl Default for ScopeState {
    fn default() -> Self {
        Self {
            buffer: vec![0.0; BUFFER_SIZE],
            mode: 0,
            channel: 2,
            zoom: 1.0,
            gain: 1.0,
            trigger_level: 0.0,
            source: 0,
            modbus_channel: 0,
            selected: 0,
            modbus_label: None,
        }
    }
}

// Param rows (axis rule: vertical list — j/k select, h/l adjust, H/L coarse)
const ROW_MODE: usize = 0;
const ROW_SOURCE: usize = 1;
const ROW_CHANNEL: usize = 2;
const ROW_MODBUS_CH: usize = 3;
const ROW_ZOOM: usize = 4;
const ROW_GAIN: usize = 5;
const ROW_TRIGGER: usize = 6;
const NUM_ROWS: usize = 7;

/// Adjust the selected param row by `steps` (negative = left/decrease).
/// Cyclic rows (mode/source/channel/modbus channel) wrap; continuous rows
/// step fine, or ×10 with `coarse`.
fn adjust(s: &mut ScopeState, steps: i32, coarse: bool) {
    use crate::keys::{cycle, step_f32};
    match s.selected {
        ROW_MODE => s.mode = cycle(s.mode, steps, 4),
        ROW_SOURCE => s.source = cycle(s.source, steps, 2),
        ROW_CHANNEL => s.channel = cycle(s.channel, steps, 3),
        ROW_MODBUS_CH => s.modbus_channel = cycle(s.modbus_channel, steps, 32),
        ROW_ZOOM => s.zoom = step_f32(s.zoom, steps, 0.1, coarse, 0.5, 10.0),
        ROW_GAIN => s.gain = step_f32(s.gain, steps, 0.1, coarse, 0.1, 10.0),
        ROW_TRIGGER => s.trigger_level = step_f32(s.trigger_level, steps, 0.05, coarse, -1.0, 1.0),
        _ => {}
    }
}

fn row_display(s: &ScopeState, row: usize) -> String {
    match row {
        ROW_MODE => format!(
            "Mode:{}",
            match s.mode {
                0 => "Braille",
                1 => "HalfBlock",
                2 => "Bars",
                _ => "Dots",
            }
        ),
        ROW_SOURCE => format!("Src:{}", if s.source == 0 { "Mix" } else { "Mod" }),
        ROW_CHANNEL => format!(
            "Ch:{}",
            match s.channel {
                0 => "L",
                1 => "R",
                _ => "S",
            }
        ),
        ROW_MODBUS_CH => match &s.modbus_label {
            Some(label) => format!("ModCh:{} ({})", s.modbus_channel, label),
            None => format!("ModCh:{}", s.modbus_channel),
        },
        ROW_ZOOM => format!("Zoom:{:.1}x", s.zoom),
        ROW_GAIN => format!("Gain:{:.1}x", s.gain),
        ROW_TRIGGER => format!("Trig:{:+.2}", s.trigger_level),
        _ => String::new(),
    }
}

fn snapshot_params(s: &ScopeState) -> state::ScopeParams {
    state::ScopeParams {
        mode: Some(s.mode),
        channel: Some(s.channel),
        zoom: Some(s.zoom),
        gain: Some(s.gain),
        source: Some(s.source),
        modbus_channel: Some(s.modbus_channel),
        trigger_level: Some(s.trigger_level),
    }
}

fn apply_params(s: &mut ScopeState, params: &state::ScopeParams) {
    if let Some(v) = params.mode { s.mode = v; }
    if let Some(v) = params.channel { s.channel = v; }
    if let Some(v) = params.zoom { s.zoom = v; }
    if let Some(v) = params.gain { s.gain = v; }
    if let Some(v) = params.source { s.source = v; }
    if let Some(v) = params.modbus_channel { s.modbus_channel = v; }
    if let Some(v) = params.trigger_level { s.trigger_level = v; }
}

impl crate::undo::ParamUndo for ScopeState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        match slot {
            ROW_MODE => Some(V::Usize(self.mode)),
            ROW_SOURCE => Some(V::Usize(self.source)),
            ROW_CHANNEL => Some(V::Usize(self.channel)),
            ROW_MODBUS_CH => Some(V::Usize(self.modbus_channel)),
            ROW_ZOOM => Some(V::F32(self.zoom)),
            ROW_GAIN => Some(V::F32(self.gain)),
            ROW_TRIGGER => Some(V::F32(self.trigger_level)),
            _ => None,
        }
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        match (slot, value) {
            (ROW_MODE, V::Usize(v)) => self.mode = v,
            (ROW_SOURCE, V::Usize(v)) => self.source = v,
            (ROW_CHANNEL, V::Usize(v)) => self.channel = v,
            (ROW_MODBUS_CH, V::Usize(v)) => self.modbus_channel = v,
            (ROW_ZOOM, V::F32(v)) => self.zoom = v,
            (ROW_GAIN, V::F32(v)) => self.gain = v,
            (ROW_TRIGGER, V::F32(v)) => self.trigger_level = v,
            _ => {}
        }
    }
}

fn scope_thread(
    state: Arc<Mutex<ScopeState>>,
    shutdown: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    // /los_mix_in exists solely for the scope: the mixer writes its summed
    // output there and we are the one consumer, so destructive read() gives
    // a contiguous 48kHz stream (peek_latest + decimation drew aliased
    // garbage). The mixer creates it up to 500ms after we spawn — keep
    // retrying instead of giving up at startup.
    let mut ringbuf: Option<AudioRingbuf> = None;
    let modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();
    let mut local_buffer = vec![0.0f32; 128];

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        if ringbuf.is_none() {
            ringbuf = AudioRingbuf::open(SHM_NAME).ok();
            if let Some(ref rb) = ringbuf {
                local_buffer = vec![0.0f32; rb.slot_len()];
            }
        }

        let mut s = state.lock().unwrap();
        let source = s.source;
        let gain = s.gain;

        if source == 0 {
            if let Some(ref mut rb) = ringbuf {
                // drain everything written since last tick (contiguous)
                while let Ok(true) = rb.read(&mut local_buffer) {
                    let channel = s.channel;
                    for frame in local_buffer.chunks_exact(2) {
                        let sample = match channel {
                            0 => frame[0],
                            1 => frame[1],
                            _ => (frame[0] + frame[1]) / 2.0,
                        };
                        s.buffer.push(sample * gain);
                    }
                }
            }
        } else if let Some(ref bus) = modbus {
            let ch = s.modbus_channel;
            let sample = bus.get(ch) * gain;
            s.buffer.push(sample);
        }

        let len = s.buffer.len();
        if len > BUFFER_SIZE {
            s.buffer.drain(..len - BUFFER_SIZE);
        }

        drop(s);
        std::thread::sleep(Duration::from_millis(16));
    }

    Ok(())
}

/// Start index of the display window: the earliest rising crossing of
/// `level` that still leaves a full `window` of samples after it. None =
/// no trigger found (free-run: show the latest window).
fn trigger_window(buf: &[f32], level: f32, window: usize) -> Option<usize> {
    if buf.len() < window + 1 {
        return None;
    }
    (1..=buf.len() - window).find(|&i| buf[i - 1] < level && buf[i] >= level)
}

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &ScopeState,
    show_help: bool,
    overlay: Option<&str>,
    picker: Option<(Vec<String>, usize)>,
    show_menu: bool,
) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        
        // the scope is the picture: chrome auto-hides when you're not
        // touching it, leaving a full-bleed waveform
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(if show_menu {
                [Constraint::Min(0), Constraint::Length(1)]
            } else {
                [Constraint::Min(0), Constraint::Length(0)]
            })
            .split(area);

        // Param list as one status line, theme anatomy: selected row inverse
        use ratatui::text::Span;
        let status_widget = match overlay {
            Some(text) => Paragraph::new(Span::styled(text.to_string(), crate::theme::value())),
            None => {
                let mut spans: Vec<Span> = vec![Span::styled("SCOPE ", crate::theme::chrome_hi())];
                for row in 0..NUM_ROWS {
                    let text = row_display(state, row);
                    let style = if row == state.selected {
                        crate::theme::selected()
                    } else {
                        crate::theme::chrome()
                    };
                    spans.push(Span::styled(text, style));
                    spans.push(Span::raw(" "));
                }
                Paragraph::new(ratatui::text::Line::from(spans))
            }
        };
        f.render_widget(status_widget, chunks[1]);

        // Trigger-synced display: window length set by zoom, aligned to the
        // first rising crossing of the trigger level (free-run when none).
        let window = ((state.buffer.len() as f32 / state.zoom) as usize)
            .clamp(16, state.buffer.len().max(16))
            .min(state.buffer.len().max(1));
        let start = trigger_window(&state.buffer, state.trigger_level, window)
            .unwrap_or_else(|| state.buffer.len().saturating_sub(window));
        let visible = &state.buffer[start..(start + window).min(state.buffer.len())];
        let data: Vec<(f64, f64)> = visible
            .iter()
            .enumerate()
            .map(|(i, &s)| (i as f64, s as f64))
            .collect();

        let marker = match state.mode {
            0 => Marker::Braille,
            1 => Marker::HalfBlock,
            2 => Marker::Block,
            _ => Marker::Dot,
        };

        let x_max = visible.len().max(1) as f64;

        let datasets = vec![Dataset::default()
            .marker(marker)
            .graph_type(GraphType::Line)
            .style(crate::theme::signal(crate::theme::audio()))
            .data(&data)];

        let chart = Chart::new(datasets)
            .x_axis(
                Axis::default()
                    .bounds([0.0, x_max])
                    .labels(vec![
                        ratatui::text::Line::from("0"),
                        ratatui::text::Line::from(format!("{:.0}", x_max)),
                    ]),
            )
            .y_axis(
                Axis::default()
                    .bounds([-1.0, 1.0])
                    .labels(vec![
                        ratatui::text::Line::from("-1"),
                        ratatui::text::Line::from("0"),
                        ratatui::text::Line::from("1"),
                    ]),
            );

        f.render_widget(chart, chunks[0]);

        // Help overlay
        if show_help {
            let help_text = vec![
                Line::from("━━━ Scope Help ━━━"),
                Line::from(""),
                Line::from("Params (j/k select, h/l adjust):"),
                Line::from("  j/k        Select param"),
                Line::from("  h/l        Adjust selected param"),
                Line::from("  H/L        Coarse adjust (10x)"),
                Line::from("  #h/#l ...  Count prefix repeats"),
                Line::from("  gg / G     First / last param"),
                Line::from(""),
                Line::from("Params: Mode, Src (audio/modbus),"),
                Line::from("  Ch (L/R/Stereo), ModCh, Zoom,"),
                Line::from("  Gain, Trig"),
                Line::from(""),
                Line::from("  u / ^r     Undo / redo (counts; sweeps coalesce)"),
                Line::from("  :w/:e/:q   Patch save/load, quit (:x save+quit)"),
                Line::from("  space      Play/pause (global)"),
                Line::from("  ?          Toggle this help"),
                Line::from("  Close pane: tmux prefix + x"),
            ];
            let help = Paragraph::new(help_text)
                .style(Style::default().fg(Color::White).bg(Color::Black))
                .block(Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title("Help"));
            f.render_widget(help, area);
        }

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
                    .title("View source (Enter selects, Esc cancels)"),
            );
            f.render_widget(list, r);
        }
    })?;

    Ok(())
}

pub fn run(instance: usize) -> Result<()> {
    // Initialize terminal with retry logic (handles tmux PTY race)
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("scope", instance);
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let _ = manifest.register("scope", instance, None, 0);
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

    let state = Arc::new(Mutex::new(ScopeState::default()));
    
    // Load saved state if available
    if let Ok(params) = state::load_module_state::<state::ScopeParams>("scope", instance) {
        apply_params(&mut state.lock().unwrap(), &params);
    }
    
    let state_clone = Arc::clone(&state);

    let (_tx, rx) = std::sync::mpsc::channel();

    let _scope_handle = std::thread::spawn(move || {
        if let Err(e) = scope_thread(state_clone, rx) {
            eprintln!("Scope thread error: {}", e);
        }
    });

    let mut show_help = false;
    let mut count = crate::keys::Count::default();
    let mut pending_g = false;
    let mut picker = crate::picker::Picker::default();
    let mut history = crate::undo::ParamHistory::default();
    let mut ex = crate::excmd::ExLine::default();
    // param strip auto-hides ~4s after the last interaction
    let mut menu_until = std::time::Instant::now() + Duration::from_secs(4);
    let mut ex_msg: Option<String> = None;
    let mut patch_name: Option<String> = None;
    let mut baseline = state::to_toml_string(&snapshot_params(&state.lock().unwrap())).unwrap_or_default();
    let mut should_quit = false;
    // Global transport handle for Space = play/pause (lazily reopened)
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();

    loop {
        // Check for save-on-signal
        if state::check_save_signal() {
            let params = snapshot_params(&state.lock().unwrap());
            let _ = state::save_module_state("scope", instance, &params);
        }
        
        // Check for reload-on-signal
        if state::check_reload_signal() {
            if let Ok(params) = state::load_module_state::<state::ScopeParams>("scope", instance) {
                apply_params(&mut state.lock().unwrap(), &params);
            }
        }
        
        {
            // refresh the live label for the selected modbus channel
            let entries = manifest.entries();
            let mut s = state.lock().unwrap();
            s.modbus_label = crate::routing::label_for_channel(&entries, s.modbus_channel)
                .map(|a| a.to_string());
        }
        let current_state = state.lock().unwrap().clone();
        let overlay = if ex.is_active() {
            Some(ex.display())
        } else {
            ex_msg.clone()
        };
        let picker_rows = if picker.is_active() { Some(picker.rows()) } else { None };
        let show_menu = std::time::Instant::now() < menu_until || overlay.is_some();
        draw_ui(&mut terminal, &current_state, show_help, overlay.as_deref(), picker_rows, show_menu)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                ex_msg = None;
                if picker.is_active() {
                    if let crate::picker::PickerEvent::Chosen(Some(addr)) = picker.handle_key(key.code) {
                        let entries = manifest.entries();
                        if let Some(ch) = crate::routing::resolve(&entries, &addr) {
                            use crate::undo::{ParamUndo, ParamValue};
                            let mut s = state.lock().unwrap();
                            let old = s.get_param(ROW_MODBUS_CH);
                            s.modbus_channel = ch;
                            if let Some(old) = old {
                                history.record(ROW_MODBUS_CH, "View source", old, ParamValue::Usize(ch));
                            }
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
                            ExCommand::Edit(name) => match state::load_patch::<state::ScopeParams>(&name) {
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
                            ExCommand::Set(k, _) => ex_msg = Some(format!("Unknown setting: {}", k)),
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
                // any interaction reveals the param strip for a few seconds
                if !matches!(key.code, KeyCode::Char(' ')) {
                    menu_until = std::time::Instant::now() + Duration::from_secs(4);
                }
                // Ctrl-s: save module state
                if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
                    let n = count.take();
                    let mut s = state.lock().unwrap();
                    ex_msg = Some(crate::undo::history_status("Redo", n, || history.redo(&mut *s)));
                    continue;
                }
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let params = snapshot_params(&state.lock().unwrap());
                    let _ = state::save_module_state("scope", 0, &params);
                    continue;
                }
                match key.code {
                    KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
                    KeyCode::Char('j') | KeyCode::Down => {
                        let n = count.take() as i32;
                        let mut s = state.lock().unwrap();
                        s.selected = crate::keys::cycle(s.selected, n, NUM_ROWS);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        let n = count.take() as i32;
                        let mut s = state.lock().unwrap();
                        s.selected = crate::keys::cycle(s.selected, -n, NUM_ROWS);
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
                        let slot = s.selected;
                        let old = s.get_param(slot);
                        adjust(&mut s, steps, coarse);
                        let new = s.get_param(slot);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(slot, "Adjust", old, new);
                        }
                    }
                    KeyCode::Char('g') => {
                        count.clear();
                        if pending_g {
                            pending_g = false;
                            state.lock().unwrap().selected = 0;
                        } else {
                            pending_g = true;
                            continue;
                        }
                    }
                    KeyCode::Char('G') => {
                        count.clear();
                        state.lock().unwrap().selected = NUM_ROWS - 1;
                    }
                    KeyCode::Char(' ') => {
                        if transport_ui.is_none() {
                            transport_ui = ShmTransport::open().ok();
                        }
                        if let Some(ref mut t) = transport_ui {
                            t.toggle_playing();
                        }
                    }
                    KeyCode::Char('u') => {
                        let n = count.take();
                        let mut s = state.lock().unwrap();
                        ex_msg = Some(crate::undo::history_status("Undo", n, || history.undo(&mut *s)));
                    }
                    KeyCode::Char('@') => {
                        count.clear();
                        let mut s = state.lock().unwrap();
                        if s.selected == ROW_MODBUS_CH || s.selected == ROW_SOURCE {
                            s.source = 1; // switch to modbus viewing
                            let entries = manifest.entries();
                            let current = crate::routing::label_for_channel(&entries, s.modbus_channel);
                            let sources = crate::routing::live_sources(&entries);
                            drop(s);
                            picker.open(sources, current.as_ref());
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

    #[test]
    fn adjust_cycles_enum_rows() {
        let mut s = ScopeState { selected: ROW_MODE, ..Default::default() };
        adjust(&mut s, 1, false);
        assert_eq!(s.mode, 1);
        adjust(&mut s, -2, false);
        assert_eq!(s.mode, 3, "mode wraps backward");
        s.selected = ROW_SOURCE;
        adjust(&mut s, 1, false);
        assert_eq!(s.source, 1);
        adjust(&mut s, 1, false);
        assert_eq!(s.source, 0, "source wraps");
        s.selected = ROW_MODBUS_CH;
        adjust(&mut s, -1, false);
        assert_eq!(s.modbus_channel, 31, "modbus channel wraps backward");
    }

    #[test]
    fn adjust_clamps_continuous_rows() {
        let mut s = ScopeState { selected: ROW_ZOOM, ..Default::default() };
        adjust(&mut s, -100, false);
        assert_eq!(s.zoom, 0.5);
        adjust(&mut s, 100, false);
        assert_eq!(s.zoom, 10.0);
        s.selected = ROW_TRIGGER;
        adjust(&mut s, 100, true);
        assert_eq!(s.trigger_level, 1.0);
    }

    #[test]
    fn coarse_is_ten_times_fine() {
        let mut fine = ScopeState { selected: ROW_GAIN, ..Default::default() };
        adjust(&mut fine, 1, false);
        let mut coarse = ScopeState { selected: ROW_GAIN, ..Default::default() };
        adjust(&mut coarse, 1, true);
        assert!((coarse.gain - 1.0) > (fine.gain - 1.0) * 9.9);
    }

    #[test]
    fn counted_adjust_equals_repeated() {
        let mut a = ScopeState { selected: ROW_ZOOM, ..Default::default() };
        adjust(&mut a, 5, false);
        let mut b = ScopeState { selected: ROW_ZOOM, ..Default::default() };
        for _ in 0..5 {
            adjust(&mut b, 1, false);
        }
        assert!((a.zoom - b.zoom).abs() < 1e-6);
    }

    #[test]
    fn trigger_finds_rising_crossing_with_room() {
        // ramp down then a clean rising edge at index 5
        let buf = [0.5, 0.3, 0.1, -0.2, -0.4, 0.1, 0.4, 0.6, 0.7, 0.8];
        assert_eq!(trigger_window(&buf, 0.0, 4), Some(5));
        // window too large to fit after the crossing -> free-run
        assert_eq!(trigger_window(&buf, 0.0, 10), None);
        // no crossing of a high level
        assert_eq!(trigger_window(&buf, 0.95, 2), None);
    }

    #[test]
    fn trigger_picks_earliest_fitting_crossing() {
        let buf = [-1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0];
        assert_eq!(trigger_window(&buf, 0.0, 3), Some(1), "earliest crossing wins");
        assert_eq!(trigger_window(&buf, 0.0, 7), Some(1));
    }

    #[test]
    fn scope_params_persist_source_and_trigger() {
        let s = ScopeState {
            source: 1,
            modbus_channel: 7,
            trigger_level: 0.25,
            ..Default::default()
        };
        let toml_str = state::to_toml_string(&snapshot_params(&s)).unwrap();
        let parsed: state::ScopeParams = toml::from_str(&toml_str).unwrap();
        let mut back = ScopeState::default();
        apply_params(&mut back, &parsed);
        assert_eq!(back.source, 1);
        assert_eq!(back.modbus_channel, 7);
        assert_eq!(back.trigger_level, 0.25);
    }
}
