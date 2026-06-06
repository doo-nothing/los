use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{canvas::Canvas, Block, Borders, Paragraph},
    Terminal,
};

use crate::shm::AudioRingbuf;

#[derive(Clone, Copy, PartialEq)]
enum RenderMode {
    Braille,
    HalfBlock,
    Bars,
    Dots,
}

impl RenderMode {
    fn name(&self) -> &'static str {
        match self {
            RenderMode::Braille => "Braille",
            RenderMode::HalfBlock => "HalfBlock",
            RenderMode::Bars => "Bars",
            RenderMode::Dots => "Dots",
        }
    }

    fn next(&self) -> Self {
        match self {
            RenderMode::Braille => RenderMode::HalfBlock,
            RenderMode::HalfBlock => RenderMode::Bars,
            RenderMode::Bars => RenderMode::Dots,
            RenderMode::Dots => RenderMode::Braille,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ChannelMode {
    Left,
    Right,
    Stereo,
}

impl ChannelMode {
    fn name(&self) -> &'static str {
        match self {
            ChannelMode::Left => "Left",
            ChannelMode::Right => "Right",
            ChannelMode::Stereo => "Stereo",
        }
    }

    fn next(&self) -> Self {
        match self {
            ChannelMode::Left => ChannelMode::Right,
            ChannelMode::Right => ChannelMode::Stereo,
            ChannelMode::Stereo => ChannelMode::Left,
        }
    }
}

#[derive(Clone)]
struct ScopeState {
    samples: Vec<f32>,
    channels: usize,
    mode: RenderMode,
    channel_mode: ChannelMode,
    zoom: f32,
    gain: f32,
}

impl Default for ScopeState {
    fn default() -> Self {
        Self {
            samples: vec![0.0; 1024],
            channels: 2,
            mode: RenderMode::Braille,
            channel_mode: ChannelMode::Stereo,
            zoom: 1.0,
            gain: 1.0,
        }
    }
}

fn scope_thread(
    state: Arc<Mutex<ScopeState>>,
    shutdown: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let ringbuf = match AudioRingbuf::open("/los_mix_in") {
        Ok(rb) => rb,
        Err(_) => AudioRingbuf::create("/los_mix_in")
            .map_err(|e| anyhow::anyhow!("creating audio ringbuffer: {}", e))?,
    };

    let channels = ringbuf.channels() as usize;
    let slot_len = ringbuf.slot_len();
    let mut slot = vec![0.0f32; slot_len];

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        if ringbuf.peek_latest(&mut slot).unwrap_or(false) {
            let mut s = state.lock().unwrap();
            s.samples = slot.clone();
            s.channels = channels;
        }

        std::thread::sleep(Duration::from_millis(16)); // ~60 FPS
    }

    Ok(())
}

fn draw_braille(f: &mut ratatui::Frame, area: ratatui::layout::Rect, samples: &[f32], channels: usize, channel_mode: ChannelMode, zoom: f32, gain: f32) {
    let canvas = Canvas::default()
        .block(Block::default().borders(Borders::ALL).title("Waveform (Braille)"))
        .x_bounds([-1.0, 1.0])
        .y_bounds([-1.0, 1.0])
        .paint(|ctx| {
            let n = samples.len() / channels;
            let step = (n as f32 * zoom).min(n as f32) / area.width as f32;

            for x in 0..area.width {
                let idx = ((x as f32 * step) as usize).min(n - 1);

                match channel_mode {
                    ChannelMode::Left => {
                        let sample = samples[idx * channels] * gain;
                        ctx.print(
                            ((x as f64 / area.width as f64) * 2.0 - 1.0),
                            sample as f64,
                            Span::styled("●", Style::default().fg(Color::Cyan)),
                        );
                    }
                    ChannelMode::Right => {
                        let sample = samples[idx * channels + 1] * gain;
                        ctx.print(
                            ((x as f64 / area.width as f64) * 2.0 - 1.0),
                            sample as f64,
                            Span::styled("●", Style::default().fg(Color::Magenta)),
                        );
                    }
                    ChannelMode::Stereo => {
                        let left = samples[idx * channels] * gain;
                        let right = samples[idx * channels + 1] * gain;
                        ctx.print(
                            ((x as f64 / area.width as f64) * 2.0 - 1.0),
                            left as f64,
                            Span::styled("●", Style::default().fg(Color::Cyan)),
                        );
                        ctx.print(
                            ((x as f64 / area.width as f64) * 2.0 - 1.0),
                            right as f64,
                            Span::styled("●", Style::default().fg(Color::Magenta)),
                        );
                    }
                }
            }
        });
    f.render_widget(canvas, area);
}

