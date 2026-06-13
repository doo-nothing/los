//! # Clouds — the granular-processor shell
//!
//! Claims one stereo audio input, records it into the granular
//! buffer, and granulates it into a cloud. The nine knobs are the
//! firmware's panel — position, size, pitch, density, texture,
//! dry/wet, stereo spread, feedback, reverb — plus a freeze toggle
//! that holds the recording buffer. Publishes clouds/N/level (an
//! output follower) on the bus. v1 is the granular playback mode.

// max/min, not clamp, where modbus values land: clamp(NaN) is NaN and
// a stale channel must die at the boundary.
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

use super::dsp::{CloudsParams, GranularProcessor};
use crate::ipc::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

const FALLBACK_RATE: f32 = 48_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Position,
    Size,
    Pitch,
    Density,
    Texture,
    DryWet,
    Spread,
    Feedback,
    Reverb,
    Freeze,
    Input,
}

const ROWS: [Row; 11] = [
    Row::Position,
    Row::Size,
    Row::Pitch,
    Row::Density,
    Row::Texture,
    Row::DryWet,
    Row::Spread,
    Row::Feedback,
    Row::Reverb,
    Row::Freeze,
    Row::Input,
];

/// The nine continuous knobs — all bindable.
const BINDABLE: [Row; 9] = [
    Row::Position,
    Row::Size,
    Row::Pitch,
    Row::Density,
    Row::Texture,
    Row::DryWet,
    Row::Spread,
    Row::Feedback,
    Row::Reverb,
];
const N_SRC: usize = BINDABLE.len();
const SRC_SLOT_BASE: usize = 20;
const INPUT_SLOT: usize = 10;

fn src_index(r: Row) -> Option<usize> {
    BINDABLE.iter().position(|b| *b == r)
}

struct CloudsState {
    knob: [f32; N_SRC], // position..reverb in BINDABLE order
    freeze: bool,
    input: Option<String>,
    input_live: bool,
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    out_now: f32,
    selected: usize,
}

fn default_knobs() -> [f32; N_SRC] {
    // position, size, pitch, density, texture, dry_wet, spread, feedback, reverb
    [0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.0, 0.0, 0.0]
}

impl CloudsState {
    fn new() -> Self {
        Self {
            knob: default_knobs(),
            freeze: false,
            input: None,
            input_live: true,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: default_knobs(),
            out_now: 0.0,
            selected: 0,
        }
    }

    fn knob_index(r: Row) -> Option<usize> {
        src_index(r)
    }

    fn get(&self, r: Row) -> f32 {
        Self::knob_index(r).map(|i| self.knob[i]).unwrap_or(0.0)
    }

    fn set(&mut self, r: Row, v: f32) {
        if let Some(i) = Self::knob_index(r) {
            self.knob[i] = v.max(0.0).min(1.0);
        }
    }
}

impl crate::undo::ParamUndo for CloudsState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            let s = self.srcs.get(i)?;
            return Some(V::Src(s.as_ref().map(|a| a.to_string())));
        }
        let r = *ROWS.get(slot)?;
        Some(match r {
            Row::Freeze => V::Bool(self.freeze),
            Row::Input => V::Src(self.input.clone()),
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
            (Row::Freeze, V::Bool(v)) => self.freeze = v,
            (Row::Input, V::Src(v)) => self.input = v,
            (_, V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &CloudsState) -> state::CloudsParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::CloudsParams {
        format: state::STATE_FORMAT,
        position: Some(s.knob[0]),
        size: Some(s.knob[1]),
        pitch: Some(s.knob[2]),
        density: Some(s.knob[3]),
        texture: Some(s.knob[4]),
        dry_wet: Some(s.knob[5]),
        spread: Some(s.knob[6]),
        feedback: Some(s.knob[7]),
        reverb: Some(s.knob[8]),
        freeze: Some(s.freeze),
        input: s.input.clone(),
        position_src: src(0),
        size_src: src(1),
        pitch_src: src(2),
        density_src: src(3),
        texture_src: src(4),
        dry_wet_src: src(5),
        spread_src: src(6),
        feedback_src: src(7),
        reverb_src: src(8),
    }
}

fn apply_params(s: &mut CloudsState, p: &state::CloudsParams) {
    let vals = [
        p.position, p.size, p.pitch, p.density, p.texture, p.dry_wet, p.spread, p.feedback,
        p.reverb,
    ];
    for (i, v) in vals.iter().enumerate() {
        if let Some(v) = v {
            s.knob[i] = v.max(0.0).min(1.0);
        }
    }
    if let Some(v) = p.freeze {
        s.freeze = v;
    }
    if p.input.is_some() {
        s.input = p.input.clone();
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.position_src),
        parse(&p.size_src),
        parse(&p.pitch_src),
        parse(&p.density_src),
        parse(&p.texture_src),
        parse(&p.dry_wet_src),
        parse(&p.spread_src),
        parse(&p.feedback_src),
        parse(&p.reverb_src),
    ];
    s.resolved = Default::default();
}

