//! The Tides module shell: four slope outputs on the modulation bus
//! AND the audio ring, notes as gates/V-oct, optional transport lock.
//!
//! (The DSP underneath is the MIT-licensed Mutable Instruments port —
//! see dsp.rs for the copyright and permission notice.)

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

use super::dsp::{
    extract_gate_flags, OutputMode, PolySlopeGenerator, RampMode, Range, GATE_FLAG_LOW,
    NUM_CHANNELS,
};
use crate::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

const FALLBACK_RATE: f32 = 48_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Mode,
    Output,
    Range,
    Freq,
    Shape,
    Slope,
    Smooth,
    Shift,
    Sync,
    Level,
    Notes,
    Amp,
}

const ROWS: [Row; 12] = [
    Row::Mode,
    Row::Output,
    Row::Range,
    Row::Freq,
    Row::Shape,
    Row::Slope,
    Row::Smooth,
    Row::Shift,
    Row::Sync,
    Row::Level,
    Row::Notes,
    Row::Amp,
];

/// CV bank, srcs[] order — every value row takes a cable.
const BINDABLE: [Row; 6] = [
    Row::Freq,
    Row::Shape,
    Row::Slope,
    Row::Smooth,
    Row::Shift,
    Row::Level,
];
const N_SRC: usize = BINDABLE.len();

const RAMP_MODES: [RampMode; 3] = [RampMode::Ad, RampMode::Looping, RampMode::Ar];
const RAMP_MODE_NAMES: [&str; 3] = ["ad", "loop", "ar"];
const OUTPUT_MODES: [OutputMode; 4] = [
    OutputMode::Gates,
    OutputMode::Amplitude,
    OutputMode::SlopePhase,
    OutputMode::Frequency,
];
const OUTPUT_MODE_NAMES: [&str; 4] = ["gates", "amplitude", "phase", "frequency"];
const RANGE_NAMES: [&str; 2] = ["control", "audio"];

struct TidesState {
    mode: usize,
    output: usize,
    range: usize,
    freq: f32,
    shape: f32,
    slope: f32,
    smooth: f32,
    shift: f32,
    sync: bool,
    level: f32,
    note_hz: f32,
    gate: bool,
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    amp_src: Option<SourceAddr>,
    amp_resolved: Option<usize>,
    notes_src: Option<SourceAddr>,
    /// Live output values for the UI meters.
    out_now: [f32; NUM_CHANNELS],
    selected: usize,
}

impl TidesState {
    fn new() -> Self {
        let mut s = TidesState {
            mode: 1, // looping, the hardware's home position
            output: 2,
            range: 0,
            freq: 0.4,
            shape: 0.5,
            slope: 0.5,
            smooth: 0.5,
            shift: 0.5,
            sync: false,
            level: 0.8,
            note_hz: 110.0,
            gate: false,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.0; N_SRC],
            amp_src: None,
            amp_resolved: None,
            notes_src: None,
            out_now: [0.0; NUM_CHANNELS],
            selected: 0,
        };
        for (k, r) in BINDABLE.iter().enumerate() {
            s.eff[k] = s.get(*r);
        }
        s
    }

    fn get(&self, r: Row) -> f32 {
        match r {
            Row::Freq => self.freq,
            Row::Shape => self.shape,
            Row::Slope => self.slope,
            Row::Smooth => self.smooth,
            Row::Shift => self.shift,
            Row::Level => self.level,
            _ => 0.0,
        }
    }

    fn set(&mut self, r: Row, v: f32) {
        let v = v.clamp(0.0, 1.0);
        match r {
            Row::Freq => self.freq = v,
            Row::Shape => self.shape = v,
            Row::Slope => self.slope = v,
            Row::Smooth => self.smooth = v,
            Row::Shift => self.shift = v,
            Row::Level => self.level = v,
            _ => {}
        }
    }
}

