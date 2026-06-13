//! # Plaits — the macro-oscillator voice shell
//!
//! A monophonic voice: a note source sets the pitch and gates the
//! engine (a trigger on each note-on), an amp source shapes the level.
//! `engine` selects a synthesis model; `harmonics`, `timbre` and
//! `morph` are its three macro parameters. The selected engine renders
//! into a main output (the audio ring) and an aux output (published as
//! plaits/N/aux). Engines run at 48 kHz.
//!
//! v1 ships the filtered-noise and 2-operator FM engines; the rest
//! (virtual-analog, waveshaping, chord, wavetable, physical, drums,
//! speech…) land incrementally.

// max/min, not clamp, where modbus values land: clamp(NaN) is NaN and a
// stale channel must die at the boundary.
#![allow(clippy::manual_clamp)]

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
    AdditiveEngine, BassDrumEngine, ChordEngine, Engine, EngineParameters, FmEngine, GrainEngine,
    ModalEngine, NoiseEngine, StringEngine, SwarmEngine, VirtualAnalogEngine, WaveshapingEngine,
    WavetableEngine, TRIGGER_HIGH, TRIGGER_RISING_EDGE,
};
use crate::ipc::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

const FALLBACK_RATE: f32 = 48_000.0;
const BLOCK: usize = 24;

pub const ENGINE_NAMES: [&str; 12] = [
    "noise", "fm", "virtual_analog", "chord", "waveshaping", "additive", "swarm", "grain",
    "wavetable", "modal", "string", "bass_drum",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Engine,
    Harmonics,
    Timbre,
    Morph,
    Level,
    Amp,
    Notes,
}

const ROWS: [Row; 7] = [
    Row::Engine,
    Row::Harmonics,
    Row::Timbre,
    Row::Morph,
    Row::Level,
    Row::Amp,
    Row::Notes,
];

const BINDABLE: [Row; 4] = [Row::Harmonics, Row::Timbre, Row::Morph, Row::Level];
const N_SRC: usize = BINDABLE.len();
const SRC_SLOT_BASE: usize = 20;
const AMP_SLOT: usize = 5;
const NOTES_SLOT: usize = 6;

fn src_index(r: Row) -> Option<usize> {
    BINDABLE.iter().position(|b| *b == r)
}

struct PlaitsState {
    engine: usize,
    harmonics: f32,
    timbre: f32,
    morph: f32,
    level: f32,
    freq: f32,
    gate: bool,
    rising: bool,
    velocity: f32,
    amp_src: Option<SourceAddr>,
    amp_resolved: Option<usize>,
    notes_src: Option<SourceAddr>,
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    out_now: f32,
    selected: usize,
}

impl PlaitsState {
    fn new() -> Self {
        Self {
            engine: 0,
            harmonics: 0.5,
            timbre: 0.5,
            morph: 0.5,
            level: 0.8,
            freq: 220.0,
            gate: false,
            rising: false,
            velocity: 0.0,
            amp_src: None,
            amp_resolved: None,
            notes_src: None,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.5, 0.5, 0.5, 0.8],
            out_now: 0.0,
            selected: 0,
        }
    }

    fn get(&self, r: Row) -> f32 {
        match r {
            Row::Harmonics => self.harmonics,
            Row::Timbre => self.timbre,
            Row::Morph => self.morph,
            Row::Level => self.level,
            _ => 0.0,
        }
    }
    fn set(&mut self, r: Row, v: f32) {
        let v = v.max(0.0).min(1.0);
        match r {
            Row::Harmonics => self.harmonics = v,
            Row::Timbre => self.timbre = v,
            Row::Morph => self.morph = v,
            Row::Level => self.level = v,
            _ => {}
        }
    }
}

