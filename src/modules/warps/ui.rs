//! # Warps — the meta-modulator shell
//!
//! Claims two audio inputs — `carrier` (L) and `modulator` (R) — runs
//! them through the cross-modulation engine, and writes the modulated
//! output to its own audio ring. The `algorithm` knob sweeps through
//! cross-fade → wavefold → ring-mod → XOR → comparator → vocoder; the
//! `timbre` knob is each algorithm's parameter. `carrier_shape`
//! swaps the external carrier for an internal oscillator at `note`.
//! Publishes the aux signal as warps/N/aux.

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

use super::dsp::{Modulator, Params};
use crate::ipc::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

const FALLBACK_RATE: f32 = 48_000.0;

pub const SHAPE_NAMES: [&str; 7] =
    ["external", "sine", "triangle", "saw", "pulse", "noise", "freq_shifter"];

fn algo_name(a: f32) -> &'static str {
    let x = a * 8.0;
    match x as usize {
        0 => "cross-fade",
        1 => "wavefold",
        2 => "analog ring",
        3 => "digital ring",
        4 => "xor",
        5 => "comparator",
        _ => {
            if a >= 0.75 {
                "vocoder"
            } else {
                "→ vocoder"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Algorithm,
    Timbre,
    Drive1,
    Drive2,
    Shape,
    Note,
    Carrier,
    Modulator,
}

const ROWS: [Row; 8] = [
    Row::Algorithm,
    Row::Timbre,
    Row::Drive1,
    Row::Drive2,
    Row::Shape,
    Row::Note,
    Row::Carrier,
    Row::Modulator,
];

/// Bindable knobs (srcs[] order). Every value knob takes a cable.
const BINDABLE: [Row; 5] = [
    Row::Algorithm,
    Row::Timbre,
    Row::Drive1,
    Row::Drive2,
    Row::Note,
];
const N_SRC: usize = BINDABLE.len();
const SRC_SLOT_BASE: usize = 20;
const CARRIER_SLOT: usize = 6;
const MODULATOR_SLOT: usize = 7;

fn src_index(r: Row) -> Option<usize> {
    BINDABLE.iter().position(|b| *b == r)
}

struct WarpsState {
    algorithm: f32,
    timbre: f32,
    drive1: f32,
    drive2: f32,
    shape: usize,
    note: f32, // 0..1 → 0..96 + 24
    carrier: Option<String>,
    modulator: Option<String>,
    carrier_live: bool,
    modulator_live: bool,
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    aux_now: f32,
    out_now: f32,
    selected: usize,
}

impl WarpsState {
    fn new() -> Self {
        Self {
            algorithm: 0.25,
            timbre: 0.5,
            drive1: 0.6,
            drive2: 0.6,
            shape: 0,
            note: 0.5,
            carrier: None,
            modulator: None,
            carrier_live: true,
            modulator_live: true,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.25, 0.5, 0.6, 0.6, 0.5],
            aux_now: 0.0,
            out_now: 0.0,
            selected: 0,
        }
    }

    fn get(&self, r: Row) -> f32 {
        match r {
            Row::Algorithm => self.algorithm,
            Row::Timbre => self.timbre,
            Row::Drive1 => self.drive1,
            Row::Drive2 => self.drive2,
            Row::Note => self.note,
            _ => 0.0,
        }
    }

    fn set(&mut self, r: Row, v: f32) {
        let v = v.max(0.0).min(1.0);
        match r {
            Row::Algorithm => self.algorithm = v,
            Row::Timbre => self.timbre = v,
            Row::Drive1 => self.drive1 = v,
            Row::Drive2 => self.drive2 = v,
            Row::Note => self.note = v,
            _ => {}
        }
    }
}

impl crate::undo::ParamUndo for WarpsState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            let s = self.srcs.get(i)?;
            return Some(V::Src(s.as_ref().map(|a| a.to_string())));
        }
        let r = *ROWS.get(slot)?;
        Some(match r {
            Row::Shape => V::Usize(self.shape),
            Row::Carrier => V::Src(self.carrier.clone()),
            Row::Modulator => V::Src(self.modulator.clone()),
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
            (Row::Shape, V::Usize(v)) => self.shape = v.min(SHAPE_NAMES.len() - 1),
            (Row::Carrier, V::Src(v)) => self.carrier = v,
            (Row::Modulator, V::Src(v)) => self.modulator = v,
            (_, V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &WarpsState) -> state::WarpsParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::WarpsParams {
        format: state::STATE_FORMAT,
        algorithm: Some(s.algorithm),
        timbre: Some(s.timbre),
        drive1: Some(s.drive1),
        drive2: Some(s.drive2),
        shape: Some(SHAPE_NAMES[s.shape].to_string()),
        note: Some(s.note),
        carrier: s.carrier.clone(),
        modulator: s.modulator.clone(),
        algorithm_src: src(0),
        timbre_src: src(1),
        drive1_src: src(2),
        drive2_src: src(3),
        note_src: src(4),
    }
}

fn apply_params(s: &mut WarpsState, p: &state::WarpsParams) {
    if let Some(v) = p.algorithm {
        s.algorithm = v.max(0.0).min(1.0);
    }
    if let Some(v) = p.timbre {
        s.timbre = v.max(0.0).min(1.0);
    }
    if let Some(v) = p.drive1 {
        s.drive1 = v.max(0.0).min(1.0);
    }
    if let Some(v) = p.drive2 {
        s.drive2 = v.max(0.0).min(1.0);
    }
    if let Some(sh) = p.shape.as_deref() {
        if let Some(i) = SHAPE_NAMES.iter().position(|n| *n == sh) {
            s.shape = i;
        }
    }
    if let Some(v) = p.note {
        s.note = v.max(0.0).min(1.0);
    }
    if p.carrier.is_some() {
        s.carrier = p.carrier.clone();
    }
    if p.modulator.is_some() {
        s.modulator = p.modulator.clone();
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.algorithm_src),
        parse(&p.timbre_src),
        parse(&p.drive1_src),
        parse(&p.drive2_src),
        parse(&p.note_src),
    ];
    s.resolved = Default::default();
}