const SRC_SLOT_BASE: usize = 50;
const AMP_SLOT: usize = 40;
const NOTES_SLOT: usize = 41;

impl crate::undo::ParamUndo for TidesState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        match slot {
            AMP_SLOT => return Some(V::Src(self.amp_src.as_ref().map(|a| a.to_string()))),
            NOTES_SLOT => return Some(V::Src(self.notes_src.as_ref().map(|a| a.to_string()))),
            _ => {}
        }
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            if i < N_SRC {
                return Some(V::Src(self.srcs[i].as_ref().map(|a| a.to_string())));
            }
            return None;
        }
        let r = *ROWS.get(slot)?;
        Some(match r {
            Row::Mode => V::Usize(self.mode),
            Row::Output => V::Usize(self.output),
            Row::Range => V::Usize(self.range),
            Row::Sync => V::Bool(self.sync),
            Row::Notes | Row::Amp => return None,
            _ => V::F32(self.get(r)),
        })
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        let parse = |v: &Option<String>| v.as_deref().and_then(SourceAddr::parse);
        match (slot, &value) {
            (AMP_SLOT, V::Src(v)) => {
                self.amp_src = parse(v);
                self.amp_resolved = None;
                return;
            }
            (NOTES_SLOT, V::Src(v)) => {
                self.notes_src = parse(v);
                return;
            }
            _ => {}
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
        match (ROWS.get(slot).copied(), value) {
            (Some(Row::Mode), V::Usize(v)) => self.mode = v.min(2),
            (Some(Row::Output), V::Usize(v)) => self.output = v.min(3),
            (Some(Row::Range), V::Usize(v)) => self.range = v.min(1),
            (Some(Row::Sync), V::Bool(v)) => self.sync = v,
            (Some(r), V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &TidesState) -> state::TidesParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::TidesParams {
        format: state::STATE_FORMAT,
        mode: Some(RAMP_MODE_NAMES[s.mode.min(2)].into()),
        output: Some(OUTPUT_MODE_NAMES[s.output.min(3)].into()),
        range: Some(RANGE_NAMES[s.range.min(1)].into()),
        freq: Some(s.freq),
        shape: Some(s.shape),
        slope: Some(s.slope),
        smooth: Some(s.smooth),
        shift: Some(s.shift),
        sync: Some(s.sync),
        level: Some(s.level),
        freq_src: src(0),
        shape_src: src(1),
        slope_src: src(2),
        smooth_src: src(3),
        shift_src: src(4),
        level_src: src(5),
        amp_src: s.amp_src.as_ref().map(|a| a.to_string()),
        notes_src: s.notes_src.as_ref().map(|a| a.to_string()),
    }
}

fn apply_params(s: &mut TidesState, p: &state::TidesParams) {
    if let Some(m) = p.mode.as_deref() {
        if let Some(i) = RAMP_MODE_NAMES.iter().position(|x| *x == m) {
            s.mode = i;
        }
    }
    if let Some(m) = p.output.as_deref() {
        if let Some(i) = OUTPUT_MODE_NAMES.iter().position(|x| *x == m) {
            s.output = i;
        }
    }
    if let Some(m) = p.range.as_deref() {
        if let Some(i) = RANGE_NAMES.iter().position(|x| *x == m) {
            s.range = i;
        }
    }
    macro_rules! f {
        ($row:expr, $field:ident) => {
            if let Some(v) = p.$field {
                s.set($row, v);
            }
        };
    }
    f!(Row::Freq, freq);
    f!(Row::Shape, shape);
    f!(Row::Slope, slope);
    f!(Row::Smooth, smooth);
    f!(Row::Shift, shift);
    f!(Row::Level, level);
    if let Some(v) = p.sync {
        s.sync = v;
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.freq_src),
        parse(&p.shape_src),
        parse(&p.slope_src),
        parse(&p.smooth_src),
        parse(&p.shift_src),
        parse(&p.level_src),
    ];
    s.amp_src = parse(&p.amp_src);
    s.notes_src = parse(&p.notes_src);
    s.resolved = Default::default();
    s.amp_resolved = None;
}

