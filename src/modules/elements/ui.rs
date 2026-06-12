//! The Elements module shell: the panel as rows, notes as gate+pitch,
//! velocity as the hardware's strength input.
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

use super::voice::{Part, Patch};
use crate::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

const FALLBACK_RATE: f32 = 48_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Contour,
    Bow,
    BowT,
    Blow,
    BlowM,
    BlowT,
    Strike,
    StrikeM,
    StrikeT,
    Geometry,
    Brightness,
    Damping,
    Position,
    Space,
    Level,
    Notes,
    Amp,
}

const ROWS: [Row; 17] = [
    Row::Contour,
    Row::Bow,
    Row::BowT,
    Row::Blow,
    Row::BlowM,
    Row::BlowT,
    Row::Strike,
    Row::StrikeM,
    Row::StrikeT,
    Row::Geometry,
    Row::Brightness,
    Row::Damping,
    Row::Position,
    Row::Space,
    Row::Level,
    Row::Notes,
    Row::Amp,
];

/// CV bank, srcs[] order — every value row is bindable (the hardware
/// exposes six CV inputs; in los every parameter takes a cable).
const BINDABLE: [Row; 15] = [
    Row::Contour,
    Row::Bow,
    Row::BowT,
    Row::Blow,
    Row::BlowM,
    Row::BlowT,
    Row::Strike,
    Row::StrikeM,
    Row::StrikeT,
    Row::Geometry,
    Row::Brightness,
    Row::Damping,
    Row::Position,
    Row::Space,
    Row::Level,
];
const N_SRC: usize = BINDABLE.len();

struct ElementsState {
    patch: Patch,
    level: f32,
    freq: f32,
    gate: bool,
    velocity: f32,
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    amp_src: Option<SourceAddr>,
    amp_resolved: Option<usize>,
    notes_src: Option<SourceAddr>,
    exc_now: f32,
    selected: usize,
}

impl ElementsState {
    fn new() -> Self {
        let mut s = ElementsState {
            patch: Patch::default(),
            level: 0.8,
            freq: 220.0,
            gate: false,
            velocity: 0.0,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.0; N_SRC],
            amp_src: None,
            amp_resolved: None,
            notes_src: None,
            exc_now: 0.0,
            selected: 0,
        };
        for (k, r) in BINDABLE.iter().enumerate() {
            s.eff[k] = s.get(*r);
        }
        s
    }

    fn get(&self, r: Row) -> f32 {
        let p = &self.patch;
        match r {
            Row::Contour => p.exciter_envelope_shape,
            Row::Bow => p.exciter_bow_level,
            Row::BowT => p.exciter_bow_timbre,
            Row::Blow => p.exciter_blow_level,
            Row::BlowM => p.exciter_blow_meta,
            Row::BlowT => p.exciter_blow_timbre,
            Row::Strike => p.exciter_strike_level,
            Row::StrikeM => p.exciter_strike_meta,
            Row::StrikeT => p.exciter_strike_timbre,
            Row::Geometry => p.resonator_geometry,
            Row::Brightness => p.resonator_brightness,
            Row::Damping => p.resonator_damping,
            Row::Position => p.resonator_position,
            Row::Space => p.space,
            Row::Level => self.level,
            Row::Notes | Row::Amp => 0.0,
        }
    }

    fn set(&mut self, r: Row, v: f32) {
        let v = v.clamp(0.0, 1.0);
        let p = &mut self.patch;
        match r {
            Row::Contour => p.exciter_envelope_shape = v,
            Row::Bow => p.exciter_bow_level = v,
            Row::BowT => p.exciter_bow_timbre = v,
            Row::Blow => p.exciter_blow_level = v,
            Row::BlowM => p.exciter_blow_meta = v,
            Row::BlowT => p.exciter_blow_timbre = v,
            Row::Strike => p.exciter_strike_level = v,
            Row::StrikeM => p.exciter_strike_meta = v,
            Row::StrikeT => p.exciter_strike_timbre = v,
            Row::Geometry => p.resonator_geometry = v,
            Row::Brightness => p.resonator_brightness = v,
            Row::Damping => p.resonator_damping = v,
            Row::Position => p.resonator_position = v,
            Row::Space => p.space = v,
            Row::Level => self.level = v,
            Row::Notes | Row::Amp => {}
        }
    }
}

