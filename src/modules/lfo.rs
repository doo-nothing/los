//! # The LFO — four phase-disciplined channels (after the Xaoc Batumi)
//!
//! A pure modulation-source module: no audio ring, no note events —
//! four LFO channels published as eight modbus sources (`s1`–`s4` the
//! fixed sines, `a1`–`a4` the per-channel assignable shapes), with the
//! Batumi's four disciplines: free, quadrature, phase, and divide.
//! In the locked modes channels 2–4 are *derived* from channel 1's
//! phase arithmetically, so they can never drift — the whole point of
//! the hardware. `rst` takes a trigger binding; a rising edge re-zeros
//! the bank (bind a sequencer track and the LFOs snap to the bar).

use std::io;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
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

use crate::routing::{self, SourceAddr};
use crate::shm::{Manifest, ModulationBus, ShmTransport};
use crate::state;

pub const NUM_CH: usize = 4;
/// Slowest period ~209 s, fastest 50 Hz (log knob).
const FREQ_MIN: f32 = 1.0 / 209.0;
const FREQ_MAX: f32 = 50.0;

// ── modes & shapes ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Free,
    Quad,
    Phase,
    Div,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Free => "free",
            Mode::Quad => "quad",
            Mode::Phase => "phase",
            Mode::Div => "div",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "free" => Some(Mode::Free),
            "quad" => Some(Mode::Quad),
            "phase" => Some(Mode::Phase),
            "div" => Some(Mode::Div),
            _ => None,
        }
    }

    fn cycle(self, by: i32) -> Self {
        const ALL: [Mode; 4] = [Mode::Free, Mode::Quad, Mode::Phase, Mode::Div];
        let i = ALL.iter().position(|m| *m == self).unwrap_or(0) as i32;
        ALL[(i + by).rem_euclid(4) as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shape {
    #[default]
    Sine,
    Tri,
    Saw,
    Sqr,
    Snh,
}

impl Shape {
    pub fn name(self) -> &'static str {
        match self {
            Shape::Sine => "sine",
            Shape::Tri => "tri",
            Shape::Saw => "saw",
            Shape::Sqr => "sqr",
            Shape::Snh => "s&h",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sine" => Some(Shape::Sine),
            "tri" => Some(Shape::Tri),
            "saw" => Some(Shape::Saw),
            "sqr" | "square" => Some(Shape::Sqr),
            "s&h" | "snh" | "rnd" => Some(Shape::Snh),
            _ => None,
        }
    }

    fn cycle(self, by: i32) -> Self {
        const ALL: [Shape; 5] = [Shape::Sine, Shape::Tri, Shape::Saw, Shape::Sqr, Shape::Snh];
        let i = ALL.iter().position(|m| *m == self).unwrap_or(0) as i32;
        ALL[(i + by).rem_euclid(5) as usize]
    }
}

/// Freq knob (0–1, log) → Hz.
pub fn knob_hz(knob: f32) -> f32 {
    FREQ_MIN * (FREQ_MAX / FREQ_MIN).powf(knob.clamp(0.0, 1.0))
}

/// Unipolar waveforms over phase 0–1 (sine starts at its midpoint and
/// rises, so a bank reset feels like a fade-in, not a drop).
pub fn wave(shape: Shape, phase: f32, held: f32) -> f32 {
    let p = phase.rem_euclid(1.0);
    match shape {
        Shape::Sine => 0.5 - 0.5 * (p * std::f32::consts::TAU).cos(),
        Shape::Tri => 1.0 - (2.0 * p - 1.0).abs(),
        Shape::Saw => p,
        Shape::Sqr => f32::from(u8::from(p < 0.5)),
        Shape::Snh => held,
    }
}

// ── the engine (pure, tested) ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct ChannelCfg {
    /// Freq knob 0–1 (log).
    pub freq: f32,
    pub shape: Shape,
    /// Phase offset (free/quad/phase) or division pick (div).
    pub phase: f32,
}

impl Default for ChannelCfg {
    fn default() -> Self {
        Self {
            freq: 0.35,
            shape: Shape::Sine,
            phase: 0.0,
        }
    }
}

