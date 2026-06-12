//! The Rings module shell: the panel as rows, notes as strums,
//! resonator and string-synth modes, an optional audio input claim
//! for external excitation.
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

use super::part::{Part, Patch, PerformanceState, ResonatorModel};
use super::string_synth::{FxType, StringSynthPart};
use crate::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

const FALLBACK_RATE: f32 = 48_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Mode,
    Model,
    Fx,
    Poly,
    Structure,
    Brightness,
    Damping,
    Position,
    Chord,
    Exciter,
    Level,
    Input,
    Notes,
    Amp,
}

const ROWS: [Row; 14] = [
    Row::Mode,
    Row::Model,
    Row::Fx,
    Row::Poly,
    Row::Structure,
    Row::Brightness,
    Row::Damping,
    Row::Position,
    Row::Chord,
    Row::Exciter,
    Row::Level,
    Row::Input,
    Row::Notes,
    Row::Amp,
];

/// CV bank, srcs[] order — every value row takes a cable (mode,
/// model, fx, polyphony and the exciter switch are discrete
/// selectors, like the swarm's chord name).
const BINDABLE: [Row; 6] = [
    Row::Structure,
    Row::Brightness,
    Row::Damping,
    Row::Position,
    Row::Chord,
    Row::Level,
];
const N_SRC: usize = BINDABLE.len();

struct RingsState {
    synth_mode: bool,
    model: usize,
    fx: usize,
    poly: usize,
    patch: Patch,
    chord: f32,
    internal_exciter: bool,
    level: f32,
    input: Option<String>,
    input_live: bool,
    note: f32,
    strum_pending: bool,
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    amp_src: Option<SourceAddr>,
    amp_resolved: Option<usize>,
    notes_src: Option<SourceAddr>,
    selected: usize,
}

impl RingsState {
    fn new() -> Self {
        let mut s = RingsState {
            synth_mode: false,
            model: 0,
            fx: FxType::Ensemble as usize,
            poly: 1,
            patch: Patch::default(),
            chord: 0.0,
            internal_exciter: true,
            level: 0.8,
            input: None,
            input_live: true,
            note: 69.0,
            strum_pending: false,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.0; N_SRC],
            amp_src: None,
            amp_resolved: None,
            notes_src: None,
            selected: 0,
        };
        for (k, r) in BINDABLE.iter().enumerate() {
            s.eff[k] = s.get(*r);
        }
        s
    }

    fn get(&self, r: Row) -> f32 {
        match r {
            Row::Structure => self.patch.structure,
            Row::Brightness => self.patch.brightness,
            Row::Damping => self.patch.damping,
            Row::Position => self.patch.position,
            Row::Chord => self.chord,
            Row::Level => self.level,
            Row::Mode
            | Row::Model
            | Row::Fx
            | Row::Poly
            | Row::Exciter
            | Row::Input
            | Row::Notes
            | Row::Amp => 0.0,
        }
    }

    fn set(&mut self, r: Row, v: f32) {
        let v = v.clamp(0.0, 1.0);
        match r {
            Row::Structure => self.patch.structure = v,
            Row::Brightness => self.patch.brightness = v,
            Row::Damping => self.patch.damping = v,
            Row::Position => self.patch.position = v,
            Row::Chord => self.chord = v,
            Row::Level => self.level = v,
            Row::Mode
            | Row::Model
            | Row::Fx
            | Row::Poly
            | Row::Exciter
            | Row::Input
            | Row::Notes
            | Row::Amp => {}
        }
    }
}

/// Manual fallback for a bindable row from the working copies.
fn row_value(patch: &Patch, chord: f32, level: f32, r: Row) -> f32 {
    match r {
        Row::Structure => patch.structure,
        Row::Brightness => patch.brightness,
        Row::Damping => patch.damping,
        Row::Position => patch.position,
        Row::Chord => chord,
        Row::Level => level,
        _ => 0.0,
    }
}

/// Write a bank-effective value into the working copies.
fn set_row_value(patch: &mut Patch, chord: &mut f32, level: &mut f32, r: Row, v: f32) {
    match r {
        Row::Structure => patch.structure = v,
        Row::Brightness => patch.brightness = v,
        Row::Damping => patch.damping = v,
        Row::Position => patch.position = v,
        Row::Chord => *chord = v,
        Row::Level => *level = v,
        _ => {}
    }
}

