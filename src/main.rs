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
use image::{imageops::FilterType, DynamicImage, RgbaImage};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect, Size},
    style::{Color, Style},
    widgets::{Block, BorderType, Borders, Paragraph},
    Terminal,
};
use ratatui_image::{picker::Picker, protocol::Protocol, Image, Resize};

// ─── constants ──────────────────────────────────────────────────────────────

const SCOPE_CAPACITY: usize = 16384;
const SCOPE_ROWS: usize = 12;
const SCOPE_CHARS: usize = 30;
const DISPLAY_SIZE: usize = 60;
const MODULE_WIDTH: u16 = 24;
const MODULE_WF_ROWS: u16 = 12;
const MODULE_HEIGHT_OPEN: u16 = MODULE_WF_ROWS + 4;
const MODULE_HEIGHT_CLOSED: u16 = 1;
const PERSISTENCE: f32 = 0.12;
const RING_SIZE: usize = 2048;
const SCOPE_ZOOM: f32 = 3.0;

// colors
const AMBER: Color = Color::Rgb(255, 175, 50);
const AMBER_DIM: Color = Color::Rgb(180, 120, 30);
const PANEL_BG: Color = Color::Rgb(24, 24, 28);
const PANEL_BORDER: Color = Color::Rgb(60, 60, 68);
const PANEL_LABEL: Color = Color::Rgb(140, 140, 150);

// phosphor
const CRT_SIZE: usize = 120;
const PHOSPHOR_DECAY: f32 = 0.88;
const GRID_BRIGHT: f32 = 0.22;
const TRACE_BRIGHT: f32 = 1.0;
const SCANLINE_DIM: f32 = 0.85;
const CIRCLE_RADIUS_RATIO: f32 = 0.92;

const LOGO: &str = "▗▖ ▄▄▄   ▄▄▄\n▐▌█   █ ▀▄▄\n▐▌▀▄▄▄▀ ▄▄▄▀\n▐▙▄▄▖";

// ─── types ──────────────────────────────────────────────────────────────────