fn draw_halfblock(f: &mut ratatui::Frame, area: ratatui::layout::Rect, samples: &[f32], channels: usize, channel_mode: ChannelMode, zoom: f32, gain: f32) {
    let canvas = Canvas::default()
        .block(Block::default().borders(Borders::ALL).title("Waveform (HalfBlock)"))
        .x_bounds([-1.0, 1.0])
        .y_bounds([-1.0, 1.0])
        .paint(|ctx| {
            let n = samples.len() / channels;
            let step = (n as f32 * zoom).min(n as f32) / area.width as f32;

            for x in 0..area.width {
                let idx = ((x as f32 * step) as usize).min(n - 1);

                match channel_mode {
                    ChannelMode::Left => {
                        let sample = samples[idx * channels] * gain;
                        let ch = if sample > 0.0 { "▀" } else { "▄" };
                        ctx.print(
                            ((x as f64 / area.width as f64) * 2.0 - 1.0),
                            sample.abs() as f64,
                            Span::styled(ch, Style::default().fg(Color::Green)),
                        );
                    }
                    ChannelMode::Right => {
                        let sample = samples[idx * channels + 1] * gain;
                        let ch = if sample > 0.0 { "▀" } else { "▄" };
                        ctx.print(
                            ((x as f64 / area.width as f64) * 2.0 - 1.0),
                            sample.abs() as f64,
                            Span::styled(ch, Style::default().fg(Color::Yellow)),
                        );
                    }
                    ChannelMode::Stereo => {
                        let left = samples[idx * channels] * gain;
                        let right = samples[idx * channels + 1] * gain;
                        let avg = (left + right) / 2.0;
                        let ch = if avg > 0.0 { "▀" } else { "▄" };
                        ctx.print(
                            ((x as f64 / area.width as f64) * 2.0 - 1.0),
                            avg.abs() as f64,
                            Span::styled(ch, Style::default().fg(Color::White)),
                        );
                    }
                }
            }
        });
    f.render_widget(canvas, area);
}

fn draw_bars(f: &mut ratatui::Frame, area: ratatui::layout::Rect, samples: &[f32], channels: usize, channel_mode: ChannelMode, zoom: f32, gain: f32) {
    let canvas = Canvas::default()
        .block(Block::default().borders(Borders::ALL).title("Waveform (Bars)"))
        .x_bounds([-1.0, 1.0])
        .y_bounds([-1.0, 1.0])
        .paint(|ctx| {
            let n = samples.len() / channels;
            let step = (n as f32 * zoom).min(n as f32) / area.width as f32;

            for x in 0..area.width {
                let idx = ((x as f32 * step) as usize).min(n - 1);

                match channel_mode {
                    ChannelMode::Left => {
                        let sample = samples[idx * channels] * gain;
                        let height = sample.abs();
                        for y in 0..(height * area.height as f32) as usize {
                            ctx.print(
                                ((x as f64 / area.width as f64) * 2.0 - 1.0),
                                ((y as f64 / area.height as f64) * 2.0 - 1.0),
                                Span::styled("█", Style::default().fg(Color::Blue)),
                            );
                        }
                    }
                    ChannelMode::Right => {
                        let sample = samples[idx * channels + 1] * gain;
                        let height = sample.abs();
                        for y in 0..(height * area.height as f32) as usize {
                            ctx.print(
                                ((x as f64 / area.width as f64) * 2.0 - 1.0),
                                ((y as f64 / area.height as f64) * 2.0 - 1.0),
                                Span::styled("█", Style::default().fg(Color::Red)),
                            );
                        }
                    }
                    ChannelMode::Stereo => {
                        let left = samples[idx * channels] * gain;
                        let right = samples[idx * channels + 1] * gain;
                        let avg = (left + right) / 2.0;
                        let height = avg.abs();
                        for y in 0..(height * area.height as f32) as usize {
                            ctx.print(
                                ((x as f64 / area.width as f64) * 2.0 - 1.0),
                                ((y as f64 / area.height as f64) * 2.0 - 1.0),
                                Span::styled("█", Style::default().fg(Color::White)),
                            );
                        }
                    }
                }
            }
        });
    f.render_widget(canvas, area);
}

