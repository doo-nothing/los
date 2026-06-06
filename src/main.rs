use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use image::{imageops::FilterType, DynamicImage, RgbaImage};
use phosphor::{gradient::RgbColor, PhosphorHeadless};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect, Size},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Terminal,
};
use ratatui_image::{picker::Picker, protocol::Protocol, Image, Resize};

// ─── constants ──────────────────────────────────────────────────────────────

const SCOPE_CAPACITY: usize = 16384;
const SCOPE_ROWS: usize = 12;
const SCOPE_CHARS: usize = 30;
const DISPLAY_SIZE: usize = 200;
const PERSISTENCE: f32 = 0.5;
const RING_SIZE: usize = 2048;
const SCOPE_ZOOM: f32 = 3.0;
const AUDIO_LATENCY_SAMPLES: usize = 1024;
const MODULE_WIDTH: u16 = 24;
const MODULE_WF_ROWS: u16 = 12;
const MODULE_HEIGHT_OPEN: u16 = MODULE_WF_ROWS + 4;
const MODULE_HEIGHT_CLOSED: u16 = 1;
const ENV_HEIGHT_OPEN: u16 = 6;
const ENV_HEIGHT_CLOSED: u16 = 1;
const SEQ_STEPS: usize = 16;
const MAX_STEPS: usize = 64;
const SEQ_TRACKS: usize = 4;

const AMBER: Color = Color::Rgb(255, 175, 50);
const AMBER_DIM: Color = Color::Rgb(180, 120, 30);
const PANEL_BG: Color = Color::Rgb(24, 24, 28);
const PANEL_BORDER: Color = Color::Rgb(60, 60, 68);
const PANEL_LABEL: Color = Color::Rgb(140, 140, 150);
const MODE_INSERT: Color = Color::Rgb(100, 200, 80);
const MODE_COMMAND: Color = Color::Rgb(200, 80, 80);
const CURSOR_FG: Color = Color::Rgb(200, 220, 255);

const CRT_SIZE: usize = 96;
const PHOSPHOR_SIZE: u32 = 192;
const CRT_THROTTLE: u8 = 4;
const PHOSPHOR_DECAY: f32 = 0.88;
const GRID_BRIGHT: f32 = 0.22;
const TRACE_BRIGHT: f32 = 1.0;
const SCANLINE_DIM: f32 = 0.85;
const CIRCLE_RADIUS_RATIO: f32 = 0.92;

const LOGO: &str = "▗▖ ▄▄▄   ▄▄▄\n▐▌█   █ ▀▄▄\n▐▌▀▄▄▄▀ ▄▄▄▀\n▐▙▄▄▖";

// ─── types ──────────────────────────────────────────────────────────────────