enum Mode {
    Normal,
    Command(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ScopeChannel {
    Left,
    Right,
    Both,
}

impl ScopeChannel {
    fn next(self) -> Self {
        match self { Self::Left => Self::Right, Self::Right => Self::Both, Self::Both => Self::Left }
    }
    fn prev(self) -> Self {
        match self { Self::Left => Self::Both, Self::Right => Self::Left, Self::Both => Self::Right }
    }
    fn label(self) -> &'static str {
        match self { Self::Left => "L", Self::Right => "R", Self::Both => "L+R" }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ScopeMode { Braille, Crt }

struct ScopeState {
    display: Vec<f32>,
    ring: VecDeque<f32>,
}

impl ScopeState {
    fn new() -> Self {
        Self { display: vec![0.0; DISPLAY_SIZE], ring: VecDeque::with_capacity(RING_SIZE) }
    }
}

struct App {
    mode: Mode,
    scope_open: bool,
    voice_open: bool,
    scope_channel: ScopeChannel,
    scope_mode: ScopeMode,
    scope_samples: Vec<f32>,
    scope_state: ScopeState,
    crt_buf: Vec<f32>,
    crt_mask: Vec<f32>,
    picker: Picker,
}

// ─── Braille scope ──────────────────────────────────────────────────────────

fn dot_bit(col: u8, row: u8) -> u8 {
    if col == 0 { [0x01, 0x02, 0x04, 0x40][row as usize] }
    else         { [0x08, 0x10, 0x20, 0x80][row as usize] }
}

fn plot_braille(canvas: &mut [Vec<u8>], px_x: usize, px_y: usize, cc: usize, cr: usize) {
    let c = px_x / 2; let dcol = (px_x % 2) as u8; let r = px_y / 4; let drow = (px_y % 4) as u8;
    if c < cc && r < cr { canvas[r][c] |= dot_bit(dcol, drow); }
}

fn line_braille(canvas: &mut [Vec<u8>], x0: usize, y0: usize, x1: usize, y1: usize, cc: usize, cr: usize) {
    let (mut x, mut y) = (x0 as isize, y0 as isize);
    let dx = (x1 as isize - x0 as isize).abs();
    let dy = -(y1 as isize - y0 as isize).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        if x >= 0 && y >= 0 { plot_braille(canvas, x as usize, y as usize, cc, cr); }
        if x == x1 as isize && y == y1 as isize { break; }
        let e2 = 2 * err;
        if e2 >= dy { if x == x1 as isize { break; } err += dy; x += sx; }
        if e2 <= dx { if y == y1 as isize { break; } err += dx; y += sy; }
    }
}

fn render_braille(display: &[f32], char_cols: usize, zoom: f32) -> Vec<String> {
    let char_rows = SCOPE_ROWS;
    let (tpx, tpy) = (char_cols * 2, char_rows * 4);
    let n = display.len();
    let step = if n > 1 { (n - 1) as f64 / (tpx - 1) as f64 } else { 0.0 };
    let points: Vec<(usize, usize)> = (0..tpx).map(|i| {
        let pos = (i as f64 * step) as usize;
        let frac = i as f64 * step - pos as f64;
        let v = if pos + 1 < n { (display[pos] as f64 * (1.0 - frac) + display[pos+1] as f64 * frac) as f32 * zoom }
                else { display[pos.min(n-1)] * zoom };
        let v = v.clamp(-1.0, 1.0);
        let py = ((1.0 - v) / 2.0 * (tpy - 1) as f32).round() as usize;
        (i, py.min(tpy - 1))
    }).collect();

    let mut canvas = vec![vec![0u8; char_cols]; char_rows];
    let cy = (tpy - 1) / 2;
    for px_x in 0..tpx { plot_braille(&mut canvas, px_x, cy, char_cols, char_rows); }
    for &gy in &[tpy/4, 3*tpy/4] {
        for px_x in (0..tpx).step_by(3) { plot_braille(&mut canvas, px_x, gy, char_cols, char_rows); }
    }
    for w in points.windows(2) { line_braille(&mut canvas, w[0].0, w[0].1, w[1].0, w[1].1, char_cols, char_rows); }

    canvas.iter().map(|row| row.iter().map(|&b| char::from_u32(0x2800 + b as u32).unwrap_or(' ')).collect()).collect()
}

// ─── CRT phosphor engine ────────────────────────────────────────────────────

fn plot_crt(buf: &mut [f32], w: usize, h: usize, px: usize, py: usize, bright: f32) {
    if px < w && py < h { buf[py * w + px] = buf[py * w + px].max(bright); }
}

fn line_crt(buf: &mut [f32], w: usize, h: usize, x0: usize, y0: usize, x1: usize, y1: usize, bright: f32) {
    let (mut x, mut y) = (x0 as isize, y0 as isize);
    let dx = (x1 as isize - x0 as isize).abs();
    let dy = -(y1 as isize - y0 as isize).abs();
    let sx = if x0 < x1 { 1isize } else { -1isize };
    let sy = if y0 < y1 { 1isize } else { -1isize };
    let mut err = dx + dy;
    loop {
        if x >= 0 && y >= 0 { plot_crt(buf, w, h, x as usize, y as usize, bright); }
        if x == x1 as isize && y == y1 as isize { break; }
        let e2 = 2 * err;
        if e2 >= dy { if x == x1 as isize { break; } err += dy; x += sx; }
        if e2 <= dx { if y == y1 as isize { break; } err += dx; y += sy; }
    }
}

fn build_crt_mask(size: usize) -> Vec<f32> {
    let mut mask = vec![1.0f32; size * size];
    let cx = size as f32 / 2.0;
    let cy = size as f32 / 2.0;
    let radius = (size as f32 / 2.0) * CIRCLE_RADIUS_RATIO;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist > radius {
                mask[y * size + x] = 0.0;
            } else if dist > radius - 3.0 {
                let fade = ((radius - dist) / 3.0).clamp(0.0, 1.0);
                mask[y * size + x] = fade * fade;
            }
        }
    }
    mask
}

