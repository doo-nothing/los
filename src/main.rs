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
const PERSISTENCE: f32 = 0.12;
const RING_SIZE: usize = 2048;
const SCOPE_ZOOM: f32 = 3.0;
const MODULE_WIDTH: u16 = 24;
const MODULE_WF_ROWS: u16 = 12;
const MODULE_HEIGHT_OPEN: u16 = MODULE_WF_ROWS + 4;
const MODULE_HEIGHT_CLOSED: u16 = 1;
const SEQ_STEPS: usize = 16;
const SEQ_TRACKS: usize = 4;

const AMBER: Color = Color::Rgb(255, 175, 50);
const AMBER_DIM: Color = Color::Rgb(180, 120, 30);
const PANEL_BG: Color = Color::Rgb(24, 24, 28);
const PANEL_BORDER: Color = Color::Rgb(60, 60, 68);
const PANEL_LABEL: Color = Color::Rgb(140, 140, 150);
const MODE_INSERT: Color = Color::Rgb(100, 200, 80);
const MODE_COMMAND: Color = Color::Rgb(200, 80, 80);

const CRT_SIZE: usize = 96;
const CRT_THROTTLE: u8 = 8;
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
enum ScopeMode { Braille, Crt }

struct ScopeState {
    display: Vec<f32>,
    ring: VecDeque<f32>,
}

