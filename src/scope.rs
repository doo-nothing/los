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
        }
    }
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

        let mode_str = match state.mode {
            0 => "Braille",
            1 => "HalfBlock",
            2 => "Bars",
            _ => "Dots",
        };
        let channel_str = match state.channel {
            0 => "L",
            1 => "R",
            _ => "S",
        };
        let source_str = if state.source == 0 { "Audio" } else { "Mod" };
        let status = format!(
            "Src: {} | Mode: {} | Ch: {} | Zoom: {:.1}x | Gain: {:.1}x | Trig: {:.2}",
            source_str, mode_str, channel_str, state.zoom, state.gain, state.trigger_level
        );
        let status_widget = Paragraph::new(status).style(Style::default().fg(Color::Cyan));
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
                Line::from("Display:"),
                Line::from("  m          Cycle render mode"),
                Line::from("             (Braille/HalfBlock/Bars/Dots)"),
                Line::from("  c          Cycle channel (L/R/Stereo)"),
                Line::from(""),
                Line::from("Source:"),
                Line::from("  b          Toggle audio/modbus source"),
                Line::from("  n/N        Next/prev modbus channel"),
                Line::from(""),
                Line::from("Controls:"),
                Line::from("  +/-        Zoom in/out"),
                Line::from("  g/G        Increase/decrease gain"),
                Line::from("  t/T        Trigger level"),
                Line::from(""),
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
    })?;

    Ok(())
}

pub fn run(instance: usize) -> Result<()> {
    // Initialize terminal with retry logic (handles tmux PTY race)
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("scope", instance);
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let _ = manifest.register("scope", instance, None);
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
        let mut s = state.lock().unwrap();
        if let Some(v) = params.mode { s.mode = v; }
        if let Some(v) = params.channel { s.channel = v; }
        if let Some(v) = params.zoom { s.zoom = v; }
        if let Some(v) = params.gain { s.gain = v; }
    }
    
    let state_clone = Arc::clone(&state);

    let (_tx, rx) = std::sync::mpsc::channel();

    let _scope_handle = std::thread::spawn(move || {
        if let Err(e) = scope_thread(state_clone, rx) {
            eprintln!("Scope thread error: {}", e);
        }
    });

    let mut show_help = false;
    // Global transport handle for Space = play/pause (lazily reopened)
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();

    loop {
        // Check for save-on-signal
        if state::check_save_signal() {
            let s = state.lock().unwrap();
            let params = state::ScopeParams {
                mode: Some(s.mode),
                channel: Some(s.channel),
                zoom: Some(s.zoom),
                gain: Some(s.gain),
            };
            drop(s);
            let _ = state::save_module_state("scope", instance, &params);
        }
        
        // Check for reload-on-signal
        if state::check_reload_signal() {
            if let Ok(params) = state::load_module_state::<state::ScopeParams>("scope", instance) {
                let mut s = state.lock().unwrap();
                if let Some(v) = params.mode { s.mode = v; }
                if let Some(v) = params.channel { s.channel = v; }
                if let Some(v) = params.zoom { s.zoom = v; }
                if let Some(v) = params.gain { s.gain = v; }
            }
        }
        
        let current_state = state.lock().unwrap().clone();
        draw_ui(&mut terminal, &current_state, show_help)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Ctrl-s: save module state
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let s = state.lock().unwrap();
                    let params = state::ScopeParams {
                        mode: Some(s.mode),
                        channel: Some(s.channel),
                        zoom: Some(s.zoom),
                        gain: Some(s.gain),
                    };
                    drop(s);
                    let _ = state::save_module_state("scope", 0, &params);
                    continue;
                }
                match key.code {
                    KeyCode::Char('m') => {
                        let mut s = state.lock().unwrap();
                        s.mode = (s.mode + 1) % 4;
                    }
                    KeyCode::Char('c') => {
                        let mut s = state.lock().unwrap();
                        s.channel = (s.channel + 1) % 3;
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        let mut s = state.lock().unwrap();
                        s.zoom = (s.zoom + 0.5).min(10.0);
                    }
                    KeyCode::Char('-') => {
                        let mut s = state.lock().unwrap();
                        s.zoom = (s.zoom - 0.5).max(0.5);
                    }
                    KeyCode::Char('g') => {
                        let mut s = state.lock().unwrap();
                        s.gain = (s.gain + 0.5).min(10.0);
                    }
                    KeyCode::Char('G') => {
                        let mut s = state.lock().unwrap();
                        s.gain = (s.gain - 0.5).max(0.1);
                    }
                    KeyCode::Char('t') => {
                        let mut s = state.lock().unwrap();
                        s.trigger_level = (s.trigger_level + 0.1).min(1.0);
                    }
                    KeyCode::Char('T') => {
                        let mut s = state.lock().unwrap();
                        s.trigger_level = (s.trigger_level - 0.1).max(-1.0);
                    }
                    KeyCode::Char('b') => {
                        let mut s = state.lock().unwrap();
                        s.source = (s.source + 1) % 2;
                    }
                    KeyCode::Char('n') => {
                        let mut s = state.lock().unwrap();
                        s.modbus_channel = (s.modbus_channel + 1) % 32;
                    }
                    KeyCode::Char('N') => {
                        let mut s = state.lock().unwrap();
                        s.modbus_channel = s.modbus_channel.saturating_sub(1);
                    }
                    KeyCode::Char(' ') => {
                        if transport_ui.is_none() {
                            transport_ui = ShmTransport::open().ok();
                        }
                        if let Some(ref mut t) = transport_ui {
                            t.toggle_playing();
                        }
                    }
                    KeyCode::Char('?') => {
                        show_help = !show_help;
                    }
                    _ => {}
                }
            }
        }
    }
}