fn render_phosphor(display: &[f32], buf: &mut [f32], mask: &[f32], w: usize, h: usize, zoom: f32) {
    // decay
    for p in buf.iter_mut() { *p *= PHOSPHOR_DECAY; }

    // scanlines
    for y in (1..h).step_by(2) {
        let row = y * w;
        for x in 0..w { buf[row + x] *= SCANLINE_DIM; }
    }

    // pre-computed circular mask
    for i in 0..buf.len() { buf[i] *= mask[i]; }

    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let radius = (w.min(h) as f32 / 2.0) * CIRCLE_RADIUS_RATIO;

    // graticule — crosshair
    let mid = w / 2;
    let mid_h = h / 2;
    for x in 0..w { plot_crt(buf, w, h, x, mid_h, GRID_BRIGHT * 0.5); }
    for y in 0..h { plot_crt(buf, w, h, mid, y, GRID_BRIGHT * 0.5); }

    // graticule — concentric circles
    for &ratio in &[0.33, 0.66] {
        let r = radius * ratio;
        let steps = (2.0 * std::f32::consts::PI * r) as usize;
        for i in 0..steps {
            let angle = i as f32 / steps as f32 * 2.0 * std::f32::consts::PI;
            let px = (cx + angle.cos() * r).round() as usize;
            let py = (cy + angle.sin() * r).round() as usize;
            plot_crt(buf, w, h, px, py, GRID_BRIGHT * 0.4);
        }
    }

    // graticule — outer circle
    let steps = (2.0 * std::f32::consts::PI * radius) as usize;
    for i in 0..steps {
        let angle = i as f32 / steps as f32 * 2.0 * std::f32::consts::PI;
        let px = (cx + angle.cos() * radius).round() as usize;
        let py = (cy + angle.sin() * radius).round() as usize;
        plot_crt(buf, w, h, px, py, GRID_BRIGHT);
    }

    // tick marks around circle (every 30 degrees)
    for deg in (0..360).step_by(30) {
        let rad = deg as f32 * std::f32::consts::PI / 180.0;
        let inner_r = radius * 0.88;
        let steps = ((radius - inner_r) as usize).max(1);
        for s in 0..steps {
            let r = inner_r + s as f32;
            let px = (cx + rad.cos() * r).round() as usize;
            let py = (cy + rad.sin() * r).round() as usize;
            plot_crt(buf, w, h, px, py, GRID_BRIGHT);
        }
    }

    // waveform
    let n = display.len();
    if n < 2 { return; }

    let points: Vec<(usize, usize)> = (0..w).map(|px| {
        let pos = (px as f64 / (w-1) as f64 * (n-1) as f64) as usize;
        let frac = px as f64 / (w-1) as f64 * (n-1) as f64 - pos as f64;
        let v = if pos + 1 < n {
            (display[pos] as f64 * (1.0 - frac) + display[pos+1] as f64 * frac) as f32 * zoom
        } else { display[pos.min(n-1)] * zoom };
        let v = v.clamp(-1.0, 1.0);
        let py = ((1.0 - v) / 2.0 * (h - 1) as f32).round() as usize;
        (px, py.min(h-1))
    }).collect();

    for wnd in points.windows(2) { line_crt(buf, w, h, wnd[0].0, wnd[0].1, wnd[1].0, wnd[1].1, TRACE_BRIGHT); }
}

fn crt_to_protocol(buf: &[f32], w: usize, h: usize, picker: &Picker, size: Size) -> Result<Protocol> {
    let mut rgba = vec![0u8; w * h * 4];
    for (idx, &v) in buf.iter().enumerate() {
        let intensity = (v.clamp(0.0, 1.0) * 255.0) as u8;
        let base = intensity as f32 / 255.0;
        let off = idx * 4;
        rgba[off] = (base * 255.0) as u8;
        rgba[off+1] = (base * 175.0) as u8;
        rgba[off+2] = (base * 50.0) as u8;
        rgba[off+3] = 255;
    }
    let img = RgbaImage::from_raw(w as u32, h as u32, rgba)
        .ok_or_else(|| anyhow::anyhow!("invalid RGBA dimensions"))?;
    let dyn_img = DynamicImage::ImageRgba8(img);
    Ok(picker.new_protocol(dyn_img, size, Resize::Fit(Some(FilterType::Lanczos3)))?)
}

// ─── scope processing ───────────────────────────────────────────────────────

fn extract_channel(samples: &[f32], ch: ScopeChannel) -> Vec<f32> {
    match ch {
        ScopeChannel::Left => samples.iter().step_by(2).copied().collect(),
        ScopeChannel::Right => samples.iter().skip(1).step_by(2).copied().collect(),
        ScopeChannel::Both => samples.chunks(2).filter(|c| c.len()==2).map(|c| (c[0]+c[1])*0.5).collect(),
    }
}