const SRC_SLOT_BASE: usize = 50;
const AMP_SLOT: usize = 40;
const NOTES_SLOT: usize = 41;
const INPUT_SLOT: usize = 42;

impl crate::undo::ParamUndo for RingsState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        match slot {
            AMP_SLOT => return Some(V::Src(self.amp_src.as_ref().map(|a| a.to_string()))),
            NOTES_SLOT => return Some(V::Src(self.notes_src.as_ref().map(|a| a.to_string()))),
            INPUT_SLOT => return Some(V::Src(self.input.clone())),
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
            Row::Mode => V::Bool(self.synth_mode),
            Row::Model => V::Usize(self.model),
            Row::Fx => V::Usize(self.fx),
            Row::Poly => V::Usize(self.poly),
            Row::Exciter => V::Bool(self.internal_exciter),
            Row::Input | Row::Notes | Row::Amp => return None,
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
            (INPUT_SLOT, V::Src(v)) => {
                self.input = v.clone();
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
            (Some(Row::Mode), V::Bool(v)) => self.synth_mode = v,
            (Some(Row::Model), V::Usize(v)) => self.model = v.min(5),
            (Some(Row::Fx), V::Usize(v)) => self.fx = v.min(5),
            (Some(Row::Poly), V::Usize(v)) => self.poly = v.clamp(1, 4),
            (Some(Row::Exciter), V::Bool(v)) => self.internal_exciter = v,
            (Some(r), V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &RingsState) -> state::RingsParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::RingsParams {
        format: state::STATE_FORMAT,
        mode: Some(if s.synth_mode { "synth" } else { "resonator" }.into()),
        model: Some(ResonatorModel::ALL[s.model.min(5)].label().into()),
        fx: Some(FxType::ALL[s.fx.min(5)].label().into()),
        poly: Some(s.poly),
        structure: Some(s.patch.structure),
        brightness: Some(s.patch.brightness),
        damping: Some(s.patch.damping),
        position: Some(s.patch.position),
        chord: Some(s.chord),
        exciter: Some(s.internal_exciter),
        level: Some(s.level),
        input: s.input.clone(),
        structure_src: src(0),
        brightness_src: src(1),
        damping_src: src(2),
        position_src: src(3),
        chord_src: src(4),
        level_src: src(5),
        amp_src: s.amp_src.as_ref().map(|a| a.to_string()),
        notes_src: s.notes_src.as_ref().map(|a| a.to_string()),
    }
}

fn apply_params(s: &mut RingsState, p: &state::RingsParams) {
    if let Some(m) = p.mode.as_deref() {
        s.synth_mode = m == "synth";
    }
    if let Some(m) = p.model.as_deref() {
        if let Some(i) = ResonatorModel::ALL.iter().position(|x| x.label() == m) {
            s.model = i;
        }
    }
    if let Some(m) = p.fx.as_deref() {
        if let Some(i) = FxType::ALL.iter().position(|x| x.label() == m) {
            s.fx = i;
        }
    }
    if let Some(v) = p.poly {
        s.poly = v.clamp(1, 4);
    }
    macro_rules! f {
        ($row:expr, $field:ident) => {
            if let Some(v) = p.$field {
                s.set($row, v);
            }
        };
    }
    f!(Row::Structure, structure);
    f!(Row::Brightness, brightness);
    f!(Row::Damping, damping);
    f!(Row::Position, position);
    f!(Row::Chord, chord);
    f!(Row::Level, level);
    if let Some(v) = p.exciter {
        s.internal_exciter = v;
    }
    if p.input.is_some() {
        s.input = p.input.clone();
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.structure_src),
        parse(&p.brightness_src),
        parse(&p.damping_src),
        parse(&p.position_src),
        parse(&p.chord_src),
        parse(&p.level_src),
    ];
    s.amp_src = parse(&p.amp_src);
    s.notes_src = parse(&p.notes_src);
    s.resolved = Default::default();
    s.amp_resolved = None;
}

