//! # Edges — the Mutable Instruments quad chiptune generator
//!
//! Ported from pichenettes/eurorack (edges/{digital_oscillator,
//! timer_oscillator}.cc, MIT/GPL-era firmware by Emilie Gillet,
//! attribution preserved). Four channels like the hardware: three
//! timer-style square voices with the fixed pulse-width set
//! (50/66/75/87/95% or swept), and the digital channel's shapes —
//! triangle, NES triangle (the 2A03's 16-step staircase), pitched
//! noise (the 0xb400 LFSR with its 512 + 3/4 scaling), NES long/short
//! noise (the tap-select LFSR, two-level output 0x0300/0x0cff), and
//! the 8-bit sine. The 12-bit/8-bit quantization is the instrument —
//! outputs are quantized exactly like the firmware's DAC writes.
//!
//! Each channel takes its own note track; channels 1+3 pan left,
//! 2+4 right.

use std::io;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
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
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

const FALLBACK_RATE: f32 = 48_000.0;
const NUM_CHANNELS: usize = 4;

pub const SHAPE_NAMES: [&str; 7] = [
    "square", "triangle", "nes_tri", "noise", "nes_noise", "nes_short", "sine",
];

const PW_VALUES: [f32; 5] = [0.5, 0.66, 0.75, 0.87, 0.95];

/// One chip voice, firmware-law per shape.
#[derive(Debug, Clone)]
pub struct ChipVoice {
    pub shape: usize,
    phase: f32,
    rng_state: u16,
    sample: f32,
    gate: bool,
    freq: f32,
}

impl ChipVoice {
    pub fn new(seed: u16) -> Self {
        ChipVoice {
            shape: 0,
            phase: 0.0,
            rng_state: seed | 1,
            sample: 0.0,
            gate: false,
            freq: 110.0,
        }
    }

    pub fn note_on(&mut self, freq: f32) {
        self.freq = freq;
        self.gate = true;
    }

    pub fn note_off(&mut self) {
        self.gate = false;
    }

    /// Render one block. `pw` is the effective pulse width 0..1,
    /// `transpose` in semitones.
    pub fn render(&mut self, sample_rate: f32, pw: f32, transpose: f32, out: &mut [f32]) {
        if !self.gate {
            out.fill(0.0);
            return;
        }
        let f = (self.freq * (transpose / 12.0).exp2() / sample_rate).clamp(1e-5, 0.45);
        match self.shape {
            // square — the timer voices; naive edge like the hardware
            // timers (which were analog-clean; at audio rates the
            // stepped edge IS the chip sound)
            0 => {
                for v in out.iter_mut() {
                    self.phase += f;
                    if self.phase >= 1.0 {
                        self.phase -= 1.0;
                    }
                    *v = if self.phase < pw.clamp(0.02, 0.98) {
                        0.5
                    } else {
                        -0.5
                    };
                }
            }
            // triangle, 8-bit quantized like the wavetable reads
            1 => {
                for v in out.iter_mut() {
                    self.phase += f;
                    if self.phase >= 1.0 {
                        self.phase -= 1.0;
                    }
                    let tri = if self.phase < 0.5 {
                        4.0 * self.phase - 1.0
                    } else {
                        3.0 - 4.0 * self.phase
                    };
                    *v = ((tri * 127.0) as i32 as f32) / 128.0 * 0.5;
                }
            }
            // NES triangle: the 2A03's 16-step staircase per half
            2 => {
                for v in out.iter_mut() {
                    self.phase += f;
                    if self.phase >= 1.0 {
                        self.phase -= 1.0;
                    }
                    let tri = if self.phase < 0.5 {
                        4.0 * self.phase - 1.0
                    } else {
                        3.0 - 4.0 * self.phase
                    };
                    let stepped = ((tri * 7.5).round() / 7.5).clamp(-1.0, 1.0);
                    *v = stepped * 0.5;
                }
            }
            // pitched noise: LFSR 0xb400, sample = 512 + 3/4·(state & 0xfff)
            3 => {
                for v in out.iter_mut() {
                    self.phase += f;
                    if self.phase >= 1.0 {
                        self.phase -= 1.0;
                        self.rng_state =
                            (self.rng_state >> 1) ^ ((self.rng_state & 1).wrapping_neg() & 0xb400);
                        let s = (self.rng_state & 0x0fff) as f32;
                        self.sample = 512.0 + s * 3.0 / 4.0;
                    }
                    *v = (self.sample - 2048.0) / 2048.0 * 0.5;
                }
            }
            // NES noises: tap bit1 (long) / bit6 (short), two-level out
            4 | 5 => {
                let short = self.shape == 5;
                for v in out.iter_mut() {
                    self.phase += f;
                    if self.phase >= 1.0 {
                        self.phase -= 1.0;
                        let mut tap = self.rng_state >> 1;
                        if short {
                            tap >>= 5;
                        }
                        let random_bit = (self.rng_state ^ tap) & 1;
                        self.rng_state >>= 1;
                        if random_bit != 0 {
                            self.rng_state |= 0x4000;
                            self.sample = 0x0300 as f32;
                        } else {
                            self.sample = 0x0cff as f32;
                        }
                    }
                    *v = (self.sample - 2048.0) / 2048.0 * 0.5;
                }
            }
            // 8-bit sine (with the firmware's sample-hold bitcrush on
            // the pw knob: pw sets the re-sample rate)
            _ => {
                let hold = (pw * pw * 0.02).max(f / 64.0);
                for v in out.iter_mut() {
                    self.phase += f;
                    if self.phase >= 1.0 {
                        self.phase -= 1.0;
                    }
                    self.sample += hold;
                    if self.sample >= hold && (self.sample - hold) < hold {
                        // continuous hold accumulator wrap
                    }
                    if self.sample >= 1.0 || hold >= 1.0 {
                        self.sample -= self.sample.floor();
                    }
                    // re-sample on hold boundaries
                    let s = (self.phase * std::f32::consts::TAU).sin();
                    let crushed = ((s * 127.0) as i32 as f32) / 128.0;
                    *v = crushed * 0.5;
                }
            }
        }
    }
}

