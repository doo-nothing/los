use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, BorderType, Borders, Paragraph},
    Terminal,
};

// ─── constants ──────────────────────────────────────────────────────────────

const SCOPE_CAPACITY: usize = 16384;
const SCOPE_ROWS: usize = 10;
const SCOPE_CHARS: usize = 30;
const DISPLAY_SIZE: usize = 60; // SCOPE_CHARS * 2 pixels wide
const PERSISTENCE: f32 = 0.12;
const RING_SIZE: usize = 2048;
const SCOPE_ZOOM: f32 = 3.0;
const AMBER: Color = Color::Rgb(255, 175, 50);
const AMBER_DIM: Color = Color::Rgb(180, 120, 30);

const LOGO: &str = "▗▖ ▄▄▄   ▄▄▄\n▐▌█   █ ▀▄▄\n▐▌▀▄▄▄▀ ▄▄▄▀\n▐▙▄▄▖";

// ─── types ──────────────────────────────────────────────────────────────────

enum Mode {
    Normal,
    Command(String),
}

#[derive(Clone, Copy, PartialEq)]
enum ScopeChannel {
    Left,
    Right,
    Both,
}

impl ScopeChannel {
    fn next(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Both,
            Self::Both => Self::Left,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Left => Self::Both,
            Self::Right => Self::Left,
            Self::Both => Self::Right,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Left => "L",
            Self::Right => "R",
            Self::Both => "L+R",
        }
    }
}

struct ScopeState {
    display: Vec<f32>,
    ring: VecDeque<f32>,
}

impl ScopeState {
    fn new() -> Self {
        Self {
            display: vec![0.0; DISPLAY_SIZE],
            ring: VecDeque::with_capacity(RING_SIZE),
        }
    }
}

struct App {
    mode: Mode,
    scope_open: bool,
    voice_open: bool,
    scope_channel: ScopeChannel,
    scope_samples: Vec<f32>,
    scope_state: ScopeState,
}

// ─── scope rendering ────────────────────────────────────────────────────────
//
// Each Braille glyph is a 2×4 dot grid. Stack SCOPE_ROWS rows for
// SCOPE_ROWS×4 vertical pixels. A triggered cycle fills DISPLAY_SIZE samples
// across SCOPE_CHARS×2 horizontal pixels, connected by Bresenham lines.

fn dot_bit(col: u8, row: u8) -> u8 {
    if col == 0 {
        [0x01, 0x02, 0x04, 0x40][row as usize]
    } else {
        [0x08, 0x10, 0x20, 0x80][row as usize]
    }
}

fn plot(
    canvas: &mut [Vec<u8>],
    px_x: usize,
    px_y: usize,
    char_cols: usize,
    char_rows: usize,
) {
    let c = px_x / 2;
    let dcol = (px_x % 2) as u8;
    let r = px_y / 4;
    let drow = (px_y % 4) as u8;
    if c < char_cols && r < char_rows {
        canvas[r][c] |= dot_bit(dcol, drow);
    }
}

fn bresenham(
    canvas: &mut [Vec<u8>],
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    char_cols: usize,
    char_rows: usize,
) {
    let mut x = x0 as isize;
    let mut y = y0 as isize;
    let dx = (x1 as isize - x0 as isize).abs();
    let dy = -(y1 as isize - y0 as isize).abs();
    let sx = if x0 < x1 { 1isize } else { -1isize };
    let sy = if y0 < y1 { 1isize } else { -1isize };
    let mut err = dx + dy;

    loop {
        if x >= 0 && y >= 0 {
            plot(canvas, x as usize, y as usize, char_cols, char_rows);
        }
        if x == x1 as isize && y == y1 as isize {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            if x == x1 as isize {
                break;
            }
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            if y == y1 as isize {
                break;
            }
            err += dx;
            y += sy;
        }
    }
}