// ── audio thread ───────────────────────────────────────────────────────────

fn audio_thread(shared: Arc<Mutex<RingsState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_rings_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating rings ring")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("rings", instance, Some(&shm_name), 0)?;
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
    let mut in_block = vec![0.0_f32; ringbuf.slot_len()];
    let mut mono_in = vec![0.0_f32; slot_frames];
    let mut out = vec![0.0_f32; slot_frames];
    let mut aux = vec![0.0_f32; slot_frames];

    let mut part = Part::new(sample_rate, slot_frames, 0x4319 ^ instance as u32);
    let mut synth = StringSynthPart::new(sample_rate, slot_frames);
    let mut note_filter: Option<u8> = None;
    let mut gain_smooth = 0.0_f32;
    let mut blocks: u64 = 0;
    let mut input: Option<AudioRingbuf> = None;
    let mut input_shm: Option<String> = None;
    let mut scratch = vec![0.0_f32; ringbuf.slot_len()];

    loop {
        if blocks.is_multiple_of(128) {
            if events.is_none() {
                events = EventRingbuf::open_dynamic().ok();
            }
            let entries = manifest.entries();
            let desired: Option<String> = {
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
                let desired = s.input.as_deref().and_then(|sel| {
                    let (m, i) = sel.split_once('/')?;
                    let i: usize = i.parse().ok()?;
                    entries
                        .iter()
                        .find(|e| e.module_name == m && e.instance == i)
                        .and_then(|e| e.audio_shm.clone())
                });
                s.input_live = s.input.is_none() || desired.is_some();
                desired
            };
            if desired != input_shm {
                input = desired.as_deref().and_then(|n| AudioRingbuf::open(n).ok());
                if let Some(rb) = input.as_mut() {
                    while rb.available() > 1 {
                        let _ = rb.read(&mut scratch);
                    }
                }
                manifest.publish_input(desired.as_deref());
                input_shm = desired;
            }
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
                        // events carry Hz; rings thinks in MIDI notes
                        s.note = 69.0 + 12.0 * (event.value / 440.0).log2();
                    }
                    s.strum_pending = true;
                }
                // note_off is silence-by-decay in rings — no gate
            }
        }

        // external input (claimed like an fx module) or silence
        let mut got = false;
        if let Some(rb) = input.as_mut() {
            let tick = Instant::now();
            loop {
                if rb.read(&mut in_block).unwrap_or(false) {
                    got = true;
                    break;
                }
                if tick.elapsed() > Duration::from_millis(4) {
                    break;
                }
                thread::sleep(Duration::from_micros(200));
            }
        }
        if got {
            for f in 0..slot_frames {
                mono_in[f] = 0.5 * (in_block[f * channels] + in_block[f * channels + 1]);
            }
        } else {
            mono_in.fill(0.0);
        }

        let (ps, patch, synth_mode, model, fx, poly, level, amp) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            // max/min, not clamp: NaN from a stale channel dies here
            #[allow(clippy::manual_clamp)]
            let cv = |k: usize, manual: f32, s: &RingsState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let mut patch = s.patch;
            let mut chord_eff = s.chord;
            let mut level_eff = s.level;
            for (k, r) in BINDABLE.iter().enumerate() {
                let manual = row_value(&patch, chord_eff, level_eff, *r);
                let v = cv(k, manual, &s);
                set_row_value(&mut patch, &mut chord_eff, &mut level_eff, *r, v);
                s.eff[k] = v;
            }
            let amp = match (s.amp_src.is_some(), s.amp_resolved, bus) {
                (false, _, _) => 1.0,
                (true, Some(ch), Some(b)) => b.get(ch).clamp(0.0, 1.0),
                (true, _, _) => 0.0,
            };
            let ps = PerformanceState {
                strum: s.strum_pending,
                internal_exciter: s.internal_exciter,
                tonic: 0.0,
                note: s.note,
                fm: 0.0,
                chord: (chord_eff * 10.999) as usize,
            };
            s.strum_pending = false;
            (
                ps,
                patch,
                s.synth_mode,
                s.model,
                s.fx,
                s.poly,
                level_eff,
                amp,
            )
        };

        if synth_mode {
            synth.set_polyphony(poly);
            synth.set_fx(FxType::ALL[fx.min(5)]);
            synth.process(&ps, &patch, &mono_in, &mut out, &mut aux);
        } else {
            part.set_polyphony(poly);
            part.set_model(ResonatorModel::ALL[model.min(5)]);
            part.process(&ps, &patch, &mono_in, &mut out, &mut aux);
        }

        let gain_target = amp * level;
        let g_alpha = 1.0 - (-1.0 / (0.0007 * sample_rate)).exp();
        for f in 0..slot_frames {
            gain_smooth += (gain_target - gain_smooth) * g_alpha;
            block[f * channels] = out[f] * gain_smooth;
            if channels > 1 {
                block[f * channels + 1] = aux[f] * gain_smooth;
            }
        }
        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            part = Part::new(sample_rate, slot_frames, 0x4319 ^ instance as u32);
            synth = StringSynthPart::new(sample_rate, slot_frames);
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
        Row::Model => "model",
        Row::Fx => "fx",
        Row::Poly => "poly",
        Row::Structure => "structure",
        Row::Brightness => "bright",
        Row::Damping => "damping",
        Row::Position => "position",
        Row::Chord => "chord",
        Row::Exciter => "exciter",
        Row::Level => "level",
        Row::Input => "input",
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
        Row::Input => Some(INPUT_SLOT),
        _ => src_index(r).map(|i| SRC_SLOT_BASE + i),
    }
}

