//! # The Wasp — a dirty multimode filter (after the Doepfer A-124 VCF5)
//!
//! A 12 dB/oct state-variable filter with the EDP Wasp's CMOS rasp:
//! `dirt` drives a soft tanh stage around the SVF, `mix` sweeps
//! LP → notch → HP like the A-124's blend knob, and `bp` adds the
//! bandpass output alongside. At `dirt 0` it's a clean SEM-ish SVF;
//! cranked, it's the cheap-chip snarl the Wasp is loved for.
//!
//! Consumes one claimed input (the fx convention); publishes an
//! envelope follower of its output as `wasp/N/env`.

#[allow(
    clippy::all,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    unused_parens,
    unused_variables,
    unused_mut,
    dead_code
)]
pub mod core {
    use crate::faust::*;
    include!("wasp/wasp_gen.rs");
}

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
use crate::shm::{AudioRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

// ParamIndex order is alphabetical for a flat UI; pinned by test.
const P_DIRT: i32 = 0;
const P_FREQ: i32 = 1;
const P_MIX: i32 = 2;
const P_RES: i32 = 3;

const FALLBACK_RATE: f32 = 48_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Freq,
    Res,
    Mix,
    Dirt,
    Bp,
    Dry,
    Input,
}

const ROWS: [Row; 7] = [
    Row::Freq,
    Row::Res,
    Row::Mix,
    Row::Dirt,
    Row::Bp,
    Row::Dry,
    Row::Input,
];

/// Bindable rows, srcs[] order.
const BINDABLE: [Row; 4] = [Row::Freq, Row::Res, Row::Mix, Row::Dirt];
const N_SRC: usize = 4;
const INPUT_SLOT: usize = 6;

struct WaspState {
    freq: f32,
    res: f32,
    mix: f32,
    dirt: f32,
    bp: f32,
    dry: f32,
    input: Option<String>,
    input_live: bool,
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    env_now: f32,
    selected: usize,
}

impl WaspState {
    fn new() -> Self {
        Self {
            freq: 0.55,
            res: 0.3,
            mix: 0.0,
            dirt: 0.5,
            bp: 0.0,
            dry: 0.0,
            input: None,
            input_live: true,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.55, 0.3, 0.0, 0.5],
            env_now: 0.0,
            selected: 0,
        }
    }

    fn get(&self, r: Row) -> f32 {
        match r {
            Row::Freq => self.freq,
            Row::Res => self.res,
            Row::Mix => self.mix,
            Row::Dirt => self.dirt,
            Row::Bp => self.bp,
            Row::Dry => self.dry,
            Row::Input => 0.0,
        }
    }

    fn set(&mut self, r: Row, v: f32) {
        let v = v.clamp(0.0, 1.0);
        match r {
            Row::Freq => self.freq = v,
            Row::Res => self.res = v,
            Row::Mix => self.mix = v,
            Row::Dirt => self.dirt = v,
            Row::Bp => self.bp = v,
            Row::Dry => self.dry = v,
            Row::Input => {}
        }
    }
}

const SRC_SLOT_BASE: usize = 10;

impl crate::undo::ParamUndo for WaspState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            let s = self.srcs.get(i)?;
            return Some(V::Src(s.as_ref().map(|a| a.to_string())));
        }
        let r = *ROWS.get(slot)?;
        Some(match r {
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
            (Row::Input, V::Src(v)) => self.input = v,
            (_, V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &WaspState) -> state::WaspParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::WaspParams {
        format: state::STATE_FORMAT,
        freq: Some(s.freq),
        res: Some(s.res),
        mix: Some(s.mix),
        dirt: Some(s.dirt),
        bp: Some(s.bp),
        dry: Some(s.dry),
        input: s.input.clone(),
        freq_src: src(0),
        res_src: src(1),
        mix_src: src(2),
        dirt_src: src(3),
    }
}

fn apply_params(s: &mut WaspState, p: &state::WaspParams) {
    if let Some(v) = p.freq {
        s.set(Row::Freq, v);
    }
    if let Some(v) = p.res {
        s.set(Row::Res, v);
    }
    if let Some(v) = p.mix {
        s.set(Row::Mix, v);
    }
    if let Some(v) = p.dirt {
        s.set(Row::Dirt, v);
    }
    if let Some(v) = p.bp {
        s.set(Row::Bp, v);
    }
    if let Some(v) = p.dry {
        s.set(Row::Dry, v);
    }
    if p.input.is_some() {
        s.input = p.input.clone();
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.freq_src),
        parse(&p.res_src),
        parse(&p.mix_src),
        parse(&p.dirt_src),
    ];
    s.resolved = Default::default();
}