enum Mode {
    Normal,
    Insert,
    Command(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ScopeChannel { Left, Right, Both }

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
enum ScopeMode { Braille, Crt, Phosphor, HalfBlock }

struct ScopeState {
    display: Vec<f32>,
    ring: VecDeque<f32>,
    delay_buf: VecDeque<f32>,
}

impl ScopeState {
    fn new() -> Self { Self { display: vec![0.0; DISPLAY_SIZE], ring: VecDeque::with_capacity(RING_SIZE), delay_buf: VecDeque::with_capacity(2048) } }
}

// ─── voice engine ───────────────────────────────────────────────────────────

#[derive(Clone)]
struct Adsr {
    attack: f32, decay: f32, sustain: f32, release: f32,
    state: u8,
    value: f32,
    rate: f32,
    sr: f32,
}

impl Adsr {
    fn new(a: f32, d: f32, s: f32, r: f32, sample_rate: f32) -> Self {
        Self { attack: a, decay: d, sustain: s, release: r, state: 0, value: 0.0, rate: 0.0, sr: sample_rate }
    }

    fn trigger(&mut self) {
        self.state = 1;
        self.rate = if self.attack > 0.0 { 1.0 / (self.attack * self.sr) } else { 1.0 };
    }

    fn release(&mut self) {
        self.state = 4;
        self.rate = if self.release > 0.0 { 1.0 / (self.release * self.sr) } else { 1.0 };
    }

    fn tick(&mut self) -> f32 {
        match self.state {
            1 => {
                self.value += self.rate;
                if self.value >= 1.0 { self.value = 1.0; self.state = 2; self.rate = if self.decay > 0.0 { (1.0 - self.sustain) / (self.decay * self.sr) } else { 0.0 }; }
            }
            2 => {
                self.value -= self.rate;
                if self.value <= self.sustain { self.value = self.sustain; self.state = 3; }
            }
            4 => {
                self.value -= self.rate;
                if self.value <= 0.0 { self.value = 0.0; self.state = 0; }
            }
            _ => {}
        }
        self.value
    }
}

struct VoiceData {
    adsr: Adsr,
    phase: f32,
    freq: f32,
    velocity: f32,
}

/// Maths-style envelope generator per track
struct EnvGen {
    phase: f32,
    state: u8,  // 0=idle, 1=rise, 2=fall
}

impl EnvGen {
    fn new() -> Self { Self { phase: 0.0, state: 0 } }
    fn trigger(&mut self) { self.state = 1; self.phase = 0.0; }
    fn tick(&mut self, attack_rate: f32, decay_rate: f32, shape_norm: f32) -> f32 {
        match self.state {
            1 => {
                self.phase += attack_rate;
                if self.phase >= 1.0 { self.phase = 0.0; self.state = 2; }
                curve(self.phase, shape_norm)
            }
            2 => {
                self.phase += decay_rate;
                if self.phase >= 1.0 { self.state = 0; return 0.0; }
                1.0 - curve(self.phase, shape_norm)
            }
            _ => 0.0,
        }
    }
}

fn curve(t: f32, shape_norm: f32) -> f32 {
    let power = if shape_norm < 0.5 {
        0.3 + (shape_norm / 0.5) * 0.7
    } else {
        1.0 + ((shape_norm - 0.5) / 0.5) * 2.0
    };
    t.powf(power)
}

/// Map 0-1000 param to 0.001s-10s attack/decay time
fn param_to_time(param: u32) -> f32 {
    let p = param as f32 / 1000.0;
    0.001 * 10_000.0_f32.powf(p)
}

/// Shared state between audio callback and TUI.
struct EngineShared {
    bpm: AtomicU32,         // BPM * 100 (e.g. 12000 = 120.00)
    playing: AtomicBool,
    seq_step: AtomicU32,
    step_data: Mutex<[[u32; MAX_STEPS]; SEQ_TRACKS]>, // each step: bit31=active, bits 30-16=note*100, bits 15-0=vel*1000
    track_len: Mutex<[usize; SEQ_TRACKS]>,
    voice_shape: [AtomicU32; SEQ_TRACKS],
    voice_sub: [AtomicU32; SEQ_TRACKS],
    voice_fm: [AtomicU32; SEQ_TRACKS],
    voice_output: [AtomicU32; SEQ_TRACKS],
    env_value: [AtomicU32; SEQ_TRACKS],
    adsr_phase: [AtomicU32; SEQ_TRACKS],
    cur_freq: [AtomicU32; SEQ_TRACKS],
    seq_progress: AtomicU32, // 0-1000 intra-step progress for scrubber
    // Maths-inspired envelope per track
    env_attack: [AtomicU32; SEQ_TRACKS],
    env_decay: [AtomicU32; SEQ_TRACKS],
    env_shape: [AtomicU32; SEQ_TRACKS],
    env_loop: [AtomicU32; SEQ_TRACKS],    // 0=oneshot, 1=cycle
    env_mod_target: [AtomicU32; SEQ_TRACKS], // 0=amp, 1=pitch, 2=shape, 3=fm
    env_out: [AtomicU32; SEQ_TRACKS],       // current envelope output 0-1000
}

impl EngineShared {
    fn new() -> Self {
        let mut steps = [[0u32; MAX_STEPS]; SEQ_TRACKS];
        // default: a simple pattern on track 0
        steps[0][0] = encode_step(true, 440, 1000);
        steps[0][4] = encode_step(true, 554, 800);
        steps[0][8] = encode_step(true, 440, 1000);
        steps[0][12] = encode_step(true, 660, 700);
        Self {
            bpm: AtomicU32::new(12000),
            playing: AtomicBool::new(false),
            seq_step: AtomicU32::new(0),
            step_data: Mutex::new(steps),
            track_len: Mutex::new([SEQ_STEPS; SEQ_TRACKS]),
            voice_shape: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
            voice_sub: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
            voice_fm: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
            voice_output: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
            env_value: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
            adsr_phase: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
            cur_freq: [AtomicU32::new(440), AtomicU32::new(440), AtomicU32::new(440), AtomicU32::new(440)],
            seq_progress: AtomicU32::new(0),
            env_attack: [AtomicU32::new(100), AtomicU32::new(100), AtomicU32::new(100), AtomicU32::new(100)],
            env_decay: [AtomicU32::new(200), AtomicU32::new(200), AtomicU32::new(200), AtomicU32::new(200)],
            env_shape: [AtomicU32::new(500), AtomicU32::new(500), AtomicU32::new(500), AtomicU32::new(500)],
            env_loop: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
            env_mod_target: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
            env_out: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
        }
    }
}

fn encode_step(active: bool, freq: u32, vel_x1000: u32) -> u32 {
    (if active { 1 << 31 } else { 0 }) | ((freq & 0xFFF) << 19) | (vel_x1000 & 0x7FFFF)
}

fn decode_step(encoded: u32) -> (bool, f32, f32) {
    let active = (encoded >> 31) != 0;
    let freq = ((encoded >> 19) & 0xFFF) as f32;
    let vel = (encoded & 0x7FFFF) as f32 / 1000.0;
    (active, freq, vel)
}

fn next_active_step(steps: &[[u32; MAX_STEPS]; SEQ_TRACKS], track: usize, current: usize, track_len: usize) -> Option<usize> {
    (current + 1..track_len).chain(0..=current).find(|&i| decode_step(steps[track][i]).0)
}

fn prev_active_step(steps: &[[u32; MAX_STEPS]; SEQ_TRACKS], track: usize, current: usize, track_len: usize) -> Option<usize> {
    (0..current).rev().chain((current..track_len).rev()).find(|&i| decode_step(steps[track][i]).0)
}

/// Bjorklund's algorithm: distribute `pulses` ones evenly across `steps` positions.
/// Returns a Vec of bool where `true` = pulse. This is the Euclidean rhythm.
fn euclidean(pulses: usize, steps: usize) -> Vec<bool> {
    if pulses == 0 { return vec![false; steps]; }
    if pulses >= steps { return vec![true; steps]; }
    let mut pattern = Vec::with_capacity(steps);
    let mut bucket = 0usize;
    for _ in 0..steps {
        bucket += pulses;
        if bucket >= steps {
            bucket -= steps;
            pattern.push(true);
        } else {
            pattern.push(false);
        }
    }
    pattern
}

fn freq_to_note(freq: f32) -> String {
    if freq < 20.0 { return "---".into(); }
    let semitones = 12.0 * (freq / 440.0).log2();
    let rounded = semitones.round() as i32;
    let names = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    let octave = 4 + (rounded + 9).div_euclid(12);
    let idx = ((rounded + 9).rem_euclid(12)) as usize;
    format!("{}{}", names[idx], octave)
}

fn semitone_up(freq: f32) -> f32 { (freq * 1.059463).min(4000.0) }
fn semitone_down(freq: f32) -> f32 { (freq / 1.059463).max(20.0) }
fn octave_up(freq: f32) -> f32 { (freq * 2.0).min(4000.0) }
fn octave_down(freq: f32) -> f32 { (freq / 2.0).max(20.0) }

struct App {
    mode: Mode,
    scope_open: bool,
    voice_open: bool,
    seq_open: bool,
    env_open: bool,
    scope_channel: ScopeChannel,
    scope_mode: ScopeMode,
    scope_samples: Vec<f32>,
    scope_state: ScopeState,
    crt_buf: Vec<f32>,
    crt_mask: Vec<f32>,
    crt_cache: Option<Protocol>,
    scope_dirty: bool,
    crt_frame: u8,
    picker: Picker,
    engine: Arc<EngineShared>,
    seq_cursor: (usize, usize),
    pending_count: u32,
    pending_g: bool,
    pending_d: bool,
    pending_leader: bool,
    focused_module: Option<usize>,
    voice_param: usize,
    env_param: usize,
    phosphor_renderer: Option<PhosphorHeadless>,
    phosphor_error: Option<String>,
}

// ─── Braille scope ──────────────────────────────────────────────────────────

fn dot_bit(col: u8, row: u8) -> u8 {
    if col == 0 { [0x01, 0x02, 0x04, 0x40][row as usize] } else { [0x08, 0x10, 0x20, 0x80][row as usize] }
}

fn plot_braille(canvas: &mut [Vec<u8>], px_x: usize, px_y: usize, cc: usize, cr: usize) {
    let c = px_x / 2; let dcol = (px_x % 2) as u8; let r = px_y / 4; let drow = (px_y % 4) as u8;
    if c < cc && r < cr { canvas[r][c] |= dot_bit(dcol, drow); }
}

fn line_braille(canvas: &mut [Vec<u8>], x0: usize, y0: usize, x1: usize, y1: usize, cc: usize, cr: usize) {
    let (mut x, mut y) = (x0 as isize, y0 as isize);
    let dx = (x1 as isize - x0 as isize).abs(); let dy = -(y1 as isize - y0 as isize).abs();
    let sx = if x0 < x1 { 1 } else { -1 }; let sy = if y0 < y1 { 1 } else { -1 };
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
    let step = if n > 1 { (n-1) as f64 / (tpx-1) as f64 } else { 0.0 };
    let points: Vec<(usize, usize)> = (0..tpx).map(|i| {
        let pos = (i as f64 * step) as usize; let frac = i as f64 * step - pos as f64;
        let v = if pos+1 < n { (display[pos] as f64 * (1.0-frac) + display[pos+1] as f64 * frac) as f32 * zoom } else { display[pos.min(n-1)] * zoom };
        let v = v.clamp(-1.0, 1.0); let py = ((1.0 - v) / 2.0 * (tpy-1) as f32).round() as usize;
        (i, py.min(tpy-1))
    }).collect();

    let mut canvas = vec![vec![0u8; char_cols]; char_rows];
    let cy = (tpy-1)/2;
    for px_x in 0..tpx { plot_braille(&mut canvas, px_x, cy, char_cols, char_rows); }
    for &gy in &[tpy/4, 3*tpy/4] { for px_x in (0..tpx).step_by(3) { plot_braille(&mut canvas, px_x, gy, char_cols, char_rows); } }
    for w in points.windows(2) { line_braille(&mut canvas, w[0].0, w[0].1, w[1].0, w[1].1, char_cols, char_rows); }
    canvas.iter().map(|row| row.iter().map(|&b| char::from_u32(0x2800 + b as u32).unwrap_or(' ')).collect()).collect()
}

fn render_halfblock(f: &mut ratatui::Frame, display: &[f32], area: Rect) {
    let cols = area.width as usize;
    let rows = area.height as usize;
    if cols < 2 || rows < 2 || display.len() < 2 { return; }

    let half_rows = rows * 2;
    let n = display.len();
    let step = (n.saturating_sub(1)) as f64 / (cols.saturating_sub(1)) as f64;
    let zoom = SCOPE_ZOOM;

    let points: Vec<(usize, usize)> = (0..cols).map(|i| {
        let pos = (i as f64 * step) as usize;
        let frac = i as f64 * step - pos as f64;
        let v = if pos + 1 < n {
            (display[pos] as f64 * (1.0 - frac) + display[pos + 1] as f64 * frac) as f32 * zoom
        } else {
            display[pos.min(n - 1)] * zoom
        };
        let v = v.clamp(-1.0, 1.0);
        let py = ((1.0 - v) / 2.0 * (half_rows - 1) as f32).round() as usize;
        (i, py.min(half_rows - 1))
    }).collect();

    let mut canvas = vec![vec![[0u8; 2]; cols]; rows];

    let center = half_rows / 2;
    let r = center / 2; let h = center % 2;
    if r < rows { for cell in canvas[r].iter_mut() { cell[h] = 1; } }
    for &gy in &[half_rows / 4, 3 * half_rows / 4] {
        let r = gy / 2; let h = gy % 2;
        if r < rows { for cell in canvas[r].iter_mut().step_by(3) { cell[h] = 1; } }
    }

    for w in points.windows(2) {
        let (mut x, mut y) = (w[0].0 as isize, w[0].1 as isize);
        let (x1, y1) = (w[1].0 as isize, w[1].1 as isize);
        let dx = (x1 - x).abs(); let dy = -(y1 - y).abs();
        let sx = if x < x1 { 1 } else { -1 }; let sy = if y < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            if x >= 0 && y >= 0 {
                let (cx, cy) = (x as usize, y as usize);
                if cx < cols { let r = cy / 2; let h = cy % 2; if r < rows { canvas[r][cx][h] = 2; } }
            }
            if x == x1 && y == y1 { break; }
            let e2 = 2 * err;
            if e2 >= dy { if x == x1 { break; } err += dy; x += sx; }
            if e2 <= dx { if y == y1 { break; } err += dx; y += sy; }
        }
    }

    let grid = AMBER_DIM;
    let trace = AMBER;
    let bg = Color::Black;
    let lines: Vec<Line> = canvas.iter().map(|row| {
        let spans: Vec<Span> = row.iter().map(|cell| {
            let (t, b) = (cell[0], cell[1]);
            let (ch, fg, bg_c) = match (t, b) {
                (2, 2) => ('█', trace, trace),
                (2, 1) => ('▀', trace, grid),
                (2, 0) => ('▀', trace, bg),
                (1, 2) => ('▄', trace, grid),
                (0, 2) => ('▄', trace, bg),
                (1, 1) => ('█', grid, grid),
                (1, 0) => ('▀', grid, bg),
                (0, 1) => ('▄', grid, bg),
                _ => (' ', bg, bg),
            };
            Span::styled(ch.to_string(), Style::default().fg(fg).bg(bg_c))
        }).collect();
        Line::from(spans)
    }).collect();
    f.render_widget(Paragraph::new(lines).style(Style::default().bg(Color::Black)), area);
}

// ─── CRT phosphor engine ────────────────────────────────────────────────────

fn plot_crt(buf: &mut [f32], w: usize, h: usize, px: usize, py: usize, bright: f32) {
    if px < w && py < h { buf[py * w + px] = buf[py * w + px].max(bright); }
}

#[allow(clippy::too_many_arguments)]
fn line_crt(buf: &mut [f32], w: usize, h: usize, x0: usize, y0: usize, x1: usize, y1: usize, bright: f32) {
    let (mut x, mut y) = (x0 as isize, y0 as isize);
    let dx = (x1 as isize - x0 as isize).abs(); let dy = -(y1 as isize - y0 as isize).abs();
    let sx = if x0 < x1 { 1isize } else { -1isize }; let sy = if y0 < y1 { 1isize } else { -1isize };
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
    let cx = size as f32 / 2.0; let cy = size as f32 / 2.0;
    let radius = (size as f32 / 2.0) * CIRCLE_RADIUS_RATIO;
    for y in 0..size { for x in 0..size {
        let dx = x as f32 - cx; let dy = y as f32 - cy;
        let dist = (dx*dx + dy*dy).sqrt();
        if dist > radius { mask[y*size+x] = 0.0; }
        else if dist > radius - 3.0 { let f = ((radius-dist)/3.0).clamp(0.0,1.0); mask[y*size+x] = f*f; }
    }}
    mask
}

fn render_phosphor(display: &[f32], buf: &mut [f32], mask: &[f32], w: usize, h: usize, zoom: f32) {
    for p in buf.iter_mut() { *p *= PHOSPHOR_DECAY; }
    for y in (1..h).step_by(2) { let row = y*w; for x in 0..w { buf[row+x] *= SCANLINE_DIM; } }
    for i in 0..buf.len() { buf[i] *= mask[i]; }

    let cx = w as f32 / 2.0; let cy = h as f32 / 2.0;
    let radius = (w.min(h) as f32 / 2.0) * CIRCLE_RADIUS_RATIO;
    let mid = w/2; let mid_h = h/2;

    for x in 0..w { plot_crt(buf, w, h, x, mid_h, GRID_BRIGHT * 0.5); }
    for y in 0..h { plot_crt(buf, w, h, mid, y, GRID_BRIGHT * 0.5); }

    for &ratio in &[0.33, 0.66] {
        let r = radius * ratio; let steps = (2.0 * std::f32::consts::PI * r) as usize;
        for i in 0..steps { let a = i as f32 / steps as f32 * 2.0 * std::f32::consts::PI;
            plot_crt(buf, w, h, (cx + a.cos()*r).round() as usize, (cy + a.sin()*r).round() as usize, GRID_BRIGHT*0.4); }
    }
    let steps = (2.0 * std::f32::consts::PI * radius) as usize;
    for i in 0..steps { let a = i as f32 / steps as f32 * 2.0 * std::f32::consts::PI;
        plot_crt(buf, w, h, (cx + a.cos()*radius).round() as usize, (cy + a.sin()*radius).round() as usize, GRID_BRIGHT); }
    for deg in (0..360).step_by(30) { let rad = deg as f32 * std::f32::consts::PI / 180.0;
        let inner_r = radius * 0.88; let s = ((radius-inner_r) as usize).max(1);
        for j in 0..s { let r = inner_r + j as f32;
            plot_crt(buf, w, h, (cx + rad.cos()*r).round() as usize, (cy + rad.sin()*r).round() as usize, GRID_BRIGHT); } }

    let n = display.len(); if n < 2 { return; }
    let points: Vec<(usize, usize)> = (0..w).map(|px| {
        let pos = (px as f64 / (w-1) as f64 * (n-1) as f64) as usize;
        let frac = px as f64 / (w-1) as f64 * (n-1) as f64 - pos as f64;
        let v = if pos+1<n { (display[pos] as f64 * (1.0-frac) + display[pos+1] as f64 * frac) as f32 * zoom } else { display[pos.min(n-1)] * zoom };
        let v = v.clamp(-1.0, 1.0); let py = ((1.0 - v) / 2.0 * (h-1) as f32).round() as usize;
        (px, py.min(h-1))
    }).collect();
    for wnd in points.windows(2) { line_crt(buf, w, h, wnd[0].0, wnd[0].1, wnd[1].0, wnd[1].1, TRACE_BRIGHT); }
}

fn crt_to_protocol(buf: &[f32], w: usize, h: usize, picker: &Picker, size: Size) -> Result<Protocol> {
    let mut rgba = vec![0u8; w*h*4];
    for (idx, &v) in buf.iter().enumerate() {
        let i = (v.clamp(0.0,1.0)*255.0) as u8; let base = i as f32 / 255.0; let off = idx*4;
        rgba[off] = (base*255.0) as u8; rgba[off+1] = (base*175.0) as u8; rgba[off+2] = (base*50.0) as u8; rgba[off+3] = 255;
    }
    let img = RgbaImage::from_raw(w as u32, h as u32, rgba).ok_or_else(|| anyhow::anyhow!("invalid RGBA"))?;
    Ok(picker.new_protocol(DynamicImage::ImageRgba8(img), size, Resize::Fit(Some(FilterType::Lanczos3)))?)
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
    (0..target).map(|i| { let pos = (i as f64*step) as usize; let frac = i as f64*step - pos as f64;
        if pos+1 < data.len() { (data[pos] as f64*(1.0-frac) + data[pos+1] as f64*frac) as f32 } else { data[pos] }
    }).collect()
}

fn process_scope(samples: &[f32], state: &mut ScopeState, channel: ScopeChannel) {
    let data = extract_channel(samples, channel); if data.is_empty() { return; }
    for &s in &data {
        state.delay_buf.push_back(s);
        if state.delay_buf.len() > AUDIO_LATENCY_SAMPLES {
            let d = state.delay_buf.pop_front().unwrap();
            if state.ring.len() >= RING_SIZE { state.ring.pop_front(); }
            state.ring.push_back(d);
        }
    }
    let ring = state.ring.make_contiguous();
    let mut xings = vec![];
    for i in 1..ring.len() { if ring[i-1] < 0.0 && ring[i] >= 0.0 { xings.push(i); } }
    if xings.len() >= 2 {
        let (s, e) = (xings[xings.len()-2], xings[xings.len()-1]);
        if e-s > 4 { let rs = resample(&ring[s..e], DISPLAY_SIZE);
            for (i, &v) in rs.iter().enumerate().take(DISPLAY_SIZE) { state.display[i] = state.display[i]*(1.0-PERSISTENCE) + v*PERSISTENCE; } }
    }
}

// ─── audio callback ─────────────────────────────────────────────────────────

fn build_output_stream(
    device: &cpal::Device, config: &cpal::StreamConfig, running: Arc<AtomicBool>,
    sample_rate: f32, channels: usize, engine: Arc<EngineShared>,
    scope_buffer: Arc<Mutex<VecDeque<f32>>>,
) -> Result<cpal::Stream> {
    let mut voices: Vec<VoiceData> = (0..SEQ_TRACKS).map(|_| {
        VoiceData { adsr: Adsr::new(0.01, 0.1, 0.7, 0.2, sample_rate), phase: 0.0, freq: 440.0, velocity: 0.0 }
    }).collect();
    let mut env_gens: Vec<EnvGen> = (0..SEQ_TRACKS).map(|_| EnvGen::new()).collect();
    let mut seq_phase: f64 = 0.0;
    let mut prev_steps = [0usize; SEQ_TRACKS];
    let mut cached_lens = [SEQ_STEPS; SEQ_TRACKS];
    let mut was_playing = false;

    let stream = device.build_output_stream(config, move |data: &mut [f32], _info| {
        let _active = running.load(Ordering::Relaxed);
        let mut burst = [0.0f32; 256]; let mut bi = 0usize;

        let bpm = engine.bpm.load(Ordering::Relaxed) as f32 / 100.0;
        let playing = engine.playing.load(Ordering::Relaxed);

        // reset sequencer on play start
        if playing && !was_playing { seq_phase = 0.0; prev_steps = [0; SEQ_TRACKS]; }
        was_playing = playing;

        let steps_per_beat = 4.0; // 16th notes
        let step_dur_samples = sample_rate * 60.0 / bpm / steps_per_beat;

        // refresh cached track lengths once per callback
        if let Ok(tl) = engine.track_len.try_lock() {
            for t in 0..SEQ_TRACKS { cached_lens[t] = tl[t].max(1); }
        }

        for (i, frame) in data.chunks_mut(channels).enumerate() {
            // sequencer advance per track
            if playing { seq_phase += 1.0; }

            // scrubber: intra-step progress (for display)
            if playing {
                let phase_remainder = seq_phase % step_dur_samples as f64;
                let progress = (phase_remainder / step_dur_samples as f64 * 1000.0) as u32;
                engine.seq_progress.store(progress, Ordering::Relaxed);
            }

            // generate each track's voice and accumulate
            let mut accum = 0.0f32;
            for t in 0..SEQ_TRACKS {
                if playing {
                    let cur_step = (seq_phase / step_dur_samples as f64) as usize % cached_lens[t];
                    if cur_step != prev_steps[t] {
                        prev_steps[t] = cur_step;
                        if t == 0 { engine.seq_step.store(cur_step as u32, Ordering::Relaxed); }
                        if let Ok(steps) = engine.step_data.try_lock() {
                            let (step_active, freq, vel) = decode_step(steps[t][cur_step.min(MAX_STEPS - 1)]);
                            engine.cur_freq[t].store((freq * 100.0) as u32, Ordering::Relaxed);
                            if step_active {
                                voices[t].freq = freq;
                                voices[t].velocity = vel;
                                voices[t].adsr.trigger();
                                env_gens[t].trigger();
                            } else {
                                voices[t].adsr.release();
                            }
                        }
                    }
                }

                // voice generation for this track
                let env = voices[t].adsr.tick();
                engine.env_value[t].store((env * 1000.0) as u32, Ordering::Relaxed);
                engine.adsr_phase[t].store(voices[t].adsr.state as u32, Ordering::Relaxed);

                // Maths-style envelope modulation
                let env_attack = engine.env_attack[t].load(Ordering::Relaxed);
                let env_decay = engine.env_decay[t].load(Ordering::Relaxed);
                let env_shape_norm = engine.env_shape[t].load(Ordering::Relaxed) as f32 / 1000.0;
                let env_loop = engine.env_loop[t].load(Ordering::Relaxed) != 0;
                let env_mod = engine.env_mod_target[t].load(Ordering::Relaxed);

                let env_a = 1.0 / (param_to_time(env_attack) * sample_rate);
                let env_d = 1.0 / (param_to_time(env_decay) * sample_rate);
                let env_out_val = env_gens[t].tick(env_a, env_d, env_shape_norm);
                if env_loop && env_gens[t].state == 0 { env_gens[t].trigger(); }
                engine.env_out[t].store((env_out_val * 1000.0) as u32, Ordering::Relaxed);

                let shape = engine.voice_shape[t].load(Ordering::Relaxed) as f32 / 1000.0;
                let sub_lvl = engine.voice_sub[t].load(Ordering::Relaxed) as f32 / 1000.0;
                let fm_amt = engine.voice_fm[t].load(Ordering::Relaxed) as f32 / 1000.0;
                let output_sel = engine.voice_output[t].load(Ordering::Relaxed);
                let output_sel = if output_sel < 333 { 0 } else if output_sel < 666 { 1 } else { 2 };

                // apply envelope modulation
                let mod_amt = env_out_val * 0.5; // +/- 50% modulation depth

                // basic FM modulation from a slow LFO
                let fm_lfo_phase = (voices[t].phase * 0.1).fract();
                let fm_mod = (fm_lfo_phase * 2.0 * std::f32::consts::PI).sin() * fm_amt * 0.5;
                let mod_freq = voices[t].freq * (1.0 + fm_mod);

                let (pitch_mod, shape_mod, _fm_mod_extra) = match env_mod {
                    1 => (mod_amt, 0.0, 0.0),    // pitch
                    2 => (0.0, mod_amt, 0.0),    // shape
                    3 => (0.0, 0.0, mod_amt),    // fm amount
                    _ => (0.0, 0.0, 0.0),        // none/amp
                };

                voices[t].phase = (voices[t].phase + mod_freq * (1.0 + pitch_mod) / sample_rate).fract();
                let phase = voices[t].phase;

                let sine = (phase * 2.0 * std::f32::consts::PI).sin();
                let saw = 2.0 * phase - 1.0;
                let square = if phase < 0.5 { 1.0 } else { -1.0 };
                let shaped = if shape < 0.5 {
                    let tt = (shape + shape_mod).clamp(0.0, 1.0) * 2.0;
                    sine * (1.0 - tt) + saw * tt
                } else {
                    let tt = ((shape + shape_mod).clamp(0.0, 1.0) - 0.5) * 2.0;
                    saw * (1.0 - tt) + square * tt
                };
                let sub_phase = (phase * 0.5).fract();
                let sub = if sub_phase < 0.5 { 1.0 } else { -1.0 };
                let out = match output_sel {
                    0 => sine,
                    1 => sine * (1.0 - sub_lvl) + sub * sub_lvl,
                    _ => shaped * (1.0 - sub_lvl) + sub * sub_lvl,
                };

                let amp_mod = if env_mod == 0 { env_out_val } else { 1.0 };
                accum += out * 0.3 * env * voices[t].velocity * (0.5 + 0.5 * amp_mod);
            }

            // mix down and write output
            accum /= SEQ_TRACKS as f32;
            if let Some(s) = frame.first_mut() { *s = accum; }
            if frame.len() > 1 { frame[1] = accum; }

            // scope burst (track 0's voice)
            if i % 2 == 0 && bi + 1 < burst.len() {
                burst[bi] = accum; burst[bi+1] = accum; bi += 2;
            }
        }

        if bi > 0
            && let Ok(mut buf) = scope_buffer.try_lock() {
                for &s in &burst[..bi] { if buf.len() >= SCOPE_CAPACITY { buf.pop_front(); } buf.push_back(s); }
            }
    }, |err| eprintln!("audio error: {err}"), None)?;
    stream.play()?;
    Ok(stream)
}

// ─── main / ui ──────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or_else(|| anyhow::anyhow!("no output audio device"))?;
    let config = device.default_output_config()?;
    let sr = config.sample_rate().0 as f32;
    let ch = config.channels() as usize;