const CHORD_NAMES: [&str; 11] = [
    "oct", "5", "sus4", "m", "m7", "m9", "m11", "69", "M9", "M7", "M",
];

fn row_text(s: &RingsState, r: Row) -> String {
    match r {
        Row::Mode => if s.synth_mode { "synth (Disastrous Peace)" } else { "resonator" }.into(),
        Row::Model => ResonatorModel::ALL[s.model.min(5)].label().into(),
        Row::Fx => FxType::ALL[s.fx.min(5)].label().into(),
        Row::Poly => format!("{}", s.poly),
        Row::Chord => {
            let i = (s.get(r) * 10.999) as usize;
            format!("{:.0}% · {}", s.get(r) * 100.0, CHORD_NAMES[i.min(10)])
        }
        Row::Exciter => if s.internal_exciter { "internal" } else { "external only" }.into(),
        Row::Input => match (&s.input, s.input_live) {
            (Some(sel), true) => sel.clone(),
            (Some(sel), false) => format!("{sel} (dead)"),
            (None, _) => "(internal exciter)".into(),
        },
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
    s: &RingsState,
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
            "RINGS",
            &format!("resonator {}", instance),
            "",
            w,
        ));
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
                vec![Span::styled(format!(" {:<10}", row_label(*r)), label_style)];
            let bound = src_index(*r).is_some_and(|k| s.srcs[k].is_some())
                || (*r == Row::Amp && s.amp_src.is_some())
                || (*r == Row::Notes && s.notes_src.is_some())
                || (*r == Row::Input && s.input.is_some());
            let hue = src_index(*r)
                .and_then(|k| s.srcs[k].as_ref())
                .or(match r {
                    Row::Amp => s.amp_src.as_ref(),
                    Row::Notes => s.notes_src.as_ref(),
                    _ => None,
                })
                .map(|a| routing::cable_color(entries, a));
            if matches!(
                r,
                Row::Structure
                    | Row::Brightness
                    | Row::Damping
                    | Row::Position
                    | Row::Chord
                    | Row::Level
            ) {
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
                Line::from("━━━ RINGS · resonator (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  mode       resonator | synth (Disastrous Peace)"),
                Line::from("  model      modal · sympathetic · string · fm"),
                Line::from("             · chords · string+verb"),
                Line::from("  fx         synth mode: formant/chorus/reverb/…"),
                Line::from("  poly       1–4 voices (odd counts ping-pong)"),
                Line::from("  structure  inharmonicity / string tuning / fm ratio"),
                Line::from("  bright/damp/position   the resonator macros"),
                Line::from("  chord      the 11-chord table (quantized modes)"),
                Line::from("  exciter    internal pluck/pulse, or external only"),
                Line::from("  input      claim an audio source as the exciter"),
                Line::from(""),
                Line::from("Each note-on strums; pitch rides the note filter."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" RINGS ", theme::chrome_hi())),
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