fn resample(data: &[f32], target: usize) -> Vec<f32> {
    if data.len() < 2 { return vec![data.first().copied().unwrap_or(0.0); target]; }
    let step = (data.len()-1) as f64 / (target.max(1)-1) as f64;
    (0..target).map(|i| {
        let pos = (i as f64 * step) as usize;
        let frac = i as f64 * step - pos as f64;
        if pos+1 < data.len() { (data[pos] as f64 * (1.0-frac) + data[pos+1] as f64 * frac) as f32 }
        else { data[pos] }
    }).collect()
}

fn process_scope(samples: &[f32], state: &mut ScopeState, channel: ScopeChannel) {
    let data = extract_channel(samples, channel);
    if data.is_empty() { return; }
    for &s in &data {
        if state.ring.len() >= RING_SIZE { state.ring.pop_front(); }
        state.ring.push_back(s);
    }
    let ring: Vec<f32> = state.ring.iter().copied().collect();
    let mut xings = vec![];
    for i in 1..ring.len() { if ring[i-1] < 0.0 && ring[i] >= 0.0 { xings.push(i); } }
    if xings.len() >= 2 {
        let (s, e) = (xings[xings.len()-2], xings[xings.len()-1]);
        if e - s > 4 {
            let rs = resample(&ring[s..e], DISPLAY_SIZE);
            for i in 0..DISPLAY_SIZE { state.display[i] = state.display[i] * (1.0-PERSISTENCE) + rs[i] * PERSISTENCE; }
        }
    }
}

// ─── audio ──────────────────────────────────────────────────────────────────

fn build_output_stream(
    device: &cpal::Device, config: &cpal::StreamConfig, running: Arc<AtomicBool>,
    sample_rate: f32, channels: usize, scope_buffer: Arc<Mutex<VecDeque<f32>>>,
) -> Result<cpal::Stream> {
    let mut phase_l = 0.0f32; let mut phase_r = 0.0f32;
    let stream = device.build_output_stream(config, move |data: &mut [f32], _info| {
        let active = running.load(Ordering::Relaxed);
        let mut burst = [0.0f32; 256]; let mut bi = 0usize;
        for (i, frame) in data.chunks_mut(channels).enumerate() {
            let l = if active { (phase_l * 2.0 * std::f32::consts::PI).sin() * 0.3 } else { 0.0 };
            let r = if active { (phase_r * 2.0 * std::f32::consts::PI).sin() * 0.2 } else { 0.0 };
            if let Some(s) = frame.first_mut() { *s = l; }
            if frame.len() > 1 { frame[1] = r; }
            phase_l = (phase_l + 440.0 / sample_rate).fract();
            phase_r = (phase_r + 554.0 / sample_rate).fract();
            if i % 2 == 0 && bi + 1 < burst.len() { burst[bi] = l; burst[bi+1] = r; bi += 2; }
        }
        if bi > 0 {
            if let Ok(mut buf) = scope_buffer.try_lock() {
                for &s in &burst[..bi] {
                    if buf.len() >= SCOPE_CAPACITY { buf.pop_front(); }
                    buf.push_back(s);
                }
            }
        }
    }, |err| eprintln!("audio error: {err}"), None)?;
    stream.play()?;
    Ok(stream)
}

// ─── main / ui ──────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let host = cpal::default_host();
    let device = host.default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no output audio device available"))?;
    let config = device.default_output_config()?;
    let sr = config.sample_rate().0 as f32;
    let ch = config.channels() as usize;

    let running = Arc::new(AtomicBool::new(true));
    let scope_buf = Arc::new(Mutex::new(VecDeque::with_capacity(SCOPE_CAPACITY)));
    let _stream = build_output_stream(&device, &config.config(), Arc::clone(&running), sr, ch, Arc::clone(&scope_buf))?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());

    let mut app = App {
        mode: Mode::Normal, scope_open: true, voice_open: false,
        scope_channel: ScopeChannel::Both, scope_mode: ScopeMode::Crt,
        scope_samples: Vec::with_capacity(SCOPE_CAPACITY), scope_state: ScopeState::new(),
        crt_buf: vec![0.0; CRT_SIZE * CRT_SIZE],
        crt_mask: build_crt_mask(CRT_SIZE),
        picker,
    };

    let result = run_ui(&mut terminal, &scope_buf, sr, &mut app);

    running.store(false, Ordering::SeqCst);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn module_block(title: &str, border_color: Color) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(PANEL_BG))
}