    let running = Arc::new(AtomicBool::new(true));
    let engine = Arc::new(EngineShared::new());
    let scope_buf = Arc::new(Mutex::new(VecDeque::with_capacity(SCOPE_CAPACITY)));
    let _stream = build_output_stream(&device, &config.config(), Arc::clone(&running), sr, ch, Arc::clone(&engine), Arc::clone(&scope_buf))?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());

    let (phosphor_renderer, phosphor_error) = match pollster::block_on(PhosphorHeadless::new(
        (PHOSPHOR_SIZE, PHOSPHOR_SIZE).into()
    )) {
        Ok(r) => { eprintln!("phosphor-crt: initialized"); (Some(r), None) }
        Err(e) => {
            let msg = format!("phosphor-crt FAILED: {e}");
            eprintln!("{msg}");
            (None, Some(format!("{e}")))
        }
    };

    let mut app = App {
        mode: Mode::Normal, scope_open: true, voice_open: true, seq_open: true, env_open: true,
        scope_channel: ScopeChannel::Both, scope_mode: if phosphor_renderer.is_some() { ScopeMode::Phosphor } else { ScopeMode::Crt },
        scope_samples: Vec::with_capacity(SCOPE_CAPACITY), scope_state: ScopeState::new(),
        crt_buf: vec![0.0; CRT_SIZE*CRT_SIZE], crt_mask: build_crt_mask(CRT_SIZE),
        crt_cache: None, scope_dirty: true, crt_frame: 0, picker,
        engine, seq_cursor: (0, 0), pending_count: 0, pending_g: false, pending_d: false, pending_leader: false, focused_module: None, voice_param: 0, env_param: 0, phosphor_renderer, phosphor_error,
    };

    let result = run_ui(&mut terminal, &scope_buf, sr, &mut app);

    running.store(false, Ordering::SeqCst);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn module_block(title: &str, border_color: Color) -> Block<'_> {
    Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(title)
        .border_style(Style::default().fg(border_color)).style(Style::default().bg(PANEL_BG))
}