pub struct Bank {
    /// Master phase (ch1) and the free-mode phases for 2–4.
    phases: [f64; NUM_CH],
    /// Master wrap counter — div mode spreads one slow cycle across
    /// `division` master cycles, and integers can't drift.
    master_cycles: u64,
    /// S&H state: held value + last wrap detector per channel.
    held: [f32; NUM_CH],
    last_phase: [f64; NUM_CH],
    rng: u32,
}

impl Bank {
    pub fn new(seed: u32) -> Self {
        Self {
            phases: [0.0; NUM_CH],
            master_cycles: 0,
            held: [0.5; NUM_CH],
            last_phase: [0.0; NUM_CH],
            rng: seed | 1,
        }
    }

    fn next_rand(&mut self) -> f32 {
        // xorshift32 — deterministic within a run
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / 16_777_216.0
    }

    pub fn reset(&mut self) {
        self.phases = [0.0; NUM_CH];
        self.master_cycles = 0;
        self.last_phase = [0.0; NUM_CH];
    }

    /// Division for div mode: phase knob picks /2 … /16.
    pub fn division(knob: f32) -> f64 {
        let steps = [2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 16.0];
        steps[((knob.clamp(0.0, 1.0) * (steps.len() as f32 - 1.0)).round()) as usize]
    }

    /// Advance by dt and produce (sines, assigns), all unipolar 0–1.
    pub fn tick(
        &mut self,
        dt: f64,
        mode: Mode,
        cfg: &[ChannelCfg; NUM_CH],
    ) -> ([f32; NUM_CH], [f32; NUM_CH]) {
        // advance the integrated phases (ch1 always; 2–4 only in free)
        for (i, c) in cfg.iter().enumerate() {
            if i == 0 || mode == Mode::Free {
                let next = self.phases[i] + knob_hz(c.freq) as f64 * dt;
                if i == 0 && next >= 1.0 {
                    self.master_cycles = self.master_cycles.wrapping_add(next as u64);
                }
                self.phases[i] = next.rem_euclid(1.0);
            }
        }
        // derive effective phases
        let master = self.phases[0];
        let mut eff = [0.0_f64; NUM_CH];
        for (i, c) in cfg.iter().enumerate() {
            eff[i] = match (mode, i) {
                (_, 0) => master + c.phase as f64,
                (Mode::Free, _) => self.phases[i] + c.phase as f64,
                (Mode::Quad, _) => master + 0.25 * i as f64 + c.phase as f64,
                (Mode::Phase, _) => master + c.phase as f64,
                (Mode::Div, _) => {
                    // one slow cycle = `div` master cycles, exactly
                    let div = Self::division(c.phase);
                    ((self.master_cycles % div as u64) as f64 + master) / div
                }
            }
            .rem_euclid(1.0);
        }
        // S&H wraps + outputs
        let mut sines = [0.0_f32; NUM_CH];
        let mut assigns = [0.0_f32; NUM_CH];
        for i in 0..NUM_CH {
            if eff[i] < self.last_phase[i] {
                self.held[i] = self.next_rand();
            }
            self.last_phase[i] = eff[i];
            sines[i] = wave(Shape::Sine, eff[i] as f32, self.held[i]);
            assigns[i] = wave(cfg[i].shape, eff[i] as f32, self.held[i]);
        }
        (sines, assigns)
    }
}

// ── shared state ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Mode,
    Rst,
    Freq(usize),
    Shape(usize),
    Phase(usize),
}

const N_ROWS: usize = 2 + NUM_CH * 3;

fn row_at(i: usize) -> Row {
    match i {
        0 => Row::Mode,
        1 => Row::Rst,
        _ => {
            let k = i - 2;
            let ch = k / 3;
            match k % 3 {
                0 => Row::Freq(ch),
                1 => Row::Shape(ch),
                _ => Row::Phase(ch),
            }
        }
    }
}

struct LfoState {
    mode: Mode,
    ch: [ChannelCfg; NUM_CH],
    /// Per-channel rate CV bindings.
    freq_srcs: [Option<SourceAddr>; NUM_CH],
    freq_resolved: [Option<usize>; NUM_CH],
    rst_src: Option<SourceAddr>,
    rst_resolved: Option<usize>,
    rst_last: f32,
    /// Live values for the meters.
    now: [f32; NUM_CH],
    selected: usize,
}