impl crate::undo::ParamUndo for PlaitsState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            let s = self.srcs.get(i)?;
            return Some(V::Src(s.as_ref().map(|a| a.to_string())));
        }
        let r = *ROWS.get(slot)?;
        Some(match r {
            Row::Engine => V::Usize(self.engine),
            Row::Amp => V::Src(self.amp_src.as_ref().map(|a| a.to_string())),
            Row::Notes => V::Src(self.notes_src.as_ref().map(|a| a.to_string())),
            _ => V::F32(self.get(r)),
        })
    }
    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            if let (Some(s), V::Src(v)) = (self.srcs.get_mut(i), value) {
                *s = v.as_deref().and_then(SourceAddr::parse);
                self.resolved[i] = None;
            }
            return;
        }
        let Some(r) = ROWS.get(slot).copied() else {
            return;
        };
        match (r, value) {
            (Row::Engine, V::Usize(v)) => self.engine = v.min(ENGINE_NAMES.len() - 1),
            (Row::Amp, V::Src(v)) => {
                self.amp_src = v.as_deref().and_then(SourceAddr::parse);
                self.amp_resolved = None;
            }
            (Row::Notes, V::Src(v)) => self.notes_src = v.as_deref().and_then(SourceAddr::parse),
            (_, V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &PlaitsState) -> state::PlaitsParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::PlaitsParams {
        format: state::STATE_FORMAT,
        engine: Some(ENGINE_NAMES[s.engine].to_string()),
        harmonics: Some(s.harmonics),
        timbre: Some(s.timbre),
        morph: Some(s.morph),
        level: Some(s.level),
        harmonics_src: src(0),
        timbre_src: src(1),
        morph_src: src(2),
        level_src: src(3),
        amp_src: s.amp_src.as_ref().map(|a| a.to_string()),
        notes_src: s.notes_src.as_ref().map(|a| a.to_string()),
    }
}

fn apply_params(s: &mut PlaitsState, p: &state::PlaitsParams) {
    if let Some(e) = p.engine.as_deref() {
        if let Some(i) = ENGINE_NAMES.iter().position(|n| *n == e) {
            s.engine = i;
        }
    }
    if let Some(v) = p.harmonics {
        s.harmonics = v.max(0.0).min(1.0);
    }
    if let Some(v) = p.timbre {
        s.timbre = v.max(0.0).min(1.0);
    }
    if let Some(v) = p.morph {
        s.morph = v.max(0.0).min(1.0);
    }
    if let Some(v) = p.level {
        s.level = v.max(0.0).min(1.0);
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.harmonics_src),
        parse(&p.timbre_src),
        parse(&p.morph_src),
        parse(&p.level_src),
    ];
    s.amp_src = parse(&p.amp_src);
    s.notes_src = parse(&p.notes_src);
    s.resolved = Default::default();
    s.amp_resolved = None;
}

/// Hz → a plaits MIDI note.
fn freq_to_note(freq: f32) -> f32 {
    69.0 + 12.0 * (freq.max(1.0) / 440.0).log2()
}

// ── audio thread ─────────────────────────────────────────────────────────────