fn render_scope(display: &[f32], char_cols: usize, zoom: f32) -> Vec<String> {
    let char_rows = SCOPE_ROWS;
    let total_px_x = char_cols * 2;
    let total_px_y = char_rows * 4;

    // resample display buffer → total_px_x pixel columns
    let points: Vec<(usize, usize)> = {
        let n = display.len();
        let step = if n > 1 {
            (n - 1) as f64 / (total_px_x - 1) as f64
        } else {
            0.0
        };

        (0..total_px_x)
            .map(|i| {
                let pos = (i as f64 * step) as usize;
                let frac = i as f64 * step - pos as f64;
                let v = if pos + 1 < n {
                    (display[pos] as f64 * (1.0 - frac)
                        + display[pos + 1] as f64 * frac) as f32
                        * zoom
                } else {
                    display[pos.min(n - 1)] * zoom
                };
                let v = v.clamp(-1.0, 1.0);
                let py =
                    ((1.0 - v) / 2.0 * (total_px_y - 1) as f32).round() as usize;
                (i, py.min(total_px_y - 1))
            })
            .collect()
    };

    let mut canvas = vec![vec![0u8; char_cols]; char_rows];

    // grid: center line (solid) and ±50% (dashed)
    let center_y = (total_px_y - 1) / 2;
    let quarter_y = total_px_y / 4;
    let three_quarter_y = 3 * total_px_y / 4;

    for px_x in 0..total_px_x {
        plot(&mut canvas, px_x, center_y, char_cols, char_rows);
    }
    for &gy in &[quarter_y, three_quarter_y] {
        for px_x in (0..total_px_x).step_by(3) {
            plot(&mut canvas, px_x, gy, char_cols, char_rows);
        }
    }

    // waveform trace
    for w in points.windows(2) {
        let (x0, y0) = w[0];
        let (x1, y1) = w[1];
        bresenham(&mut canvas, x0, y0, x1, y1, char_cols, char_rows);
    }

    let mut rows = Vec::with_capacity(char_rows);
    for row in &canvas {
        let s: String = row
            .iter()
            .map(|&bits| char::from_u32(0x2800 + bits as u32).unwrap_or(' '))
            .collect();
        rows.push(s);
    }
    rows
}

// ─── scope processing ───────────────────────────────────────────────────────

fn extract_channel(samples: &[f32], channel: ScopeChannel) -> Vec<f32> {
    match channel {
        ScopeChannel::Left => samples.iter().step_by(2).copied().collect(),
        ScopeChannel::Right => samples.iter().skip(1).step_by(2).copied().collect(),
        ScopeChannel::Both => samples
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| (c[0] + c[1]) * 0.5)
            .collect(),
    }
}

fn resample(data: &[f32], target_len: usize) -> Vec<f32> {
    if data.len() < 2 {
        return vec![data.first().copied().unwrap_or(0.0); target_len];
    }
    let step = (data.len() - 1) as f64 / (target_len.max(1) - 1) as f64;
    (0..target_len)
        .map(|i| {
            let pos = (i as f64 * step) as usize;
            let frac = i as f64 * step - pos as f64;
            if pos + 1 < data.len() {
                (data[pos] as f64 * (1.0 - frac) + data[pos + 1] as f64 * frac) as f32
            } else {
                data[pos]
            }
        })
        .collect()
}

fn process_scope(samples: &[f32], state: &mut ScopeState, channel: ScopeChannel) {
    let trigger_data = extract_channel(samples, channel);
    if trigger_data.is_empty() {
        return;
    }

    // append to ring
    for &s in &trigger_data {
        if state.ring.len() >= RING_SIZE {
            state.ring.pop_front();
        }
        state.ring.push_back(s);
    }

    // find positive zero-crossings in the ring
    let ring: Vec<f32> = state.ring.iter().copied().collect();
    let mut crossings: Vec<usize> = Vec::new();
    for i in 1..ring.len() {
        if ring[i - 1] < 0.0 && ring[i] >= 0.0 {
            crossings.push(i);
        }
    }

    // take the most recent complete cycle (between the last two crossings)
    if crossings.len() >= 2 {
        let start = crossings[crossings.len() - 2];
        let end = crossings[crossings.len() - 1];
        let cycle_len = end - start;
        if cycle_len > 4 {
            let resampled = resample(&ring[start..end], DISPLAY_SIZE);
            for i in 0..DISPLAY_SIZE {
                state.display[i] =
                    state.display[i] * (1.0 - PERSISTENCE) + resampled[i] * PERSISTENCE;
            }
        }
    }
}

// ─── audio ──────────────────────────────────────────────────────────────────

fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    running: Arc<AtomicBool>,
    sample_rate: f32,
    channels: usize,
    scope_buffer: Arc<Mutex<VecDeque<f32>>>,
) -> Result<cpal::Stream> {
    let mut phase_l: f32 = 0.0;
    let mut phase_r: f32 = 0.0;

    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _info| {
            let active = running.load(Ordering::Relaxed);
            let mut burst = [0.0f32; 256];
            let mut burst_idx = 0usize;

            for (i, frame) in data.chunks_mut(channels).enumerate() {
                let l = if active {
                    (phase_l * 2.0 * std::f32::consts::PI).sin() * 0.3
                } else {
                    0.0
                };
                let r = if active {
                    (phase_r * 2.0 * std::f32::consts::PI).sin() * 0.2
                } else {
                    0.0
                };

                if let Some(s) = frame.first_mut() {
                    *s = l;
                }
                if frame.len() > 1 {
                    frame[1] = r;
                }

                phase_l = (phase_l + 440.0 / sample_rate).fract();
                phase_r = (phase_r + 554.0 / sample_rate).fract();

                // push every 2nd frame to scope (interleaved L,R)
                if i % 2 == 0 && burst_idx + 1 < burst.len() {
                    burst[burst_idx] = l;
                    burst[burst_idx + 1] = r;
                    burst_idx += 2;
                }
            }

            if burst_idx > 0 {
                if let Ok(mut buf) = scope_buffer.try_lock() {
                    for &s in &burst[..burst_idx] {
                        if buf.len() >= SCOPE_CAPACITY {
                            buf.pop_front();
                        }
                        buf.push_back(s);
                    }
                }
            }
        },
        |err| eprintln!("audio error: {err}"),
        None,
    )?;

    stream.play()?;
    Ok(stream)
}