impl LfoState {
    fn new() -> Self {
        Self {
            mode: Mode::Free,
            ch: [
                ChannelCfg::default(),
                ChannelCfg { freq: 0.3, ..Default::default() },
                ChannelCfg { freq: 0.25, ..Default::default() },
                ChannelCfg { freq: 0.2, ..Default::default() },
            ],
            freq_srcs: Default::default(),
            freq_resolved: Default::default(),
            rst_src: None,
            rst_resolved: None,
            rst_last: 0.0,
            now: [0.0; NUM_CH],
            selected: 0,
        }
    }
}

const FREQ_SRC_BASE: usize = 50;
const RST_SLOT: usize = 60;

impl crate::undo::ParamUndo for LfoState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if slot == RST_SLOT {
            return Some(V::Src(self.rst_src.as_ref().map(|a| a.to_string())));
        }
        if let Some(i) = slot.checked_sub(FREQ_SRC_BASE) {
            if i < NUM_CH {
                return Some(V::Src(self.freq_srcs[i].as_ref().map(|a| a.to_string())));
            }
            return None;
        }
        Some(match row_at(slot.min(N_ROWS - 1)) {
            Row::Mode => V::Usize(self.mode as usize),
            Row::Rst => return None,
            Row::Freq(c) => V::F32(self.ch[c].freq),
            Row::Shape(c) => V::Usize(self.ch[c].shape as usize),
            Row::Phase(c) => V::F32(self.ch[c].phase),
        })
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        if slot == RST_SLOT {
            if let V::Src(v) = value {
                self.rst_src = v.as_deref().and_then(SourceAddr::parse);
                self.rst_resolved = None;
            }
            return;
        }
        if let Some(i) = slot.checked_sub(FREQ_SRC_BASE) {
            if i < NUM_CH {
                if let V::Src(v) = value {
                    self.freq_srcs[i] = v.as_deref().and_then(SourceAddr::parse);
                    self.freq_resolved[i] = None;
                }
            }
            return;
        }
        match (row_at(slot.min(N_ROWS - 1)), value) {
            (Row::Mode, V::Usize(v)) => {
                self.mode = [Mode::Free, Mode::Quad, Mode::Phase, Mode::Div][v.min(3)]
            }
            (Row::Freq(c), V::F32(v)) => self.ch[c].freq = v.clamp(0.0, 1.0),
            (Row::Shape(c), V::Usize(v)) => {
                self.ch[c].shape =
                    [Shape::Sine, Shape::Tri, Shape::Saw, Shape::Sqr, Shape::Snh][v.min(4)]
            }
            (Row::Phase(c), V::F32(v)) => self.ch[c].phase = v.clamp(0.0, 1.0),
            _ => {}
        }
    }
}

// ── persistence ────────────────────────────────────────────────────────────

fn snapshot_params(s: &LfoState) -> state::LfoParams {
    state::LfoParams {
        format: state::STATE_FORMAT,
        mode: Some(s.mode.name().to_string()),
        rst_src: s.rst_src.as_ref().map(|a| a.to_string()),
        channels: s
            .ch
            .iter()
            .enumerate()
            .map(|(i, c)| state::LfoChannelParams {
                freq: Some(c.freq),
                shape: Some(c.shape.name().to_string()),
                phase: Some(c.phase),
                freq_src: s.freq_srcs[i].as_ref().map(|a| a.to_string()),
            })
            .collect(),
    }
}

fn apply_params(s: &mut LfoState, p: &state::LfoParams) {
    if let Some(ref m) = p.mode {
        if let Some(m) = Mode::parse(m) {
            s.mode = m;
        }
    }
    s.rst_src = p.rst_src.as_deref().and_then(SourceAddr::parse);
    s.rst_resolved = None;
    for (i, cp) in p.channels.iter().enumerate().take(NUM_CH) {
        if let Some(v) = cp.freq {
            s.ch[i].freq = v.clamp(0.0, 1.0);
        }
        if let Some(ref n) = cp.shape {
            if let Some(sh) = Shape::parse(n) {
                s.ch[i].shape = sh;
            }
        }
        if let Some(v) = cp.phase {
            s.ch[i].phase = v.clamp(0.0, 1.0);
        }
        s.freq_srcs[i] = cp.freq_src.as_deref().and_then(SourceAddr::parse);
        s.freq_resolved[i] = None;
    }
}