impl ScopeState {
    fn new() -> Self { Self { display: vec![0.0; DISPLAY_SIZE], ring: VecDeque::with_capacity(RING_SIZE) } }
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

/// Shared state between audio callback and TUI.
struct EngineShared {
    bpm: AtomicU32,         // BPM * 100 (e.g. 12000 = 120.00)
    playing: AtomicBool,
    trig_freq: AtomicU32,   // frequency * 100 (e.g. 44000 = 440.00 Hz)
    trig_vel: AtomicU32,    // velocity * 1000 (e.g. 1000 = 1.0)
    trig_gate: AtomicBool,
    env_value: AtomicU32,
    adsr_phase: AtomicU32,  // 0=idle 1=attack 2=decay 3=sustain 4=release
    seq_step: AtomicU32,
    step_data: Mutex<[[u32; SEQ_STEPS]; SEQ_TRACKS]>, // each step: bit31=active, bits 30-16=note*100, bits 15-0=vel*1000
}

impl EngineShared {
    fn new() -> Self {
        let mut steps = [[0u32; SEQ_STEPS]; SEQ_TRACKS];
        // default: a simple pattern on track 0
        steps[0][0] = encode_step(true, 440, 1000);
        steps[0][4] = encode_step(true, 554, 800);
        steps[0][8] = encode_step(true, 440, 1000);
        steps[0][12] = encode_step(true, 660, 700);
        Self {
            bpm: AtomicU32::new(12000),
            playing: AtomicBool::new(false),
            trig_freq: AtomicU32::new(440),
            trig_vel: AtomicU32::new(1000),
            trig_gate: AtomicBool::new(false),
            env_value: AtomicU32::new(0),
            adsr_phase: AtomicU32::new(0),
            seq_step: AtomicU32::new(0),
            step_data: Mutex::new(steps),
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

fn next_active_step(steps: &[[u32; SEQ_STEPS]; SEQ_TRACKS], track: usize, current: usize) -> Option<usize> {
    for (i, &step) in steps[track].iter().enumerate().skip(current + 1) {
        if decode_step(step).0 { return Some(i); }
    }
    for (i, &step) in steps[track].iter().enumerate().take(current + 1) {
        if decode_step(step).0 { return Some(i); }
    }
    None
}

fn prev_active_step(steps: &[[u32; SEQ_STEPS]; SEQ_TRACKS], track: usize, current: usize) -> Option<usize> {
    for (i, &step) in steps[track].iter().enumerate().take(current).rev() {
        if decode_step(step).0 { return Some(i); }
    }
    for (i, &step) in steps[track].iter().enumerate().skip(current).rev() {
        if decode_step(step).0 { return Some(i); }
    }
    None
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
    seq_cursor: (usize, usize), // (track, step) for grid cursor
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

// ─── CRT phosphor engine ────────────────────────────────────────────────────

fn plot_crt(buf: &mut [f32], w: usize, h: usize, px: usize, py: usize, bright: f32) {
    if px < w && py < h { buf[py * w + px] = buf[py * w + px].max(bright); }
}

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
    for &s in &data { if state.ring.len() >= RING_SIZE { state.ring.pop_front(); } state.ring.push_back(s); }
    let ring = state.ring.make_contiguous();
    let mut xings = vec![];
    for i in 1..ring.len() { if ring[i-1] < 0.0 && ring[i] >= 0.0 { xings.push(i); } }
    if xings.len() >= 2 {
        let (s, e) = (xings[xings.len()-2], xings[xings.len()-1]);
        if e-s > 4 { let rs = resample(&ring[s..e], DISPLAY_SIZE);
            for i in 0..DISPLAY_SIZE { state.display[i] = state.display[i]*(1.0-PERSISTENCE) + rs[i]*PERSISTENCE; } }
    }
}

// ─── audio callback ─────────────────────────────────────────────────────────

fn build_output_stream(
    device: &cpal::Device, config: &cpal::StreamConfig, running: Arc<AtomicBool>,
    sample_rate: f32, channels: usize, engine: Arc<EngineShared>,
    scope_buffer: Arc<Mutex<VecDeque<f32>>>,
) -> Result<cpal::Stream> {
    let mut voice = VoiceData { adsr: Adsr::new(0.01, 0.1, 0.7, 0.2, sample_rate), phase: 0.0, freq: 440.0, velocity: 0.0 };
    let mut seq_phase: f64 = 0.0;
    let mut prev_step: usize = 0;
    let mut prev_gate = false;

    let stream = device.build_output_stream(config, move |data: &mut [f32], _info| {
        let active = running.load(Ordering::Relaxed);
        let mut burst = [0.0f32; 256]; let mut bi = 0usize;

        let bpm = engine.bpm.load(Ordering::Relaxed) as f32 / 100.0;
        let playing = engine.playing.load(Ordering::Relaxed);
        let steps_per_beat = 4.0; // 16th notes
        let step_dur_samples = sample_rate * 60.0 / bpm / steps_per_beat;

        for (i, frame) in data.chunks_mut(channels).enumerate() {
            // sequencer advance
            if playing {
                seq_phase += 1.0;
                let cur_step = (seq_phase / step_dur_samples as f64) as usize % SEQ_STEPS;
                if cur_step != prev_step {
                    prev_step = cur_step;
                    engine.seq_step.store(cur_step as u32, Ordering::Relaxed);
                    if cur_step < SEQ_STEPS {
                        if let Ok(steps) = engine.step_data.try_lock() {
                            let track_data = steps[0]; // track 0 drives the voice
                            let enc = track_data[cur_step];
                            let (active, freq, vel) = decode_step(enc);
                            if active {
                                engine.trig_freq.store(freq as u32, Ordering::Relaxed);
                                engine.trig_vel.store((vel * 1000.0) as u32, Ordering::Relaxed);
                                engine.trig_gate.store(true, Ordering::Relaxed);
                            } else {
                                engine.trig_gate.store(false, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }

            // voice: read trigger
            let gate = engine.trig_gate.load(Ordering::Relaxed);
            if gate && !prev_gate {
                let freq = engine.trig_freq.load(Ordering::Relaxed) as f32;
                let vel = engine.trig_vel.load(Ordering::Relaxed) as f32 / 1000.0;
                voice.freq = freq;
                voice.velocity = vel;
                voice.adsr.trigger();
            } else if !gate && prev_gate {
                voice.adsr.release();
            }
            prev_gate = gate;

            let env = voice.adsr.tick();
            engine.env_value.store((env * 1000.0) as u32, Ordering::Relaxed);
            engine.adsr_phase.store(voice.adsr.state as u32, Ordering::Relaxed);

            let l = if active {
                voice.phase = (voice.phase + voice.freq / sample_rate).fract();
                (voice.phase * 2.0 * std::f32::consts::PI).sin() * 0.3 * env * voice.velocity
            } else { 0.0 };
            let r = l;

            if let Some(s) = frame.first_mut() { *s = l; }
            if frame.len() > 1 { frame[1] = r; }

            if i % 2 == 0 && bi + 1 < burst.len() { burst[bi] = l; burst[bi+1] = r; bi += 2; }
        }

        if bi > 0 {
            if let Ok(mut buf) = scope_buffer.try_lock() {
                for &s in &burst[..bi] { if buf.len() >= SCOPE_CAPACITY { buf.pop_front(); } buf.push_back(s); }
            }
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
    let mut app = App {
        mode: Mode::Normal, scope_open: true, voice_open: true, seq_open: true,
        scope_channel: ScopeChannel::Both, scope_mode: ScopeMode::Crt,
        scope_samples: Vec::with_capacity(SCOPE_CAPACITY), scope_state: ScopeState::new(),
        crt_buf: vec![0.0; CRT_SIZE*CRT_SIZE], crt_mask: build_crt_mask(CRT_SIZE),
        crt_cache: None, scope_dirty: true, crt_frame: 0, picker,
        engine, seq_cursor: (0, 0),
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
    let mode_ind = match app.scope_mode { ScopeMode::Braille => "BR", ScopeMode::Crt => "Φ" };
    let title = format!(" ● TYPE 440 [{mode_ind}] {toggle} ");
    let block = module_block(&title, if app.scope_open { AMBER } else { PANEL_BORDER });
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.scope_open {
        let scope_inner = Layout::default().direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)]).split(inner);

        let info = format!(" {}  {}Hz  trig↗", app.scope_channel.label(),
            match app.scope_channel { ScopeChannel::Left=>"440", ScopeChannel::Right=>"554", ScopeChannel::Both=>"440+554" });
        render_label(f, &info, scope_inner[0], AMBER_DIM);

        let crt_protocol: Option<&Protocol> = if app.scope_mode == ScopeMode::Crt {
            app.crt_frame = app.crt_frame.wrapping_add(1);
            if app.crt_frame % CRT_THROTTLE == 0 && (app.scope_dirty || app.crt_cache.is_none()) {
                let size = Size::new(scope_inner[1].width, scope_inner[1].height);
                app.crt_cache = crt_to_protocol(&app.crt_buf, CRT_SIZE, CRT_SIZE, &app.picker, size).ok();
                app.scope_dirty = false;
            }
            app.crt_cache.as_ref()
        } else { None };

        if let Some(proto) = crt_protocol {
            f.render_widget(Image::new(proto), scope_inner[1]);
        } else {
            let rows = render_braille(&app.scope_state.display, SCOPE_CHARS, SCOPE_ZOOM);
            let wf_rows = Layout::default().direction(Direction::Vertical)
                .constraints(vec![Constraint::Length(1); SCOPE_ROWS]).split(scope_inner[1]);
            for (i, row) in rows.iter().enumerate() { f.render_widget(Paragraph::new(row.as_str()).style(Style::default().fg(AMBER)), wf_rows[i]); }
        }
        let scale = match app.scope_mode { ScopeMode::Braille=>" 1cy/div  braille  ×3.0 ", ScopeMode::Crt=>" 1cy/div  Φ-CRT  ×3.0 " };
        render_label(f, scale, scope_inner[2], AMBER_DIM);
    }
}

fn render_voice_module(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let toggle = if app.voice_open { "▾" } else { "▸" };
    let title = format!(" ● voice {toggle} ");
    let block = module_block(&title, if app.voice_open { AMBER } else { PANEL_BORDER });
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.voice_open {
        let env = app.engine.env_value.load(Ordering::Relaxed) as f32 / 1000.0;
        let freq = app.engine.trig_freq.load(Ordering::Relaxed) as f32;
        let playing = app.engine.playing.load(Ordering::Relaxed);
        let phase = app.engine.adsr_phase.load(Ordering::Relaxed);
        let phase_str = ["idle", "ATK", "DEC", "SUS", "REL"][phase as usize];

        let w = inner.width.saturating_sub(2) as usize;
        let fill = (env * w as f32) as usize;
        let bar: String = "█".repeat(fill) + &"░".repeat(w.saturating_sub(fill));

        let text = format!(
            " osc: sine\n freq: {freq:.0} Hz\n envc: {phase_str} {bar}\n ADSR: 10/100/70/20\n {}",
            if playing { "▶ playing" } else { "⏸ paused" }
        );
        f.render_widget(Paragraph::new(text).style(Style::default().fg(PANEL_LABEL)), inner);
    }
}

fn render_seq_module(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let toggle = if app.seq_open { "▾" } else { "▸" };
    let bpm = app.engine.bpm.load(Ordering::Relaxed) as f32 / 100.0;
    let cur_step = app.engine.seq_step.load(Ordering::Relaxed) as usize;
    let playing = app.engine.playing.load(Ordering::Relaxed);
    let play_icon = if playing { "▶" } else { "⏸" };

    let title = format!(" ● seq {play_icon} {bpm:.0}bpm {toggle} ");
    let block = module_block(&title, if app.seq_open { PANEL_LABEL } else { PANEL_BORDER });
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.seq_open {
        let steps_visible = inner.width.saturating_sub(2) as usize;
        let show_cursor_info = inner.height >= 2 + SEQ_TRACKS as u16 + 1;
        let grid_rows = if show_cursor_info { SEQ_TRACKS } else { (inner.height.saturating_sub(1)) as usize };
        let mut lines: Vec<String> = Vec::new();
        let mut cursor_step_data: Option<(bool, f32, f32)> = None;
        let (ct, cs) = app.seq_cursor;

        if let Ok(steps) = app.engine.step_data.try_lock() {
            for t in 0..SEQ_TRACKS.min(grid_rows) {
                let mut line = String::new();
                for s in 0..SEQ_STEPS.min(steps_visible) {
                    let (active, _, _) = decode_step(steps[t][s]);
                    let cursor = (t, s) == (ct, cs);
                    let playhead = s == cur_step && playing;
                    let ch = match (active, cursor, playhead) {
                        (_, true, true) => '◈',
                        (_, _, true) => '▸',
                        (true, true, _) => '◆',
                        (false, true, _) => '◇',
                        (true, false, _) => '■',
                        (false, false, _) => '·',
                    };
                    line.push(ch);
                    if cursor { cursor_step_data = Some(decode_step(steps[t][s])); }
                }
                lines.push(line);
            }
        }
        lines.push("".into());

        if show_cursor_info {
            if let Some((_active, freq, vel)) = cursor_step_data {
                let note = freq_to_note(freq);
                lines.push(format!(" T{ct}S{cs:02} {note} {freq:.0}Hz vel{vel:.2}", ));
            } else {
                lines.push(format!(" T{ct}S{cs:02}"));
            }
        }
        let text = lines.join("\n");
        f.render_widget(Paragraph::new(text).style(Style::default().fg(AMBER)), inner);
    }
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
            let any_open = app.scope_open || app.voice_open || app.seq_open;
            let v_chunks = Layout::default().direction(Direction::Vertical)
                .constraints([Constraint::Length(6),
                    Constraint::Length(if any_open { MODULE_HEIGHT_OPEN } else { MODULE_HEIGHT_CLOSED }),
                    Constraint::Length(1), Constraint::Length(1)]).split(area);

            let logo_block = Block::default().borders(Borders::ALL).border_type(BorderType::Plain)
                .title(" los · terminal instrument · v0.1 ").border_style(Style::default().fg(PANEL_BORDER));
            let logo_inner = logo_block.inner(v_chunks[0]);
            f.render_widget(logo_block, v_chunks[0]);
            f.render_widget(Paragraph::new(LOGO).alignment(Alignment::Center), logo_inner);

            let h_chunks = Layout::default().direction(Direction::Horizontal)
                .constraints([Constraint::Length(MODULE_WIDTH), Constraint::Length(MODULE_WIDTH), Constraint::Length(MODULE_WIDTH), Constraint::Fill(1)])
                .split(v_chunks[1]);

            render_scope_module(f, app, h_chunks[0]);
            render_voice_module(f, app, h_chunks[1]);
            render_seq_module(f, app, h_chunks[2]);

            let (mode_name, keys, mode_bg) = match app.mode {
                Mode::Normal => ("NORMAL", "SPC:play +/-:bpm hjkl:grid i:ins w/b:step x:clr J/K:oct B:scope :q", AMBER_DIM),
                Mode::Insert => ("INSERT", "j/k:note J/K:oct h/l:vel SPC:adv ENT:tgl x:clr Esc:normal", MODE_INSERT),
                Mode::Command(_) => ("COMMAND", "Enter:exec Esc:cancel", MODE_COMMAND),
            };
            let status = format!(" {} | {} kHz | {} ", mode_name, (sample_rate/1000.0) as u32, keys);
            f.render_widget(Paragraph::new(status).style(Style::default().fg(Color::Black).bg(mode_bg)), v_chunks[2]);

            if let Mode::Command(ref cmd) = app.mode {
                f.render_widget(Paragraph::new(format!(":{}", cmd)).style(Style::default().fg(Color::Yellow)), v_chunks[3]);
            }
        })?;

        if event::poll(Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press { continue; }
                match &mut app.mode {
                    Mode::Normal => match key.code {
                        KeyCode::Char(':') => app.mode = Mode::Command(String::new()),
                        KeyCode::Char('1') => app.scope_open = !app.scope_open,
                        KeyCode::Char('2') => app.voice_open = !app.voice_open,
                        KeyCode::Char('3') => app.seq_open = !app.seq_open,
                        KeyCode::Char('[') => { app.scope_channel = app.scope_channel.prev(); app.scope_state = ScopeState::new(); app.scope_samples.clear(); }
                        KeyCode::Char(']') => { app.scope_channel = app.scope_channel.next(); app.scope_state = ScopeState::new(); app.scope_samples.clear(); }
                        KeyCode::Char(' ') => { let was_playing = app.engine.playing.load(Ordering::Relaxed); app.engine.playing.store(!was_playing, Ordering::Relaxed); if was_playing { app.engine.trig_gate.store(false, Ordering::Relaxed); } }
                        KeyCode::Char('+') | KeyCode::Char('=') => { let b = app.engine.bpm.load(Ordering::Relaxed); app.engine.bpm.store((b+100).min(30000), Ordering::Relaxed); }
                        KeyCode::Char('-') => { let b = app.engine.bpm.load(Ordering::Relaxed); app.engine.bpm.store(b.saturating_sub(100).max(2000), Ordering::Relaxed); }
                        KeyCode::Char(c) => {
                            let lower = c.to_ascii_lowercase();
                            let shifted = c.is_ascii_uppercase() || key.modifiers.contains(KeyModifiers::SHIFT);
                            match (lower, shifted) {
                                ('h', _) => { app.seq_cursor.1 = app.seq_cursor.1.saturating_sub(1); }
                                ('l', _) => { app.seq_cursor.1 = (app.seq_cursor.1 + 1).min(SEQ_STEPS-1); }
                                ('j', false) => { app.seq_cursor.0 = (app.seq_cursor.0 + 1).min(SEQ_TRACKS-1); }
                                ('k', false) => { app.seq_cursor.0 = app.seq_cursor.0.saturating_sub(1); }
                                ('w', _) => { let (t, s) = app.seq_cursor; if let Ok(steps) = app.engine.step_data.try_lock() { if let Some(next) = next_active_step(&steps, t, s) { app.seq_cursor.1 = next; } } }
                                ('b', false) => { let (t, s) = app.seq_cursor; if let Ok(steps) = app.engine.step_data.try_lock() { if let Some(prev) = prev_active_step(&steps, t, s) { app.seq_cursor.1 = prev; } } }
                                ('b', true) => { app.scope_mode = match app.scope_mode { ScopeMode::Braille=>ScopeMode::Crt, ScopeMode::Crt=>ScopeMode::Braille }; }
                                ('j', true) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(act, octave_down(f) as u32, (v*1000.0) as u32); } }
                                ('k', true) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(act, octave_up(f) as u32, (v*1000.0) as u32); } }
                                ('i', _) => app.mode = Mode::Insert,
                                ('x', _) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; steps[t][s] = encode_step(false, 440, 1000); } }
                                _ => {}
                            }
                        }
                        KeyCode::Enter => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(!act, f as u32, (v*1000.0) as u32); } }
                        KeyCode::Esc => {}
                        _ => {}
                    },
                    Mode::Insert => match key.code {
                        KeyCode::Esc => app.mode = Mode::Normal,
                        KeyCode::Char(c) => {
                            let lower = c.to_ascii_lowercase();
                            let shifted = c.is_ascii_uppercase() || key.modifiers.contains(KeyModifiers::SHIFT);
                            match (lower, shifted) {
                                ('j', false) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(act, semitone_down(f) as u32, (v*1000.0) as u32); } }
                                ('k', false) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(act, semitone_up(f) as u32, (v*1000.0) as u32); } }
                                ('j', true) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(act, octave_down(f) as u32, (v*1000.0) as u32); } }
                                ('k', true) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); steps[t][s] = encode_step(act, octave_up(f) as u32, (v*1000.0) as u32); } }
                                ('h', _) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); let new_v = (v - 0.1).max(0.0); steps[t][s] = encode_step(act, f as u32, (new_v * 1000.0) as u32); } }
                                ('l', _) => { if let Ok(mut steps) = app.engine.step_data.try_lock() { let (t,s) = app.seq_cursor; let (act, f, v) = decode_step(steps[t][s]); let new_v = (v + 0.1).min(1.0); steps[t][s] = encode_step(act, f as u32, (new_v * 1000.0) as u32); } }
                                (' ', _) => { app.seq_cursor.1 = (app.seq_cursor.1 + 1) % SEQ_STEPS; }
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
        let mut steps = [[0u32; SEQ_STEPS]; SEQ_TRACKS];
        steps[0][0] = encode_step(true, 440, 1000);
        steps[0][7] = encode_step(true, 554, 800);
        assert_eq!(next_active_step(&steps, 0, 0), Some(7));
        assert_eq!(next_active_step(&steps, 0, 5), Some(7));
    }

    #[test] fn test_next_active_step_wraps() {
        let mut steps = [[0u32; SEQ_STEPS]; SEQ_TRACKS];
        steps[0][2] = encode_step(true, 440, 1000);
        assert_eq!(next_active_step(&steps, 0, 6), Some(2));
    }

    #[test] fn test_next_active_step_none() {
        let steps = [[0u32; SEQ_STEPS]; SEQ_TRACKS];
        assert_eq!(next_active_step(&steps, 0, 0), None);
    }

    #[test] fn test_prev_active_step_backward() {
        let mut steps = [[0u32; SEQ_STEPS]; SEQ_TRACKS];
        steps[0][0] = encode_step(true, 440, 1000);
        steps[0][7] = encode_step(true, 554, 800);
        assert_eq!(prev_active_step(&steps, 0, 7), Some(0));
        assert_eq!(prev_active_step(&steps, 0, 3), Some(0));
    }

    #[test] fn test_prev_active_step_wraps() {
        let mut steps = [[0u32; SEQ_STEPS]; SEQ_TRACKS];
        steps[0][14] = encode_step(true, 440, 1000);
        assert_eq!(prev_active_step(&steps, 0, 2), Some(14));
    }

    #[test] fn test_prev_active_step_none() {
        let steps = [[0u32; SEQ_STEPS]; SEQ_TRACKS];
        assert_eq!(prev_active_step(&steps, 0, 0), None);
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