fn render_module_label(f: &mut ratatui::Frame, label: &str, area: Rect, color: Color) {
    f.render_widget(
        Paragraph::new(label).style(Style::default().fg(color)),
        area,
    );
}

fn run_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    scope_buffer: &Arc<Mutex<VecDeque<f32>>>,
    sample_rate: f32,
    app: &mut App,
) -> Result<()> {
    loop {
        // drain scope samples
        {
            let old_len = app.scope_samples.len();
            if let Ok(mut buf) = scope_buffer.try_lock() { app.scope_samples.extend(buf.drain(..)); }
            let new_len = app.scope_samples.len();
            if new_len > old_len { process_scope(&app.scope_samples[old_len..], &mut app.scope_state, app.scope_channel); }
            if new_len > SCOPE_CAPACITY { app.scope_samples.drain(0..new_len - SCOPE_CAPACITY); }
        }

        // phosphor update
        if app.scope_mode == ScopeMode::Crt {
            render_phosphor(&app.scope_state.display, &mut app.crt_buf, &app.crt_mask, CRT_SIZE, CRT_SIZE, SCOPE_ZOOM);
        }

        let braille_rows: Option<Vec<String>> = if app.scope_mode == ScopeMode::Braille {
            Some(render_braille(&app.scope_state.display, SCOPE_CHARS, SCOPE_ZOOM))
        } else { None };

        terminal.draw(|f| {
            let area = f.area();

            let v_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(6),
                    Constraint::Length(if app.scope_open || app.voice_open { MODULE_HEIGHT_OPEN } else { MODULE_HEIGHT_CLOSED }),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .split(area);

            // ── header ──
            let logo_block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .title(" los · terminal instrument · v0.1 ")
                .border_style(Style::default().fg(PANEL_BORDER));
            let logo_inner = logo_block.inner(v_chunks[0]);
            f.render_widget(logo_block, v_chunks[0]);
            f.render_widget(Paragraph::new(LOGO).alignment(Alignment::Center), logo_inner);

            // ── modules row ──
            let h_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(MODULE_WIDTH), Constraint::Length(MODULE_WIDTH), Constraint::Fill(1)])
                .split(v_chunks[1]);

            // ── scope module ──
            {
                let toggle = if app.scope_open { "▾" } else { "▸" };
                let mode_ind = match app.scope_mode { ScopeMode::Braille => "BR", ScopeMode::Crt => "Φ" };
                let title = format!(" ● TYPE 440 [{mode_ind}] {toggle} ");
                let block = module_block(&title, if app.scope_open { AMBER } else { PANEL_BORDER });
                let inner = block.inner(h_chunks[0]);
                f.render_widget(block, h_chunks[0]);

                if app.scope_open {
                    let scope_inner = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)])
                        .split(inner);

                    // top label row
                    let info = format!(" {}  {}Hz  trig↗", app.scope_channel.label(),
                        match app.scope_channel { ScopeChannel::Left => "440", ScopeChannel::Right => "554", ScopeChannel::Both => "440+554" });
                    render_module_label(f, &info, scope_inner[0], AMBER_DIM);

                    // waveform
                    let crt_protocol: Option<Protocol> = if app.scope_mode == ScopeMode::Crt {
                        let size = Size::new(scope_inner[1].width, scope_inner[1].height);
                        crt_to_protocol(&app.crt_buf, CRT_SIZE, CRT_SIZE, &app.picker, size).ok()
                    } else { None };

                    if let Some(ref proto) = crt_protocol {
                        f.render_widget(Image::new(proto), scope_inner[1]);
                    } else if let Some(ref rows) = braille_rows {
                        let wf_rows = Layout::default().direction(Direction::Vertical)
                            .constraints(vec![Constraint::Length(1); SCOPE_ROWS]).split(scope_inner[1]);
                        for (i, row) in rows.iter().enumerate() {
                            f.render_widget(Paragraph::new(row.as_str()).style(Style::default().fg(AMBER)), wf_rows[i]);
                        }
                    }

                    // bottom scale
                    let scale = match app.scope_mode { ScopeMode::Braille => " 1cy/div  braille  ×3.0 ", ScopeMode::Crt => " 1cy/div  Φ-CRT  ×3.0 " };
                    render_module_label(f, scale, scope_inner[2], AMBER_DIM);
                }
            }

            // ── voice module ──
            {
                let toggle = if app.voice_open { "▾" } else { "▸" };
                let title = format!(" ● voice {toggle} ");
                let block = module_block(&title, if app.voice_open { PANEL_LABEL } else { PANEL_BORDER });
                let inner = block.inner(h_chunks[1]);
                f.render_widget(block, h_chunks[1]);

                if app.voice_open {
                    let vi = Layout::default().direction(Direction::Vertical)
                        .constraints([Constraint::Length(1), Constraint::Fill(1)]).split(inner);
                    render_module_label(f, " oscillator  ", vi[0], AMBER_DIM);
                    f.render_widget(
                        Paragraph::new(" sine 440/554 \n ADSR: —— \n filter: —— \n fb: —— ")
                            .style(Style::default().fg(PANEL_LABEL)),
                        vi[1],
                    );
                }
            }

            // ── status ──
            let status = format!(" {} | {} kHz | s:scope v:voice b:mode [:]ch :q:quit ",
                match app.mode { Mode::Normal => "NORMAL", Mode::Command(_) => "COMMAND" },
                (sample_rate / 1000.0) as u32);
            f.render_widget(
                Paragraph::new(status).style(Style::default().fg(Color::Black).bg(AMBER_DIM)),
                v_chunks[2],
            );

            // ── command line ──
            if let Mode::Command(ref cmd) = app.mode {
                f.render_widget(
                    Paragraph::new(format!(":{}", cmd)).style(Style::default().fg(Color::Yellow)),
                    v_chunks[3],
                );
            }
        })?;

        // input
        if event::poll(Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press { continue; }
                match &mut app.mode {
                    Mode::Normal => match key.code {
                        KeyCode::Char(':') => app.mode = Mode::Command(String::new()),
                        KeyCode::Char('s') => app.scope_open = !app.scope_open,
                        KeyCode::Char('v') => app.voice_open = !app.voice_open,
                        KeyCode::Char('b') => app.scope_mode = match app.scope_mode { ScopeMode::Braille => ScopeMode::Crt, ScopeMode::Crt => ScopeMode::Braille },
                        KeyCode::Char('[') => { app.scope_channel = app.scope_channel.prev(); app.scope_state = ScopeState::new(); app.scope_samples.clear(); }
                        KeyCode::Char(']') => { app.scope_channel = app.scope_channel.next(); app.scope_state = ScopeState::new(); app.scope_samples.clear(); }
                        KeyCode::Esc => {}
                        _ => {}
                    },
                    Mode::Command(buf) => match key.code {
                        KeyCode::Enter => { if matches!(buf.as_str(), "q"|"quit"|"wq"|"x") { return Ok(()); } app.mode = Mode::Normal; }
                        KeyCode::Esc => app.mode = Mode::Normal,
                        KeyCode::Char(c) => { buf.push(c); }
                        KeyCode::Backspace => { buf.pop(); }
                        _ => {}
                    },
                }
            }
        }
    }
}