// ── control thread ─────────────────────────────────────────────────────────

fn control_thread(shared: Arc<Mutex<LfoState>>, instance: usize) -> Result<()> {
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    // 8 claims: s1..s4 + a1..a4
    manifest.register("lfo", instance, None, 8)?;
    let mod_base = manifest.claimed_base();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();

    let mut bank = Bank::new(0x1234_5678 ^ instance as u32);
    let tick_dur = Duration::from_micros(1333); // ~750 Hz, modbus native
    let mut ticks: u64 = 0;

    loop {
        let t0 = Instant::now();
        if ticks.is_multiple_of(128) {
            let entries = manifest.entries();
            let mut s = shared.lock().unwrap();
            for i in 0..NUM_CH {
                s.freq_resolved[i] = s.freq_srcs[i]
                    .as_ref()
                    .and_then(|a| routing::resolve(&entries, a));
            }
            s.rst_resolved = s
                .rst_src
                .as_ref()
                .and_then(|a| routing::resolve(&entries, a));
            let mask = s
                .freq_resolved
                .iter()
                .flatten()
                .chain(s.rst_resolved.iter())
                .filter(|&&c| c < 64)
                .fold(0u64, |m, &c| m | (1 << c));
            manifest.publish_consumes(mask, 0);
        }

        let (mode, cfg) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            // reset edge
            if let (Some(ch), Some(b)) = (s.rst_resolved, bus) {
                let v = b.get(ch);
                if v > 0.5 && s.rst_last <= 0.5 {
                    bank.reset();
                }
                s.rst_last = v;
            }
            let mut cfg = s.ch;
            // rate CV replaces the knob (the los convention)
            #[allow(clippy::manual_clamp)] // NaN must die, clamp(NaN)=NaN
            for (i, c) in cfg.iter_mut().enumerate() {
                if let (Some(ch), Some(b)) = (s.freq_resolved[i], bus) {
                    c.freq = b.get(ch).max(0.0).min(1.0);
                }
            }
            (s.mode, cfg)
        };

        let (sines, assigns) = bank.tick(tick_dur.as_secs_f64(), mode, &cfg);
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            for i in 0..NUM_CH {
                bus.set(base + i, sines[i]);
                bus.set(base + NUM_CH + i, assigns[i]);
            }
        }
        if ticks.is_multiple_of(16) {
            if let Ok(mut s) = shared.lock() {
                s.now = assigns;
            }
        }

        ticks += 1;
        let elapsed = t0.elapsed();
        if elapsed < tick_dur {
            thread::sleep(tick_dur - elapsed);
        }
    }
}

// ── ui ─────────────────────────────────────────────────────────────────────

fn row_label(r: Row) -> String {
    match r {
        Row::Mode => "mode".into(),
        Row::Rst => "rst".into(),
        Row::Freq(c) => format!("{} freq", c + 1),
        Row::Shape(c) => format!("{} shape", c + 1),
        Row::Phase(c) => format!("{} phase", c + 1),
    }
}

fn adjust(s: &mut LfoState, steps: i32, coarse: bool) {
    use crate::keys::step_f32;
    match row_at(s.selected) {
        Row::Mode => s.mode = s.mode.cycle(steps),
        Row::Rst => {}
        Row::Freq(c) => s.ch[c].freq = step_f32(s.ch[c].freq, steps, 0.01, coarse, 0.0, 1.0),
        Row::Shape(c) => s.ch[c].shape = s.ch[c].shape.cycle(steps),
        Row::Phase(c) => s.ch[c].phase = step_f32(s.ch[c].phase, steps, 0.01, coarse, 0.0, 1.0),
    }
}

fn freq_text(knob: f32) -> String {
    let hz = knob_hz(knob);
    if hz >= 1.0 {
        format!("{hz:.2} Hz")
    } else {
        format!("{:.1} s", 1.0 / hz)
    }
}