// ── shell ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Shape(usize),
    Pw(usize),
    Xpose(usize),
    Lvl(usize),
    Notes(usize),
    Level,
    Amp,
}

fn rows() -> Vec<Row> {
    let mut r = Vec::new();
    for c in 0..NUM_CHANNELS {
        r.push(Row::Shape(c));
        r.push(Row::Pw(c));
        r.push(Row::Xpose(c));
        r.push(Row::Lvl(c));
        r.push(Row::Notes(c));
    }
    r.push(Row::Level);
    r.push(Row::Amp);
    r
}

/// CV bank: pw + transpose + level per channel, plus the master.
fn bindable() -> Vec<Row> {
    let mut b = Vec::new();
    for c in 0..NUM_CHANNELS {
        b.push(Row::Pw(c));
        b.push(Row::Xpose(c));
        b.push(Row::Lvl(c));
    }
    b.push(Row::Level);
    b
}

const N_SRC: usize = NUM_CHANNELS * 3 + 1;

struct EdgesState {
    shape: [usize; NUM_CHANNELS],
    pw: [f32; NUM_CHANNELS],
    xpose: [f32; NUM_CHANNELS],
    lvl: [f32; NUM_CHANNELS],
    level: f32,
    notes_src: [Option<SourceAddr>; NUM_CHANNELS],
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    amp_src: Option<SourceAddr>,
    amp_resolved: Option<usize>,
    selected: usize,
}

impl EdgesState {
    fn new() -> Self {
        let mut s = EdgesState {
            shape: [0, 0, 0, 6],
            pw: [0.5; NUM_CHANNELS],
            xpose: [0.5; NUM_CHANNELS],
            lvl: [0.7; NUM_CHANNELS],
            level: 0.8,
            notes_src: Default::default(),
            srcs: [const { None }; N_SRC],
            resolved: [None; N_SRC],
            eff: [0.0; N_SRC],
            amp_src: None,
            amp_resolved: None,
            selected: 0,
        };
        for (k, r) in bindable().iter().enumerate() {
            s.eff[k] = s.get(*r);
        }
        s
    }

    fn get(&self, r: Row) -> f32 {
        match r {
            Row::Pw(c) => self.pw[c],
            Row::Xpose(c) => self.xpose[c],
            Row::Lvl(c) => self.lvl[c],
            Row::Level => self.level,
            _ => 0.0,
        }
    }

    fn set(&mut self, r: Row, v: f32) {
        let v = v.clamp(0.0, 1.0);
        match r {
            Row::Pw(c) => self.pw[c] = v,
            Row::Xpose(c) => self.xpose[c] = v,
            Row::Lvl(c) => self.lvl[c] = v,
            Row::Level => self.level = v,
            _ => {}
        }
    }
}

