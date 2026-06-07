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
    text::Line,
    widgets::{Block, Borders, Gauge, Paragraph},
    Terminal,
};

use crate::shm::{AudioRingbuf, EventRingbuf, ModulationBus, ShmTransport};
use crate::state;

#[derive(Clone, Copy)]
struct VoiceState {
    shape: f32,
    sub: f32,
    fm: f32,
    output: u8,
    freq: f32,
    gate: bool,
    level: f32,
    velocity: f32, // 0.0-1.0 from last note_on
}

impl Default for VoiceState {
    fn default() -> Self {
        Self {
            shape: 0.5,
            sub: 0.0,
            fm: 0.0,
            output: 0,
            freq: 440.0,
            gate: false,
            level: 0.0,
            velocity: 0.0,
        }
    }
}

fn voice_thread(
    state: Arc<Mutex<VoiceState>>,
    shutdown: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let mut ringbuf = AudioRingbuf::open("/los_mix_in")
        .or_else(|_| AudioRingbuf::create("/los_mix_in"))?;

    let mut events = EventRingbuf::open(0).ok();
    let mut modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();

    let _transport = ShmTransport::open()
        .or_else(|_| ShmTransport::create(48000))?;

    let mut phase = 0.0f64;
    let mut sub_phase = 0.0f64;

    let sample_rate = 48000.0;
    let block_size = 64;

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        // Reconnect to shared resources if disconnected
        if events.is_none() {
            events = EventRingbuf::open(0).ok();
        }
        if modbus.is_none() {
            modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();
        }

        // Read events (note_on sets pitch + velocity, note_off sets gate=false)
        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                let mut s = state.lock().unwrap();
                match event.event_type {
                    0 => { // Note on
                        s.freq = event.value; // frequency from note
                        s.velocity = event.param as f32 / 127.0;
                        s.gate = true;
                    }
                    1 => { // Note off
                        s.gate = false;
                        // velocity stays as last value for release tail
                    }
                    _ => {}
                }
            }
        }

        // Generate audio
        let s = state.lock().unwrap();
        let freq = s.freq as f64;
        let output_mode = s.output;
        let velocity = s.velocity;

        // Read modulation from bus
        // ch0  = envelope ch1 output (primary amplitude control)
        // ch12 = param mod: voice shape
        // ch13 = param mod: voice sub
        // ch14 = param mod: voice fm
        let envelope_level = modbus.as_ref().map(|m| m.get(0)).unwrap_or(0.0);
        let mod_shape = modbus.as_ref().map(|m| m.get(12)).unwrap_or(0.0);
        let mod_sub = modbus.as_ref().map(|m| m.get(13)).unwrap_or(0.0);
        let mod_fm = modbus.as_ref().map(|m| m.get(14)).unwrap_or(0.0);

        let shape = (s.shape + mod_shape).clamp(0.0, 1.0);
        let sub_mix = (s.sub + mod_sub).clamp(0.0, 1.0);
        let fm_amount = (s.fm + mod_fm).clamp(0.0, 1.0);

        // Final amplitude: envelope × velocity
        // envelope_level comes from envelope module (modbus ch0)
        // velocity comes from sequencer step (0.0-1.0)
        let level = envelope_level * velocity;

        let mut block = vec![0.0f32; block_size * 2];

        for i in 0..block_size {
            // FM
            let fm_mod = (phase * fm_amount as f64 * 2.0 * std::f64::consts::PI).sin() * 0.1;

            // Main oscillator with shape morphing
            let main_phase = (phase + fm_mod).fract();
            let sine = (main_phase * 2.0 * std::f64::consts::PI).sin() as f32;
            let saw = (main_phase * 2.0 - 1.0) as f32;
            let square = if main_phase < 0.5 { 1.0f32 } else { -1.0f32 };

            let main = if shape < 0.5 {
                sine * (1.0 - shape * 2.0) + saw * (shape * 2.0)
            } else {
                saw * (1.0 - (shape - 0.5) * 2.0) + square * ((shape - 0.5) * 2.0)
            };

            // Sub oscillator (square, one octave down)
            let sub = if sub_phase < 0.5 { 1.0f32 } else { -1.0f32 };

            // Mix
            let sample = match output_mode {
                0 => main,
                1 => main * (1.0 - sub_mix) + sub * sub_mix,
                _ => main * (1.0 - sub_mix) + sub * sub_mix * 0.5,
            };

            let output = sample * level * 0.5;
            block[i * 2] = output;
            block[i * 2 + 1] = output;

            phase = (phase + freq / sample_rate).fract();
            sub_phase = (sub_phase + freq / (sample_rate * 2.0)).fract();
        }

        drop(s);

        // Update level meter for TUI
        {
            let mut s = state.lock().unwrap();
            s.level = level;
        }

        // Write to ringbuffer — retry when full, don't drop blocks
        loop {
            match ringbuf.write(&block) {
                Ok(()) => break,
                Err(_) => {
                    std::thread::yield_now();
                }
            }
        }
    }

    Ok(())
}

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &VoiceState,
    selected: usize,
    show_help: bool,
) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),  // Shape
                Constraint::Length(1),  // Sub
                Constraint::Length(1),  // FM
                Constraint::Length(1),  // Output
                Constraint::Length(1),  // Level meter
                Constraint::Min(0),
                Constraint::Length(1),  // Status
            ])
            .split(area);

        // Status
        let gate_str = if state.gate { "●" } else { "○" };
        let status = format!(
            "{} {:.1} Hz | Output: {} | Env: {:.0}% | Vel: {:.0}% | Level: {:.0}%",
            gate_str,
            state.freq,
            match state.output { 0 => "Main", 1 => "Main+Sub", _ => "Mix" },
            state.level / state.velocity.max(0.001) * 100.0, // env = level / velocity
            state.velocity * 100.0,
            state.level * 100.0
        );
        let status_widget = Paragraph::new(status).style(Style::default().fg(Color::Cyan));
        f.render_widget(status_widget, chunks[6]);

        // Parameters
        let params = [
            ("Shape", state.shape, selected == 0),
            ("Sub", state.sub, selected == 1),
            ("FM", state.fm, selected == 2),
        ];

        for (i, (name, value, is_selected)) in params.iter().enumerate() {
            let style = if *is_selected {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::White)
            };

            let gauge = Gauge::default()
                .gauge_style(style)
                .ratio(*value as f64)
                .label(format!("{}: {:.2}", name, value));
            f.render_widget(gauge, chunks[i]);
        }

        // Output mode
        let output_style = if selected == 3 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };
        let output_text = format!(
            "Output: [{}] Main  [{}] Main+Sub  [{}] Mix",
            if state.output == 0 { "●" } else { "○" },
            if state.output == 1 { "●" } else { "○" },
            if state.output == 2 { "●" } else { "○" },
        );
        let output_widget = Paragraph::new(output_text).style(output_style);
        f.render_widget(output_widget, chunks[3]);

        // Level meter
        let level_gauge = Gauge::default()
            .gauge_style(Style::default().fg(Color::Green))
            .ratio(state.level as f64)
            .label(format!("Level: {:.0}%", state.level * 100.0));
        f.render_widget(level_gauge, chunks[4]);

        // Help overlay
        if show_help {
            let help_text = vec![
                Line::from("━━━ Voice Help ━━━"),
                Line::from(""),
                Line::from("Parameters:"),
                Line::from("  j/k, ↑/↓  Select parameter"),
                Line::from("  h/l, ←/→  Adjust value"),
                Line::from(""),
                Line::from("Output modes:"),
                Line::from("  1          Main (sine/saw/square)"),
                Line::from("  2          Main + Sub"),
                Line::from("  3          Mix"),
                Line::from(""),
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
    state::write_pid_file("voice", instance);
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

    let state = Arc::new(Mutex::new(VoiceState::default()));
    
    // Load saved state if available
    if let Ok(params) = state::load_module_state::<state::VoiceParams>("voice", instance) {
        let mut s = state.lock().unwrap();
        if let Some(v) = params.shape { s.shape = v; }
        if let Some(v) = params.sub { s.sub = v; }
        if let Some(v) = params.fm { s.fm = v; }
        if let Some(v) = params.output { s.output = v; }
        if let Some(v) = params.freq { s.freq = v; }
        if let Some(v) = params.gate { s.gate = v; }
        if let Some(v) = params.level { s.level = v; }
    }
    
    let state_clone = Arc::clone(&state);

    let (_tx, rx) = std::sync::mpsc::channel();

    let _voice_handle = std::thread::spawn(move || {
        if let Err(e) = voice_thread(state_clone, rx) {
            eprintln!("Voice thread error: {}", e);
        }
    });

    let mut selected = 0usize;
    let mut show_help = false;

    loop {
        // Check for save-on-signal
        if state::check_save_signal() {
            let s = state.lock().unwrap();
            let params = state::VoiceParams {
                shape: Some(s.shape),
                sub: Some(s.sub),
                fm: Some(s.fm),
                output: Some(s.output),
                freq: Some(s.freq),
                gate: Some(s.gate),
                level: Some(s.level),
            };
            drop(s);
            let _ = state::save_module_state("voice", 0, &params);
        }
        
        // Check for reload-on-signal
        if state::check_reload_signal() {
            if let Ok(params) = state::load_module_state::<state::VoiceParams>("voice", instance) {
                let mut s = state.lock().unwrap();
                if let Some(v) = params.shape { s.shape = v; }
                if let Some(v) = params.sub { s.sub = v; }
                if let Some(v) = params.fm { s.fm = v; }
                if let Some(v) = params.output { s.output = v; }
                if let Some(v) = params.freq { s.freq = v; }
                if let Some(v) = params.gate { s.gate = v; }
                if let Some(v) = params.level { s.level = v; }
            }
        }
        
        let current_state = *state.lock().unwrap();
        draw_ui(&mut terminal, &current_state, selected, show_help)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Ctrl-s: save module state
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let s = state.lock().unwrap();
                    let params = state::VoiceParams {
                        shape: Some(s.shape),
                        sub: Some(s.sub),
                        fm: Some(s.fm),
                        output: Some(s.output),
                        freq: Some(s.freq),
                        gate: Some(s.gate),
                        level: Some(s.level),
                    };
                    drop(s);
                    let _ = state::save_module_state("voice", 0, &params);
                    continue;
                }
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        selected = (selected + 1) % 4;
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        selected = if selected == 0 { 3 } else { selected - 1 };
                    }
                    KeyCode::Char('h') | KeyCode::Left => {
                        let mut s = state.lock().unwrap();
                        match selected {
                            0 => s.shape = (s.shape - 0.05).max(0.0),
                            1 => s.sub = (s.sub - 0.05).max(0.0),
                            2 => s.fm = (s.fm - 0.05).max(0.0),
                            3 => s.output = if s.output == 0 { 2 } else { s.output - 1 },
                            _ => {}
                        }
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        let mut s = state.lock().unwrap();
                        match selected {
                            0 => s.shape = (s.shape + 0.05).min(1.0),
                            1 => s.sub = (s.sub + 0.05).min(1.0),
                            2 => s.fm = (s.fm + 0.05).min(1.0),
                            3 => s.output = (s.output + 1) % 3,
                            _ => {}
                        }
                    }
                    KeyCode::Char('1') => {
                        let mut s = state.lock().unwrap();
                        s.output = 0;
                    }
                    KeyCode::Char('2') => {
                        let mut s = state.lock().unwrap();
                        s.output = 1;
                    }
                    KeyCode::Char('3') => {
                        let mut s = state.lock().unwrap();
                        s.output = 2;
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