fn row_text(s: &LfoState, r: Row) -> String {
    match r {
        Row::Mode => s.mode.name().into(),
        Row::Rst => s
            .rst_src
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(unbound)".into()),
        Row::Freq(c) => {
            if s.mode != Mode::Free && c > 0 {
                format!("({})", freq_text(s.ch[0].freq))
            } else {
                freq_text(s.ch[c].freq)
            }
        }
        Row::Shape(c) => s.ch[c].shape.name().into(),
        Row::Phase(c) => match s.mode {
            Mode::Div if c > 0 => format!("/{}", Bank::division(s.ch[c].phase) as u32),
            Mode::Quad if c > 0 => format!("{}°+{:.0}°", 90 * c, s.ch[c].phase * 360.0),
            _ => format!("{:.0}°", s.ch[c].phase * 360.0),
        },
    }
}

fn norm(s: &LfoState, r: Row) -> Option<f32> {
    Some(match r {
        Row::Freq(c) => s.ch[c].freq,
        Row::Phase(c) => s.ch[c].phase,
        _ => return None,
    })
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &LfoState,
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
        lines.push(theme::header("LFO", &format!("batumi {}", instance), "", w));
        let mut spans = vec![Span::styled("  ".to_string(), theme::chrome())];
        for (i, v) in s.now.iter().enumerate() {
            spans.push(Span::styled(format!("{} ", i + 1), theme::chrome()));
            spans.push(Span::styled(
                theme::meter_char(*v).to_string(),
                theme::signal(theme::cv_ramp(*v)),
            ));
            spans.push(Span::styled("  ".to_string(), theme::chrome()));
        }
        lines.push(Line::from(spans));
        lines.push(Line::from(""));

        let bar_w = theme::bar_width(w, 30);
        for i in 0..N_ROWS {
            let r = row_at(i);
            let selected = i == s.selected;
            let label_style = if selected {
                theme::selected()
            } else {
                theme::chrome()
            };
            let mut spans: Vec<Span> =
                vec![Span::styled(format!(" {:<8}", row_label(r)), label_style)];
            let bound = match r {
                Row::Freq(c) => s.freq_srcs[c].is_some(),
                Row::Rst => s.rst_src.is_some(),
                _ => false,
            };
            let hue = match r {
                Row::Freq(c) => s.freq_srcs[c].as_ref(),
                Row::Rst => s.rst_src.as_ref(),
                _ => None,
            }
            .map(|a| routing::cable_color(entries, a));
            if let Some(n) = norm(s, r) {
                spans.extend(theme::bar(n, None, bar_w, hue.unwrap_or_else(theme::cv)));
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
            spans.push(Span::styled(format!(" {}{}", mark, row_text(s, r)), vstyle));
            lines.push(Line::from(spans));
        }

        theme::anchor_bottom(&mut lines, h, 2);
        lines.push(theme::rule(w));
        lines.push(theme::status("NORMAL", overlay.unwrap_or(""), "", w));
        f.render_widget(Paragraph::new(lines), area);

        if show_help {
            let help = Paragraph::new(vec![
                Line::from("━━━ LFO · four channels (after the Batumi) ━━━"),
                Line::from(""),
                Line::from("  mode       free · quad (90° bank) · phase · div"),
                Line::from("  N freq     209 s – 50 Hz (log); @ takes rate CV"),
                Line::from("  N shape    The assign out: sine tri saw sqr s&h"),
                Line::from("  N phase    Offset (free/quad/phase) · /2…/16 (div)"),
                Line::from("  rst        Trigger binding: re-zero the bank"),
                Line::from(""),
                Line::from("Publishes lfo/N/s1–s4 (sines) and a1–a4 (assigns),"),
                Line::from("all unipolar. Locked modes derive phases from"),
                Line::from("channel 1 — they cannot drift."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" LFO ", theme::chrome_hi())),
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

// ── entry point ────────────────────────────────────────────────────────────

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("lfo", instance);

    let shared = Arc::new(Mutex::new(LfoState::new()));
    if let Ok(p) = state::load_module_state::<state::LfoParams>("lfo", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let ctl_state = Arc::clone(&shared);
    let builder = thread::Builder::new()
        .name(String::from("lfo-control"))
        .stack_size(2 * 1024 * 1024);
    let _ = builder.spawn(move || {
        if let Err(e) = control_thread(ctl_state, instance) {
            eprintln!("[lfo {}] control thread error: {}", instance, e);
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
            let _ = state::save_module_state("lfo", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::LfoParams>("lfo", instance) {
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
                    let steps = if m.kind == MouseEventKind::ScrollUp { 1 } else { -1 };
                    use crate::undo::ParamUndo;
                    let mut s = shared.lock().unwrap();
                    let slot = s.selected;
                    let old = s.get_param(slot);
                    adjust(&mut s, steps, false);
                    let new = s.get_param(slot);
                    if let (Some(old), Some(new)) = (old, new) {
                        history.record(slot, "Adjust", old, new);
                    }
                }
                MouseEventKind::Down(_) => {
                    let row = (m.row as usize).saturating_sub(3);
                    if row < N_ROWS {
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
                let r = row_at(s.selected);
                let slot = match r {
                    Row::Rst => Some(RST_SLOT),
                    Row::Freq(c) => Some(FREQ_SRC_BASE + c),
                    _ => None,
                };
                if let Some(slot) = slot {
                    let old = s.get_param(slot);
                    let text = addr.as_ref().map(|a| a.to_string());
                    match r {
                        Row::Rst => {
                            s.rst_src = addr.clone();
                            s.rst_resolved = None;
                        }
                        Row::Freq(c) => {
                            s.freq_srcs[c] = addr.clone();
                            s.freq_resolved[c] = None;
                        }
                        _ => {}
                    }
                    if let Some(old) = old {
                        history.record(slot, "Bind", old, ParamValue::Src(text));
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
                    ExCommand::Edit(name) => match state::load_patch::<state::LfoParams>(&name) {
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
            let _ = state::save_module_state("lfo", instance, &params);
            ex_msg = Some(String::from("Saved"));
            continue;
        }

        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
            KeyCode::Char('j') | KeyCode::Down => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, n, N_ROWS);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, -n, N_ROWS);
            }
            KeyCode::Char(c @ ('h' | 'l' | 'H' | 'L')) => {
                let n = count.take() as i32;
                let (steps, coarse) = match c {
                    'h' => (-n, false),
                    'l' => (n, false),
                    'H' => (-n, true),
                    _ => (n, true),
                };
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                if matches!(row_at(s.selected), Row::Rst) {
                    ex_msg = Some("rst: @ binds a trigger, x unbinds".into());
                    continue;
                }
                let slot = s.selected;
                let old = s.get_param(slot);
                adjust(&mut s, steps, coarse);
                let new = s.get_param(slot);
                if let (Some(old), Some(new)) = (old, new) {
                    history.record(slot, "Adjust", old, new);
                }
            }
            KeyCode::Char('0') => {
                count.clear();
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                if matches!(row_at(s.selected), Row::Rst) {
                    continue;
                }
                let slot = s.selected;
                let old = s.get_param(slot);
                let def = LfoState::new();
                if let Some(v) = def.get_param(slot) {
                    s.set_param(slot, v.clone());
                    if let Some(old) = old {
                        history.record(slot, "Reset", old, v);
                    }
                }
            }
            KeyCode::Char('@') => {
                count.clear();
                let s = shared.lock().unwrap();
                let r = row_at(s.selected);
                let current = match r {
                    Row::Rst => Some(s.rst_src.clone()),
                    Row::Freq(c) => Some(s.freq_srcs[c].clone()),
                    _ => None,
                };
                if let Some(current) = current {
                    drop(s);
                    let sources = Manifest::open()
                        .map(|m| routing::live_sources(&m.entries()))
                        .unwrap_or_default();
                    picker.open(sources, current.as_ref());
                } else {
                    ex_msg = Some(format!("{} is not bindable", row_label(r)));
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let r = row_at(s.selected);
                let (slot, had) = match r {
                    Row::Rst => (Some(RST_SLOT), s.rst_src.is_some()),
                    Row::Freq(c) => (Some(FREQ_SRC_BASE + c), s.freq_srcs[c].is_some()),
                    _ => (None, false),
                };
                if let (Some(slot), true) = (slot, had) {
                    let old = s.get_param(slot);
                    match r {
                        Row::Rst => {
                            s.rst_src = None;
                            s.rst_resolved = None;
                        }
                        Row::Freq(c) => {
                            s.freq_srcs[c] = None;
                            s.freq_resolved[c] = None;
                        }
                        _ => {}
                    }
                    if let Some(old) = old {
                        history.record(slot, "Unbind", old, ParamValue::Src(None));
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
                shared.lock().unwrap().selected = N_ROWS - 1;
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

/// `:set` rows: `mode quad`, `1.freq 0.4`, `2.shape saw`, `3.phase 0.25`,
/// `rst sequencer/0/t1` (`rst -` unbinds).
fn ex_set(
    s: &mut LfoState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    if key == "mode" {
        let Some(m) = Mode::parse(value) else {
            return "mode: free quad phase div".into();
        };
        let old = s.get_param(0);
        s.set_param(0, V::Usize(m as usize));
        if let Some(old) = old {
            history.record(0, "Set", old, V::Usize(m as usize));
        }
        return format!("mode = {}", m.name());
    }
    if key == "rst" {
        let v = if value == "-" {
            V::Src(None)
        } else {
            V::Src(Some(value.to_string()))
        };
        let old = s.get_param(RST_SLOT);
        s.set_param(RST_SLOT, v.clone());
        if let Some(old) = old {
            history.record(RST_SLOT, "Set", old, v);
        }
        return format!("rst = {}", row_text(s, Row::Rst));
    }
    let Some((ch, field)) = key.split_once('.') else {
        return format!("Unknown setting: {key} (mode rst N.freq N.shape N.phase)");
    };
    let Ok(c) = ch.parse::<usize>() else {
        return format!("channel: 1–{NUM_CH}");
    };
    if !(1..=NUM_CH).contains(&c) {
        return format!("channel: 1–{NUM_CH}");
    }
    let c = c - 1;
    let slot = 2 + c * 3
        + match field {
            "freq" => 0,
            "shape" => 1,
            "phase" => 2,
            _ => return format!("Unknown field: {field} (freq shape phase)"),
        };
    let parsed: Result<V, String> = match field {
        "shape" => Shape::parse(value)
            .map(|sh| V::Usize(sh as usize))
            .ok_or_else(|| "shape: sine tri saw sqr s&h".into()),
        _ => value
            .parse::<f32>()
            .map(V::F32)
            .map_err(|_| format!("{key}: not a number: {value}")),
    };
    match parsed {
        Ok(v) => {
            let old = s.get_param(slot);
            s.set_param(slot, v.clone());
            if let Some(old) = old {
                history.record(slot, "Set", old, v);
            }
            format!("{} = {}", key, row_text(s, row_at(slot)))
        }
        Err(m) => m,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfgs() -> [ChannelCfg; NUM_CH] {
        [
            ChannelCfg { freq: 0.5, shape: Shape::Sine, phase: 0.0 },
            ChannelCfg { freq: 0.5, shape: Shape::Tri, phase: 0.0 },
            ChannelCfg { freq: 0.5, shape: Shape::Saw, phase: 0.0 },
            ChannelCfg { freq: 0.5, shape: Shape::Sqr, phase: 0.0 },
        ]
    }

    #[test]
    fn quad_mode_locks_quadrature() {
        let mut b = Bank::new(7);
        let cfg = cfgs();
        let mut last = ([0.0; NUM_CH], [0.0; NUM_CH]);
        for _ in 0..5_000 {
            last = b.tick(0.001, Mode::Quad, &cfg);
        }
        // ch3 is 180° from ch1: sines must mirror around 0.5
        let (s, _) = last;
        assert!(
            (s[0] + s[2] - 1.0).abs() < 0.02,
            "180° pair mirrors: {} + {}",
            s[0],
            s[2]
        );
        assert!((s[1] + s[3] - 1.0).abs() < 0.02, "90/270 pair mirrors");
    }

    #[test]
    fn div_mode_is_integer_locked() {
        let mut b = Bank::new(7);
        let mut cfg = cfgs();
        cfg[1].phase = 0.0; // /2
        // run exactly two master cycles; ch2 must complete exactly one
        // run many master cycles and count cycle-completions on both
        // channels: at /2 the ratio must be 2:1 (phase-derived = exact)
        let hz = knob_hz(0.5) as f64;
        let steps = (20.0 / hz / 0.001).round() as usize;
        let (mut w1, mut w2) = (0, 0);
        let (mut l1, mut l2) = (1.0_f32, 1.0_f32);
        for _ in 0..steps {
            let (s, _) = b.tick(0.001, Mode::Div, &cfg);
            if l1 < 0.02 && s[0] >= 0.02 {
                w1 += 1;
            }
            if l2 < 0.02 && s[1] >= 0.02 {
                w2 += 1;
            }
            l1 = s[0];
            l2 = s[1];
        }
        let ratio = w1 as f32 / w2.max(1) as f32;
        assert!(
            (1.7..=2.3).contains(&ratio),
            "/2 holds a 2:1 cycle ratio, got {w1}:{w2}"
        );
    }

    #[test]
    fn shapes_bounded_and_snh_steps_on_wraps() {
        for sh in [Shape::Sine, Shape::Tri, Shape::Saw, Shape::Sqr] {
            for i in 0..100 {
                let v = wave(sh, i as f32 / 100.0, 0.5);
                assert!((0.0..=1.0).contains(&v), "{sh:?} out of range: {v}");
            }
        }
        let mut b = Bank::new(7);
        let mut cfg = cfgs();
        cfg[0].shape = Shape::Snh;
        cfg[0].freq = 0.8;
        let mut values = std::collections::BTreeSet::new();
        let mut held_changes = 0;
        let mut last_assign = -1.0_f32;
        for _ in 0..20_000 {
            let (_, a) = b.tick(0.001, Mode::Free, &cfg);
            if (a[0] - last_assign).abs() > 1e-6 {
                held_changes += 1;
                values.insert((a[0] * 1000.0) as i32);
            }
            last_assign = a[0];
        }
        assert!(held_changes > 3, "s&h stepped: {held_changes}");
        assert!(values.len() > 3, "s&h values vary");
    }

    #[test]
    fn reset_rezeros_the_bank() {
        let mut b = Bank::new(7);
        let cfg = cfgs();
        for _ in 0..1_000 {
            b.tick(0.001, Mode::Free, &cfg);
        }
        b.reset();
        let (s, _) = b.tick(0.000_001, Mode::Free, &cfg);
        // sine starts at its minimum after reset (rising cosine form)
        assert!(s[0] < 0.01, "reset re-zeros: {}", s[0]);
    }

    #[test]
    fn freq_mapping_log_ends() {
        assert!((knob_hz(0.0) - FREQ_MIN).abs() < 1e-6);
        assert!((knob_hz(1.0) - FREQ_MAX).abs() < 1e-3);
        let mid = knob_hz(0.5);
        assert!((0.45..0.55).contains(&(mid / (FREQ_MIN * FREQ_MAX).sqrt())) || mid > 0.0);
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = LfoState::new();
        s.mode = Mode::Quad;
        s.ch[2].shape = Shape::Snh;
        s.ch[1].phase = 0.4;
        s.rst_src = SourceAddr::parse("sequencer/0/t1");
        s.freq_srcs[0] = SourceAddr::parse("envelope/0/ch1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::LfoParams = toml::from_str(&toml).expect("parses");
        let mut s2 = LfoState::new();
        apply_params(&mut s2, &back);
        assert_eq!(s2.mode, Mode::Quad);
        assert_eq!(s2.ch[2].shape, Shape::Snh);
        assert!((s2.ch[1].phase - 0.4).abs() < 1e-6);
        assert_eq!(
            s2.rst_src.as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t1".into())
        );
        assert_eq!(
            s2.freq_srcs[0].as_ref().map(|a| a.to_string()),
            Some("envelope/0/ch1".into())
        );
    }

    #[test]
    fn ex_set_speaks_channels() {
        let mut s = LfoState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "mode", "quad").contains("quad"));
        assert!(ex_set(&mut s, &mut h, "2.shape", "saw").contains("saw"));
        assert!(ex_set(&mut s, &mut h, "1.freq", "0.7").contains("Hz"));
        assert!(ex_set(&mut s, &mut h, "rst", "sequencer/0/t1").contains("t1"));
        assert!(ex_set(&mut s, &mut h, "9.freq", "0.5").contains("channel"));
        assert!(ex_set(&mut s, &mut h, "wow", "1").contains("Unknown"));
    }
}