/// Cycle a discrete selector row left/right.
fn adjust_selector(s: &mut RingsState, r: Row, steps: i32) {
    let bump = |v: usize, n: usize| -> usize {
        (v as i32 + steps).rem_euclid(n as i32) as usize
    };
    match r {
        Row::Mode => s.synth_mode = !s.synth_mode,
        Row::Model => s.model = bump(s.model, 6),
        Row::Fx => s.fx = bump(s.fx, 6),
        Row::Poly => s.poly = ((s.poly as i32 + steps).clamp(1, 4)) as usize,
        Row::Exciter => s.internal_exciter = !s.internal_exciter,
        _ => {}
    }
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("rings", instance);

    let shared = Arc::new(Mutex::new(RingsState::new()));
    if let Ok(p) = state::load_module_state::<state::RingsParams>("rings", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let audio_builder = thread::Builder::new()
        .name(String::from("rings-audio"))
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
        eprintln!("[rings {instance}] audio thread died: {msg}");
        let path = crate::state::tmp_dir().join(format!("rings_{instance}.crash"));
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
    let mut picking_input = false;
    let mut input_options: Vec<String> = Vec::new();
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
            let _ = state::save_module_state("rings", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::RingsParams>("rings", instance) {
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
                    let row = (m.row as usize).saturating_sub(2);
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
            match picker.handle_key(key.code) {
                crate::picker::PickerEvent::Chosen(addr) if !picking_input => {
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
                crate::picker::PickerEvent::Chosen(_) => {
                    // "— none —" on the input picker unpatches
                    use crate::undo::{ParamUndo, ParamValue};
                    picking_input = false;
                    let mut s = shared.lock().unwrap();
                    let old = s.get_param(INPUT_SLOT);
                    s.input = None;
                    if let Some(old) = old {
                        history.record(INPUT_SLOT, "Unpatch", old, ParamValue::Src(None));
                    }
                }
                crate::picker::PickerEvent::ChosenSpecial(i) if picking_input => {
                    use crate::undo::{ParamUndo, ParamValue};
                    picking_input = false;
                    if let Some(sel) = input_options.get(i.saturating_sub(1)).cloned() {
                        let mut s = shared.lock().unwrap();
                        let old = s.get_param(INPUT_SLOT);
                        s.input = Some(sel.clone());
                        s.input_live = true;
                        if let Some(old) = old {
                            history.record(INPUT_SLOT, "Patch", old, ParamValue::Src(Some(sel)));
                        }
                    }
                }
                _ => {}
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
                    ExCommand::Edit(name) => match state::load_patch::<state::RingsParams>(&name) {
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
            let _ = state::save_module_state("rings", instance, &params);
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
                if matches!(r, Row::Input | Row::Notes | Row::Amp) {
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
                if matches!(r, Row::Input | Row::Notes | Row::Amp) {
                    continue;
                }
                let slot = s.selected;
                let old = s.get_param(slot);
                let def = RingsState::new();
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
                if r == Row::Input {
                    let current = s.input.clone();
                    drop(s);
                    let entries = Manifest::open().map(|m| m.entries()).unwrap_or_default();
                    input_options = entries
                        .iter()
                        .filter(|e| e.audio_shm.is_some())
                        .filter(|e| !(e.module_name == "rings" && e.instance == instance))
                        .map(|e| format!("{}/{}", e.module_name, e.instance))
                        .collect();
                    input_options.sort();
                    let mut specials = vec![String::from("— none —")];
                    specials.extend(input_options.iter().map(|o| o.replace('/', " ")));
                    let cur_special = current
                        .as_ref()
                        .and_then(|c| input_options.iter().position(|o| o == c))
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    picking_input = true;
                    picker.open_with(specials, Vec::new(), None, cur_special);
                } else if binding_slot(r).is_some() {
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
                        Row::Input => s.input.is_some(),
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
                            Row::Input => s.input = None,
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
    s: &mut RingsState,
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
    // selectors take names; routing rows take addresses
    match r {
        Row::Mode => {
            s.synth_mode = matches!(value, "synth" | "green");
            return format!("mode = {}", row_text(s, r));
        }
        Row::Model => {
            if let Some(i) = ResonatorModel::ALL.iter().position(|m| m.label() == value) {
                s.model = i;
                return format!("model = {}", value);
            }
            return format!(
                "model: one of {}",
                ResonatorModel::ALL.map(|m| m.label()).join(" ")
            );
        }
        Row::Fx => {
            if let Some(i) = FxType::ALL.iter().position(|m| m.label() == value) {
                s.fx = i;
                return format!("fx = {}", value);
            }
            return format!("fx: one of {}", FxType::ALL.map(|m| m.label()).join(" "));
        }
        Row::Poly => {
            return match value.parse::<usize>() {
                Ok(v) if (1..=4).contains(&v) => {
                    s.poly = v;
                    format!("poly = {v}")
                }
                _ => "poly: 1–4".into(),
            };
        }
        Row::Exciter => {
            s.internal_exciter = matches!(value, "internal" | "on" | "1" | "true");
            return format!("exciter = {}", row_text(s, r));
        }
        Row::Input | Row::Notes | Row::Amp => {
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
        // not a number: a cable — bind or unbind ("-")
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
        let mut s = RingsState::new();
        s.set(Row::Structure, 0.7);
        s.set(Row::Chord, 0.4);
        s.model = 2;
        s.poly = 4;
        s.synth_mode = false;
        s.notes_src = SourceAddr::parse("sequencer/0/t3");
        s.srcs[0] = SourceAddr::parse("lfo/0/s1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::RingsParams = toml::from_str(&toml).expect("parses");
        let mut s2 = RingsState::new();
        apply_params(&mut s2, &back);
        assert!((s2.get(Row::Structure) - 0.7).abs() < 1e-6);
        assert!((s2.get(Row::Chord) - 0.4).abs() < 1e-6);
        assert_eq!(s2.model, 2);
        assert_eq!(s2.poly, 4);
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
    fn every_value_row_is_bindable() {
        use crate::undo::ParamUndo;
        use crate::undo::ParamValue as V;
        // the no-row-without-a-jack contract: every continuous row
        // binds (selectors and routing rows are the documented
        // exceptions)
        for r in ROWS {
            if matches!(
                r,
                Row::Mode
                    | Row::Model
                    | Row::Fx
                    | Row::Poly
                    | Row::Exciter
                    | Row::Input
                    | Row::Notes
                    | Row::Amp
            ) {
                continue;
            }
            let i = src_index(r).unwrap_or_else(|| panic!("{r:?} must be bindable"));
            let mut s = RingsState::new();
            s.set_param(SRC_SLOT_BASE + i, V::Src(Some("lfo/0/a1".into())));
            assert_eq!(
                s.srcs[i].as_ref().map(|a| a.to_string()),
                Some("lfo/0/a1".into()),
                "{r:?} binds"
            );
        }
    }

    #[test]
    fn ex_set_handles_selectors_values_and_cables() {
        let mut s = RingsState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "model", "fm").contains("fm"));
        assert_eq!(s.model, 3);
        assert!(ex_set(&mut s, &mut h, "poly", "4").contains("4"));
        assert!(ex_set(&mut s, &mut h, "structure", "0.4").contains("40%"));
        assert!(ex_set(&mut s, &mut h, "structure", "lfo/0/s1").contains("lfo/0/s1"));
        assert!(ex_set(&mut s, &mut h, "structure", "-").contains("unbound"));
        assert!(ex_set(&mut s, &mut h, "mode", "synth").contains("synth"));
        assert!(ex_set(&mut s, &mut h, "wow", "1").contains("Unknown"));
    }

    #[test]
    fn chord_row_maps_to_the_eleven_chords() {
        let mut s = RingsState::new();
        s.set(Row::Chord, 0.0);
        assert!(row_text(&s, Row::Chord).contains("oct"));
        s.set(Row::Chord, 1.0);
        assert!(row_text(&s, Row::Chord).contains('M'));
    }
}