// ── audio thread ───────────────────────────────────────────────────────────

/// Control-range rate law: 0–1 → 0.05 Hz … 50 Hz (log).
fn control_hz(knob: f32) -> f32 {
    0.05 * 1000.0_f32.powf(knob.clamp(0.0, 1.0))
}

fn audio_thread(shared: Arc<Mutex<TidesState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_tides_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating tides ring")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    // four claims: the four slope outputs on the modulation bus
    manifest.register("tides", instance, Some(&shm_name), NUM_CHANNELS as u32)?;
    let mod_base = manifest.claimed_base();
    let mut modbus = ModulationBus::open()
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
    let mut out = vec![[0.0_f32; NUM_CHANNELS]; slot_frames];
    let mut gates = vec![GATE_FLAG_LOW; slot_frames];
    let mut ramp = vec![0.0_f32; slot_frames];

    let mut psg = PolySlopeGenerator::new();
    let mut note_filter: Option<u8> = None;
    let mut prev_gate_flag = GATE_FLAG_LOW;
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
            note_filter = s.notes_src.as_ref().and_then(routing::note_source_track);
            let mask = s
                .resolved
                .iter()
                .flatten()
                .chain(s.amp_resolved.iter())
                .filter(|&&c| c < 64)
                .fold(0u64, |m, &c| m | (1 << c));
            let notes = note_filter.filter(|&t| t < 8).map_or(0u8, |t| 1 << t);
            manifest.publish_consumes(mask, notes);
        }

        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                if let Some(t) = note_filter {
                    if event.source != t {
                        continue;
                    }
                }
                let mut s = shared.lock().unwrap();
                if event.is_note_on() {
                    if event.value.is_finite() && event.value > 0.0 {
                        s.note_hz = event.value;
                    }
                    s.gate = true;
                } else if event.is_note_off() {
                    s.gate = false;
                }
            }
        }

        let (mode, output_mode, range, f0, shape, slope, smooth, shift, sync, level, amp, gate) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            // max/min, not clamp: NaN from a stale channel dies here
            #[allow(clippy::manual_clamp)]
            let cv = |k: usize, manual: f32, s: &TidesState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let mut vals = [0.0_f32; N_SRC];
            for (k, r) in BINDABLE.iter().enumerate() {
                vals[k] = cv(k, s.get(*r), &s);
                s.eff[k] = vals[k];
            }
            let amp = match (s.amp_src.is_some(), s.amp_resolved, bus) {
                (false, _, _) => 1.0,
                (true, Some(ch), Some(b)) => b.get(ch).clamp(0.0, 1.0),
                (true, _, _) => 0.0,
            };
            let range = [Range::Control, Range::Audio][s.range.min(1)];
            // frequency: audio range tracks the note (knob = ±2 oct
            // offset); control range is the rate knob alone
            let f0 = match range {
                Range::Audio => {
                    s.note_hz * 2.0_f32.powf((vals[0] - 0.5) * 4.0) / sample_rate
                }
                Range::Control => control_hz(vals[0]) / sample_rate,
            };
            (
                RAMP_MODES[s.mode.min(2)],
                OUTPUT_MODES[s.output.min(3)],
                range,
                f0.min(0.25),
                vals[1],
                vals[2],
                vals[3],
                vals[4],
                s.sync,
                vals[5],
                amp,
                s.gate,
            )
        };

        // gate flags from the note gate
        for g in gates.iter_mut() {
            prev_gate_flag = extract_gate_flags(prev_gate_flag, gate);
            *g = prev_gate_flag;
        }

        // transport lock: the external ramp is the beat phase
        let external = if sync && mode == RampMode::Looping {
            if let Some(ref t) = transport {
                let bpm = f64::from(t.bpm()).clamp(20.0, 300.0);
                let spb = (60.0 / bpm * f64::from(sample_rate)).max(1.0);
                let clock = t.clock();
                for (i, r) in ramp.iter_mut().enumerate() {
                    let pos = (clock as f64 + i as f64) % spb;
                    *r = (pos / spb) as f32;
                }
                Some(&ramp[..])
            } else {
                None
            }
        } else {
            None
        };

        psg.render(
            mode,
            output_mode,
            range,
            f0,
            slope,
            shape,
            smooth,
            shift,
            &gates,
            external,
            &mut out,
        );

        // publish the four slopes on the bus, normalized to 0..1
        let norm = |v: f32| -> f32 {
            if mode == RampMode::Looping {
                (v / 10.0 + 0.5).clamp(0.0, 1.0)
            } else {
                (v / 8.0).clamp(0.0, 1.0)
            }
        };
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            let last = out[slot_frames - 1];
            for (i, v) in last.iter().enumerate() {
                bus.set(base + i, norm(*v));
            }
            let mut s = shared.lock().unwrap();
            for (o, v) in s.out_now.iter_mut().zip(last.iter()) {
                *o = norm(*v);
            }
        }

        // audio out: ch1/ch2 as L/R (frequency mode mixes all four) —
        // volts to sample scale per mode
        let v_scale = if mode == RampMode::Looping { 0.2 } else { 0.125 };
        let gain_target = amp * level;
        let g_alpha = 1.0 - (-1.0 / (0.0007 * sample_rate)).exp();
        for f in 0..slot_frames {
            gain_smooth += (gain_target - gain_smooth) * g_alpha;
            let (l, r) = if output_mode == OutputMode::Frequency {
                (
                    (out[f][0] + out[f][2]) * 0.5 * v_scale,
                    (out[f][1] + out[f][3]) * 0.5 * v_scale,
                )
            } else {
                (out[f][0] * v_scale, out[f][1] * v_scale)
            };
            block[f * channels] = l * gain_smooth;
            if channels > 1 {
                block[f * channels + 1] = r * gain_smooth;
            }
        }
        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            psg = PolySlopeGenerator::new();
        }
        while ringbuf.write(&block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }
        blocks += 1;
    }
}

