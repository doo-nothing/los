//! # Branches — the Mutable Instruments dual Bernoulli gate, as a
//! note router
//!
//! Ported from pichenettes/eurorack (branches/branches.cc, MIT,
//! copyright 2012 Emilie Gillet, attribution preserved). The firmware
//! logic is kept exactly: the Galois LFSR, the 16-bit threshold
//! comparison with its `!= 65535` guard, the linear ADC table's dead
//! zones at both ends, toggle mode's XOR with the previous outcome,
//! and latch mode's held gates.
//!
//! In los each channel CONSUMES a note track and RE-EMITS every
//! note-on onto one of two outputs (a coin flip per the probability
//! knob); voices bind `branches/N/1a` etc. as their `notes_src`. The
//! four outputs also publish as gate levels on the modulation bus, so
//! they double as triggers for the dld/lfo-style edge inputs.

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
use crate::shm::{AudioEvent, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

/// Note-source id base for re-emitted events: well above the
/// sequencer band (instance*8 + track ≤ ~200 would need 25
/// sequencers). Channel outputs are BASE + instance*4 + {0..3}.
pub const NOTE_SOURCE_BASE: u8 = 200;

// ── the firmware core ──────────────────────────────────────────────────────

/// branches.cc Galois LFSR (the commented-out LCG stays commented).
#[derive(Debug, Clone)]
pub struct Lfsr {
    state: u32,
}

impl Lfsr {
    pub fn new(seed: u32) -> Self {
        Lfsr { state: seed.max(1) }
    }

    #[allow(clippy::should_implement_trait)] // the firmware's name
    #[inline]
    pub fn next(&mut self) -> u32 {
        let s = self.state;
        self.state = (s >> 1) ^ ((s & 1).wrapping_neg() & 0xD000_0001);
        self.state
    }
}

/// The ADC linear table's law: a 256-entry ramp with `table[0] =
/// table[1] = 0` and the top two entries pinned to 65535 — dead zones
/// at both extremes so the knob's ends are deterministic.
pub fn linear_threshold(p: f32) -> u16 {
    let idx = (p.clamp(0.0, 1.0) * 255.0) as u32;
    if idx <= 1 {
        return 0;
    }
    if idx >= 254 {
        return 65535;
    }
    // table[k] = round((k-1) * 65535 / 253) for the interior
    (((idx - 1) as u64 * 65535) / 253).min(65535) as u16
}

/// One Bernoulli channel, firmware-exact.
#[derive(Debug, Clone)]
pub struct Bernoulli {
    pub toggle: bool,
    pub latch: bool,
    previous_state: bool,
    lfsr: Lfsr,
}

impl Bernoulli {
    pub fn new(seed: u32) -> Self {
        Bernoulli {
            toggle: false,
            latch: false,
            previous_state: false,
            lfsr: Lfsr::new(seed),
        }
    }

    /// The coin flip on a rising edge: returns `true` for output A.
    /// `outcome = random >= threshold && threshold != 65535`, XOR'd
    /// with the previous outcome in toggle mode (branches.cc).
    pub fn flip(&mut self, p: f32) -> bool {
        let random = (self.lfsr.next() & 0xffff) as u16;
        let threshold = linear_threshold(p);
        let mut outcome = random >= threshold && threshold != 65535;
        if self.toggle {
            outcome ^= self.previous_state;
        }
        self.previous_state = outcome;
        outcome
    }
}

// ── module state ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    P1,
    Toggle1,
    Latch1,
    Notes1,
    P2,
    Toggle2,
    Latch2,
    Notes2,
}

const ROWS: [Row; 8] = [
    Row::P1,
    Row::Toggle1,
    Row::Latch1,
    Row::Notes1,
    Row::P2,
    Row::Toggle2,
    Row::Latch2,
    Row::Notes2,
];

/// CV bank — both probability knobs take cables.
const BINDABLE: [Row; 2] = [Row::P1, Row::P2];
const N_SRC: usize = BINDABLE.len();

struct BranchesState {
    p: [f32; 2],
    toggle: [bool; 2],
    latch: [bool; 2],
    notes_src: [Option<SourceAddr>; 2],
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    /// Live outcome display: -1 none, 0 = a, 1 = b per channel.
    last_out: [i8; 2],
    selected: usize,
}