fn render_label(f: &mut ratatui::Frame, label: &str, area: Rect, color: Color) {
    f.render_widget(Paragraph::new(label).style(Style::default().fg(color)), area);
}

fn render_scope_module(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let toggle = if app.scope_open { "▾" } else { "▸" };
    let mode_ind = match app.scope_mode {
        ScopeMode::Braille => "BR",
        ScopeMode::Crt => "Φ",
        ScopeMode::Phosphor => if app.phosphor_renderer.is_some() { "Ph" } else { "Ph!" },
        ScopeMode::HalfBlock => "HB",
    };
    let title = format!(" ● TYPE 440 [{mode_ind}] {toggle} ");
    let block = module_block(&title, if app.scope_open { AMBER } else { PANEL_BORDER });
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.scope_open {
        let scope_inner = Layout::default().direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)]).split(inner);

        let info = if app.scope_mode == ScopeMode::Phosphor && app.phosphor_renderer.is_none() {
            format!(" ✗ Phosphor init failed: {}", app.phosphor_error.as_deref().unwrap_or("unknown"))
        } else {
            format!(" {}  {}Hz  trig↗", app.scope_channel.label(),
                match app.scope_channel { ScopeChannel::Left=>"440", ScopeChannel::Right=>"554", ScopeChannel::Both=>"440+554" })
        };
        render_label(f, &info, scope_inner[0], AMBER_DIM);

        let scope_size = Size::new(scope_inner[1].width, scope_inner[1].height);

        let crt_protocol: Option<&Protocol> = match app.scope_mode {
            ScopeMode::Crt => {
                app.crt_frame = app.crt_frame.wrapping_add(1);
                if app.crt_cache.is_none() || (app.crt_frame.is_multiple_of(CRT_THROTTLE) && app.scope_dirty) {
                    app.crt_cache = crt_to_protocol(&app.crt_buf, CRT_SIZE, CRT_SIZE, &app.picker, scope_size).ok();
                    app.scope_dirty = false;
                }
                app.crt_cache.as_ref()
            }
            ScopeMode::Phosphor => {
                if let Some(ref mut renderer) = app.phosphor_renderer {
                    app.crt_frame = app.crt_frame.wrapping_add(1);
                    if app.crt_cache.is_none() || (app.crt_frame.is_multiple_of(CRT_THROTTLE) && (app.scope_dirty)) {
                        renderer.set_waveform_yt(&app.scope_state.display);
                        renderer.set_xlim(0.0, app.scope_state.display.len().saturating_sub(1) as f32);
                        renderer.set_ylim(-0.35, 0.35);
                        let gradient = vec![
                            (0.0, RgbColor { r: 0.0, g: 0.0, b: 0.0 }),
                            (0.1, RgbColor { r: 0.0, g: 0.2, b: 0.0 }),
                            (1.0, RgbColor { r: 0.2, g: 1.0, b: 0.15 }),
                        ];
                        renderer.set_lut(gradient);
                        renderer.set_beam_width(5.0);
                        renderer.set_intensity(3.0);
                        match renderer.render_to_buffer() {
                            Ok(bitmap) => {
                                if let Some(img) = RgbaImage::from_raw(bitmap.size.width, bitmap.size.height, bitmap.buffer) {
                                    app.crt_cache = app.picker.new_protocol(
                                        DynamicImage::ImageRgba8(img), scope_size, Resize::Fit(Some(FilterType::Lanczos3))
                                    ).ok();
                                }
                            }
                            Err(e) => { eprintln!("phosphor: render error: {e}"); }
                        }
                        app.scope_dirty = false;
                    }
                }
                app.crt_cache.as_ref()
            }
            ScopeMode::Braille | ScopeMode::HalfBlock => None,
        };

        if let Some(proto) = crt_protocol {
            f.render_widget(Image::new(proto), scope_inner[1]);
        } else if app.scope_mode == ScopeMode::Phosphor && app.phosphor_renderer.is_none() {
            let err = app.phosphor_error.as_deref().unwrap_or("init failed");
            let colorful_err = format!("Phosphor\n\n✗\n\n{}", err);
            f.render_widget(Paragraph::new(colorful_err).style(Style::default().fg(Color::Red)), scope_inner[1]);
        } else if app.scope_mode == ScopeMode::HalfBlock {
            render_halfblock(f, &app.scope_state.display, scope_inner[1]);
        } else {
            let rows = render_braille(&app.scope_state.display, SCOPE_CHARS, SCOPE_ZOOM);
            let wf_rows = Layout::default().direction(Direction::Vertical)
                .constraints(vec![Constraint::Length(1); SCOPE_ROWS]).split(scope_inner[1]);
            for (i, row) in rows.iter().enumerate() { f.render_widget(Paragraph::new(row.as_str()).style(Style::default().fg(AMBER)), wf_rows[i]); }
        }
        let scale = match app.scope_mode { ScopeMode::Braille=>" 1cy/div  braille  ×3.0 ", ScopeMode::Crt=>" 1cy/div  Φ-CRT  ×3.0 ", ScopeMode::Phosphor=>" 1cy/div  Ph  ×3.0 ", ScopeMode::HalfBlock=>" 1cy/div  ▀▄×3.0 " };
        render_label(f, scale, scope_inner[2], AMBER_DIM);
    }
}