/// Manual fallback value for a bindable row, read from the working
/// copies the audio thread mutates (the patch and the output level).
fn row_value(p: &Patch, level: f32, r: Row) -> f32 {
    match r {
        Row::Contour => p.exciter_envelope_shape,
        Row::Bow => p.exciter_bow_level,
        Row::BowT => p.exciter_bow_timbre,
        Row::Blow => p.exciter_blow_level,
        Row::BlowM => p.exciter_blow_meta,
        Row::BlowT => p.exciter_blow_timbre,
        Row::Strike => p.exciter_strike_level,
        Row::StrikeM => p.exciter_strike_meta,
        Row::StrikeT => p.exciter_strike_timbre,
        Row::Geometry => p.resonator_geometry,
        Row::Brightness => p.resonator_brightness,
        Row::Damping => p.resonator_damping,
        Row::Position => p.resonator_position,
        Row::Space => p.space,
        Row::Level => level,
        Row::Notes | Row::Amp => 0.0,
    }
}

/// Write a bank-effective value into the working copies.
fn set_row_value(p: &mut Patch, level: &mut f32, r: Row, v: f32) {
    match r {
        Row::Contour => p.exciter_envelope_shape = v,
        Row::Bow => p.exciter_bow_level = v,
        Row::BowT => p.exciter_bow_timbre = v,
        Row::Blow => p.exciter_blow_level = v,
        Row::BlowM => p.exciter_blow_meta = v,
        Row::BlowT => p.exciter_blow_timbre = v,
        Row::Strike => p.exciter_strike_level = v,
        Row::StrikeM => p.exciter_strike_meta = v,
        Row::StrikeT => p.exciter_strike_timbre = v,
        Row::Geometry => p.resonator_geometry = v,
        Row::Brightness => p.resonator_brightness = v,
        Row::Damping => p.resonator_damping = v,
        Row::Position => p.resonator_position = v,
        Row::Space => p.space = v,
        Row::Level => *level = v,
        Row::Notes | Row::Amp => {}
    }
}

const SRC_SLOT_BASE: usize = 50;
const AMP_SLOT: usize = 40;
const NOTES_SLOT: usize = 41;

impl crate::undo::ParamUndo for ElementsState {
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
        if let (Some(r), V::F32(v)) = (ROWS.get(slot).copied(), value) {
            self.set(r, v);
        }
    }
}

fn snapshot_params(s: &ElementsState) -> state::ElementsParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    let p = &s.patch;
    state::ElementsParams {
        format: state::STATE_FORMAT,
        contour: Some(p.exciter_envelope_shape),
        bow: Some(p.exciter_bow_level),
        bow_timbre: Some(p.exciter_bow_timbre),
        blow: Some(p.exciter_blow_level),
        blow_meta: Some(p.exciter_blow_meta),
        blow_timbre: Some(p.exciter_blow_timbre),
        strike: Some(p.exciter_strike_level),
        strike_meta: Some(p.exciter_strike_meta),
        strike_timbre: Some(p.exciter_strike_timbre),
        geometry: Some(p.resonator_geometry),
        brightness: Some(p.resonator_brightness),
        damping: Some(p.resonator_damping),
        position: Some(p.resonator_position),
        space: Some(p.space),
        level: Some(s.level),
        freq: Some(s.freq),
        gate: Some(s.gate),
        contour_src: src(0),
        bow_src: src(1),
        bow_timbre_src: src(2),
        blow_src: src(3),
        blow_meta_src: src(4),
        blow_timbre_src: src(5),
        strike_src: src(6),
        strike_meta_src: src(7),
        strike_timbre_src: src(8),
        geometry_src: src(9),
        brightness_src: src(10),
        damping_src: src(11),
        position_src: src(12),
        space_src: src(13),
        level_src: src(14),
        amp_src: s.amp_src.as_ref().map(|a| a.to_string()),
        notes_src: s.notes_src.as_ref().map(|a| a.to_string()),
    }
}