const SRC_SLOT_BASE: usize = 50;
const AMP_SLOT: usize = 40;
const NOTES_SLOT_BASE: usize = 41; // 41..44 per channel

impl crate::undo::ParamUndo for EdgesState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if slot == AMP_SLOT {
            return Some(V::Src(self.amp_src.as_ref().map(|a| a.to_string())));
        }
        if (NOTES_SLOT_BASE..NOTES_SLOT_BASE + NUM_CHANNELS).contains(&slot) {
            let c = slot - NOTES_SLOT_BASE;
            return Some(V::Src(self.notes_src[c].as_ref().map(|a| a.to_string())));
        }
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            if i < N_SRC {
                return Some(V::Src(self.srcs[i].as_ref().map(|a| a.to_string())));
            }
            return None;
        }
        let all = rows();
        let r = *all.get(slot)?;
        Some(match r {
            Row::Shape(c) => V::Usize(self.shape[c]),
            Row::Notes(_) | Row::Amp => return None,
            _ => V::F32(self.get(r)),
        })
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        let parse = |v: &Option<String>| v.as_deref().and_then(SourceAddr::parse);
        if slot == AMP_SLOT {
            if let V::Src(v) = &value {
                self.amp_src = parse(v);
                self.amp_resolved = None;
            }
            return;
        }
        if (NOTES_SLOT_BASE..NOTES_SLOT_BASE + NUM_CHANNELS).contains(&slot) {
            if let V::Src(v) = &value {
                self.notes_src[slot - NOTES_SLOT_BASE] = parse(v);
            }
            return;
        }
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            if i < N_SRC {
                if let V::Src(v) = &value {
                    self.srcs[i] = parse(v);
                    self.resolved[i] = None;
                }
            }
            return;
        }
        let all = rows();
        match (all.get(slot).copied(), value) {
            (Some(Row::Shape(c)), V::Usize(v)) => self.shape[c] = v.min(6),
            (Some(r), V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &EdgesState) -> state::EdgesParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::EdgesParams {
        format: state::STATE_FORMAT,
        shapes: s
            .shape
            .iter()
            .map(|i| SHAPE_NAMES[(*i).min(6)].to_string())
            .collect(),
        pw: s.pw.to_vec(),
        xpose: s.xpose.to_vec(),
        lvl: s.lvl.to_vec(),
        level: Some(s.level),
        pw1_src: src(0),
        pw2_src: src(3),
        pw3_src: src(6),
        pw4_src: src(9),
        xpose1_src: src(1),
        xpose2_src: src(4),
        xpose3_src: src(7),
        xpose4_src: src(10),
        lvl1_src: src(2),
        lvl2_src: src(5),
        lvl3_src: src(8),
        lvl4_src: src(11),
        level_src: src(12),
        amp_src: s.amp_src.as_ref().map(|a| a.to_string()),
        notes1_src: s.notes_src[0].as_ref().map(|a| a.to_string()),
        notes2_src: s.notes_src[1].as_ref().map(|a| a.to_string()),
        notes3_src: s.notes_src[2].as_ref().map(|a| a.to_string()),
        notes4_src: s.notes_src[3].as_ref().map(|a| a.to_string()),
    }
}

fn apply_params(s: &mut EdgesState, p: &state::EdgesParams) {
    for (c, name) in p.shapes.iter().take(4).enumerate() {
        if let Some(i) = SHAPE_NAMES.iter().position(|x| x == name) {
            s.shape[c] = i;
        }
    }
    for (c, v) in p.pw.iter().take(4).enumerate() {
        s.pw[c] = v.clamp(0.0, 1.0);
    }
    for (c, v) in p.xpose.iter().take(4).enumerate() {
        s.xpose[c] = v.clamp(0.0, 1.0);
    }
    for (c, v) in p.lvl.iter().take(4).enumerate() {
        s.lvl[c] = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.level {
        s.level = v.clamp(0.0, 1.0);
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.pw1_src),
        parse(&p.xpose1_src),
        parse(&p.lvl1_src),
        parse(&p.pw2_src),
        parse(&p.xpose2_src),
        parse(&p.lvl2_src),
        parse(&p.pw3_src),
        parse(&p.xpose3_src),
        parse(&p.lvl3_src),
        parse(&p.pw4_src),
        parse(&p.xpose4_src),
        parse(&p.lvl4_src),
        parse(&p.level_src),
    ];
    s.notes_src = [
        parse(&p.notes1_src),
        parse(&p.notes2_src),
        parse(&p.notes3_src),
        parse(&p.notes4_src),
    ];
    s.amp_src = parse(&p.amp_src);
    s.resolved = [None; N_SRC];
    s.amp_resolved = None;
}