// ── audio thread ───────────────────────────────────────────────────────────

fn audio_thread(shared: Arc<Mutex<WaspState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_wasp_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating wasp ring")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("wasp", instance, Some(&shm_name), 1)?;
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
    let mut mono = vec![0.0_f32; slot_frames];
    let mut out_mix = vec![0.0_f32; slot_frames];
    let mut out_bp = vec![0.0_f32; slot_frames];

    let mut fx = core::Wasp::new();
    fx.init(sample_rate as i32);

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
                fx = core::Wasp::new();
                fx.init(sample_rate as i32);
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
            mono[f] = 0.5 * (block[f * channels] + block[f * channels + 1]);
        }

        let (bp, dry) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            // max/min, not clamp: clamp(NaN) is NaN and a stale modbus
            // value must die here (the swarm lesson, pinned there by test)
            #[allow(clippy::manual_clamp)]
            let eff = |k: usize, manual: f32, s: &WaspState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let vals = [
                eff(0, s.freq, &s),
                eff(1, s.res, &s),
                eff(2, s.mix, &s),
                eff(3, s.dirt, &s),
            ];
            s.eff = vals;
            s.env_now = follower;
            fx.set_param(crate::faust::ParamIndex(P_FREQ), vals[0]);
            fx.set_param(crate::faust::ParamIndex(P_RES), vals[1]);
            fx.set_param(crate::faust::ParamIndex(P_MIX), vals[2]);
            fx.set_param(crate::faust::ParamIndex(P_DIRT), vals[3]);
            (s.bp, s.dry)
        };

        {
            let ins = [&mono[..]];
            let mut outs = [&mut out_mix[..], &mut out_bp[..]];
            fx.compute(slot_frames, &ins, &mut outs);
        }

        for f in 0..slot_frames {
            let wet = out_mix[f] * (1.0 - bp) + out_bp[f] * bp;
            let v = wet + mono[f] * dry;
            follower = follower.max(v.abs()) * 0.9995;
            block[f * channels] = v;
            if channels > 1 {
                block[f * channels + 1] = v;
            }
        }
        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            fx = core::Wasp::new();
            fx.init(sample_rate as i32);
        }
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            bus.set(base, follower.clamp(0.0, 1.0));
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
        Row::Freq => "freq",
        Row::Res => "res",
        Row::Mix => "mix",
        Row::Dirt => "dirt",
        Row::Bp => "bp",
        Row::Dry => "dry",
        Row::Input => "input",
    }
}

fn src_index(r: Row) -> Option<usize> {
    BINDABLE.iter().position(|b| *b == r)
}