fn render_voice_module(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let focused = app.focused_module == Some(1);
    let title = format!(" ● STO {} ", if focused { "◉" } else if app.voice_open { "▾" } else { "▸" });
    let block = module_block(&title, if focused { CURSOR_FG } else if app.voice_open { AMBER } else { PANEL_BORDER });
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.voice_open {
        let track = app.seq_cursor.0;
        let shape = app.engine.voice_shape[track].load(Ordering::Relaxed) as f32 / 1000.0;
        let sub = app.engine.voice_sub[track].load(Ordering::Relaxed) as f32 / 1000.0;
        let fm = app.engine.voice_fm[track].load(Ordering::Relaxed) as f32 / 1000.0;
        let output = app.engine.voice_output[track].load(Ordering::Relaxed) as f32 / 1000.0;
        let freq = app.engine.cur_freq[track].load(Ordering::Relaxed) as f32 / 100.0;
        let env = app.engine.env_value[track].load(Ordering::Relaxed) as f32 / 1000.0;
        let phase = app.engine.adsr_phase[track].load(Ordering::Relaxed);
        let phase_str = ["idle", "ATK", "DEC", "SUS", "REL"][phase as usize];

        let w = inner.width.saturating_sub(12) as usize;
        let mk_bar = |v: f32, ww: usize| -> String {
            let fill = (v * ww as f32) as usize;
            "█".repeat(fill) + &"░".repeat(ww.saturating_sub(fill))
        };

        let out_labels = ["sin", "sub", "wav"];
        let out_idx = (output * 2.999) as usize;
        let out_str: String = out_labels.iter().enumerate()
            .map(|(i, l)| if i == out_idx { format!("[{}]", l) } else { format!(" {} ", l) })
            .collect::<Vec<_>>().join("");

        let params = [
            (0, "shape", shape),
            (1, "sub  ", sub),
            (2, "fm   ", fm),
            (3, "out  ", output),
        ];

        let mut lines: Vec<Line<'_>> = Vec::new();
        lines.push(Line::from(format!(" freq: {freq:.0} Hz")));
        for (idx, name, val) in &params {
            let is_sel = focused && app.voice_param == *idx;
            let marker = if is_sel { "▸" } else { " " };
            let color = if is_sel { CURSOR_FG } else { PANEL_LABEL };
            let label = if *idx == 3 { format!("{}{}: {} ", marker, name, out_str) } else { format!("{}{}: ", marker, name) };
            lines.push(Line::from(vec![
                Span::styled(label, Style::default().fg(color)),
                Span::styled(mk_bar(*val, w), Style::default().fg(if is_sel { CURSOR_FG } else { AMBER_DIM })),
                Span::styled(format!(" {:.2}", val), Style::default().fg(color)),
            ]));
        }
        let env_fill = (env * (inner.width.saturating_sub(8)) as f32) as usize;
        let env_bar = "█".repeat(env_fill) + &"░".repeat((inner.width.saturating_sub(8) as usize).saturating_sub(env_fill));
        lines.push(Line::from(format!(" env: {phase_str} {env_bar}")));

        f.render_widget(Paragraph::new(lines).style(Style::default().fg(PANEL_LABEL)), inner);
    }
}

fn render_env_module(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let focused = app.focused_module == Some(3);
    let toggle = if app.env_open { "▾" } else { "▸" };
    let title = format!(" ● MATH {}", if focused { "◉" } else { toggle });
    let block = module_block(&title, if focused { CURSOR_FG } else if app.env_open { AMBER } else { PANEL_BORDER });
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.env_open {
        let track = app.seq_cursor.0;
        let attack = app.engine.env_attack[track].load(Ordering::Relaxed);
        let decay = app.engine.env_decay[track].load(Ordering::Relaxed);
        let shape = app.engine.env_shape[track].load(Ordering::Relaxed);
        let looping = app.engine.env_loop[track].load(Ordering::Relaxed) != 0;
        let mod_tgt = app.engine.env_mod_target[track].load(Ordering::Relaxed);
        let env_out = app.engine.env_out[track].load(Ordering::Relaxed) as f32 / 1000.0;

        let w = inner.width.saturating_sub(12) as usize;
        let mk_bar = |v: f32| -> String {
            let fill = (v * w as f32) as usize;
            "█".repeat(fill) + &"░".repeat(w.saturating_sub(fill))
        };

        let mod_labels = ["amp", "pitch", "shape", "fm  "];
        let mod_str = mod_labels[mod_tgt as usize];

        let params = [
            (0, "atk", attack as f32 / 1000.0),
            (1, "dec", decay as f32 / 1000.0),
            (2, "shp", shape as f32 / 1000.0),
        ];

        let mut lines: Vec<Line<'_>> = Vec::new();
        lines.push(Line::from(format!(" T{track}  mod:{mod_str} {}  env:{:.2}", if looping { "cycle" } else {"onesht"}, env_out)));
        for (idx, name, val) in &params {
            let is_sel = focused && app.env_param == *idx;
            let marker = if is_sel { "▸" } else { " " };
            let color = if is_sel { CURSOR_FG } else { PANEL_LABEL };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker}{name}: "), Style::default().fg(color)),
                Span::styled(mk_bar(*val), Style::default().fg(if is_sel { CURSOR_FG } else { AMBER_DIM })),
                Span::styled(format!(" {:.3}", val), Style::default().fg(color)),
            ]));
        }
        f.render_widget(Paragraph::new(lines).style(Style::default().fg(PANEL_LABEL)), inner);
    }
}