fn apply_params(s: &mut ElementsState, p: &state::ElementsParams) {
    macro_rules! f {
        ($row:expr, $field:ident) => {
            if let Some(v) = p.$field {
                s.set($row, v);
            }
        };
    }
    f!(Row::Contour, contour);
    f!(Row::Bow, bow);
    f!(Row::BowT, bow_timbre);
    f!(Row::Blow, blow);
    f!(Row::BlowM, blow_meta);
    f!(Row::BlowT, blow_timbre);
    f!(Row::Strike, strike);
    f!(Row::StrikeM, strike_meta);
    f!(Row::StrikeT, strike_timbre);
    f!(Row::Geometry, geometry);
    f!(Row::Brightness, brightness);
    f!(Row::Damping, damping);
    f!(Row::Position, position);
    f!(Row::Space, space);
    f!(Row::Level, level);
    if let Some(v) = p.freq {
        s.freq = v;
    }
    if let Some(v) = p.gate {
        s.gate = v;
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.contour_src),
        parse(&p.bow_src),
        parse(&p.bow_timbre_src),
        parse(&p.blow_src),
        parse(&p.blow_meta_src),
        parse(&p.blow_timbre_src),
        parse(&p.strike_src),
        parse(&p.strike_meta_src),
        parse(&p.strike_timbre_src),
        parse(&p.geometry_src),
        parse(&p.brightness_src),
        parse(&p.damping_src),
        parse(&p.position_src),
        parse(&p.space_src),
        parse(&p.level_src),
    ];
    s.amp_src = parse(&p.amp_src);
    s.notes_src = parse(&p.notes_src);
    s.resolved = Default::default();
    s.amp_resolved = None;
}

// ── audio thread ───────────────────────────────────────────────────────────

fn audio_thread(shared: Arc<Mutex<ElementsState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_elements_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating elements ring")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    // one claim: the exciter-level follower (the hardware's env out)
    manifest.register("elements", instance, Some(&shm_name), 1)?;
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
    let mut out_l = vec![0.0_f32; slot_frames];
    let mut out_r = vec![0.0_f32; slot_frames];

    let mut part = Part::new(sample_rate, slot_frames, 0x5eed ^ instance as u32);
    let mut note_filter: Option<u8> = None;
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
                        s.freq = event.value;
                    }
                    s.velocity = event.param as f32 / 127.0;
                    s.gate = true;
                } else if event.is_note_off() {
                    s.gate = false;
                }
            }
        }

        let (patch, freq, gate, strength, level, amp) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            // max/min, not clamp: NaN from a stale channel dies here
            #[allow(clippy::manual_clamp)]
            let cv = |k: usize, manual: f32, s: &ElementsState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let mut patch = s.patch;
            let mut level_eff = s.level;
            for (k, r) in BINDABLE.iter().enumerate() {
                let manual = row_value(&patch, level_eff, *r);
                let v = cv(k, manual, &s);
                set_row_value(&mut patch, &mut level_eff, *r, v);
                s.eff[k] = v;
            }
            let amp = match (s.amp_src.is_some(), s.amp_resolved, bus) {
                (false, _, _) => 1.0,
                (true, Some(ch), Some(b)) => b.get(ch).clamp(0.0, 1.0),
                (true, _, _) => 0.0,
            };
            let strength = if s.gate && s.velocity < 0.001 {
                0.7
            } else {
                s.velocity
            };
            s.exc_now = part.voice.exciter_level().min(1.0);
            (patch, s.freq, s.gate, strength, level_eff, amp)
        };

        part.process(&patch, freq, strength, gate, &mut out_l, &mut out_r);

        let gain_target = amp * level;
        let g_alpha = 1.0 - (-1.0 / (0.0007 * sample_rate)).exp();
        for f in 0..slot_frames {
            gain_smooth += (gain_target - gain_smooth) * g_alpha;
            block[f * channels] = out_l[f] * gain_smooth;
            if channels > 1 {
                block[f * channels + 1] = out_r[f] * gain_smooth;
            }
        }
        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            part = Part::new(sample_rate, slot_frames, 0x5eed ^ instance as u32);
        }
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            bus.set(base, part.voice.exciter_level().clamp(0.0, 1.0));
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
        Row::Contour => "contour",
        Row::Bow => "bow",
        Row::BowT => "bow_t",
        Row::Blow => "blow",
        Row::BlowM => "blow_m",
        Row::BlowT => "blow_t",
        Row::Strike => "strike",
        Row::StrikeM => "strike_m",
        Row::StrikeT => "strike_t",
        Row::Geometry => "geometry",
        Row::Brightness => "bright",
        Row::Damping => "damping",
        Row::Position => "position",
        Row::Space => "space",
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