// ── audio thread ─────────────────────────────────────────────────────────────

fn audio_thread(shared: Arc<Mutex<CloudsState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_clouds_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating clouds ring")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("clouds", instance, Some(&shm_name), 1)?;
    let mod_base = manifest.claimed_base();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();
    let transport = ShmTransport::open().ok();
    let mut sample_rate = transport
        .as_ref()
        .map(|t| t.sample_rate() as f32)
        .filter(|r| *r > 0.0)
        .unwrap_or(FALLBACK_RATE);

    let channels = ringbuf.channels() as usize;
    let slot_frames = ringbuf.slot_frames() as usize;
    let mut block = vec![0.0_f32; ringbuf.slot_len()];
    let mut scratch = vec![0.0_f32; ringbuf.slot_len()];
    // interleaved stereo working buffer, processed in 32-frame chunks
    let mut io = vec![0.0_f32; slot_frames * 2];

    // a ~3 second granular buffer
    let mut proc = GranularProcessor::new(sample_rate, 3.0, 0xc10d ^ instance as u32);

    let mut input: Option<AudioRingbuf> = None;
    let mut input_shm: Option<String> = None;
    let mut follower = 0.0_f32;
    let mut blocks: u64 = 0;

    loop {
        if blocks.is_multiple_of(64) {
            let now_rate = transport
                .as_ref()
                .map(|t| t.sample_rate() as f32)
                .filter(|r| *r > 0.0)
                .unwrap_or(FALLBACK_RATE);
            if (now_rate - sample_rate).abs() > 0.5 {
                sample_rate = now_rate;
                proc = GranularProcessor::new(sample_rate, 3.0, 0xc10d ^ instance as u32);
            }
            let entries = manifest.entries();
            let desired: Option<String> = {
                let mut s = shared.lock().unwrap();
                for k in 0..N_SRC {
                    s.resolved[k] = s.srcs[k]
                        .as_ref()
                        .and_then(|a| routing::resolve(&entries, a));
                }
                let desired = s.input.as_deref().and_then(|sel| {
                    let (m, i) = sel.split_once('/')?;
                    let i: usize = i.parse().ok()?;
                    entries
                        .iter()
                        .find(|e| e.module_name == m && e.instance == i)
                        .and_then(|e| e.audio_shm.clone())
                });
                s.input_live = s.input.is_none() || desired.is_some();
                let mask = s
                    .resolved
                    .iter()
                    .flatten()
                    .filter(|&&c| c < 64)
                    .fold(0u64, |m, &c| m | (1 << c));
                manifest.publish_consumes(mask, 0);
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

        let tick = Instant::now();
        let mut got = false;
        if let Some(rb) = input.as_mut() {
            loop {
                if rb.read(&mut block).unwrap_or(false) {
                    got = true;
                    break;
                }
                if tick.elapsed() > Duration::from_millis(4) {
                    break;
                }
                thread::sleep(Duration::from_micros(200));
            }
        }
        if !got {
            block.iter_mut().for_each(|v| *v = 0.0);
        }
        for f in 0..slot_frames {
            io[f * 2] = block[f * channels];
            io[f * 2 + 1] = if channels > 1 {
                block[f * channels + 1]
            } else {
                block[f * channels]
            };
        }

        let (params, _) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            let cv = |k: usize, manual: f32, s: &CloudsState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let mut v = [0.0_f32; N_SRC];
            for (k, x) in v.iter_mut().enumerate() {
                *x = cv(k, s.knob[k], &s);
            }
            s.eff = v;
            let freeze = s.freeze;
            (
                CloudsParams {
                    position: v[0],
                    size: v[1],
                    pitch: (v[2] - 0.5) * 48.0, // ±2 octaves
                    density: v[3],
                    texture: v[4],
                    dry_wet: v[5],
                    stereo_spread: v[6],
                    feedback: v[7],
                    reverb: v[8],
                    freeze,
                    trigger: false,
                },
                freeze,
            )
        };

        // process in 32-frame chunks (the granular block size)
        let mut f = 0;
        while f < slot_frames {
            let n = (slot_frames - f).min(super::dsp::MAX_BLOCK);
            proc.process(&mut io[f * 2..(f + n) * 2], &params, n);
            f += n;
        }

        for f in 0..slot_frames {
            let l = io[f * 2];
            let r = io[f * 2 + 1];
            follower = follower.max(l.abs().max(r.abs())) * 0.9995;
            block[f * channels] = l;
            if channels > 1 {
                block[f * channels + 1] = r;
            }
        }
        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            proc = GranularProcessor::new(sample_rate, 3.0, 0xc10d ^ instance as u32);
        }
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            bus.set(base, follower.min(1.0));
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
        Row::Position => "position",
        Row::Size => "size",
        Row::Pitch => "pitch",
        Row::Density => "density",
        Row::Texture => "texture",
        Row::DryWet => "dry/wet",
        Row::Spread => "spread",
        Row::Feedback => "feedback",
        Row::Reverb => "reverb",
        Row::Freeze => "freeze",
        Row::Input => "input",
    }
}