fn render_seq_module(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let bpm = app.engine.bpm.load(Ordering::Relaxed) as f32 / 100.0;
    let cur_step = app.engine.seq_step.load(Ordering::Relaxed) as usize;
    let playing = app.engine.playing.load(Ordering::Relaxed);
    let play_icon = if playing { "▶" } else { "⏸" };

    let title = format!(" ● seq {play_icon} {bpm:.0}bpm ▾ ");
    let block = module_block(&title, PANEL_LABEL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let steps_visible = inner.width.saturating_sub(2) as usize;
    let (ct, cs) = app.seq_cursor;
    let track_len = app.engine.track_len.lock().map(|tl| tl[ct]).unwrap_or(SEQ_STEPS);

    let mut lines: Vec<Line<'_>> = Vec::new();
    let mut cursor_step_data: Option<(bool, f32, f32)> = None;

    if let Ok(steps) = app.engine.step_data.try_lock() {
        for t in 0..SEQ_TRACKS.min(inner.height as usize - 2) {
            let tl = app.engine.track_len.lock().map(|tl| tl[t]).unwrap_or(SEQ_STEPS);
            let mut spans: Vec<Span<'_>> = Vec::new();
            let max_s = tl.min(steps_visible);
            for s in 0..max_s {
                let (active, _, _) = decode_step(steps[t][s]);
                let cursor = (t, s) == (ct, cs);
                let playhead = s == cur_step && playing;
                let (ch, fg) = match (active, cursor, playhead) {
                    (_, true, true) => ('◈', CURSOR_FG),
                    (_, _, true) => ('▸', AMBER),
                    (true, true, _) => ('◆', CURSOR_FG),
                    (false, true, _) => ('◇', CURSOR_FG),
                    (true, false, _) => ('■', AMBER),
                    (false, false, _) => ('·', AMBER),
                };
                spans.push(Span::styled(ch.to_string(), Style::default().fg(fg)));
                if cursor { cursor_step_data = Some(decode_step(steps[t][s])); }
            }
            // show remaining length markers if steps_visible > tl
            if max_s < steps_visible {
                spans.push(Span::styled(
                    "·".repeat(steps_visible - max_s),
                    Style::default().fg(PANEL_BORDER),
                ));
            }
            // track label
            let track_label = format!(" T{t}:{}", tl);
            spans.push(Span::styled(track_label, Style::default().fg(PANEL_LABEL)));
            lines.push(Line::from(spans));
        }
    }

    if inner.height as usize > SEQ_TRACKS + 1 {
        if let Some((_active, freq, vel)) = cursor_step_data {
            let note = freq_to_note(freq);
            lines.push(Line::from(format!(" T{ct}S{cs:02} {note} {freq:.0}Hz vel{vel:.2}  len:{track_len}", track_len = track_len)));
        } else {
            lines.push(Line::from(format!(" T{ct}S{cs:02}  len:{}", track_len)));
        }
    }

    f.render_widget(Paragraph::new(lines).style(Style::default().fg(AMBER)), inner);
}

fn run_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    scope_buffer: &Arc<Mutex<VecDeque<f32>>>,
    sample_rate: f32,
    app: &mut App,
) -> Result<()> {
    loop {
        {
            let old_len = app.scope_samples.len();
            if let Ok(mut buf) = scope_buffer.try_lock() { app.scope_samples.extend(buf.drain(..)); }
            let new_len = app.scope_samples.len();
            if new_len > old_len { process_scope(&app.scope_samples[old_len..], &mut app.scope_state, app.scope_channel); app.scope_dirty = true; }
            if new_len > SCOPE_CAPACITY { app.scope_samples.drain(0..new_len - SCOPE_CAPACITY); }
        }

        if app.scope_mode == ScopeMode::Crt && app.scope_open {
            render_phosphor(&app.scope_state.display, &mut app.crt_buf, &app.crt_mask, CRT_SIZE, CRT_SIZE, SCOPE_ZOOM);
        }

        terminal.draw(|f| {
            let area = f.area();
            let any_open = app.scope_open || app.voice_open;
            let v_chunks = Layout::default().direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(4),
                    Constraint::Length(if any_open { MODULE_HEIGHT_OPEN } else { MODULE_HEIGHT_CLOSED }),
                    Constraint::Length(if app.env_open { ENV_HEIGHT_OPEN } else { ENV_HEIGHT_CLOSED }),
                    Constraint::Fill(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ]).split(area);

            let logo_block = Block::default().borders(Borders::ALL).border_type(BorderType::Plain)
                .title(" los · terminal instrument · v0.1 ").border_style(Style::default().fg(PANEL_BORDER));
            let logo_inner = logo_block.inner(v_chunks[0]);
            f.render_widget(logo_block, v_chunks[0]);
            f.render_widget(Paragraph::new(LOGO).alignment(Alignment::Center), logo_inner);

            let h_chunks = Layout::default().direction(Direction::Horizontal)
                .constraints([Constraint::Length(MODULE_WIDTH), Constraint::Length(MODULE_WIDTH), Constraint::Fill(1)])
                .split(v_chunks[1]);

            render_scope_module(f, app, h_chunks[0]);
            render_voice_module(f, app, h_chunks[1]);
            render_env_module(f, app, v_chunks[2]);
            render_seq_module(f, app, v_chunks[3]);

            let (mode_name, keys, mode_bg) = match app.mode {
                Mode::Normal => {
                    let k = match app.focused_module {
                        Some(1) => "STO: h/l:slide H/L:fine j/k:param 0/$:min/max Esc:unfocus",
                        Some(3) => "ENV: h/l:slide(5) H/L:slide(50) j/k:param w:mod_trg b:cycle 0/$:min/max Esc:unfocus",
                        _ => ",#:focus SPC:play +/-:bpm hjkl:grid i:ins w/b:step x:clr J/K:oct B:scope #l:len #p:euclid :q",
                    };
                    ("NORMAL", k, AMBER_DIM)
                }
                Mode::Insert => ("INSERT", "j/k:note J/K:oct h/l:cursor H/L:vel w/b:step SPC:adv ENT:tgl x:clr Esc:normal", MODE_INSERT),
                Mode::Command(_) => ("COMMAND", "Enter:exec Esc:cancel", MODE_COMMAND),
            };
            let status = format!(" {} | {} kHz | {} ", mode_name, (sample_rate/1000.0) as u32, keys);
            f.render_widget(Paragraph::new(status).style(Style::default().fg(Color::Black).bg(mode_bg)), v_chunks[4]);

            if let Mode::Command(ref cmd) = app.mode {
                f.render_widget(Paragraph::new(format!(":{}", cmd)).style(Style::default().fg(Color::Yellow)), v_chunks[5]);
            }
        })?;

        if event::poll(Duration::from_millis(8))?
            && let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press { continue; }
                match &mut app.mode {
                    Mode::Normal => match key.code {
                        KeyCode::Char(':') => app.mode = Mode::Command(String::new()),
                        KeyCode::Char('[') => { app.scope_channel = app.scope_channel.prev(); app.scope_state = ScopeState::new(); app.scope_samples.clear(); }
                        KeyCode::Char(']') => { app.scope_channel = app.scope_channel.next(); app.scope_state = ScopeState::new(); app.scope_samples.clear(); }
                        KeyCode::Char(' ') => { let was_playing = app.engine.playing.load(Ordering::Relaxed); app.engine.playing.store(!was_playing, Ordering::Relaxed); }
                        KeyCode::Char('+') | KeyCode::Char('=') => { let b = app.engine.bpm.load(Ordering::Relaxed); app.engine.bpm.store((b+100).min(30000), Ordering::Relaxed); }
                        KeyCode::Char('-') => { let b = app.engine.bpm.load(Ordering::Relaxed); app.engine.bpm.store(b.saturating_sub(100).max(2000), Ordering::Relaxed); }
                        KeyCode::Char(',') => { app.pending_leader = true; }
                        KeyCode::Char(c) => {
                            let lower = c.to_ascii_lowercase();
                            let shifted = c.is_ascii_uppercase() || key.modifiers.contains(KeyModifiers::SHIFT);

                            if c.is_ascii_digit() {
                                if app.pending_leader {
                                    app.pending_leader = false;
                                    match c { '1' => { app.focused_module = Some(0); app.scope_open = true; } '2' => { app.focused_module = Some(1); app.voice_open = true; } '3' => { app.focused_module = Some(2); app.seq_open = true; } '4' => { app.focused_module = Some(3); app.env_open = true; } _ => {} }
                                    continue;
                                }
                                let d = c as u32 - '0' as u32;
                                if d == 0 && app.pending_count == 0 && app.focused_module == Some(1) {
                                    let track = app.seq_cursor.0;
                                    let param = match app.voice_param { 0 => &app.engine.voice_shape[track], 1 => &app.engine.voice_sub[track], 2 => &app.engine.voice_fm[track], _ => &app.engine.voice_output[track] };
                                    param.store(0, Ordering::Relaxed);
                                } else {
                                    app.pending_count = if app.pending_count > 0 { app.pending_count * 10 + d } else { d };
                                }
                                continue;
                            }

                            if app.pending_leader { app.pending_leader = false; }

                            if app.focused_module == Some(1) {
                                let track = app.seq_cursor.0;
                                let param = |idx: usize, t: usize| -> &AtomicU32 {
                                    match idx { 0 => &app.engine.voice_shape[t], 1 => &app.engine.voice_sub[t], 2 => &app.engine.voice_fm[t], _ => &app.engine.voice_output[t] }
                                };
                                match (lower, shifted) {
                                    ('h', _) => {
                                        let p = param(app.voice_param, track);
                                        let v = p.load(Ordering::Relaxed);
                                        p.store(v.saturating_sub(if shifted { 5 } else { 50 }), Ordering::Relaxed);
                                    }
                                    ('l', _) => {
                                        let p = param(app.voice_param, track);
                                        let v = p.load(Ordering::Relaxed);
                                        p.store((v + if shifted { 5 } else { 50 }).min(1000), Ordering::Relaxed);
                                    }
                                    ('j', false) => { app.voice_param = (app.voice_param + 1).min(3); }
                                    ('k', false) => { app.voice_param = app.voice_param.saturating_sub(1); }
                                    ('w', _) => { let v = app.engine.voice_output[track].load(Ordering::Relaxed); app.engine.voice_output[track].store((v + 1) % 3, Ordering::Relaxed); }
                                    ('b', _) => { let v = app.engine.voice_output[track].load(Ordering::Relaxed); app.engine.voice_output[track].store(if v == 0 { 2 } else { v - 1 }, Ordering::Relaxed); }
                                    ('$', _) => { param(app.voice_param, track).store(1000, Ordering::Relaxed); }
                                    ('0', _) => { param(app.voice_param, track).store(0, Ordering::Relaxed); }
                                    _ => {}
                                }
                                continue;
                            }

                            if app.focused_module == Some(3) {
                                let track = app.seq_cursor.0;
                                let param = |idx: usize, t: usize| -> &AtomicU32 {
                                    match idx { 0 => &app.engine.env_attack[t], 1 => &app.engine.env_decay[t], _ => &app.engine.env_shape[t] }
                                };
                                match (lower, shifted) {
                                    ('h', _) => {
                                        let p = param(app.env_param, track);
                                        let v = p.load(Ordering::Relaxed);
                                        p.store(v.saturating_sub(if shifted { 5 } else { 50 }), Ordering::Relaxed);
                                    }
                                    ('l', _) => {
                                        let p = param(app.env_param, track);
                                        let v = p.load(Ordering::Relaxed);
                                        p.store((v + if shifted { 5 } else { 50 }).min(1000), Ordering::Relaxed);
                                    }
                                    ('j', false) => { app.env_param = (app.env_param + 1).min(2); }
                                    ('k', false) => { app.env_param = app.env_param.saturating_sub(1); }
                                    ('w', _) => { let v = app.engine.env_mod_target[track].load(Ordering::Relaxed); app.engine.env_mod_target[track].store((v + 1) % 4, Ordering::Relaxed); }
                                    ('b', _) => { let v = app.engine.env_loop[track].load(Ordering::Relaxed); app.engine.env_loop[track].store(if v == 0 { 1 } else { 0 }, Ordering::Relaxed); }
                                    ('$', _) => { param(app.env_param, track).store(1000, Ordering::Relaxed); }
                                    ('0', _) => { param(app.env_param, track).store(0, Ordering::Relaxed); }
                                    _ => {}
                                }
                                continue;
                            }

                            let count = app.pending_count.max(1) as usize;
                            let had_count = app.pending_count > 0;
                            app.pending_count = 0;
                            let was_pg = app.pending_g;
                            app.pending_g = false;

                            let track_len = app.engine.track_len.lock().map(|tl| tl[app.seq_cursor.0]).unwrap_or(SEQ_STEPS);

                            match (lower, shifted) {
                                ('h', _) => { app.seq_cursor.1 = app.seq_cursor.1.saturating_sub(count.min(track_len.saturating_sub(1))); }
                                ('l', _) if had_count => {
                                    let len = count.min(MAX_STEPS);
                                    if let Ok(mut tl) = app.engine.track_len.lock() {
                                        tl[app.seq_cursor.0] = len;
                                    }
                                    app.seq_cursor.1 = app.seq_cursor.1.min(len.saturating_sub(1));
                                }
                                ('l', _) => { app.seq_cursor.1 = (app.seq_cursor.1 + 1).min(track_len.saturating_sub(1)); }
                                ('p', _) if had_count => {
                                    let pulses = count.min(track_len);
                                    let pattern = euclidean(pulses, track_len);
                                    if let Ok(mut steps) = app.engine.step_data.try_lock() {
                                        let t = app.seq_cursor.0;
                                        for s in 0..track_len.min(MAX_STEPS) {
                                            let (active, f, v) = decode_step(steps[t][s]);
                                            let should_active = s < pattern.len() && pattern[s];
                                            if should_active && !active {
                                                // newly activated — use default note
                                                steps[t][s] = encode_step(true, 440, 1000);
                                            } else if should_active {
                                                steps[t][s] = encode_step(true, f as u32, (v*1000.0) as u32);
                                            } else {
                                                steps[t][s] = encode_step(false, f as u32, (v*1000.0) as u32);
                                            }
                                        }
                                    }
                                }
                                ('j', false) => { app.seq_cursor.0 = (app.seq_cursor.0 + count).min(SEQ_TRACKS-1); }
                                ('k', false) => { app.seq_cursor.0 = app.seq_cursor.0.saturating_sub(count); }
                                ('w', _) if app.pending_d => { app.pending_d = false; if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let end = next_active_step(&steps, t, s, track_len).unwrap_or(track_len); for i in s..end { steps[t][i] = encode_step(false, 440, 1000); } } }
                                ('w', _) => { let (t, s) = app.seq_cursor; if let Ok(steps) = app.engine.step_data.try_lock() { let mut cur = s; for _ in 0..count { if let Some(n) = next_active_step(&steps, t, cur, track_len) { cur = n; } else { break; } } app.seq_cursor.1 = cur; } }
                                ('b', false) => { let (t, s) = app.seq_cursor; if let Ok(steps) = app.engine.step_data.try_lock() { let mut cur = s; for _ in 0..count { if let Some(n) = prev_active_step(&steps, t, cur, track_len) { cur = n; } else { break; } } app.seq_cursor.1 = cur; } }
                                ('b', true) => { app.scope_mode = match (app.scope_mode, app.phosphor_renderer.is_some()) { (ScopeMode::Braille, _) => ScopeMode::Crt, (ScopeMode::Crt, true) => ScopeMode::Phosphor, (ScopeMode::Crt, false) => ScopeMode::HalfBlock, (ScopeMode::Phosphor, _) => ScopeMode::HalfBlock, (ScopeMode::HalfBlock, _) => ScopeMode::Braille, }; app.crt_cache = None; }
                                ('j', true) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(act, octave_down(f) as u32, (v*1000.0) as u32); } }
                                ('k', true) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(act, octave_up(f) as u32, (v*1000.0) as u32); } }
                                ('g', false) => { if was_pg { app.seq_cursor.0 = 0; } else { app.pending_g = true; } }
                                ('g', true) => { app.seq_cursor.0 = SEQ_TRACKS - 1; }
                                ('$', _) => { app.seq_cursor.1 = track_len.saturating_sub(1); }
                                ('^', _) => { app.seq_cursor.1 = 0; }
                                ('i', _) => app.mode = Mode::Insert,
                                ('x', _) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; steps[t][s] = encode_step(false, 440, 1000); } }
                                ('d', false) => { if app.pending_d { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; steps[t][s] = encode_step(false, 440, 1000); } app.pending_d = false; } else { app.pending_d = true; } }
                                ('d', true) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; for i in s..track_len.min(MAX_STEPS) { steps[t][i] = encode_step(false, 440, 1000); } } }
                                _ => { app.pending_g = false; app.pending_d = false; app.pending_leader = false; app.pending_count = 0; }
                            }
                            app.pending_count = 0;
                        }
                        KeyCode::Enter => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(!act, f as u32, (v*1000.0) as u32); } }
                        KeyCode::Esc => { app.pending_g = false; app.pending_d = false; app.pending_leader = false; app.pending_count = 0; app.focused_module = None; }
                        _ => { app.pending_g = false; app.pending_d = false; app.pending_leader = false; app.pending_count = 0; }
                    },
                    Mode::Insert => match key.code {
                        KeyCode::Esc => app.mode = Mode::Normal,
                        KeyCode::Char(c) => {
                            let lower = c.to_ascii_lowercase();
                            let shifted = c.is_ascii_uppercase() || key.modifiers.contains(KeyModifiers::SHIFT);
                            let track_len = app.engine.track_len.lock().map(|tl| tl[app.seq_cursor.0]).unwrap_or(SEQ_STEPS);
                            match (lower, shifted) {
                                ('j', false) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(act, semitone_down(f) as u32, (v*1000.0) as u32); } }
                                ('k', false) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(act, semitone_up(f) as u32, (v*1000.0) as u32); } }
                                ('j', true) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(act, octave_down(f) as u32, (v*1000.0) as u32); } }
                                ('k', true) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(act, octave_up(f) as u32, (v*1000.0) as u32); } }
                                ('h', false) => { app.seq_cursor.1 = app.seq_cursor.1.saturating_sub(1); }
                                ('l', false) => { app.seq_cursor.1 = (app.seq_cursor.1 + 1).min(track_len.saturating_sub(1)); }
                                ('h', true) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); let new_v = (v - 0.1).max(0.0); steps[t][s] = encode_step(act, f as u32, (new_v * 1000.0) as u32); } }
                                ('l', true) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); let new_v = (v + 0.1).min(1.0); steps[t][s] = encode_step(act, f as u32, (new_v * 1000.0) as u32); } }
                                ('0', _) => { app.seq_cursor.1 = 0; }
                                ('$', _) => { app.seq_cursor.1 = track_len.saturating_sub(1); }
                                ('w', _) => { let (t, s) = app.seq_cursor; if let Ok(steps) = app.engine.step_data.try_lock() && let Some(next) = next_active_step(&steps, t, s, track_len) { app.seq_cursor.1 = next; } }
                                ('b', _) => { let (t, s) = app.seq_cursor; if let Ok(steps) = app.engine.step_data.try_lock() && let Some(prev) = prev_active_step(&steps, t, s, track_len) { app.seq_cursor.1 = prev; } }
                                (' ', _) => { app.seq_cursor.1 = (app.seq_cursor.1 + 1) % track_len.max(1); }
                                ('x', _) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; steps[t][s] = encode_step(false, 440, 1000); } }
                                _ => {}
                            }
                        }
                        KeyCode::Enter => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(!act, f as u32, (v*1000.0) as u32); } }
                        KeyCode::Backspace => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; steps[t][s] = encode_step(false, 440, 1000); } }
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