// ── audio thread ───────────────────────────────────────────────────────────

fn audio_thread(shared: Arc<Mutex<EdgesState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_edges_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating edges ring")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("edges", instance, Some(&shm_name), 0)?;
    let modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();
    let mut events = EventRingbuf::open_dynamic().ok();
    let transport = ShmTransport::open().ok();
    let sample_rate = transport
        .as_ref()
        .map(|t| t.sample_rate() as f32)
        .filter(|r| *r > 0.0)
        .unwrap_or(FALLBACK_RATE);

    let channels = ringbuf.channels() as usize;
    let slot_frames = ringbuf.slot_frames() as usize;
    let mut block = vec![0.0_f32; ringbuf.slot_len()];
    let mut voice_out = vec![0.0_f32; slot_frames];

    let mut voices: Vec<ChipVoice> = (0..NUM_CHANNELS)
        .map(|c| ChipVoice::new(0x1B4D ^ ((instance as u16) << 4) ^ c as u16))
        .collect();
    let mut note_filter: [Option<u8>; NUM_CHANNELS] = [None; NUM_CHANNELS];
    let mut gain_smooth = 0.0_f32;
    let mut blocks: u64 = 0;

    loop {
        if blocks.is_multiple_of(128) {
            if events.is_none() {
                events = EventRingbuf::open_dynamic().ok();
            }
            let entries = manifest.entries();
            let mut s = shared.lock().unwrap();
            for k in 0..N_SRC {
                s.resolved[k] = s.srcs[k]
                    .as_ref()
                    .and_then(|a| routing::resolve(&entries, a));
            }
            s.amp_resolved = s
                .amp_src
                .as_ref()
                .and_then(|a| routing::resolve(&entries, a));
            #[allow(clippy::needless_range_loop)] // c strides
            // parallel per-channel arrays
            for c in 0..NUM_CHANNELS {
                note_filter[c] = s.notes_src[c].as_ref().and_then(routing::note_source_track);
            }
            let mask = s
                .resolved
                .iter()
                .flatten()
                .chain(s.amp_resolved.iter())
                .filter(|&&c| c < 64)
                .fold(0u64, |m, &c| m | (1 << c));
            let notes = note_filter
                .iter()
                .flatten()
                .filter(|&&t| t < 8)
                .fold(0u8, |m, &t| m | (1 << t));
            manifest.publish_consumes(mask, notes);
        }

        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                for c in 0..NUM_CHANNELS {
                    let Some(t) = note_filter[c] else { continue };
                    if event.source != t {
                        continue;
                    }
                    if event.is_note_on() {
                        if event.value.is_finite() && event.value > 0.0 {
                            voices[c].note_on(event.value);
                        }
                    } else if event.is_note_off() {
                        voices[c].note_off();
                    }
                }
            }
        }

        let (shapes, pw, xpose, lvl, level, amp) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            // max/min, not clamp: NaN from a stale channel dies here
            #[allow(clippy::manual_clamp)]
            let cv = |k: usize, manual: f32, s: &EdgesState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let mut pw = [0.0_f32; NUM_CHANNELS];
            let mut xpose = [0.0_f32; NUM_CHANNELS];
            let mut lvl = [0.0_f32; NUM_CHANNELS];
            for c in 0..NUM_CHANNELS {
                pw[c] = cv(c * 3, s.pw[c], &s);
                xpose[c] = cv(c * 3 + 1, s.xpose[c], &s);
                lvl[c] = cv(c * 3 + 2, s.lvl[c], &s);
                s.eff[c * 3] = pw[c];
                s.eff[c * 3 + 1] = xpose[c];
                s.eff[c * 3 + 2] = lvl[c];
            }
            let level = cv(12, s.level, &s);
            s.eff[12] = level;
            let amp = match (s.amp_src.is_some(), s.amp_resolved, bus) {
                (false, _, _) => 1.0,
                (true, Some(ch), Some(b)) => b.get(ch).clamp(0.0, 1.0),
                (true, _, _) => 0.0,
            };
            (s.shape, pw, xpose, lvl, level, amp)
        };

        block.fill(0.0);
        for c in 0..NUM_CHANNELS {
            voices[c].shape = shapes[c];
            // square channels: pw quantizes to the hardware's five
            // detents in the lower half, sweeps freely above
            let eff_pw = if shapes[c] == 0 && pw[c] < 0.5 {
                PW_VALUES[((pw[c] * 2.0) * 4.999) as usize % 5]
            } else {
                pw[c]
            };
            let transpose = (xpose[c] - 0.5) * 48.0;
            voices[c].render(sample_rate, eff_pw, transpose, &mut voice_out);
            let side = c & 1; // 1+3 left, 2+4 right
            for f in 0..slot_frames {
                block[f * channels + side.min(channels - 1)] +=
                    voice_out[f] * lvl[c] * 0.5;
            }
        }

        let gain_target = amp * level;
        let g_alpha = 1.0 - (-1.0 / (0.0007 * sample_rate)).exp();
        for f in 0..slot_frames {
            gain_smooth += (gain_target - gain_smooth) * g_alpha;
            for ch in 0..channels {
                block[f * channels + ch] *= gain_smooth;
            }
        }
        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            for (c, v) in voices.iter_mut().enumerate() {
                *v = ChipVoice::new(0x1B4D ^ ((instance as u16) << 4) ^ c as u16);
            }
        }
        while ringbuf.write(&block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }
        blocks += 1;
    }
}