fn row_text(s: &ElementsState, r: Row) -> String {
    match r {
        Row::Contour => {
            let v = s.get(r);
            let region = if v < 0.4 {
                "perc"
            } else if v < 0.6 {
                "adsr"
            } else {
                "swell"
            };
            format!("{:.0}% · {}", v * 100.0, region)
        }
        Row::StrikeM => {
            let v = s.get(r);
            // zone boundaries after the voice.cc meta remap:
            // samples < 0.40, mallet 0.40–0.60, plectrum 0.60–0.80,
            // particles ≥ 0.80
            let zone = if v < 0.4 {
                "samples"
            } else if v < 0.6 {
                "mallet"
            } else if v < 0.8 {
                "plectrum"
            } else {
                "particles"
            };
            format!("{:.0}% · {}", v * 100.0, zone)
        }
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
    s: &ElementsState,
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
            "ELEMENTS",
            &format!("modal {}", instance),
            "",
            w,
        ));
        lines.push(Line::from(vec![
            Span::styled("  exc ".to_string(), theme::chrome()),
            Span::styled(
                theme::meter_char(s.exc_now).to_string(),
                theme::signal(theme::cv_ramp(s.exc_now)),
            ),
            Span::styled(
                format!("  {}", if s.gate { "●" } else { "○" }),
                if s.gate { theme::value() } else { theme::dim() },
            ),
        ]));
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
                vec![Span::styled(format!(" {:<9}", row_label(*r)), label_style)];
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
            if !matches!(r, Row::Notes | Row::Amp) {
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
                Line::from("━━━ ELEMENTS · modal voice (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  contour    Envelope: perc → adsr → swell"),
                Line::from("  bow        Friction + banded waveguides"),
                Line::from("  blow       Granular breath through the tube"),
                Line::from("  strike     samples → mallet → plectrum → particles"),
                Line::from("  geometry   Plate → string → bar (partial spacing)"),
                Line::from("  bright/damp/position  The modal resonator"),
                Line::from("  space      Dry → wide → the reverb cathedral"),
                Line::from("  @ / x      CV: geometry bright damping position"),
                Line::from("             space contour · notes/amp as usual"),
                Line::from(""),
                Line::from("Velocity is the hardware's strength input."),
                Line::from("Publishes elements/N/exc (exciter follower)."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" ELEMENTS ", theme::chrome_hi())),
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
    state::write_pid_file("elements", instance);

    let shared = Arc::new(Mutex::new(ElementsState::new()));
    if let Ok(p) = state::load_module_state::<state::ElementsParams>("elements", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let audio_builder = thread::Builder::new()
        .name(String::from("elements-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = audio_builder.spawn(move || {
        // A dead audio thread silences this strip with no visible trace
        // in a detached render — catch panics and write a black box.
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
        eprintln!("[elements {instance}] audio thread died: {msg}");
        let path = crate::state::tmp_dir().join(format!("elements_{instance}.crash"));
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
            let _ = state::save_module_state("elements", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::ElementsParams>("elements", instance)
            {
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
                    if matches!(r, Row::Notes | Row::Amp) {
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
                    ExCommand::Edit(name) => {
                        match state::load_patch::<state::ElementsParams>(&name) {
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
                        }
                    }
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
            let _ = state::save_module_state("elements", instance, &params);
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
                let v = step_f32(s.get(r), steps, 0.01, coarse, 0.0, 1.0);
                s.set(r, v);
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
                let def = ElementsState::new();
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
    s: &mut ElementsState,
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
    if matches!(r, Row::Notes | Row::Amp) {
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
    match value.parse::<f32>() {
        Ok(v) => {
            let old = s.get_param(slot);
            s.set_param(slot, V::F32(v));
            if let Some(old) = old {
                history.record(slot, "Set", old, V::F32(v));
            }
            format!("{} = {}", key, row_text(s, r))
        }
        // not a number: treat as a cable — bind ("module/N/out") or
        // unbind ("-") the row's CV-bank slot
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
        let mut s = ElementsState::new();
        s.set(Row::Geometry, 0.7);
        s.set(Row::Strike, 0.9);
        s.set(Row::Space, 0.8);
        s.notes_src = SourceAddr::parse("sequencer/0/t3");
        s.srcs[0] = SourceAddr::parse("lfo/0/s1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::ElementsParams = toml::from_str(&toml).expect("parses");
        let mut s2 = ElementsState::new();
        apply_params(&mut s2, &back);
        assert!((s2.get(Row::Geometry) - 0.7).abs() < 1e-6);
        assert!((s2.get(Row::Strike) - 0.9).abs() < 1e-6);
        assert!((s2.get(Row::Space) - 0.8).abs() < 1e-6);
        assert_eq!(
            s2.notes_src.as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t3".into())
        );
        assert_eq!(
            s2.srcs[0].as_ref().map(|a| a.to_string()),
            Some("lfo/0/s1".into())
        );
    }

    #[test]
    fn undo_slots_round_trip() {
        use crate::undo::{ParamUndo, ParamValue as V};
        let mut s = ElementsState::new();
        s.set_param(9, V::F32(0.66)); // geometry row
        assert!((s.patch.resonator_geometry - 0.66).abs() < 1e-6);
        s.set_param(SRC_SLOT_BASE + 4, V::Src(Some("lfo/0/a2".into())));
        assert_eq!(
            s.srcs[4].as_ref().map(|a| a.to_string()),
            Some("lfo/0/a2".into())
        );
        s.set_param(NOTES_SLOT, V::Src(Some("sequencer/0/t1".into())));
        assert_eq!(
            s.notes_src.as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t1".into())
        );
    }

    #[test]
    fn ex_set_parses() {
        let mut s = ElementsState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "geometry", "0.4").contains("40%"));
        assert!(ex_set(&mut s, &mut h, "strike_m", "0.9").contains("particles"));
        assert!(ex_set(&mut s, &mut h, "notes", "sequencer/0/t2").contains("t2"));
        assert!(ex_set(&mut s, &mut h, "wow", "1").contains("Unknown"));
    }

    #[test]
    fn every_value_row_is_bindable() {
        use crate::undo::ParamUndo;
        use crate::undo::ParamValue as V;
        // the whole point of a modular: no row without a jack
        for r in ROWS {
            if matches!(r, Row::Notes | Row::Amp) {
                continue;
            }
            let i = src_index(r).unwrap_or_else(|| panic!("{:?} must be bindable", r));
            let mut s = ElementsState::new();
            s.set_param(SRC_SLOT_BASE + i, V::Src(Some("lfo/0/a1".into())));
            assert_eq!(
                s.srcs[i].as_ref().map(|a| a.to_string()),
                Some("lfo/0/a1".into()),
                "{r:?} binds"
            );
        }
    }

    #[test]
    fn ex_set_binds_and_unbinds_value_rows() {
        let mut s = ElementsState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "strike_m", "lfo/0/s1").contains("lfo/0/s1"));
        let i = src_index(Row::StrikeM).unwrap();
        assert!(s.srcs[i].is_some());
        assert!(ex_set(&mut s, &mut h, "strike_m", "-").contains("unbound"));
        assert!(s.srcs[i].is_none());
        assert!(ex_set(&mut s, &mut h, "level", "lfo/0/a4").contains("lfo/0/a4"));
    }
}