impl BranchesState {
    fn new() -> Self {
        BranchesState {
            p: [0.5, 0.5],
            toggle: [false; 2],
            latch: [false; 2],
            notes_src: [None, None],
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.5; N_SRC],
            last_out: [-1; 2],
            selected: 0,
        }
    }

    fn get(&self, r: Row) -> f32 {
        match r {
            Row::P1 => self.p[0],
            Row::P2 => self.p[1],
            _ => 0.0,
        }
    }

    fn set(&mut self, r: Row, v: f32) {
        let v = v.clamp(0.0, 1.0);
        match r {
            Row::P1 => self.p[0] = v,
            Row::P2 => self.p[1] = v,
            _ => {}
        }
    }
}

const SRC_SLOT_BASE: usize = 50;
const NOTES1_SLOT: usize = 41;
const NOTES2_SLOT: usize = 42;

impl crate::undo::ParamUndo for BranchesState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        match slot {
            NOTES1_SLOT => {
                return Some(V::Src(self.notes_src[0].as_ref().map(|a| a.to_string())))
            }
            NOTES2_SLOT => {
                return Some(V::Src(self.notes_src[1].as_ref().map(|a| a.to_string())))
            }
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
            Row::P1 | Row::P2 => V::F32(self.get(r)),
            Row::Toggle1 => V::Bool(self.toggle[0]),
            Row::Toggle2 => V::Bool(self.toggle[1]),
            Row::Latch1 => V::Bool(self.latch[0]),
            Row::Latch2 => V::Bool(self.latch[1]),
            Row::Notes1 | Row::Notes2 => return None,
        })
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        let parse = |v: &Option<String>| v.as_deref().and_then(SourceAddr::parse);
        match (slot, &value) {
            (NOTES1_SLOT, V::Src(v)) => {
                self.notes_src[0] = parse(v);
                return;
            }
            (NOTES2_SLOT, V::Src(v)) => {
                self.notes_src[1] = parse(v);
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
            (Some(Row::Toggle1), V::Bool(v)) => self.toggle[0] = v,
            (Some(Row::Toggle2), V::Bool(v)) => self.toggle[1] = v,
            (Some(Row::Latch1), V::Bool(v)) => self.latch[0] = v,
            (Some(Row::Latch2), V::Bool(v)) => self.latch[1] = v,
            (Some(r), V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &BranchesState) -> state::BranchesParams {
    state::BranchesParams {
        format: state::STATE_FORMAT,
        p1: Some(s.p[0]),
        p2: Some(s.p[1]),
        toggle1: Some(s.toggle[0]),
        toggle2: Some(s.toggle[1]),
        latch1: Some(s.latch[0]),
        latch2: Some(s.latch[1]),
        p1_src: s.srcs[0].as_ref().map(|a| a.to_string()),
        p2_src: s.srcs[1].as_ref().map(|a| a.to_string()),
        notes1_src: s.notes_src[0].as_ref().map(|a| a.to_string()),
        notes2_src: s.notes_src[1].as_ref().map(|a| a.to_string()),
    }
}

fn apply_params(s: &mut BranchesState, p: &state::BranchesParams) {
    if let Some(v) = p.p1 {
        s.p[0] = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.p2 {
        s.p[1] = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.toggle1 {
        s.toggle[0] = v;
    }
    if let Some(v) = p.toggle2 {
        s.toggle[1] = v;
    }
    if let Some(v) = p.latch1 {
        s.latch[0] = v;
    }
    if let Some(v) = p.latch2 {
        s.latch[1] = v;
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [parse(&p.p1_src), parse(&p.p2_src)];
    s.notes_src = [parse(&p.notes1_src), parse(&p.notes2_src)];
    s.resolved = Default::default();
}

// ── engine thread ──────────────────────────────────────────────────────────

fn engine_thread(shared: Arc<Mutex<BranchesState>>, instance: usize) -> Result<()> {
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    // four claims: the a/b gate levels of both channels on the bus
    manifest.register("branches", instance, None, 4)?;
    let mod_base = manifest.claimed_base();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();
    let mut events = EventRingbuf::open_dynamic().ok();
    let mut producer = EventRingbuf::open_producer().ok();

    let base = NOTE_SOURCE_BASE.saturating_add((instance as u8) << 2);
    let mut bernoulli = [
        Bernoulli::new(0xb4a9_c001 ^ instance as u32),
        Bernoulli::new(0x5eed_2002 ^ instance as u32),
    ];
    // which side the CURRENT note of each channel went to, so the
    // note-off follows it
    let mut active_side: [Option<bool>; 2] = [None, None];
    let mut note_filter: [Option<u8>; 2] = [None, None];
    let mut gate_level: [[f32; 2]; 2] = [[0.0; 2]; 2];
    let mut ticks: u64 = 0;

    loop {
        if ticks.is_multiple_of(64) {
            if events.is_none() {
                events = EventRingbuf::open_dynamic().ok();
            }
            if producer.is_none() {
                producer = EventRingbuf::open_producer().ok();
            }
            let entries = manifest.entries();
            let mut s = shared.lock().unwrap();
            for k in 0..N_SRC {
                s.resolved[k] = s.srcs[k]
                    .as_ref()
                    .and_then(|a| routing::resolve(&entries, a));
            }
            #[allow(clippy::needless_range_loop)] // c strides
            // parallel per-channel arrays
            for c in 0..2 {
                note_filter[c] = s.notes_src[c].as_ref().and_then(routing::note_source_track);
            }
            let mask = s
                .resolved
                .iter()
                .flatten()
                .filter(|&&c| c < 64)
                .fold(0u64, |m, &c| m | (1 << c));
            let notes = note_filter
                .iter()
                .flatten()
                .filter(|&&t| t < 8)
                .fold(0u8, |m, &t| m | (1 << t));
            manifest.publish_consumes(mask, notes);
        }

        let (p, toggle, latch) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            // max/min, not clamp: NaN from a stale channel dies here
            #[allow(clippy::manual_clamp)]
            let cv = |k: usize, manual: f32, s: &BranchesState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => b.get(ch).max(0.0).min(1.0),
                    _ => manual,
                }
            };
            let p = [cv(0, s.p[0], &s), cv(1, s.p[1], &s)];
            s.eff = p;
            (p, s.toggle, s.latch)
        };
        for c in 0..2 {
            bernoulli[c].toggle = toggle[c];
            bernoulli[c].latch = latch[c];
        }

        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                for c in 0..2 {
                    let Some(t) = note_filter[c] else { continue };
                    if event.source != t {
                        continue;
                    }
                    if event.is_note_on() {
                        let outcome = bernoulli[c].flip(p[c]);
                        // outcome=true is the firmware's GateOn(A)
                        let side = usize::from(!outcome);
                        let out_id = base + (c as u8) * 2 + side as u8;
                        if let Some(ref mut pr) = producer {
                            let _ = pr.write_event(&AudioEvent::note_on_hz(
                                event.value,
                                event.param,
                                out_id,
                                event.step,
                            ));
                        }
                        // latch: the previous side's gate keeps ringing
                        if !latch[c] {
                            if let Some(prev) = active_side[c] {
                                let prev_side = usize::from(!prev);
                                gate_level[c][prev_side] = 0.0;
                            }
                        }
                        active_side[c] = Some(outcome);
                        gate_level[c][side] = 1.0;
                        let mut s = shared.lock().unwrap();
                        s.last_out[c] = side as i8;
                    } else if event.is_note_off() {
                        if let Some(outcome) = active_side[c] {
                            let side = usize::from(!outcome);
                            let out_id = base + (c as u8) * 2 + side as u8;
                            if let Some(ref mut pr) = producer {
                                let _ = pr.write_event(&AudioEvent::note_off_source(
                                    0, out_id, event.step,
                                ));
                            }
                            if !latch[c] {
                                gate_level[c][side] = 0.0;
                            }
                        }
                    }
                }
            }
        }

        if let (Some(b), Some(bus)) = (mod_base, modbus.as_mut()) {
            #[allow(clippy::needless_range_loop)] // c strides
            // parallel per-channel arrays
            for c in 0..2 {
                bus.set(b + c * 2, gate_level[c][0]);
                bus.set(b + c * 2 + 1, gate_level[c][1]);
            }
        }

        ticks += 1;
        thread::sleep(Duration::from_millis(1));
    }
}