fn draw_dots(f: &mut ratatui::Frame, area: ratatui::layout::Rect, samples: &[f32], channels: usize, channel_mode: ChannelMode, zoom: f32, gain: f32) {
    let canvas = Canvas::default()
        .block(Block::default().borders(Borders::ALL).title("Waveform (Dots)"))
        .x_bounds([-1.0, 1.0])
        .y_bounds([-1.0, 1.0])
        .paint(|ctx| {
            let n = samples.len() / channels;
            let step = (n as f32 * zoom).min(n as f32) / area.width as f32;

            for x in 0..area.width {
                let idx = ((x as f32 * step) as usize).min(n - 1);

                match channel_mode {
                    ChannelMode::Left => {
                        let sample = samples[idx * channels] * gain;
                        ctx.print(
                            ((x as f64 / area.width as f64) * 2.0 - 1.0),
                            sample as f64,
                            Span::styled("·", Style::default().fg(Color::Cyan)),
                        );
                    }
                    ChannelMode::Right => {
                        let sample = samples[idx * channels + 1] * gain;
                        ctx.print(
                            ((x as f64 / area.width as f64) * 2.0 - 1.0),
                            sample as f64,
                            Span::styled("·", Style::default().fg(Color::Magenta)),
                        );
                    }
                    ChannelMode::Stereo => {
                        let left = samples[idx * channels] * gain;
                        let right = samples[idx * channels + 1] * gain;
                        ctx.print(
                            ((x as f64 / area.width as f64) * 2.0 - 1.0),
                            left as f64,
                            Span::styled("·", Style::default().fg(Color::Cyan)),
                        );
                        ctx.print(
                            ((x as f64 / area.width as f64) * 2.0 - 1.0),
                            right as f64,
                            Span::styled("·", Style::default().fg(Color::Magenta)),
                        );
                    }
                }
            }
        });
    f.render_widget(canvas, area);
}

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &ScopeState,
) -> Result<()> {
    terminal.draw(|f| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
            .split(f.area());

        // Title
        let title = Paragraph::new("LOS Scope")
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(title, chunks[0]);

        // Waveform
        match state.mode {
            RenderMode::Braille => draw_braille(f, chunks[1], &state.samples, state.channels, state.channel_mode, state.zoom, state.gain),
            RenderMode::HalfBlock => draw_halfblock(f, chunks[1], &state.samples, state.channels, state.channel_mode, state.zoom, state.gain),
            RenderMode::Bars => draw_bars(f, chunks[1], &state.samples, state.channels, state.channel_mode, state.zoom, state.gain),
            RenderMode::Dots => draw_dots(f, chunks[1], &state.samples, state.channels, state.channel_mode, state.zoom, state.gain),
        }

        // Controls
        let controls = vec![Line::from(vec![
            Span::raw("Mode: "),
            Span::styled(state.mode.name(), Style::default().fg(Color::Yellow)),
            Span::raw("  Channel: "),
            Span::styled(state.channel_mode.name(), Style::default().fg(Color::Green)),
            Span::raw("  Zoom: "),
            Span::styled(format!("{:.1}x", state.zoom), Style::default().fg(Color::Cyan)),
            Span::raw("  Gain: "),
            Span::styled(format!("{:.1}x", state.gain), Style::default().fg(Color::Magenta)),
            Span::raw("  [m]ode [c]hannel [+]zoom [-]zoom [g]ain [q]uit"),
        ])];
        let controls_widget = Paragraph::new(controls)
            .block(Block::default().borders(Borders::ALL).title("Controls"));
        f.render_widget(controls_widget, chunks[2]);
    })?;

    Ok(())
}

pub fn run(_instance: usize) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let state = Arc::new(Mutex::new(ScopeState::default()));
    let state_clone = Arc::clone(&state);

    let (tx, rx) = std::sync::mpsc::channel();

    let scope_handle = std::thread::spawn(move || {
        if let Err(e) = scope_thread(state_clone, rx) {
            eprintln!("Scope thread error: {}", e);
        }
    });

    loop {
        let current_state = state.lock().unwrap().clone();
        draw_ui(&mut terminal, &current_state)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('m') => {
                        let mut s = state.lock().unwrap();
                        s.mode = s.mode.next();
                    }
                    KeyCode::Char('c') => {
                        let mut s = state.lock().unwrap();
                        s.channel_mode = s.channel_mode.next();
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
                        s.gain = if s.gain < 5.0 { s.gain + 0.5 } else { 1.0 };
                    }
                    _ => {}
                }
            }
        }
    }

    let _ = tx.send(());
    scope_handle.join().unwrap();

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