// ── audio thread ─────────────────────────────────────────────────────────────

fn resolve_input(
    entries: &[crate::shm::ManifestEntry],
    sel: Option<&str>,
) -> Option<String> {
    let sel = sel?;
    let (m, i) = sel.split_once('/')?;
    let i: usize = i.parse().ok()?;
    entries
        .iter()
        .find(|e| e.module_name == m && e.instance == i)
        .and_then(|e| e.audio_shm.clone())
}

fn audio_thread(shared: Arc<Mutex<WarpsState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_warps_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating warps ring")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    // one claim: the aux output as a modulation source
    manifest.register("warps", instance, Some(&shm_name), 1)?;
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
    let mut carrier = vec![0.0_f32; slot_frames];
    let mut modulator = vec![0.0_f32; slot_frames];
    let mut main = vec![0.0_f32; slot_frames];
    let mut aux = vec![0.0_f32; slot_frames];
    let mut cblock = vec![0.0_f32; ringbuf.slot_len()];
    let mut mblock = vec![0.0_f32; ringbuf.slot_len()];

    let mut modu = Modulator::new(sample_rate);

    let mut carrier_in: Option<AudioRingbuf> = None;
    let mut modulator_in: Option<AudioRingbuf> = None;
    let mut carrier_shm: Option<String> = None;
    let mut modulator_shm: Option<String> = None;
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
                modu = Modulator::new(sample_rate);
            }
            let entries = manifest.entries();
            let (want_c, want_m) = {
                let mut s = shared.lock().unwrap();
                for k in 0..N_SRC {
                    s.resolved[k] = s.srcs[k]
                        .as_ref()
                        .and_then(|a| routing::resolve(&entries, a));
                }
                let want_c = resolve_input(&entries, s.carrier.as_deref());
                let want_m = resolve_input(&entries, s.modulator.as_deref());
                s.carrier_live = s.carrier.is_none() || want_c.is_some();
                s.modulator_live = s.modulator.is_none() || want_m.is_some();
                (want_c, want_m)
            };
            if want_c != carrier_shm {
                carrier_in = want_c.as_deref().and_then(|n| AudioRingbuf::open(n).ok());
                if let Some(rb) = carrier_in.as_mut() {
                    while rb.available() > 1 {
                        let _ = rb.read(&mut scratch);
                    }
                }
                manifest.publish_input(want_c.as_deref());
                carrier_shm = want_c;
            }
            if want_m != modulator_shm {
                modulator_in = want_m.as_deref().and_then(|n| AudioRingbuf::open(n).ok());
                if let Some(rb) = modulator_in.as_mut() {
                    while rb.available() > 1 {
                        let _ = rb.read(&mut scratch);
                    }
                }
                modulator_shm = want_m;
            }
        }

        let tick = Instant::now();
        let read_into = |rb: &mut Option<AudioRingbuf>, buf: &mut [f32]| -> bool {
            if let Some(r) = rb.as_mut() {
                let t = Instant::now();
                loop {
                    if r.read(buf).unwrap_or(false) {
                        return true;
                    }
                    if t.elapsed() > Duration::from_millis(4) {
                        return false;
                    }
                    thread::sleep(Duration::from_micros(200));
                }
            }
            false
        };
        let _ = tick;
        if !read_into(&mut carrier_in, &mut cblock) {
            cblock.iter_mut().for_each(|v| *v = 0.0);
        }
        if !read_into(&mut modulator_in, &mut mblock) {
            mblock.iter_mut().for_each(|v| *v = 0.0);
        }
        for f in 0..slot_frames {
            carrier[f] = 0.5 * (cblock[f * channels] + cblock[f * channels + 1]);
            modulator[f] = 0.5 * (mblock[f * channels] + mblock[f * channels + 1]);
        }

        let params = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            let cv = |k: usize, manual: f32, s: &WarpsState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let vals: Vec<f32> = BINDABLE
                .iter()
                .enumerate()
                .map(|(k, r)| cv(k, s.get(*r), &s))
                .collect();
            for (k, v) in vals.iter().enumerate() {
                s.eff[k] = *v;
            }
            Params {
                algorithm: vals[0],
                timbre: vals[1],
                drive1: vals[2],
                drive2: vals[3],
                // shape 6 = the SSB frequency-shifter easter egg (external in)
                carrier_shape: if s.shape == 6 { 0 } else { s.shape },
                note: 24.0 + vals[4] * 96.0,
                frequency_shifter: s.shape == 6,
            }
        };

        modu.process(&params, &carrier, &modulator, &mut main, &mut aux);

        for f in 0..slot_frames {
            let v = main[f];
            follower = follower.max(v.abs()) * 0.9995;
            block[f * channels] = v;
            if channels > 1 {
                block[f * channels + 1] = v;
            }
        }
        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            modu = Modulator::new(sample_rate);
        }
        let aux_level = aux.iter().fold(0.0_f32, |m, v| m.max(v.abs())).min(1.0);
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            bus.set(base, aux_level);
        }
        {
            let mut s = shared.lock().unwrap();
            s.out_now = follower.min(1.0);
            s.aux_now = aux_level;
        }
        while ringbuf.write(&block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }
        blocks += 1;
    }
}