fn row_text(s: &WaspState, r: Row) -> String {
    match r {
        Row::Freq => {
            let hz = 30.0 * 400.0_f32.powf(s.freq);
            if hz >= 1000.0 {
                format!("{:.1} kHz", hz / 1000.0)
            } else {
                format!("{:.0} Hz", hz)
            }
        }
        Row::Res => format!("{:.0}%", s.res * 100.0),
        Row::Mix => {
            if s.mix < 0.05 {
                "LP".into()
            } else if (s.mix - 0.5).abs() < 0.05 {
                "notch".into()
            } else if s.mix > 0.95 {
                "HP".into()
            } else {
                format!("LP {:.0}·{:.0} HP", (1.0 - s.mix) * 100.0, s.mix * 100.0)
            }
        }
        Row::Dirt => format!("{:.0}%", s.dirt * 100.0),
        Row::Bp => format!("{:.0}%", s.bp * 100.0),
        Row::Dry => format!("{:.0}%", s.dry * 100.0),
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
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &WaspState,
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
        lines.push(theme::header("WASP", &format!("vcf {}", instance), "", w));
        lines.push(Line::from(vec![
            Span::styled("  out ".to_string(), theme::chrome()),
            Span::styled(
                theme::meter_char(s.env_now).to_string(),
                theme::signal(theme::cv_ramp(s.env_now)),
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
                vec![Span::styled(format!(" {:<6}", row_label(*r)), label_style)];
            let bound = src_index(*r).is_some_and(|k| s.srcs[k].is_some());
            let hue = src_index(*r)
                .and_then(|k| s.srcs[k].as_ref())
                .map(|a| routing::cable_color(entries, a));
            if *r != Row::Input {
                let shown = match src_index(*r) {
                    Some(k) if bound => s.eff[k],
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
                Line::from("━━━ WASP · dirty multimode VCF (after the A-124) ━━━"),
                Line::from(""),
                Line::from("  j/k h/l    Rows / adjust (H/L coarse)"),
                Line::from("  freq·res   The SVF; res to the edge of scream"),
                Line::from("  mix        LP → notch → HP (the A-124 blend)"),
                Line::from("  dirt       The CMOS rasp; 0 = clean SEM-ish"),
                Line::from("  bp         Blends the bandpass output in"),
                Line::from("  @ / x      Bind / unbind · input row picks source"),
                Line::from(""),
                Line::from("Publishes wasp/N/env (output follower)."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" WASP ", theme::chrome_hi())),
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
    state::write_pid_file("wasp", instance);

    let shared = Arc::new(Mutex::new(WaspState::new()));
    if let Ok(p) = state::load_module_state::<state::WaspParams>("wasp", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let audio_builder = thread::Builder::new()
        .name(String::from("wasp-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = audio_builder.spawn(move || {
        if let Err(e) = audio_thread(audio_state, instance) {
            eprintln!("[wasp {}] audio thread error: {}", instance, e);
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
            let _ = state::save_module_state("wasp", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::WaspParams>("wasp", instance) {
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
                    if r == Row::Input {
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
                    ExCommand::Edit(name) => match state::load_patch::<state::WaspParams>(&name) {
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
            let _ = state::save_module_state("wasp", instance, &params);
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
                if r == Row::Input {
                    ex_msg = Some("input: @ patches, x unpatches".into());
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
                let slot = s.selected;
                let old = s.get_param(slot);
                let def = WaspState::new();
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
                        .filter(|e| !(e.module_name == "wasp" && e.instance == instance))
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
                    ex_msg = Some(format!("{} is not bindable", row_label(r)));
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
    s: &mut WaspState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    let Some(slot) = ROWS.iter().position(|r| row_label(*r) == key) else {
        return format!("Unknown setting: {key} (freq res mix dirt bp dry input)");
    };
    let r = ROWS[slot];
    let parsed: Result<V, String> = if r == Row::Input {
        if value == "-" {
            Ok(V::Src(None))
        } else {
            Ok(V::Src(Some(value.to_string())))
        }
    } else {
        value
            .parse::<f32>()
            .map(V::F32)
            .map_err(|_| format!("{key}: not a number: {value}"))
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
    fn core_params_pinned_and_filter_filters() {
        let mut map = crate::faust::ParamMap::default();
        core::Wasp::build_user_interface_static(&mut map);
        let idx = |name: &str| {
            map.params
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, i, _)| *i)
                .unwrap_or(-1)
        };
        assert_eq!(idx("dirt"), P_DIRT);
        assert_eq!(idx("freq"), P_FREQ);
        assert_eq!(idx("mix"), P_MIX);
        assert_eq!(idx("res"), P_RES);
        assert_eq!(core::FAUST_INPUTS, 1);
        assert_eq!(core::FAUST_OUTPUTS, 2);

        // noise through a closed vs open LP must differ in energy
        let render = |freq: f32| -> f32 {
            let mut fx = core::Wasp::new();
            fx.init(48_000);
            fx.set_param(crate::faust::ParamIndex(P_FREQ), freq);
            fx.set_param(crate::faust::ParamIndex(P_MIX), 0.0);
            fx.set_param(crate::faust::ParamIndex(P_RES), 0.2);
            fx.set_param(crate::faust::ParamIndex(P_DIRT), 0.0);
            let mut e = 0.0;
            let mut state = 0x12345u32;
            for _ in 0..200 {
                let inp: Vec<f32> = (0..64)
                    .map(|_| {
                        state = state.wrapping_mul(1103515245).wrapping_add(12345);
                        (state >> 16) as f32 / 32768.0 - 1.0
                    })
                    .collect();
                let mut a = vec![0.0; 64];
                let mut b = vec![0.0; 64];
                let ins = [&inp[..]];
                let mut outs = [&mut a[..], &mut b[..]];
                fx.compute(64, &ins, &mut outs);
                e += a.iter().map(|s| s * s).sum::<f32>();
            }
            e
        };
        let closed = render(0.05);
        let open = render(0.95);
        assert!(open.is_finite() && closed.is_finite());
        assert!(
            closed < open * 0.5,
            "LP attenuates noise: closed {closed} open {open}"
        );
    }

    #[test]
    fn dirt_stays_bounded_at_full_scream() {
        let mut fx = core::Wasp::new();
        fx.init(48_000);
        fx.set_param(crate::faust::ParamIndex(P_FREQ), 0.9);
        fx.set_param(crate::faust::ParamIndex(P_DIRT), 1.0);
        fx.set_param(crate::faust::ParamIndex(P_RES), 1.0);
        let mut peak = 0.0_f32;
        for i in 0..400 {
            let inp: Vec<f32> = (0..64)
                .map(|k| (((i * 64 + k) as f32) * 0.02).sin() * 0.9)
                .collect();
            let mut a = vec![0.0; 64];
            let mut b = vec![0.0; 64];
            let ins = [&inp[..]];
            let mut outs = [&mut a[..], &mut b[..]];
            fx.compute(64, &ins, &mut outs);
            peak = a.iter().chain(b.iter()).fold(peak, |m, s| m.max(s.abs()));
        }
        assert!(peak.is_finite() && peak < 3.0, "dirt stays bounded: {peak}");
        assert!(peak > 0.1, "dirt didn't kill the signal: {peak}");
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = WaspState::new();
        s.freq = 0.7;
        s.dirt = 0.9;
        s.input = Some("sampler/0".into());
        s.srcs[0] = SourceAddr::parse("envelope/0/ch1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::WaspParams = toml::from_str(&toml).expect("parses");
        let mut s2 = WaspState::new();
        apply_params(&mut s2, &back);
        assert!((s2.freq - 0.7).abs() < 1e-6);
        assert!((s2.dirt - 0.9).abs() < 1e-6);
        assert_eq!(s2.input.as_deref(), Some("sampler/0"));
        assert_eq!(
            s2.srcs[0].as_ref().map(|a| a.to_string()),
            Some("envelope/0/ch1".into())
        );
    }

    #[test]
    fn ex_set_parses() {
        let mut s = WaspState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "freq", "0.8").contains("Hz"));
        assert!(ex_set(&mut s, &mut h, "input", "voice/0").contains("voice/0"));
        assert!(ex_set(&mut s, &mut h, "wow", "1").contains("Unknown"));
    }
}