// ─── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_channel_left() {
        let samples = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let left = extract_channel(&samples, ScopeChannel::Left);
        assert_eq!(left, vec![1.0, 3.0, 5.0]);
    }

    #[test]
    fn test_extract_channel_right() {
        let samples = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let right = extract_channel(&samples, ScopeChannel::Right);
        assert_eq!(right, vec![2.0, 4.0, 6.0]);
    }

    #[test]
    fn test_extract_channel_both() {
        let samples = vec![1.0, 3.0, 2.0, 4.0];
        let both = extract_channel(&samples, ScopeChannel::Both);
        assert_eq!(both, vec![2.0, 3.0]);
    }

    #[test]
    fn test_resample_upscale() {
        let data = vec![0.0, 1.0];
        let result = resample(&data, 5);
        assert_eq!(result.len(), 5);
        assert!((result[0] - 0.0).abs() < 0.01);
        assert!((result[4] - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_resample_downscale() {
        let data = vec![0.0, 0.5, 1.0, 0.5, 0.0];
        let result = resample(&data, 2);
        assert_eq!(result.len(), 2);
        assert!((result[0] - 0.0).abs() < 0.01);
        assert!((result[1] - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_resample_single_element() {
        let data = vec![0.5];
        let result = resample(&data, 3);
        assert_eq!(result.len(), 3);
        assert_eq!(result, vec![0.5, 0.5, 0.5]);
    }

    #[test]
    fn test_dot_bit_left_col() {
        assert_eq!(dot_bit(0, 0), 0x01);
        assert_eq!(dot_bit(0, 1), 0x02);
        assert_eq!(dot_bit(0, 2), 0x04);
        assert_eq!(dot_bit(0, 3), 0x40);
    }

    #[test]
    fn test_dot_bit_right_col() {
        assert_eq!(dot_bit(1, 0), 0x08);
        assert_eq!(dot_bit(1, 1), 0x10);
        assert_eq!(dot_bit(1, 2), 0x20);
        assert_eq!(dot_bit(1, 3), 0x80);
    }

    #[test]
    fn test_plot_braille_sets_correct_bit() {
        let mut canvas = vec![vec![0u8; 2]; 2];
        plot_braille(&mut canvas, 0, 0, 2, 2); // top-left of first char
        assert_eq!(canvas[0][0], 0x01);
        plot_braille(&mut canvas, 1, 0, 2, 2); // top-right of first char
        assert_eq!(canvas[0][0], 0x01 | 0x08);
        plot_braille(&mut canvas, 0, 4, 2, 2); // top-left of second row
        assert_eq!(canvas[1][0], 0x01);
    }

    #[test]
    fn test_plot_braille_clips_bounds() {
        let mut canvas = vec![vec![0u8; 1]; 1];
        plot_braille(&mut canvas, 99, 99, 1, 1); // out of bounds
        assert_eq!(canvas[0][0], 0);
    }

    #[test]
    fn test_plot_crt_sets_value() {
        let mut buf = vec![0.0f32; 4];
        plot_crt(&mut buf, 2, 2, 0, 0, 0.8);
        assert_eq!(buf[0], 0.8);
        plot_crt(&mut buf, 2, 2, 1, 1, 0.5);
        assert_eq!(buf[3], 0.5);
    }

    #[test]
    fn test_plot_crt_max_holds() {
        let mut buf = vec![0.0f32; 1];
        plot_crt(&mut buf, 1, 1, 0, 0, 0.6);
        plot_crt(&mut buf, 1, 1, 0, 0, 0.9);
        assert_eq!(buf[0], 0.9); // max wins
        plot_crt(&mut buf, 1, 1, 0, 0, 0.3);
        assert_eq!(buf[0], 0.9); // still max
    }

    #[test]
    fn test_plot_crt_clips_bounds() {
        let mut buf = vec![0.0f32; 1];
        plot_crt(&mut buf, 1, 1, 99, 99, 1.0);
        assert_eq!(buf[0], 0.0);
    }

    #[test]
    fn test_build_crt_mask_center_is_bright() {
        let mask = build_crt_mask(20);
        let c = 20 / 2;
        assert!(mask[c * 20 + c] > 0.9); // center is inside circle
    }

    #[test]
    fn test_build_crt_mask_corners_are_dark() {
        let mask = build_crt_mask(20);
        assert_eq!(mask[0], 0.0); // top-left corner outside
        assert_eq!(mask[19], 0.0); // top-right corner outside
    }

    #[test]
    fn test_render_braille_output_dimensions() {
        let display = vec![0.0; DISPLAY_SIZE];
        let rows = render_braille(&display, 10, 1.0);
        assert_eq!(rows.len(), SCOPE_ROWS);
        for row in &rows {
            assert_eq!(row.chars().count(), 10);
        }
    }

    #[test]
    fn test_scope_channel_cycle() {
        let ch = ScopeChannel::Left;
        assert_eq!(ch.next(), ScopeChannel::Right);
        assert_eq!(ch.next().next(), ScopeChannel::Both);
        assert_eq!(ch.next().next().next(), ScopeChannel::Left);
        assert_eq!(ch.prev(), ScopeChannel::Both);
    }

    #[test]
    fn test_scope_channel_label() {
        assert_eq!(ScopeChannel::Left.label(), "L");
        assert_eq!(ScopeChannel::Right.label(), "R");
        assert_eq!(ScopeChannel::Both.label(), "L+R");
    }
}