// ── ui ───────────────────────────────────────────────────────────────────────

fn row_label(r: Row) -> &'static str {
    match r {
        Row::Algorithm => "algorithm",
        Row::Timbre => "timbre",
        Row::Drive1 => "drive 1",
        Row::Drive2 => "drive 2",
        Row::Shape => "carrier",
        Row::Note => "note",
        Row::Carrier => "carrier in",
        Row::Modulator => "mod in",
    }
}

fn input_text(sel: &Option<String>, live: bool) -> String {
    sel.clone()
        .map(|i| if live { i } else { format!("{i} ✗ offline") })
        .unwrap_or_else(|| "(unpatched)".into())
}

fn row_text(s: &WarpsState, r: Row) -> String {
    match r {
        Row::Algorithm => format!("{} {:.0}%", algo_name(s.algorithm), s.algorithm * 100.0),
        Row::Timbre | Row::Drive1 | Row::Drive2 => format!("{:.0}%", s.get(r) * 100.0),
        Row::Shape => SHAPE_NAMES[s.shape].to_string(),
        Row::Note => {
            let n = 24.0 + s.note * 96.0;
            format!("{:.0}", n)
        }
        Row::Carrier => input_text(&s.carrier, s.carrier_live),
        Row::Modulator => input_text(&s.modulator, s.modulator_live),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &WarpsState,
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
        lines.push(theme::header("WARPS", &format!("meta {}", instance), "", w));
        lines.push(Line::from(vec![
            Span::styled("  out ".to_string(), theme::chrome()),
            Span::styled(
                theme::meter_char(s.out_now).to_string(),
                theme::signal(theme::cv_ramp(s.out_now)),
            ),
            Span::styled("  aux ".to_string(), theme::chrome()),
            Span::styled(
                theme::meter_char(s.aux_now).to_string(),
                theme::signal(theme::cv_ramp(s.aux_now)),
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
                vec![Span::styled(format!(" {:<11}", row_label(*r)), label_style)];
            let is_input = matches!(r, Row::Carrier | Row::Modulator);
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
            } else if is_input {
                theme::value()
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
                Line::from("━━━ WARPS · meta-modulator (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  algorithm  cross-fade · wavefold · ring-mod ×2"),
                Line::from("             · xor · comparator · vocoder (the sweep)"),
                Line::from("  timbre     each algorithm's parameter"),
                Line::from("  drive 1/2  per-channel overdrive into the gate"),
                Line::from("  carrier    external, or an internal sine/tri/saw/…"),
                Line::from("  carrier/mod in   the two audio inputs (L/R)"),
                Line::from(""),
                Line::from("Modulated output on its ring; publishes warps/N/aux."),
                Line::from("Patch a bass to mod in and a pad to carrier in,"),
                Line::from("then ride the algorithm knob."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" WARPS ", theme::chrome_hi())),
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
    Input(usize), // CARRIER_SLOT or MODULATOR_SLOT
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("warps", instance);

    let shared = Arc::new(Mutex::new(WarpsState::new()));
    if let Ok(p) = state::load_module_state::<state::WarpsParams>("warps", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let builder = thread::Builder::new()
        .name(String::from("warps-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = builder.spawn(move || {
        if let Err(e) = audio_thread(audio_state, instance) {
            eprintln!("[warps {}] audio thread error: {}", instance, e);
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
            let _ = state::save_module_state("warps", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::WarpsParams>("warps", instance) {
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
                crate::picker::PickerEvent::Chosen(addr) => {
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
                        Picking::Input(slot) => {
                            let old = s.get_param(slot);
                            if slot == CARRIER_SLOT {
                                s.carrier = None;
                            } else {
                                s.modulator = None;
                            }
                            if let Some(old) = old {
                                history.record(slot, "Unpatch", old, ParamValue::Src(None));
                            }
                        }
                    }
                }
                crate::picker::PickerEvent::ChosenSpecial(i) => {
                    if let Picking::Input(slot) = picking {
                        use crate::undo::{ParamUndo, ParamValue};
                        if let Some(sel) = input_options.get(i.saturating_sub(1)).cloned() {
                            let mut s = shared.lock().unwrap();
                            let old = s.get_param(slot);
                            if slot == CARRIER_SLOT {
                                s.carrier = Some(sel.clone());
                                s.carrier_live = true;
                            } else {
                                s.modulator = Some(sel.clone());
                                s.modulator_live = true;
                            }
                            if let Some(old) = old {
                                history.record(
                                    slot,
                                    "Patch",
                                    old,
                                    ParamValue::Src(Some(sel)),
                                );
                            }
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
                    ExCommand::Edit(name) => match state::load_patch::<state::WarpsParams>(&name) {
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
            let _ = state::save_module_state("warps", instance, &params);
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
                    Row::Shape => {
                        let old = s.get_param(slot);
                        let v = (s.shape as i32 + steps).rem_euclid(SHAPE_NAMES.len() as i32) as usize;
                        s.shape = v;
                        if let Some(old) = old {
                            history.record(slot, "Shape", old, ParamValue::Usize(v));
                        }
                    }
                    Row::Carrier | Row::Modulator => {
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
                let def = WarpsState::new();
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
                match r {
                    Row::Carrier | Row::Modulator => {
                        let slot = if r == Row::Carrier { CARRIER_SLOT } else { MODULATOR_SLOT };
                        let current = if r == Row::Carrier {
                            s.carrier.clone()
                        } else {
                            s.modulator.clone()
                        };
                        drop(s);
                        let entries = Manifest::open().map(|m| m.entries()).unwrap_or_default();
                        input_options = entries
                            .iter()
                            .filter(|e| e.audio_shm.is_some())
                            .filter(|e| !(e.module_name == "warps" && e.instance == instance))
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
                        picking = Picking::Input(slot);
                        picker.open_with(specials, Vec::new(), None, cur_special);
                    }
                    _ => {
                        if let Some(k) = src_index(r) {
                            let current = s.srcs[k].clone();
                            drop(s);
                            let sources = Manifest::open()
                                .map(|m| routing::live_sources(&m.entries()))
                                .unwrap_or_default();
                            picking = Picking::ModSource;
                            picker.open(sources, current.as_ref());
                        } else {
                            ex_msg = Some("carrier row: h/l cycles the shape".into());
                        }
                    }
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                match r {
                    Row::Carrier | Row::Modulator => {
                        let slot = if r == Row::Carrier { CARRIER_SLOT } else { MODULATOR_SLOT };
                        let old = s.get_param(slot);
                        if r == Row::Carrier {
                            s.carrier = None;
                        } else {
                            s.modulator = None;
                        }
                        if let Some(old) = old {
                            history.record(slot, "Unpatch", old, ParamValue::Src(None));
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
    s: &mut WarpsState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    let Some(slot) = ROWS.iter().position(|r| row_label(*r) == key) else {
        return format!(
            "Unknown setting: {key} (algorithm timbre drive 1 drive 2 carrier note carrier in mod in)"
        );
    };
    let r = ROWS[slot];
    let parsed: Result<V, String> = match r {
        Row::Shape => SHAPE_NAMES
            .iter()
            .position(|n| *n == value)
            .map(V::Usize)
            .ok_or_else(|| format!("{key}: one of {}", SHAPE_NAMES.join(" "))),
        Row::Carrier | Row::Modulator => {
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
            let knob = !matches!(r, Row::Shape | Row::Carrier | Row::Modulator);
            if knob {
                assert!(src_index(r).is_some(), "{r:?} must be bindable");
            }
        }
        assert_eq!(N_SRC, 5);
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = WarpsState::new();
        s.algorithm = 0.8;
        s.shape = 2;
        s.carrier = Some("voice/0".into());
        s.modulator = Some("swarm/0".into());
        s.srcs[0] = SourceAddr::parse("lfo/0/s1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::WarpsParams = toml::from_str(&toml).expect("parses");
        let mut s2 = WarpsState::new();
        apply_params(&mut s2, &back);
        assert!((s2.algorithm - 0.8).abs() < 1e-6);
        assert_eq!(s2.shape, 2);
        assert_eq!(s2.carrier.as_deref(), Some("voice/0"));
        assert_eq!(s2.modulator.as_deref(), Some("swarm/0"));
        assert_eq!(
            s2.srcs[0].as_ref().map(|a| a.to_string()),
            Some("lfo/0/s1".into())
        );
    }

    #[test]
    fn ex_set_parses() {
        let mut s = WarpsState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "algorithm", "0.5").contains('%'));
        assert!(ex_set(&mut s, &mut h, "carrier", "triangle").contains("triangle"));
        assert!(ex_set(&mut s, &mut h, "carrier in", "voice/0").contains("voice/0"));
        assert!(ex_set(&mut s, &mut h, "timbre", "lfo/0/s2").contains("lfo/0/s2"));
        assert_eq!(s.shape, 2);
        assert!(s.srcs[1].is_some());
    }

    #[test]
    fn algo_names_cover_the_sweep() {
        assert_eq!(algo_name(0.0), "cross-fade");
        assert_eq!(algo_name(0.95), "vocoder");
    }
}