// ── ui ─────────────────────────────────────────────────────────────────────

fn row_label(r: Row) -> &'static str {
    match r {
        Row::Mode => "mode",
        Row::Output => "output",
        Row::Range => "range",
        Row::Freq => "freq",
        Row::Shape => "shape",
        Row::Slope => "slope",
        Row::Smooth => "smooth",
        Row::Shift => "shift",
        Row::Sync => "sync",
        Row::Level => "level",
        Row::Notes => "notes",
        Row::Amp => "amp",
    }
}

fn src_index(r: Row) -> Option<usize> {
    BINDABLE.iter().position(|b| *b == r)
}

fn binding_slot(r: Row) -> Option<usize> {
    match r {
        Row::Amp => Some(AMP_SLOT),
        Row::Notes => Some(NOTES_SLOT),
        _ => src_index(r).map(|i| SRC_SLOT_BASE + i),
    }
}

fn row_text(s: &TidesState, r: Row) -> String {
    match r {
        Row::Mode => RAMP_MODE_NAMES[s.mode.min(2)].into(),
        Row::Output => OUTPUT_MODE_NAMES[s.output.min(3)].into(),
        Row::Range => RANGE_NAMES[s.range.min(1)].into(),
        Row::Freq => {
            if s.range == 1 {
                format!("{:.0}% · note ±2 oct", s.freq * 100.0)
            } else {
                format!("{:.2} Hz", control_hz(s.freq))
            }
        }
        Row::Sync => if s.sync { "transport beat" } else { "free" }.into(),
        Row::Notes => s
            .notes_src
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(all tracks)".into()),
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
    s: &TidesState,
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
        lines.push(theme::header(
            "TIDES",
            &format!("slopes {}", instance),
            "",
            w,
        ));
        // the four output meters
        let mut meter_spans = vec![Span::styled("  out ".to_string(), theme::chrome())];
        for v in s.out_now.iter() {
            meter_spans.push(Span::styled(
                theme::meter_char(*v).to_string(),
                theme::signal(theme::cv_ramp(*v)),
            ));
            meter_spans.push(Span::raw(" "));
        }
        lines.push(Line::from(meter_spans));
        lines.push(Line::from(""));

        let bar_w = theme::bar_width(w, 32);
        for (i, r) in ROWS.iter().enumerate() {
            let selected = i == s.selected;
            let label_style = if selected {
                theme::selected()
            } else {
                theme::chrome()
            };
            let mut spans: Vec<Span> =
                vec![Span::styled(format!(" {:<8}", row_label(*r)), label_style)];
            let bound = src_index(*r).is_some_and(|k| s.srcs[k].is_some())
                || (*r == Row::Amp && s.amp_src.is_some())
                || (*r == Row::Notes && s.notes_src.is_some());
            let hue = src_index(*r)
                .and_then(|k| s.srcs[k].as_ref())
                .or(match r {
                    Row::Amp => s.amp_src.as_ref(),
                    Row::Notes => s.notes_src.as_ref(),
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
                Line::from("━━━ TIDES · tidal modulator (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  mode     ad (one-shot) · loop · ar (sustain)"),
                Line::from("  output   gates · amplitude · phase · frequency"),
                Line::from("  range    control (LFO/env) · audio (V/oct osc)"),
                Line::from("  freq     rate (control) / note offset (audio)"),
                Line::from("  shape    the waveshaper bank"),
                Line::from("  slope    attack/decay skew"),
                Line::from("  smooth   < 50% filters · > 50% wavefolds"),
                Line::from("  shift    phase spread / ratios / level scan"),
                Line::from("  sync     lock the loop to the transport beat"),
                Line::from(""),
                Line::from("Outputs tides/N/o1–o4 on the bus; o1/o2 are the"),
                Line::from("audio pair. Notes gate the ramps; pitch = V/oct."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" TIDES ", theme::chrome_hi())),
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

fn adjust_selector(s: &mut TidesState, r: Row, steps: i32) {
    let bump = |v: usize, n: usize| ((v as i32 + steps).rem_euclid(n as i32)) as usize;
    match r {
        Row::Mode => s.mode = bump(s.mode, 3),
        Row::Output => s.output = bump(s.output, 4),
        Row::Range => s.range = bump(s.range, 2),
        Row::Sync => s.sync = !s.sync,
        _ => {}
    }
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("tides", instance);

    let shared = Arc::new(Mutex::new(TidesState::new()));
    if let Ok(p) = state::load_module_state::<state::TidesParams>("tides", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let audio_builder = thread::Builder::new()
        .name(String::from("tides-audio"))
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
        eprintln!("[tides {instance}] audio thread died: {msg}");
        let path = crate::state::tmp_dir().join(format!("tides_{instance}.crash"));
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
            let _ = state::save_module_state("tides", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::TidesParams>("tides", instance) {
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
                    let steps: f32 = if m.kind == MouseEventKind::ScrollUp {
                        0.01
                    } else {
                        -0.01
                    };
                    use crate::undo::ParamUndo;
                    let mut s = shared.lock().unwrap();
                    let slot = s.selected;
                    let r = ROWS[slot.min(ROWS.len() - 1)];
                    if src_index(r).is_none() {
                        continue;
                    }
                    let old = s.get_param(slot);
                    let v = s.get(r) + steps;
                    s.set(r, v);
                    let new = s.get_param(slot);
                    if let (Some(old), Some(new)) = (old, new) {
                        history.record(slot, "Adjust", old, new);
                    }
                }
                MouseEventKind::Down(_) => {
                    let row = (m.row as usize).saturating_sub(3);
                    if row < ROWS.len() {
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
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                if let Some(slot) = binding_slot(r) {
                    let old = s.get_param(slot);
                    let text = addr.as_ref().map(|a| a.to_string());
                    match r {
                        Row::Amp => {
                            s.amp_src = addr.clone();
                            s.amp_resolved = None;
                        }
                        Row::Notes => s.notes_src = addr.clone(),
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
                    ExCommand::Edit(name) => match state::load_patch::<state::TidesParams>(&name) {
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
            let _ = state::save_module_state("tides", instance, &params);
            ex_msg = Some(String::from("Saved"));
            continue;
        }

        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
            KeyCode::Char('j') | KeyCode::Down => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, n, ROWS.len());
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, -n, ROWS.len());
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
                let r = ROWS[slot.min(ROWS.len() - 1)];
                if matches!(r, Row::Notes | Row::Amp) {
                    ex_msg = Some("routing row: @ binds, x unbinds".into());
                    continue;
                }
                let old = s.get_param(slot);
                if src_index(r).is_some() {
                    let v = step_f32(s.get(r), steps, 0.01, coarse, 0.0, 1.0);
                    s.set(r, v);
                } else {
                    adjust_selector(&mut s, r, steps.signum());
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
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                if matches!(r, Row::Notes | Row::Amp) {
                    continue;
                }
                let slot = s.selected;
                let old = s.get_param(slot);
                let def = TidesState::new();
                if let Some(v) = def.get_param(slot) {
                    s.set_param(slot, v.clone());
                    if let Some(old) = old {
                        history.record(slot, "Reset", old, v);
                    }
                }
            }
            KeyCode::Char('@') | KeyCode::Enter => {
                count.clear();
                let s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                if binding_slot(r).is_some() {
                    let current = match r {
                        Row::Amp => s.amp_src.clone(),
                        Row::Notes => s.notes_src.clone(),
                        _ => src_index(r).and_then(|k| s.srcs[k].clone()),
                    };
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
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                if let Some(slot) = binding_slot(r) {
                    let had = match r {
                        Row::Amp => s.amp_src.is_some(),
                        Row::Notes => s.notes_src.is_some(),
                        _ => src_index(r).is_some_and(|k| s.srcs[k].is_some()),
                    };
                    if had {
                        let old = s.get_param(slot);
                        match r {
                            Row::Amp => {
                                s.amp_src = None;
                                s.amp_resolved = None;
                            }
                            Row::Notes => s.notes_src = None,
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
                shared.lock().unwrap().selected = ROWS.len() - 1;
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
    s: &mut TidesState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    let labels: Vec<&str> = ROWS.iter().map(|r| row_label(*r)).collect();
    let Some(slot) = labels.iter().position(|l| *l == key) else {
        return format!("Unknown setting: {key}");
    };
    let r = ROWS[slot];
    match r {
        Row::Mode => {
            if let Some(i) = RAMP_MODE_NAMES.iter().position(|m| *m == value) {
                s.mode = i;
                return format!("mode = {value}");
            }
            return format!("mode: one of {}", RAMP_MODE_NAMES.join(" "));
        }
        Row::Output => {
            if let Some(i) = OUTPUT_MODE_NAMES.iter().position(|m| *m == value) {
                s.output = i;
                return format!("output = {value}");
            }
            return format!("output: one of {}", OUTPUT_MODE_NAMES.join(" "));
        }
        Row::Range => {
            if let Some(i) = RANGE_NAMES.iter().position(|m| *m == value) {
                s.range = i;
                return format!("range = {value}");
            }
            return format!("range: one of {}", RANGE_NAMES.join(" "));
        }
        Row::Sync => {
            s.sync = matches!(value, "on" | "1" | "true" | "beat" | "transport");
            return format!("sync = {}", row_text(s, r));
        }
        Row::Notes | Row::Amp => {
            let v = if value == "-" {
                V::Src(None)
            } else {
                V::Src(Some(value.to_string()))
            };
            let slot = binding_slot(r).unwrap_or(slot);
            let old = s.get_param(slot);
            s.set_param(slot, v.clone());
            if let Some(old) = old {
                history.record(slot, "Set", old, v);
            }
            return format!("{} = {}", key, row_text(s, r));
        }
        _ => {}
    }
    match value.parse::<f32>() {
        Ok(v) => {
            let old = s.get_param(slot);
            s.set_param(slot, V::F32(v));
            if let Some(old) = old {
                history.record(slot, "Set", old, V::F32(v));
            }
            format!("{} = {}", key, row_text(s, r))
        }
        Err(_) => {
            let Some(bind_slot) = binding_slot(r) else {
                return format!("{key}: not a number: {value}");
            };
            let v = if value == "-" {
                V::Src(None)
            } else {
                if SourceAddr::parse(value).is_none() {
                    return format!("{key}: not a number or source: {value}");
                }
                V::Src(Some(value.to_string()))
            };
            let old = s.get_param(bind_slot);
            s.set_param(bind_slot, v.clone());
            if let Some(old) = old {
                history.record(bind_slot, "Set", old, v);
            }
            match &s.srcs[src_index(r).unwrap_or(0)] {
                Some(a) => format!("{key} ← {a}"),
                None => format!("{key} unbound"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = TidesState::new();
        s.set(Row::Shape, 0.7);
        s.mode = 0;
        s.output = 3;
        s.range = 1;
        s.sync = true;
        s.notes_src = SourceAddr::parse("sequencer/0/t2");
        s.srcs[1] = SourceAddr::parse("lfo/0/s1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::TidesParams = toml::from_str(&toml).expect("parses");
        let mut s2 = TidesState::new();
        apply_params(&mut s2, &back);
        assert!((s2.get(Row::Shape) - 0.7).abs() < 1e-6);
        assert_eq!(s2.mode, 0);
        assert_eq!(s2.output, 3);
        assert_eq!(s2.range, 1);
        assert!(s2.sync);
        assert_eq!(
            s2.srcs[1].as_ref().map(|a| a.to_string()),
            Some("lfo/0/s1".into())
        );
    }

    #[test]
    fn every_value_row_is_bindable() {
        use crate::undo::ParamUndo;
        use crate::undo::ParamValue as V;
        for r in ROWS {
            if matches!(
                r,
                Row::Mode | Row::Output | Row::Range | Row::Sync | Row::Notes | Row::Amp
            ) {
                continue;
            }
            let i = src_index(r).unwrap_or_else(|| panic!("{r:?} must be bindable"));
            let mut s = TidesState::new();
            s.set_param(SRC_SLOT_BASE + i, V::Src(Some("lfo/0/a1".into())));
            assert!(s.srcs[i].is_some(), "{r:?} binds");
        }
    }

    #[test]
    fn ex_set_selectors_and_cables() {
        let mut s = TidesState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "mode", "ar").contains("ar"));
        assert_eq!(s.mode, 2);
        assert!(ex_set(&mut s, &mut h, "output", "frequency").contains("frequency"));
        assert!(ex_set(&mut s, &mut h, "shape", "lfo/0/s2").contains("lfo/0/s2"));
        assert!(ex_set(&mut s, &mut h, "shape", "-").contains("unbound"));
        assert!(ex_set(&mut s, &mut h, "sync", "on").contains("transport"));
    }

    #[test]
    fn control_rate_law_is_log_and_musical() {
        assert!((control_hz(0.0) - 0.05).abs() < 1e-4);
        assert!((control_hz(1.0) - 50.0).abs() < 0.1);
        assert!(control_hz(0.5) > 1.0 && control_hz(0.5) < 2.5);
    }
}