// ── ui ─────────────────────────────────────────────────────────────────────

fn row_label(r: Row) -> &'static str {
    match r {
        Row::P1 => "p 1",
        Row::Toggle1 => "toggle 1",
        Row::Latch1 => "latch 1",
        Row::Notes1 => "notes 1",
        Row::P2 => "p 2",
        Row::Toggle2 => "toggle 2",
        Row::Latch2 => "latch 2",
        Row::Notes2 => "notes 2",
    }
}

fn src_index(r: Row) -> Option<usize> {
    BINDABLE.iter().position(|b| *b == r)
}

fn binding_slot(r: Row) -> Option<usize> {
    match r {
        Row::Notes1 => Some(NOTES1_SLOT),
        Row::Notes2 => Some(NOTES2_SLOT),
        _ => src_index(r).map(|i| SRC_SLOT_BASE + i),
    }
}

fn row_text(s: &BranchesState, r: Row) -> String {
    match r {
        Row::P1 | Row::P2 => format!("{:.0}% → b", s.get(r) * 100.0),
        Row::Toggle1 => if s.toggle[0] { "on" } else { "off" }.into(),
        Row::Toggle2 => if s.toggle[1] { "on" } else { "off" }.into(),
        Row::Latch1 => if s.latch[0] { "on" } else { "off" }.into(),
        Row::Latch2 => if s.latch[1] { "on" } else { "off" }.into(),
        Row::Notes1 => s.notes_src[0]
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(unbound — silent)".into()),
        Row::Notes2 => s.notes_src[1]
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(unbound — silent)".into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &BranchesState,
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
            "BRANCHES",
            &format!("bernoulli {}", instance),
            "",
            w,
        ));
        let side = |v: i8| match v {
            0 => "→a",
            1 => "→b",
            _ => "  ",
        };
        lines.push(Line::from(vec![
            Span::styled("  last ".to_string(), theme::chrome()),
            Span::styled(side(s.last_out[0]).to_string(), theme::value()),
            Span::styled("  ".to_string(), theme::chrome()),
            Span::styled(side(s.last_out[1]).to_string(), theme::value()),
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
                || (*r == Row::Notes1 && s.notes_src[0].is_some())
                || (*r == Row::Notes2 && s.notes_src[1].is_some());
            let hue = src_index(*r)
                .and_then(|k| s.srcs[k].as_ref())
                .or(match r {
                    Row::Notes1 => s.notes_src[0].as_ref(),
                    Row::Notes2 => s.notes_src[1].as_ref(),
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
                Line::from("━━━ BRANCHES · Bernoulli gate (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  p          coin bias: 0% all → a, 100% all → b"),
                Line::from("  toggle     heads flips the previous outcome"),
                Line::from("  latch      gates hold until the next flip"),
                Line::from("  notes      the track this channel routes"),
                Line::from(""),
                Line::from("Each note-on re-emits on branches/N/1a·1b·2a·2b —"),
                Line::from("bind a voice's notes to one side. The four outputs"),
                Line::from("also sit on the bus as gate levels (dld hold, lfo"),
                Line::from("reset, anything with a trigger input)."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" BRANCHES ", theme::chrome_hi())),
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
    state::write_pid_file("branches", instance);

    let shared = Arc::new(Mutex::new(BranchesState::new()));
    if let Ok(p) = state::load_module_state::<state::BranchesParams>("branches", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let engine_state = Arc::clone(&shared);
    let builder = thread::Builder::new().name(String::from("branches-engine"));
    let _ = builder.spawn(move || {
        // black box: a dead engine thread must leave a trace
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine_thread(engine_state, instance)
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
        eprintln!("[branches {instance}] engine thread died: {msg}");
        let path = crate::state::tmp_dir().join(format!("branches_{instance}.crash"));
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
            let _ = state::save_module_state("branches", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::BranchesParams>("branches", instance)
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
            if let MouseEventKind::Down(_) = m.kind {
                let row = (m.row as usize).saturating_sub(3);
                if row < ROWS.len() {
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
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                if let Some(slot) = binding_slot(r) {
                    let old = s.get_param(slot);
                    let text = addr.as_ref().map(|a| a.to_string());
                    match r {
                        Row::Notes1 => s.notes_src[0] = addr.clone(),
                        Row::Notes2 => s.notes_src[1] = addr.clone(),
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
                        match state::load_patch::<state::BranchesParams>(&name) {
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
            let _ = state::save_module_state("branches", instance, &params);
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
                let old = s.get_param(slot);
                match r {
                    Row::P1 | Row::P2 => {
                        let v = step_f32(s.get(r), steps, 0.01, coarse, 0.0, 1.0);
                        s.set(r, v);
                    }
                    Row::Toggle1 => s.toggle[0] = !s.toggle[0],
                    Row::Toggle2 => s.toggle[1] = !s.toggle[1],
                    Row::Latch1 => s.latch[0] = !s.latch[0],
                    Row::Latch2 => s.latch[1] = !s.latch[1],
                    Row::Notes1 | Row::Notes2 => {
                        ex_msg = Some("routing row: @ binds, x unbinds".into());
                        continue;
                    }
                }
                let new = s.get_param(slot);
                if let (Some(old), Some(new)) = (old, new) {
                    history.record(slot, "Adjust", old, new);
                }
            }
            KeyCode::Char('@') => {
                count.clear();
                let s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                if binding_slot(r).is_some() {
                    let current = match r {
                        Row::Notes1 => s.notes_src[0].clone(),
                        Row::Notes2 => s.notes_src[1].clone(),
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
                        Row::Notes1 => s.notes_src[0].is_some(),
                        Row::Notes2 => s.notes_src[1].is_some(),
                        _ => src_index(r).is_some_and(|k| s.srcs[k].is_some()),
                    };
                    if had {
                        let old = s.get_param(slot);
                        match r {
                            Row::Notes1 => s.notes_src[0] = None,
                            Row::Notes2 => s.notes_src[1] = None,
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
    s: &mut BranchesState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    let on = matches!(value, "on" | "1" | "true");
    match key {
        "toggle1" => {
            s.toggle[0] = on;
            return format!("toggle1 = {}", s.toggle[0]);
        }
        "toggle2" => {
            s.toggle[1] = on;
            return format!("toggle2 = {}", s.toggle[1]);
        }
        "latch1" => {
            s.latch[0] = on;
            return format!("latch1 = {}", s.latch[0]);
        }
        "latch2" => {
            s.latch[1] = on;
            return format!("latch2 = {}", s.latch[1]);
        }
        "notes1" | "notes2" => {
            let slot = if key == "notes1" {
                NOTES1_SLOT
            } else {
                NOTES2_SLOT
            };
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
        _ => {}
    }
    let row = match key {
        "p1" => Row::P1,
        "p2" => Row::P2,
        _ => return format!("Unknown setting: {key}"),
    };
    match value.parse::<f32>() {
        Ok(v) => {
            s.set(row, v);
            format!("{key} = {:.0}%", s.get(row) * 100.0)
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

    #[test]
    fn threshold_law_has_the_dead_zones() {
        assert_eq!(linear_threshold(0.0), 0);
        assert_eq!(linear_threshold(0.004), 0, "bottom dead zone");
        assert_eq!(linear_threshold(1.0), 65535);
        assert_eq!(linear_threshold(0.999), 65535, "top dead zone");
        let mid = linear_threshold(0.5);
        assert!((mid as i32 - 32700).abs() < 700, "midpoint ~half: {mid}");
    }

    #[test]
    fn extremes_are_deterministic() {
        let mut b = Bernoulli::new(0x1234);
        for _ in 0..100 {
            assert!(b.flip(0.0), "p=0: always A");
        }
        for _ in 0..100 {
            assert!(!b.flip(1.0), "p=1: always B");
        }
    }

    #[test]
    fn midpoint_is_roughly_fair() {
        let mut b = Bernoulli::new(0xfeed);
        let heads = (0..10_000).filter(|_| b.flip(0.5)).count();
        assert!(
            (3500..=6500).contains(&heads),
            "fair-ish at the midpoint: {heads}"
        );
    }

    #[test]
    fn toggle_mode_alternates_on_heads() {
        let mut b = Bernoulli::new(0x42);
        b.toggle = true;
        // p=0 means the raw outcome is always "heads" (true); toggle
        // XORs with the previous outcome -> strict alternation
        let seq: Vec<bool> = (0..6).map(|_| b.flip(0.0)).collect();
        assert_eq!(seq, vec![true, false, true, false, true, false]);
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = BranchesState::new();
        s.p[0] = 0.7;
        s.toggle[1] = true;
        s.latch[0] = true;
        s.notes_src[0] = SourceAddr::parse("sequencer/0/t1");
        s.srcs[1] = SourceAddr::parse("lfo/0/s1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::BranchesParams = toml::from_str(&toml).expect("parses");
        let mut s2 = BranchesState::new();
        apply_params(&mut s2, &back);
        assert!((s2.p[0] - 0.7).abs() < 1e-6);
        assert!(s2.toggle[1]);
        assert!(s2.latch[0]);
        assert!(s2.notes_src[0].is_some());
        assert!(s2.srcs[1].is_some());
    }

    #[test]
    fn every_value_row_is_bindable() {
        use crate::undo::ParamUndo;
        use crate::undo::ParamValue as V;
        for r in [Row::P1, Row::P2] {
            let i = src_index(r).unwrap_or_else(|| panic!("{r:?} must be bindable"));
            let mut s = BranchesState::new();
            s.set_param(SRC_SLOT_BASE + i, V::Src(Some("lfo/0/a1".into())));
            assert!(s.srcs[i].is_some(), "{r:?} binds");
        }
    }
}
