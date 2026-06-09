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

const BUFFER_SIZE: usize = 512;
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
        ROW_SOURCE => format!("Src:{}", if s.source == 0 { "Audio" } else { "Mod" }),
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
    }
}

fn apply_params(s: &mut ScopeState, params: &state::ScopeParams) {
    if let Some(v) = params.mode { s.mode = v; }
    if let Some(v) = params.channel { s.channel = v; }
    if let Some(v) = params.zoom { s.zoom = v; }
    if let Some(v) = params.gain { s.gain = v; }
}

fn scope_thread(
    state: Arc<Mutex<ScopeState>>,
    shutdown: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let ringbuf = AudioRingbuf::open(SHM_NAME).ok();
    let modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();

    let slot_len = ringbuf.as_ref().map(|r| r.slot_len()).unwrap_or(128);
    let mut local_buffer = vec![0.0f32; slot_len];

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        let mut s = state.lock().unwrap();
        let source = s.source;
        let gain = s.gain;

        if source == 0 {
            if let Some(ref rb) = ringbuf {
                if let Ok(true) = rb.peek_latest(&mut local_buffer) {
                    let channel = s.channel;
                    for i in (0..slot_len).step_by(2) {
                        let sample = match channel {
                            0 => local_buffer[i],
                            1 => local_buffer[i + 1],
                            _ => (local_buffer[i] + local_buffer[i + 1]) / 2.0,
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

        while s.buffer.len() > BUFFER_SIZE {
            s.buffer.remove(0);
        }

        drop(s);
        std::thread::sleep(Duration::from_millis(16));
    }

    Ok(())
}

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &ScopeState,
    show_help: bool,
    overlay: Option<&str>,
    picker: Option<(Vec<String>, usize)>,
) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        // Param list rendered as one status line; the selected row is
        // bracketed (j/k select, h/l adjust, H/L coarse)
        let status = (0..NUM_ROWS)
            .map(|row| {
                let text = row_display(state, row);
                if row == state.selected {
                    format!("[{}]", text)
                } else {
                    format!(" {} ", text)
                }
            })
            .collect::<Vec<_>>()
            .join("|");
        let status_widget = match overlay {
            Some(text) => Paragraph::new(text.to_string()).style(Style::default().fg(Color::Yellow)),
            None => Paragraph::new(status).style(Style::default().fg(Color::Cyan)),
        };
        f.render_widget(status_widget, chunks[1]);

        let data: Vec<(f64, f64)> = state
            .buffer
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

        let x_max = (state.buffer.len() as f64 / state.zoom as f64).min(state.buffer.len() as f64);

        let datasets = vec![Dataset::default()
            .marker(marker)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Green))
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
    let mut ex = crate::excmd::ExLine::default();
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
        draw_ui(&mut terminal, &current_state, show_help, overlay.as_deref(), picker_rows)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                ex_msg = None;
                if picker.is_active() {
                    if let crate::picker::PickerEvent::Chosen(Some(addr)) = picker.handle_key(key.code) {
                        let entries = manifest.entries();
                        if let Some(ch) = crate::routing::resolve(&entries, &addr) {
                            state.lock().unwrap().modbus_channel = ch;
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
                // Ctrl-s: save module state
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
                    KeyCode::Char('h') | KeyCode::Left => {
                        let n = count.take() as i32;
                        adjust(&mut state.lock().unwrap(), -n, false);
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        let n = count.take() as i32;
                        adjust(&mut state.lock().unwrap(), n, false);
                    }
                    KeyCode::Char('H') => {
                        let n = count.take() as i32;
                        adjust(&mut state.lock().unwrap(), -n, true);
                    }
                    KeyCode::Char('L') => {
                        let n = count.take() as i32;
                        adjust(&mut state.lock().unwrap(), n, true);
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
}