fn audio_thread(shared: Arc<Mutex<PlaitsState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_plaits_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating plaits ring")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("plaits", instance, Some(&shm_name), 1)?;
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

    let mut noise = NoiseEngine::new(0x9e1 ^ instance as u32);
    let mut fm = FmEngine::new();
    let mut va = VirtualAnalogEngine::new();
    let mut chord = ChordEngine::new();
    let mut waveshaping = WaveshapingEngine::new();
    let mut additive = AdditiveEngine::new();
    let mut swarm = SwarmEngine::new();
    let mut grain = GrainEngine::new();
    let mut wavetable = WavetableEngine::new();
    let mut modal = ModalEngine::new();
    let mut string = StringEngine::new();
    let mut bass_drum = BassDrumEngine::new();
    let mut out_buf = vec![0.0_f32; slot_frames + BLOCK];
    let mut aux_buf = vec![0.0_f32; slot_frames + BLOCK];

    let mut note_filter: Option<u8> = None;
    let mut gain_smooth = 0.0_f32;
    let mut follower = 0.0_f32;
    let mut aux_follow = 0.0_f32;
    let mut blocks: u64 = 0;
    // engines render at 48k; resample if the session differs
    let ratio = super::dsp::SAMPLE_RATE / sample_rate;
    let mut resample_pos = 0.0_f32;
    let mut prev_out = 0.0_f32;
    let mut prev_aux = 0.0_f32;

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
                    s.rising = true;
                } else if event.is_note_off() {
                    s.gate = false;
                }
            }
        }

        let (engine, params, level, amp) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            let cv = |k: usize, manual: f32, s: &PlaitsState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let harmonics = cv(0, s.harmonics, &s);
            let timbre = cv(1, s.timbre, &s);
            let morph = cv(2, s.morph, &s);
            let level = cv(3, s.level, &s);
            s.eff = [harmonics, timbre, morph, level];
            let amp = match (s.amp_src.is_some(), s.amp_resolved, bus) {
                (false, _, _) => 1.0,
                (true, Some(ch), Some(b)) => b.get(ch).clamp(0.0, 1.0),
                (true, _, _) => 0.0,
            };
            let trigger = if s.rising {
                TRIGGER_RISING_EDGE | TRIGGER_HIGH
            } else if s.gate {
                TRIGGER_HIGH
            } else {
                0
            };
            s.rising = false;
            let p = EngineParameters {
                trigger,
                note: freq_to_note(s.freq),
                harmonics,
                timbre,
                morph,
                accent: s.velocity.max(0.4),
            };
            (s.engine, p, level, amp)
        };

        // render at 48k into out/aux, then resample to the session rate
        let need = (slot_frames as f32 * ratio).ceil() as usize + 1;
        if out_buf.len() < need {
            out_buf.resize(need, 0.0);
            aux_buf.resize(need, 0.0);
        }
        let mut rendered = 0;
        while rendered < need {
            let n = (need - rendered).min(BLOCK);
            let eng: &mut dyn Engine = match engine {
                1 => &mut fm,
                2 => &mut va,
                3 => &mut chord,
                4 => &mut waveshaping,
                5 => &mut additive,
                6 => &mut swarm,
                7 => &mut grain,
                8 => &mut wavetable,
                9 => &mut modal,
                10 => &mut string,
                11 => &mut bass_drum,
                _ => &mut noise,
            };
            eng.render(
                &params,
                &mut out_buf[rendered..rendered + n],
                &mut aux_buf[rendered..rendered + n],
            );
            rendered += n;
        }

        let g_alpha = 1.0 - (-1.0 / (0.0007 * sample_rate)).exp();
        let strength = if amp < 0.999 { amp } else { 0.8 };
        let gain_target = strength * level;
        for f in 0..slot_frames {
            let idx = resample_pos.floor() as usize;
            let frac = resample_pos - idx as f32;
            let a = if idx == 0 { prev_out } else { out_buf[(idx - 1).min(need - 1)] };
            let b = out_buf[idx.min(need - 1)];
            let av = if idx == 0 { prev_aux } else { aux_buf[(idx - 1).min(need - 1)] };
            let bv = aux_buf[idx.min(need - 1)];
            let o = a + (b - a) * frac;
            let ax = av + (bv - av) * frac;
            gain_smooth += (gain_target - gain_smooth) * g_alpha;
            let v = o * gain_smooth;
            follower = follower.max(v.abs()) * 0.9995;
            aux_follow = aux_follow.max((ax * gain_smooth).abs()) * 0.9995;
            block[f * channels] = v;
            if channels > 1 {
                block[f * channels + 1] = v;
            }
            resample_pos += ratio;
        }
        let consumed = resample_pos.floor() as usize;
        prev_out = out_buf[consumed.saturating_sub(1).min(need - 1)];
        prev_aux = aux_buf[consumed.saturating_sub(1).min(need - 1)];
        resample_pos -= consumed as f32;

        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            noise = NoiseEngine::new(0x9e1 ^ instance as u32);
            fm = FmEngine::new();
            va = VirtualAnalogEngine::new();
            chord = ChordEngine::new();
            waveshaping = WaveshapingEngine::new();
            additive = AdditiveEngine::new();
            swarm = SwarmEngine::new();
            grain = GrainEngine::new();
            wavetable = WavetableEngine::new();
            modal = ModalEngine::new();
            string = StringEngine::new();
            bass_drum = BassDrumEngine::new();
            resample_pos = 0.0;
        }
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            bus.set(base, aux_follow.min(1.0));
        }
        shared.lock().unwrap().out_now = follower.min(1.0);
        while ringbuf.write(&block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }
        blocks += 1;
    }
}