// ─── main / ui ──────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no output audio device available"))?;
    let config = device.default_output_config()?;
    let sample_rate = config.sample_rate().0 as f32;
    let channels = config.channels() as usize;

    let running = Arc::new(AtomicBool::new(true));
    let running_clone = Arc::clone(&running);

    let scope_buffer = Arc::new(Mutex::new(VecDeque::with_capacity(SCOPE_CAPACITY)));
    let scope_clone = Arc::clone(&scope_buffer);

    let _stream = build_output_stream(
        &device,
        &config.config(),
        running_clone,
        sample_rate,
        channels,
        scope_clone,
    )?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App {
        mode: Mode::Normal,
        scope_open: true,
        voice_open: false,
        scope_channel: ScopeChannel::Both,
        scope_samples: Vec::with_capacity(SCOPE_CAPACITY),
        scope_state: ScopeState::new(),
    };

    let result = run_ui(&mut terminal, &scope_buffer, sample_rate, &mut app);

    running.store(false, Ordering::SeqCst);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    scope_buffer: &Arc<Mutex<VecDeque<f32>>>,
    sample_rate: f32,
    app: &mut App,
) -> Result<()> {
    loop {
        // drain scope buffer and run trigger on new samples only
        {
            let old_len = app.scope_samples.len();
            {
                if let Ok(mut buf) = scope_buffer.try_lock() {
                    app.scope_samples.extend(buf.drain(..));
                }
            }
            let new_len = app.scope_samples.len();
            if new_len > old_len {
                let new_data = &app.scope_samples[old_len..];
                process_scope(new_data, &mut app.scope_state, app.scope_channel);
            }
            if new_len > SCOPE_CAPACITY {
                let excess = new_len - SCOPE_CAPACITY;
                app.scope_samples.drain(0..excess);
            }
        }

        // render
        let scope_rows = render_scope(&app.scope_state.display, SCOPE_CHARS, SCOPE_ZOOM);

        terminal.draw(|f| {
            let area = f.area();

            let mut constraints: Vec<Constraint> = vec![Constraint::Min(6)];

            if app.scope_open {
                constraints.push(Constraint::Length(SCOPE_ROWS as u16 + 4)); // +info +scale +border
            } else {
                constraints.push(Constraint::Length(1));
            }

            if app.voice_open {
                constraints.push(Constraint::Length(3));
            } else {
                constraints.push(Constraint::Length(1));
            }

            constraints.push(Constraint::Length(1)); // status
            constraints.push(Constraint::Length(1)); // cmd line

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);

            // logo
            let main_block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Thick)
                .title(" los · terminal instrument ");
            let inner = main_block.inner(chunks[0]);
            f.render_widget(main_block, chunks[0]);
            f.render_widget(Paragraph::new(LOGO).alignment(Alignment::Center), inner);

            // scope module
            let scope_toggle = if app.scope_open { "▾" } else { "▸" };
            let scope_title = format!(
                " ● scope {toggle} ",
                toggle = scope_toggle
            );

            if app.scope_open {
                let scope_block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .title(scope_title)
                    .border_style(Style::default().fg(AMBER));
                let scope_inner = scope_block.inner(chunks[1]);
                f.render_widget(scope_block, chunks[1]);

                let scope_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),                         // info bar
                        Constraint::Length(SCOPE_ROWS as u16),         // waveform
                        Constraint::Length(1),                         // scale bar
                    ])
                    .split(scope_inner);

                // info bar: channel + freq | trig
                let info = format!(
                    " {}  {}Hz           trig ↗ ",
                    app.scope_channel.label(),
                    match app.scope_channel {
                        ScopeChannel::Left => "440",
                        ScopeChannel::Right => "554",
                        ScopeChannel::Both => "440+554",
                    },
                );
                f.render_widget(
                    Paragraph::new(info).style(Style::default().fg(AMBER_DIM)),
                    scope_chunks[0],
                );

                // waveform rows
                let wf_rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(vec![Constraint::Length(1); SCOPE_ROWS])
                    .split(scope_chunks[1]);
                for (i, row) in scope_rows.iter().enumerate() {
                    f.render_widget(
                        Paragraph::new(row.as_str()).style(Style::default().fg(AMBER)),
                        wf_rows[i],
                    );
                }

                // scale bar
                let scale = format!(
                    " 1cy/div                     ×{:.1} ",
                    SCOPE_ZOOM
                );
                f.render_widget(
                    Paragraph::new(scale)
                        .style(Style::default().fg(AMBER_DIM)),
                    scope_chunks[2],
                );
            } else {
                f.render_widget(
                    Paragraph::new(scope_title)
                        .style(Style::default().fg(Color::DarkGray)),
                    chunks[1],
                );
            }

            // voice module (placeholder)
            let voice_toggle = if app.voice_open { "▾" } else { "▸" };
            if app.voice_open {
                let voice_block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .title(format!(" ● voice {voice_toggle} "))
                    .border_style(Style::default().fg(Color::DarkGray));
                let v_inner = voice_block.inner(chunks[2]);
                f.render_widget(voice_block, chunks[2]);
                f.render_widget(
                    Paragraph::new(" osc: sine 440/554  ADSR: —  filter: — ")
                        .style(Style::default().fg(Color::DarkGray)),
                    v_inner,
                );
            } else {
                f.render_widget(
                    Paragraph::new(format!(" ● voice {voice_toggle} "))
                        .style(Style::default().fg(Color::DarkGray)),
                    chunks[2],
                );
            }

            // status line
            let status = format!(
                " {} | {} kHz | {} ",
                match app.mode {
                    Mode::Normal => "NORMAL",
                    Mode::Command(_) => "COMMAND",
                },
                (sample_rate / 1000.0) as u32,
                match app.mode {
                    Mode::Normal => "s:scope v:voice [:]channel :q:quit",
                    Mode::Command(_) => "esc:cancel  enter:run",
                },
            );
            f.render_widget(
                Paragraph::new(status)
                    .style(Style::default().fg(Color::Black).bg(AMBER_DIM)),
                chunks[3],
            );

            // command line
            if let Mode::Command(ref cmd) = app.mode {
                f.render_widget(
                    Paragraph::new(format!(":{}", cmd))
                        .style(Style::default().fg(Color::Yellow)),
                    chunks[4],
                );
            }
        })?;

        // input
        if event::poll(Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match &mut app.mode {
                    Mode::Normal => match key.code {
                        KeyCode::Char(':') => {
                            app.mode = Mode::Command(String::new());
                        }
                        KeyCode::Char('s') => {
                            app.scope_open = !app.scope_open;
                        }
                        KeyCode::Char('v') => {
                            app.voice_open = !app.voice_open;
                        }
                        KeyCode::Char('[') => {
                            app.scope_channel = app.scope_channel.prev();
                            app.scope_state = ScopeState::new();
                            app.scope_samples.clear();
                        }
                        KeyCode::Char(']') => {
                            app.scope_channel = app.scope_channel.next();
                            app.scope_state = ScopeState::new();
                            app.scope_samples.clear();
                        }
                        KeyCode::Esc => {}
                        _ => {}
                    },
                    Mode::Command(buf) => match key.code {
                        KeyCode::Enter => {
                            match buf.as_str() {
                                "q" | "quit" | "wq" | "x" => return Ok(()),
                                _ => {}
                            }
                            app.mode = Mode::Normal;
                        }
                        KeyCode::Esc => {
                            app.mode = Mode::Normal;
                        }
                        KeyCode::Char(c) => {
                            buf.push(c);
                        }
                        KeyCode::Backspace => {
                            buf.pop();
                        }
                        _ => {}
                    },
                }
            }
        }
    }
}