fn row_text(s: &CloudsState, r: Row) -> String {
    match r {
        Row::Freeze => if s.freeze { "held" } else { "live" }.to_string(),
        Row::Pitch => {
            let st = (s.get(r) - 0.5) * 48.0;
            format!("{:+.0} st", st)
        }
        Row::Input => s
            .input
            .clone()
            .map(|i| {
                if s.input_live {
                    i
                } else {
                    format!("{i} ✗ offline")
                }
            })
            .unwrap_or_else(|| "(unpatched · silent)".into()),
        _ => format!("{:.0}%", s.get(r) * 100.0),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &CloudsState,
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
        lines.push(theme::header("CLOUDS", &format!("granular {}", instance), "", w));
        lines.push(Line::from(vec![
            Span::styled("  out ".to_string(), theme::chrome()),
            Span::styled(
                theme::meter_char(s.out_now).to_string(),
                theme::signal(theme::cv_ramp(s.out_now)),
            ),
        ]));
        lines.push(Line::from(""));

        let bar_w = theme::bar_width(w, 26);
        for (i, r) in ROWS.iter().enumerate() {
            let selected = i == s.selected;
            let label_style = if selected {
                theme::selected()
            } else {
                theme::chrome()
            };
            let mut spans: Vec<Span> =
                vec![Span::styled(format!(" {:<9}", row_label(*r)), label_style)];
            let bound = src_index(*r).is_some_and(|k| s.srcs[k].is_some());
            let hue = src_index(*r)
                .and_then(|k| s.srcs[k].as_ref())
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
                Line::from("━━━ CLOUDS · granular processor (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  position  where in the buffer grains are drawn"),
                Line::from("  size      grain length (1024–16384 samples)"),
                Line::from("  pitch     grain transpose (±2 octaves)"),
                Line::from("  density   grain rate — centre is silent, both ways thicken"),
                Line::from("  texture   window shape, then diffusion past 75%"),
                Line::from("  dry/wet   blend; spread = stereo scatter"),
                Line::from("  feedback  re-inject the cloud; reverb = the space"),
                Line::from("  freeze    hold the buffer (stop recording)"),
                Line::from(""),
                Line::from("Patch a pad or break to the input and granulate it."),
                Line::from("Publishes clouds/N/level. (v1: granular mode.)"),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" CLOUDS ", theme::chrome_hi())),
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
    Input,
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("clouds", instance);

    let shared = Arc::new(Mutex::new(CloudsState::new()));
    if let Ok(p) = state::load_module_state::<state::CloudsParams>("clouds", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let builder = thread::Builder::new()
        .name(String::from("clouds-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = builder.spawn(move || {
        if let Err(e) = audio_thread(audio_state, instance) {
            eprintln!("[clouds {}] audio thread error: {}", instance, e);
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
            let _ = state::save_module_state("clouds", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::CloudsParams>("clouds", instance) {
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
            match picker.handle_key(key.code) {
                crate::picker::PickerEvent::Chosen(addr) => match picking {
                    Picking::ModSource => {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = shared.lock().unwrap();
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
                    Picking::Input => {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = shared.lock().unwrap();
                        let old = s.get_param(INPUT_SLOT);
                        s.input = None;
                        if let Some(old) = old {
                            history.record(INPUT_SLOT, "Unpatch", old, ParamValue::Src(None));
                        }
                    }
                },
                crate::picker::PickerEvent::ChosenSpecial(i) if picking == Picking::Input => {
                    use crate::undo::{ParamUndo, ParamValue};
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
                    ExCommand::Edit(name) => match state::load_patch::<state::CloudsParams>(&name) {
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
            let _ = state::save_module_state("clouds", instance, &params);
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
                    Row::Freeze => {
                        let old = s.get_param(slot);
                        s.freeze = !s.freeze;
                        let new = ParamValue::Bool(s.freeze);
                        if let Some(old) = old {
                            history.record(slot, "Toggle", old, new);
                        }
                    }
                    Row::Input => {
                        ex_msg = Some("input row: @ patches, x unpatches".into());
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
                let def = CloudsState::new();
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
                if r == Row::Input {
                    let current = s.input.clone();
                    drop(s);
                    let entries = Manifest::open().map(|m| m.entries()).unwrap_or_default();
                    input_options = entries
                        .iter()
                        .filter(|e| e.audio_shm.is_some())
                        .filter(|e| !(e.module_name == "clouds" && e.instance == instance))
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
                    picking = Picking::Input;
                    picker.open_with(specials, Vec::new(), None, cur_special);
                } else if let Some(k) = src_index(r) {
                    let current = s.srcs[k].clone();
                    drop(s);
                    let sources = Manifest::open()
                        .map(|m| routing::live_sources(&m.entries()))
                        .unwrap_or_default();
                    picking = Picking::ModSource;
                    picker.open(sources, current.as_ref());
                } else {
                    ex_msg = Some("freeze: h/l toggles".into());
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                if r == Row::Input {
                    let old = s.get_param(INPUT_SLOT);
                    s.input = None;
                    if let Some(old) = old {
                        history.record(INPUT_SLOT, "Unpatch", old, ParamValue::Src(None));
                    }
                } else if let Some(k) = src_index(r) {
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
    s: &mut CloudsState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    let Some(slot) = ROWS.iter().position(|r| row_label(*r) == key) else {
        return format!(
            "Unknown setting: {key} (position size pitch density texture dry/wet spread feedback reverb freeze input)"
        );
    };
    let r = ROWS[slot];
    let parsed: Result<V, String> = match r {
        Row::Freeze => match value {
            "on" | "held" | "true" | "1" => Ok(V::Bool(true)),
            "off" | "live" | "false" | "0" => Ok(V::Bool(false)),
            _ => Err(format!("{key}: live/held")),
        },
        Row::Input => {
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
            if !matches!(r, Row::Freeze | Row::Input) {
                assert!(src_index(r).is_some(), "{r:?} must be bindable");
            }
        }
        assert_eq!(N_SRC, 9);
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = CloudsState::new();
        s.knob = [0.1, 0.2, 0.3, 0.8, 0.9, 0.7, 0.4, 0.5, 0.6];
        s.freeze = true;
        s.input = Some("voice/0".into());
        s.srcs[3] = SourceAddr::parse("lfo/0/s1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::CloudsParams = toml::from_str(&toml).expect("parses");
        let mut s2 = CloudsState::new();
        apply_params(&mut s2, &back);
        assert!((s2.knob[0] - 0.1).abs() < 1e-6);
        assert!((s2.knob[3] - 0.8).abs() < 1e-6);
        assert!(s2.freeze);
        assert_eq!(s2.input.as_deref(), Some("voice/0"));
        assert_eq!(
            s2.srcs[3].as_ref().map(|a| a.to_string()),
            Some("lfo/0/s1".into())
        );
    }

    #[test]
    fn ex_set_parses() {
        let mut s = CloudsState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "position", "0.7").contains('%'));
        assert!(ex_set(&mut s, &mut h, "freeze", "held").contains("held"));
        assert!(ex_set(&mut s, &mut h, "density", "lfo/0/s1").contains("lfo/0/s1"));
        assert!(ex_set(&mut s, &mut h, "input", "voice/0").contains("voice/0"));
        assert!(s.freeze);
        assert!(s.srcs[3].is_some());
    }
}