// ── ui ───────────────────────────────────────────────────────────────────────

fn row_label(r: Row) -> &'static str {
    match r {
        Row::Engine => "engine",
        Row::Harmonics => "harmonics",
        Row::Timbre => "timbre",
        Row::Morph => "morph",
        Row::Level => "level",
        Row::Amp => "amp",
        Row::Notes => "notes",
    }
}

fn row_text(s: &PlaitsState, r: Row) -> String {
    match r {
        Row::Engine => ENGINE_NAMES[s.engine].to_string(),
        Row::Harmonics | Row::Timbre | Row::Morph | Row::Level => {
            format!("{:.0}%", s.get(r) * 100.0)
        }
        Row::Amp => s
            .amp_src
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(gate · velocity)".into()),
        Row::Notes => s
            .notes_src
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(none)".into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &PlaitsState,
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
        lines.push(theme::header("PLAITS", &format!("macro {}", instance), "", w));
        lines.push(Line::from(vec![
            Span::styled("  out ".to_string(), theme::chrome()),
            Span::styled(
                theme::meter_char(s.out_now).to_string(),
                theme::signal(theme::cv_ramp(s.out_now)),
            ),
        ]));
        lines.push(Line::from(""));

        let bar_w = theme::bar_width(w, 28);
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
            spans.push(Span::styled(format!(" {}{}", mark, row_text(s, *r)), vstyle));
            if let Some(addr) = src_index(*r).and_then(|k| s.srcs[k].as_ref()) {
                spans.push(Span::styled(format!("  ◂ {}", addr), theme::dim()));
            }
            lines.push(Line::from(spans));
        }

        theme::anchor_bottom(&mut lines, h, 2);
        lines.push(theme::rule(w));
        lines.push(theme::status("NORMAL", overlay.unwrap_or(""), "", w));
        f.render_widget(Paragraph::new(lines), area);

        if show_help {
            let help = Paragraph::new(vec![
                Line::from("━━━ PLAITS · macro-oscillator (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  engine     noise (filtered) · fm (2-op)"),
                Line::from("  harmonics  the engine's first macro (ratio / formant)"),
                Line::from("  timbre     the second (index / clock)"),
                Line::from("  morph      the third (feedback / resonance)"),
                Line::from("  notes      a note track sets the pitch + triggers"),
                Line::from("  amp        an envelope channel shapes the level"),
                Line::from(""),
                Line::from("Engines run at 48 kHz, resampled to the session."),
                Line::from("(v1: noise + fm; more engines follow.)"),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" PLAITS ", theme::chrome_hi())),
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

#[derive(PartialEq)]
enum Picking {
    ModSource,
    Amp,
    Notes,
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("plaits", instance);

    let shared = Arc::new(Mutex::new(PlaitsState::new()));
    if let Ok(p) = state::load_module_state::<state::PlaitsParams>("plaits", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let builder = thread::Builder::new()
        .name(String::from("plaits-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = builder.spawn(move || {
        if let Err(e) = audio_thread(audio_state, instance) {
            eprintln!("[plaits {}] audio thread error: {}", instance, e);
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
    let mut picking = Picking::ModSource;
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
            let _ = state::save_module_state("plaits", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::PlaitsParams>("plaits", instance) {
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
                    let steps: i32 = if m.kind == MouseEventKind::ScrollUp { 1 } else { -1 };
                    use crate::undo::ParamUndo;
                    let mut s = shared.lock().unwrap();
                    let slot = s.selected;
                    let r = ROWS[slot.min(ROWS.len() - 1)];
                    if src_index(r).is_none() {
                        continue;
                    }
                    let old = s.get_param(slot);
                    let v = s.get(r) + steps as f32 * 0.01;
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
                match picking {
                    Picking::ModSource => {
                        if let Some(k) = src_index(ROWS[s.selected.min(ROWS.len() - 1)]) {
                            let slot = SRC_SLOT_BASE + k;
                            let old = s.get_param(slot);
                            s.srcs[k] = addr.clone();
                            s.resolved[k] = None;
                            if let Some(old) = old {
                                history.record(
                                    slot,
                                    "Bind",
                                    old,
                                    ParamValue::Src(addr.map(|a| a.to_string())),
                                );
                            }
                        }
                    }
                    Picking::Amp => {
                        let old = s.get_param(AMP_SLOT);
                        s.amp_src = addr.clone();
                        s.amp_resolved = None;
                        if let Some(old) = old {
                            history.record(AMP_SLOT, "Bind", old, ParamValue::Src(addr.map(|a| a.to_string())));
                        }
                    }
                    Picking::Notes => {
                        let old = s.get_param(NOTES_SLOT);
                        s.notes_src = addr.clone();
                        if let Some(old) = old {
                            history.record(NOTES_SLOT, "Bind", old, ParamValue::Src(addr.map(|a| a.to_string())));
                        }
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
                            match crate::excmd::ex_write(name, &mut patch_name, &mut baseline, &params) {
                                Ok(m) | Err(m) => m,
                            },
                        );
                    }
                    ExCommand::Edit(name) => match state::load_patch::<state::PlaitsParams>(&name) {
                        Ok(p) => {
                            apply_params(&mut shared.lock().unwrap(), &p);
                            baseline = state::to_toml_string(&snapshot_params(&shared.lock().unwrap()))
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
                        match crate::excmd::ex_write(name, &mut patch_name, &mut baseline, &params) {
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
            ex_msg = Some(crate::undo::history_status("Redo", n, || history.redo(&mut *s)));
            continue;
        }
        if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
            let params = snapshot_params(&shared.lock().unwrap());
            let _ = state::save_module_state("plaits", instance, &params);
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
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let slot = s.selected;
                let r = ROWS[slot.min(ROWS.len() - 1)];
                match r {
                    Row::Engine => {
                        let old = s.get_param(slot);
                        let v = (s.engine as i32 + steps).rem_euclid(ENGINE_NAMES.len() as i32) as usize;
                        s.engine = v;
                        if let Some(old) = old {
                            history.record(slot, "Engine", old, ParamValue::Usize(v));
                        }
                    }
                    Row::Amp | Row::Notes => {
                        ex_msg = Some("source row: @ binds, x clears".into());
                    }
                    _ => {
                        let old = s.get_param(slot);
                        let v = step_f32(s.get(r), steps, 0.01, coarse, 0.0, 1.0);
                        s.set(r, v);
                        let new = s.get_param(slot);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(slot, "Adjust", old, new);
                        }
                    }
                }
            }
            KeyCode::Char('0') => {
                count.clear();
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                let slot = s.selected;
                let old = s.get_param(slot);
                let def = PlaitsState::new();
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
                let (pick, current) = match r {
                    Row::Amp => (Some(Picking::Amp), s.amp_src.clone()),
                    Row::Notes => (Some(Picking::Notes), s.notes_src.clone()),
                    _ => match src_index(r) {
                        Some(k) => (Some(Picking::ModSource), s.srcs[k].clone()),
                        None => (None, None),
                    },
                };
                drop(s);
                if let Some(p) = pick {
                    let sources = Manifest::open()
                        .map(|m| routing::live_sources(&m.entries()))
                        .unwrap_or_default();
                    picking = p;
                    picker.open(sources, current.as_ref());
                } else {
                    ex_msg = Some("engine row: h/l cycles".into());
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                match r {
                    Row::Amp => {
                        if s.amp_src.is_some() {
                            let old = s.get_param(AMP_SLOT);
                            s.amp_src = None;
                            s.amp_resolved = None;
                            if let Some(old) = old {
                                history.record(AMP_SLOT, "Unbind", old, ParamValue::Src(None));
                            }
                        }
                    }
                    Row::Notes => {
                        if s.notes_src.is_some() {
                            let old = s.get_param(NOTES_SLOT);
                            s.notes_src = None;
                            if let Some(old) = old {
                                history.record(NOTES_SLOT, "Unbind", old, ParamValue::Src(None));
                            }
                        }
                    }
                    _ => {
                        if let Some(k) = src_index(r) {
                            if s.srcs[k].is_some() {
                                let slot = SRC_SLOT_BASE + k;
                                let old = s.get_param(slot);
                                s.srcs[k] = None;
                                s.resolved[k] = None;
                                if let Some(old) = old {
                                    history.record(slot, "Unbind", old, ParamValue::Src(None));
                                }
                            }
                        }
                    }
                }
            }
            KeyCode::Char('u') => {
                let n = count.take();
                let mut s = shared.lock().unwrap();
                ex_msg = Some(crate::undo::history_status("Undo", n, || history.undo(&mut *s)));
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
    s: &mut PlaitsState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    let Some(slot) = ROWS.iter().position(|r| row_label(*r) == key) else {
        return format!("Unknown setting: {key} (engine harmonics timbre morph level amp notes)");
    };
    let r = ROWS[slot];
    let parsed: Result<V, String> = match r {
        Row::Engine => ENGINE_NAMES
            .iter()
            .position(|n| *n == value)
            .map(V::Usize)
            .ok_or_else(|| format!("{key}: one of {}", ENGINE_NAMES.join(" "))),
        Row::Amp | Row::Notes => {
            if value == "-" {
                Ok(V::Src(None))
            } else {
                Ok(V::Src(Some(value.to_string())))
            }
        }
        _ => {
            if let Ok(v) = value.parse::<f32>() {
                Ok(V::F32(v))
            } else if let Some(k) = src_index(r) {
                let slot = SRC_SLOT_BASE + k;
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
                return format!("{} ◂ {}", key, value);
            } else {
                Err(format!("{key}: not a number: {value}"))
            }
        }
    };
    match parsed {
        Ok(v) => {
            let old = s.get_param(slot);
            s.set_param(slot, v.clone());
            if let Some(old) = old {
                history.record(slot, "Set", old, v);
            }
            format!("{} = {}", key, row_text(s, r))
        }
        Err(m) => m,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_knob_takes_a_cable() {
        for r in ROWS {
            if matches!(r, Row::Harmonics | Row::Timbre | Row::Morph | Row::Level) {
                assert!(src_index(r).is_some(), "{r:?} bindable");
            }
        }
        assert_eq!(N_SRC, 4);
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = PlaitsState::new();
        s.engine = 1;
        s.harmonics = 0.7;
        s.morph = 0.3;
        s.srcs[0] = SourceAddr::parse("lfo/0/s1");
        s.amp_src = SourceAddr::parse("envelope/0/ch1");
        s.notes_src = SourceAddr::parse("sequencer/0/t1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::PlaitsParams = toml::from_str(&toml).expect("parses");
        let mut s2 = PlaitsState::new();
        apply_params(&mut s2, &back);
        assert_eq!(s2.engine, 1);
        assert!((s2.harmonics - 0.7).abs() < 1e-6);
        assert_eq!(s2.amp_src.as_ref().map(|a| a.to_string()), Some("envelope/0/ch1".into()));
        assert_eq!(s2.notes_src.as_ref().map(|a| a.to_string()), Some("sequencer/0/t1".into()));
    }

    #[test]
    fn ex_set_parses() {
        let mut s = PlaitsState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "engine", "fm").contains("fm"));
        assert!(ex_set(&mut s, &mut h, "harmonics", "0.6").contains('%'));
        assert!(ex_set(&mut s, &mut h, "morph", "lfo/0/s2").contains("lfo/0/s2"));
        assert!(ex_set(&mut s, &mut h, "notes", "sequencer/0/t1").contains("t1"));
        assert_eq!(s.engine, 1);
        assert!(s.srcs[2].is_some());
    }

    #[test]
    fn freq_to_note_is_a4() {
        assert!((freq_to_note(440.0) - 69.0).abs() < 1e-3);
        assert!((freq_to_note(880.0) - 81.0).abs() < 1e-3);
    }
}