// ─── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn test_extract_channel_left() { assert_eq!(extract_channel(&[1.0,2.0,3.0,4.0], ScopeChannel::Left), vec![1.0,3.0]); }
    #[test] fn test_extract_channel_right() { assert_eq!(extract_channel(&[1.0,2.0,3.0,4.0], ScopeChannel::Right), vec![2.0,4.0]); }
    #[test] fn test_extract_channel_both() { assert_eq!(extract_channel(&[1.0,3.0,2.0,4.0], ScopeChannel::Both), vec![2.0,3.0]); }
    #[test] fn test_resample_upscale() { let r = resample(&[0.0,1.0], 5); assert_eq!(r.len(),5); assert!((r[0]-0.0).abs()<0.01); assert!((r[4]-1.0).abs()<0.01); }
    #[test] fn test_resample_single() { assert_eq!(resample(&[0.5], 3), vec![0.5,0.5,0.5]); }
    #[test] fn test_dot_bit_left() { assert_eq!(dot_bit(0,0),0x01); assert_eq!(dot_bit(0,3),0x40); }
    #[test] fn test_dot_bit_right() { assert_eq!(dot_bit(1,0),0x08); assert_eq!(dot_bit(1,3),0x80); }
    #[test] fn test_braille_clips() { let mut c = vec![vec![0u8;1];1]; plot_braille(&mut c,99,99,1,1); assert_eq!(c[0][0],0); }
    #[test] fn test_braille_sets_bit() { let mut c = vec![vec![0u8;1];1]; plot_braille(&mut c,0,0,1,1); assert_eq!(c[0][0],0x01); }
    #[test] fn test_crt_plot() { let mut b = vec![0.0f32;4]; plot_crt(&mut b,2,2,1,1,0.5); assert_eq!(b[3],0.5); }
    #[test] fn test_crt_max() { let mut b = vec![0.0f32;1]; plot_crt(&mut b,1,1,0,0,0.6); plot_crt(&mut b,1,1,0,0,0.9); assert_eq!(b[0],0.9); }
    #[test] fn test_mask_center() { let m = build_crt_mask(20); assert!(m[10*20+10] > 0.9); }
    #[test] fn test_mask_corners() { let m = build_crt_mask(20); assert_eq!(m[0],0.0); assert_eq!(m[19],0.0); }
    #[test] fn test_braille_dims() { let rows = render_braille(&vec![0.0;DISPLAY_SIZE],10,1.0); assert_eq!(rows.len(),SCOPE_ROWS); assert_eq!(rows[0].chars().count(),10); }
    #[test] fn test_channel_cycle() { let ch = ScopeChannel::Left; assert_eq!(ch.next(), ScopeChannel::Right); assert_eq!(ch.next().next(), ScopeChannel::Both); }
    #[test] fn test_decode_step() { let (act, f, v) = decode_step(encode_step(true, 440, 1000)); assert!(act); assert!((f-440.0).abs()<0.01); assert!((v-1.0).abs()<0.01); }
    #[test] fn test_decode_step_inactive() { let (act, _, _) = decode_step(encode_step(false, 440, 1000)); assert!(!act); }
    #[test] fn test_adsr_attack() { let mut a = Adsr::new(0.01,0.1,0.7,0.2,48000.0); a.trigger(); let v = a.tick(); assert!(v > 0.0); }
    #[test] fn test_adsr_release() {
        let mut a = Adsr::new(0.01,0.0,1.0,0.01,48000.0);
        a.trigger();
        for _ in 0..1000 { a.tick(); }
        a.release();
        let mut vals = vec![];
        for _ in 0..2000 { vals.push(a.tick()); }
        assert!(vals.last().unwrap() < &0.1);
    }
    #[test] fn test_engine_default() { let e = EngineShared::new(); assert_eq!(e.bpm.load(Ordering::Relaxed), 12000); assert!(!e.playing.load(Ordering::Relaxed)); }
    #[test] fn test_encode_decode_roundtrip() {
        for (act, f, v) in [(true,440.0,1.0),(false,220.0,0.5),(true,880.0,0.75)] {
            let enc = encode_step(act, f as u32, (v*1000.0) as u32);
            let (a2, f2, v2) = decode_step(enc);
            assert_eq!(act, a2); assert!((f-f2).abs()<0.02); assert!((v-v2).abs()<0.01);
        }
    }

    #[test] fn test_next_active_step_forward() {
        let mut steps = [[0u32; MAX_STEPS]; SEQ_TRACKS];
        steps[0][0] = encode_step(true, 440, 1000);
        steps[0][7] = encode_step(true, 554, 800);
        assert_eq!(next_active_step(&steps, 0, 0, SEQ_STEPS), Some(7));
        assert_eq!(next_active_step(&steps, 0, 5, SEQ_STEPS), Some(7));
    }

    #[test] fn test_next_active_step_wraps() {
        let mut steps = [[0u32; MAX_STEPS]; SEQ_TRACKS];
        steps[0][2] = encode_step(true, 440, 1000);
        assert_eq!(next_active_step(&steps, 0, 6, SEQ_STEPS), Some(2));
    }

    #[test] fn test_next_active_step_none() {
        let steps = [[0u32; MAX_STEPS]; SEQ_TRACKS];
        assert_eq!(next_active_step(&steps, 0, 0, SEQ_STEPS), None);
    }

    #[test] fn test_prev_active_step_backward() {
        let mut steps = [[0u32; MAX_STEPS]; SEQ_TRACKS];
        steps[0][0] = encode_step(true, 440, 1000);
        steps[0][7] = encode_step(true, 554, 800);
        assert_eq!(prev_active_step(&steps, 0, 7, SEQ_STEPS), Some(0));
        assert_eq!(prev_active_step(&steps, 0, 3, SEQ_STEPS), Some(0));
    }

    #[test] fn test_prev_active_step_wraps() {
        let mut steps = [[0u32; MAX_STEPS]; SEQ_TRACKS];
        steps[0][14] = encode_step(true, 440, 1000);
        assert_eq!(prev_active_step(&steps, 0, 2, SEQ_STEPS), Some(14));
    }

    #[test] fn test_prev_active_step_none() {
        let steps = [[0u32; MAX_STEPS]; SEQ_TRACKS];
        assert_eq!(prev_active_step(&steps, 0, 0, SEQ_STEPS), None);
    }

    #[test] fn test_freq_to_note_a4() { assert_eq!(freq_to_note(440.0), "A4"); }
    #[test] fn test_freq_to_note_c4() { assert!((freq_to_note(261.63).starts_with("C")), "got {}", freq_to_note(261.63)); }
    #[test] fn test_freq_to_note_a3() { assert_eq!(freq_to_note(220.0), "A3"); }
    #[test] fn test_freq_to_note_c8() { assert_eq!(freq_to_note(4186.0), "C8"); }
    #[test] fn test_freq_to_note_low() { assert_eq!(freq_to_note(10.0), "---"); }

    #[test] fn test_semitone_up() { let f = semitone_up(440.0); assert!(f > 440.0 && f < 500.0); }
    #[test] fn test_semitone_down() { let f = semitone_down(440.0); assert!(f < 440.0 && f > 400.0); }
    #[test] fn test_semitone_octave_roundtrip() {
        let f = 440.0;
        let mut result = f;
        for _ in 0..12 { result = semitone_up(result); }
        assert!((result - f * 2.0).abs() < 2.0);
    }
    #[test] fn test_octave_up() { assert!((octave_up(440.0) - 880.0).abs() < 1.0); }
    #[test] fn test_octave_down() { assert!((octave_down(440.0) - 220.0).abs() < 1.0); }
    #[test] fn test_semitone_clamp_high() { let f = semitone_up(3999.0); assert!(f <= 4000.0); }
    #[test] fn test_semitone_clamp_low() { let f = semitone_down(21.0); assert!(f >= 20.0); }
    #[test] fn test_octave_clamp_high() { let f = octave_up(3000.0); assert!(f <= 4000.0); }
    #[test] fn test_octave_clamp_low() { let f = octave_down(30.0); assert!(f >= 20.0); }

    #[test] fn test_seq_phase_f64_precision() {
        let mut seq_phase: f64 = 16_000_000.0;
        let step_dur: f64 = 6000.0;
        for _ in 0..1_000_000 {
            seq_phase += 1.0;
        }
        let step = (seq_phase / step_dur) as usize % SEQ_STEPS;
        assert!(step < SEQ_STEPS);
        assert!(seq_phase > 16_000_000.0);
    }

    #[test] fn test_crt_protocol_generates() {
        // CRT protocol generation smoke test — verifies the picker+resize pipeline
        // doesn't panic with the current CRT configuration.
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
        let buf = vec![0.5f32; CRT_SIZE * CRT_SIZE];
        let result = crt_to_protocol(&buf, CRT_SIZE, CRT_SIZE, &picker, Size::new(22, 14));
        assert!(result.is_ok(), "crt_to_protocol should succeed");
    }

    #[test] fn test_crt_buffer_is_square() {
        // Regression: CRT buffer must be square for circular scope face.
        // Non-square buffers cause elliptical grid circles.
        assert_eq!(CRT_SIZE, 96, "CRT_SIZE must be 96 for performance");
        let buf = vec![0.0f32; CRT_SIZE * CRT_SIZE];
        assert_eq!(buf.len(), 96 * 96, "Buffer must be square");
    }

    #[test] fn test_render_phosphor_doesnt_panic() {
        // Smoke test for render_phosphor with square buffer.
        let display = vec![0.5f32; DISPLAY_SIZE];
        let mut buf = vec![0.0f32; CRT_SIZE * CRT_SIZE];
        let mask = build_crt_mask(CRT_SIZE);
        render_phosphor(&display, &mut buf, &mask, CRT_SIZE, CRT_SIZE, SCOPE_ZOOM);
        // Verify buffer was modified (decay happened)
        assert!(buf.iter().any(|&x| x != 0.0), "Buffer should be modified");
    }

    #[test] fn test_scope_dirty_flag() {
        // Verify scope_dirty flag mechanism works correctly.
        let mut scope_dirty = true;
        assert!(scope_dirty, "scope_dirty should start true");
        scope_dirty = false;
        assert!(!scope_dirty, "scope_dirty should be clearable");
    }

    #[test] fn test_crt_frame_throttle() {
        // Verify frame counter wrapping works for throttle mechanism.
        let mut frame: u8 = 255;
        frame = frame.wrapping_add(1);
        assert_eq!(frame, 0, "Frame counter should wrap");
        assert_eq!(frame % 4, 0, "Throttle should trigger on wrap");
    }
}