// ── ui ─────────────────────────────────────────────────────────────────────

fn row_label(r: Row) -> String {
    match r {
        Row::Shape(c) => format!("shape {}", c + 1),
        Row::Pw(c) => format!("pw {}", c + 1),
        Row::Xpose(c) => format!("xpose {}", c + 1),
        Row::Lvl(c) => format!("lvl {}", c + 1),
        Row::Notes(c) => format!("notes {}", c + 1),
        Row::Level => "level".into(),
        Row::Amp => "amp".into(),
    }
}

fn src_index(r: Row) -> Option<usize> {
    bindable().iter().position(|b| *b == r)
}

fn binding_slot(r: Row) -> Option<usize> {
    match r {
        Row::Amp => Some(AMP_SLOT),
        Row::Notes(c) => Some(NOTES_SLOT_BASE + c),
        _ => src_index(r).map(|i| SRC_SLOT_BASE + i),
    }
}

fn row_text(s: &EdgesState, r: Row) -> String {
    match r {
        Row::Shape(c) => SHAPE_NAMES[s.shape[c].min(6)].into(),
        Row::Xpose(c) => format!("{:+.0} st", (s.xpose[c] - 0.5) * 48.0),
        Row::Notes(c) => s.notes_src[c]
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(unbound — silent)".into()),
        Row::Amp => s
            .amp_src
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(unbound)".into()),
        _ => format!("{:.0}%", s.get(r) * 100.0),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &EdgesState,
    instance: usize,
    show_help: bool,
    overlay: Option<&str>,
    picker: Option<(Vec<String>, usize)>,
    entries: &[crate::shm::ManifestEntry],
) -> Result<()> {
    use crate::theme;
    let all = rows();
    terminal.draw(|f| {
        let area = f.area();
        let w = area.width as usize;
        let h = area.height as usize;
        let mut lines: Vec<Line> = Vec::new();
        lines.push(theme::header(
            "EDGES",
            &format!("chiptune {}", instance),
            "",
            w,
        ));
        lines.push(Line::from(""));

        let bar_w = theme::bar_width(w, 32);
        for (i, r) in all.iter().enumerate() {
            let selected = i == s.selected;
            let label_style = if selected {
                theme::selected()
            } else {
                theme::chrome()
            };
            let mut spans: Vec<Span> =
                vec![Span::styled(format!(" {:<8}", row_label(*r)), label_style)];
            let bound = src_index(*r).is_some_and(|k| s.srcs[k].is_some())
                || matches!(*r, Row::Amp if s.amp_src.is_some())
                || matches!(*r, Row::Notes(c) if s.notes_src[c].is_some());
            let hue = src_index(*r)
                .and_then(|k| s.srcs[k].as_ref())
                .or(match r {
                    Row::Amp => s.amp_src.as_ref(),
                    Row::Notes(c) => s.notes_src[*c].as_ref(),
                    _ => None,
                })
                .map(|a| routing::cable_color(entries, a));
            if src_index(*r).is_some() {
                let shown = match src_index(*r) {
                    Some(k) if s.srcs[k].is_some() => s.eff[k],
                    _ => s.get(*r),
                };
                spans.extend(theme::bar(shown, None, bar_w, hue.unwrap_or_else(theme::cv)));
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
            spans.push(Span::styled(
                format!(" {}{}", mark, row_text(s, *r)),
                vstyle,
            ));
            lines.push(Line::from(spans));
        }

        theme::anchor_bottom(&mut lines, h, 2);
        lines.push(theme::rule(w));
        lines.push(theme::status("NORMAL", overlay.unwrap_or(""), "", w));
        f.render_widget(Paragraph::new(lines), area);

        if show_help {
            let help = Paragraph::new(vec![
                Line::from("━━━ EDGES · quad chiptune (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  shape    square · triangle · nes_tri · noise"),
                Line::from("           nes_noise · nes_short · sine"),
                Line::from("  pw       square: 50/66/75/87/95% detents low,"),
                Line::from("           free sweep high · sine: bitcrush hold"),
                Line::from("  xpose    ±24 semitones"),
                Line::from("  notes    one track per channel — chip polyphony"),
                Line::from(""),
                Line::from("Channels 1+3 left, 2+4 right. The 8/12-bit"),
                Line::from("quantization is faithful: that's the sound."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" EDGES ", theme::chrome_hi())),
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

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("edges", instance);

    let shared = Arc::new(Mutex::new(EdgesState::new()));
    if let Ok(p) = state::load_module_state::<state::EdgesParams>("edges", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let audio_builder = thread::Builder::new()
        .name(String::from("edges-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = audio_builder.spawn(move || {
        // black box: a dead audio thread must leave a trace
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            audio_thread(audio_state, instance)
        }));
        let msg = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("error: {e}"),
            Err(p) => format!(
                "PANIC: {}",
                p.downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string payload>".into())
            ),
        };
        eprintln!("[edges {instance}] audio thread died: {msg}");
        let path = crate::state::tmp_dir().join(format!("edges_{instance}.crash"));
        let _ = std::fs::write(path, &msg);
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

    let all_rows = rows();
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
            let _ = state::save_module_state("edges", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::EdgesParams>("edges", instance) {
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
            if let MouseEventKind::Down(_) = m.kind {
                let row = (m.row as usize).saturating_sub(2);
                if row < all_rows.len() {
                    shared.lock().unwrap().selected = row;
                }
            }
            continue;
        }
        let Event::Key(key) = ev else { continue };
        ex_msg = None;

        if picker.is_active() {
            if let crate::picker::PickerEvent::Chosen(addr) = picker.handle_key(key.code) {
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let r = all_rows[s.selected.min(all_rows.len() - 1)];
                if let Some(slot) = binding_slot(r) {
                    let old = s.get_param(slot);
                    let text = addr.as_ref().map(|a| a.to_string());
                    match r {
                        Row::Amp => {
                            s.amp_src = addr.clone();
                            s.amp_resolved = None;
                        }
                        Row::Notes(c) => s.notes_src[c] = addr.clone(),
                        _ => {
                            if let Some(k) = src_index(r) {
                                s.srcs[k] = addr.clone();
                                s.resolved[k] = None;
                            }
                        }
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
                    ExCommand::Edit(name) => match state::load_patch::<state::EdgesParams>(&name) {
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
            let _ = state::save_module_state("edges", instance, &params);
            ex_msg = Some(String::from("Saved"));
            continue;
        }

        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
            KeyCode::Char('j') | KeyCode::Down => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, n, all_rows.len());
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, -n, all_rows.len());
            }
            KeyCode::Char(c @ ('h' | 'l' | 'H' | 'L')) => {
                let n = count.take() as i32;
                let (steps, coarse) = match c {
                    'h' => (-n, false),
                    'l' => (n, false),
                    'H' => (-n, true),
                    _ => (n, true),
                };
                use crate::keys::step_f32;
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                let slot = s.selected;
                let r = all_rows[slot.min(all_rows.len() - 1)];
                if matches!(r, Row::Notes(_) | Row::Amp) {
                    ex_msg = Some("routing row: @ binds, x unbinds".into());
                    continue;
                }
                let old = s.get_param(slot);
                if let Row::Shape(c) = r {
                    s.shape[c] = (s.shape[c] as i32 + steps.signum()).rem_euclid(7) as usize;
                } else {
                    let v = step_f32(s.get(r), steps, 0.01, coarse, 0.0, 1.0);
                    s.set(r, v);
                }
                let new = s.get_param(slot);
                if let (Some(old), Some(new)) = (old, new) {
                    history.record(slot, "Adjust", old, new);
                }
            }
            KeyCode::Char('0') => {
                count.clear();
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                let r = all_rows[s.selected.min(all_rows.len() - 1)];
                if matches!(r, Row::Notes(_) | Row::Amp) {
                    continue;
                }
                let slot = s.selected;
                let old = s.get_param(slot);
                let def = EdgesState::new();
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
                let r = all_rows[s.selected.min(all_rows.len() - 1)];
                if binding_slot(r).is_some() {
                    let current = match r {
                        Row::Amp => s.amp_src.clone(),
                        Row::Notes(c) => s.notes_src[c].clone(),
                        _ => src_index(r).and_then(|k| s.srcs[k].clone()),
                    };
                    drop(s);
                    let sources = Manifest::open()
                        .map(|m| routing::live_sources(&m.entries()))
                        .unwrap_or_default();
                    picker.open(sources, current.as_ref());
                } else {
                    ex_msg = Some("shape rows cycle with h/l".into());
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let r = all_rows[s.selected.min(all_rows.len() - 1)];
                if let Some(slot) = binding_slot(r) {
                    let had = match r {
                        Row::Amp => s.amp_src.is_some(),
                        Row::Notes(c) => s.notes_src[c].is_some(),
                        _ => src_index(r).is_some_and(|k| s.srcs[k].is_some()),
                    };
                    if had {
                        let old = s.get_param(slot);
                        match r {
                            Row::Amp => {
                                s.amp_src = None;
                                s.amp_resolved = None;
                            }
                            Row::Notes(c) => s.notes_src[c] = None,
                            _ => {
                                if let Some(k) = src_index(r) {
                                    s.srcs[k] = None;
                                    s.resolved[k] = None;
                                }
                            }
                        }
                        if let Some(old) = old {
                            history.record(slot, "Unbind", old, ParamValue::Src(None));
                        }
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
                shared.lock().unwrap().selected = all_rows.len() - 1;
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

fn ex_set(
    s: &mut EdgesState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    // keys: shapeN, pwN, xposeN, lvlN, notesN (N=1..4), level, amp
    let chan = |k: &str, prefix: &str| -> Option<usize> {
        k.strip_prefix(prefix)
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| (1..=4).contains(&n).then_some(n - 1))
    };
    if let Some(c) = chan(key, "shape") {
        if let Some(i) = SHAPE_NAMES.iter().position(|n| *n == value) {
            s.shape[c] = i;
            return format!("{key} = {value}");
        }
        return format!("{key}: one of {}", SHAPE_NAMES.join(" "));
    }
    if let Some(c) = chan(key, "notes") {
        let slot = NOTES_SLOT_BASE + c;
        let v = if value == "-" {
            V::Src(None)
        } else {
            V::Src(Some(value.to_string()))
        };
        let old = s.get_param(slot);
        s.set_param(slot, v.clone());
        if let Some(old) = old {
            history.record(slot, "Set", old, v);
        }
        return format!("{key} set");
    }
    if key == "amp" {
        let v = if value == "-" {
            V::Src(None)
        } else {
            V::Src(Some(value.to_string()))
        };
        s.set_param(AMP_SLOT, v);
        return "amp set".into();
    }
    let row = if let Some(c) = chan(key, "pw") {
        Row::Pw(c)
    } else if let Some(c) = chan(key, "xpose") {
        Row::Xpose(c)
    } else if let Some(c) = chan(key, "lvl") {
        Row::Lvl(c)
    } else if key == "level" {
        Row::Level
    } else {
        return format!("Unknown setting: {key}");
    };
    match value.parse::<f32>() {
        Ok(v) => {
            s.set(row, v);
            format!("{key} = {}", row_text(s, row))
        }
        Err(_) => {
            let Some(k) = src_index(row) else {
                return format!("{key}: not bindable");
            };
            let v = if value == "-" {
                V::Src(None)
            } else {
                if SourceAddr::parse(value).is_none() {
                    return format!("{key}: not a number or source: {value}");
                }
                V::Src(Some(value.to_string()))
            };
            let bind_slot = SRC_SLOT_BASE + k;
            let old = s.get_param(bind_slot);
            s.set_param(bind_slot, v.clone());
            if let Some(old) = old {
                history.record(bind_slot, "Set", old, v);
            }
            format!("{key} cable updated")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_voice(shape: usize, n: usize) -> Vec<f32> {
        let mut v = ChipVoice::new(0xACE1);
        v.shape = shape;
        v.note_on(220.0);
        let mut out = vec![0.0; n];
        v.render(48_000.0, 0.5, 0.0, &mut out);
        out
    }

    #[test]
    fn every_shape_sounds_and_stays_bounded() {
        for shape in 0..7 {
            let out = render_voice(shape, 4800);
            let energy: f32 = out.iter().map(|v| v * v).sum();
            assert!(energy > 0.0, "shape {shape} sounds");
            assert!(
                out.iter().all(|v| v.abs() <= 1.0 && v.is_finite()),
                "shape {shape} bounded"
            );
        }
    }

    #[test]
    fn square_pw_shifts_the_duty_cycle() {
        let duty = |pw: f32| -> f32 {
            let mut v = ChipVoice::new(0x1);
            v.shape = 0;
            v.note_on(100.0);
            let mut out = vec![0.0; 48_000];
            v.render(48_000.0, pw, 0.0, &mut out);
            out.iter().filter(|x| **x > 0.0).count() as f32 / out.len() as f32
        };
        assert!((duty(0.5) - 0.5).abs() < 0.02);
        assert!((duty(0.95) - 0.95).abs() < 0.02);
    }

    #[test]
    fn nes_noise_short_repeats_faster_than_long() {
        // measure the LFSR orbit directly (the firmware law: shift
        // right, feedback bit0^bit1 long / bit0^bit6 short into bit14)
        let orbit = |short: bool| -> usize {
            let step = |state: &mut u16| {
                let mut tap = *state >> 1;
                if short {
                    tap >>= 5;
                }
                let random_bit = (*state ^ tap) & 1;
                *state >>= 1;
                if random_bit != 0 {
                    *state |= 0x4000;
                }
            };
            let mut state: u16 = 0xACE1 | 1;
            // settle onto the cycle first — the seed may sit on a
            // tail leading into it
            for _ in 0..65_536 {
                step(&mut state);
            }
            let anchor = state;
            for steps in 1..200_000 {
                step(&mut state);
                if state == anchor {
                    return steps;
                }
            }
            usize::MAX
        };
        let short = orbit(true);
        let long = orbit(false);
        assert!(short < long, "short orbit {short} < long {long}");
    }

    #[test]
    fn transpose_shifts_pitch() {
        let zc = |xpose: f32| -> usize {
            let mut v = ChipVoice::new(0x2);
            v.shape = 1;
            v.note_on(220.0);
            let mut out = vec![0.0; 48_000];
            v.render(48_000.0, 0.5, xpose, &mut out);
            out.windows(2)
                .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
                .count()
        };
        let base = zc(0.0);
        let up = zc(12.0);
        assert!(
            (up as f32 / base as f32 - 2.0).abs() < 0.1,
            "+12 st doubles: {base} -> {up}"
        );
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = EdgesState::new();
        s.shape = [0, 2, 4, 6];
        s.pw[1] = 0.8;
        s.notes_src[2] = SourceAddr::parse("sequencer/0/t3");
        s.srcs[4] = SourceAddr::parse("lfo/0/s1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::EdgesParams = toml::from_str(&toml).expect("parses");
        let mut s2 = EdgesState::new();
        apply_params(&mut s2, &back);
        assert_eq!(s2.shape, [0, 2, 4, 6]);
        assert!((s2.pw[1] - 0.8).abs() < 1e-6);
        assert!(s2.notes_src[2].is_some());
        assert!(s2.srcs[4].is_some());
    }

    #[test]
    fn every_value_row_is_bindable() {
        use crate::undo::ParamUndo;
        use crate::undo::ParamValue as V;
        for r in rows() {
            if matches!(r, Row::Shape(_) | Row::Notes(_) | Row::Amp) {
                continue;
            }
            let i = src_index(r).unwrap_or_else(|| panic!("{r:?} must be bindable"));
            let mut s = EdgesState::new();
            s.set_param(SRC_SLOT_BASE + i, V::Src(Some("lfo/0/a1".into())));
            assert!(s.srcs[i].is_some(), "{r:?} binds");
        }
    }
}
