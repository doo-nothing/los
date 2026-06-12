//! The sampler module: eight slots, the designer rows, the browser.
//!
//! Layout: a slot strip (a–h, the edit/load target), the selected
//! slot's designer rows, then globals (kit · notes · amp). `/` opens
//! the a-u.supply browser (Tab flips drums ↔ raw reels); search and
//! downloads run on a worker thread so the UI never blocks on the
//! network. Without a key (or built without `au-api`) the browser
//! lists the local cache instead.

use std::io;
use std::sync::mpsc;
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

use super::engine::{self, Engine, Mode, Reel, SlotParams};
use super::fetch::{self, Hit};
use crate::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

pub const NUM_SLOTS: usize = 8;
const FALLBACK_RATE: f32 = 48_000.0;

// ── rows ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Slot,
    Sample,
    Mode,
    Start,
    Len,
    Pitch,
    Speed,
    Gene,
    Slide,
    Atk,
    Dec,
    Level,
    Kit,
    Notes,
    Amp,
}

const ROWS: [Row; 15] = [
    Row::Slot,
    Row::Sample,
    Row::Mode,
    Row::Start,
    Row::Len,
    Row::Pitch,
    Row::Speed,
    Row::Gene,
    Row::Slide,
    Row::Atk,
    Row::Dec,
    Row::Level,
    Row::Kit,
    Row::Notes,
    Row::Amp,
];

/// Global mod-input bindings, srcs[] order (one set, Morphagene-style
/// single CV bank applied to whichever slot a voice plays).
const BINDABLE: [Row; 5] = [Row::Pitch, Row::Speed, Row::Gene, Row::Slide, Row::Level];
const N_SRC: usize = 5;

// ── shared state ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct SlotState {
    params: SlotParams,
    /// Cache path of the loaded sample (persisted).
    path: Option<String>,
    /// Display name.
    name: Option<String>,
}

struct SamplerState {
    slots: [SlotState; NUM_SLOTS],
    edit: usize,
    kit: bool,
    notes_src: Option<SourceAddr>,
    amp_src: Option<SourceAddr>,
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    amp_resolved: Option<usize>,
    /// UI: live engine envelope.
    env_now: f32,
    selected: usize,
    /// Reel-load requests for the audio thread: (slot, path).
    load_req: Vec<(usize, String)>,
    unload_req: Vec<usize>,
}

impl SamplerState {
    fn new() -> Self {
        Self {
            slots: Default::default(),
            edit: 0,
            kit: true,
            notes_src: None,
            amp_src: None,
            srcs: Default::default(),
            resolved: Default::default(),
            amp_resolved: None,
            env_now: 0.0,
            selected: 0,
            load_req: Vec::new(),
            unload_req: Vec::new(),
        }
    }
}

// ── persistence ────────────────────────────────────────────────────────────

fn snapshot_params(s: &SamplerState) -> state::SamplerParams {
    state::SamplerParams {
        format: state::STATE_FORMAT,
        kit: Some(s.kit),
        edit: Some(s.edit),
        notes_src: s.notes_src.as_ref().map(|a| a.to_string()),
        amp_src: s.amp_src.as_ref().map(|a| a.to_string()),
        pitch_src: s.srcs[0].as_ref().map(|a| a.to_string()),
        speed_src: s.srcs[1].as_ref().map(|a| a.to_string()),
        gene_src: s.srcs[2].as_ref().map(|a| a.to_string()),
        slide_src: s.srcs[3].as_ref().map(|a| a.to_string()),
        level_src: s.srcs[4].as_ref().map(|a| a.to_string()),
        slots: s
            .slots
            .iter()
            .map(|sl| state::SamplerSlotParams {
                sample: sl.path.clone(),
                mode: Some(sl.params.mode.name().to_string()),
                start: Some(sl.params.start),
                len: Some(sl.params.len),
                pitch: Some(sl.params.pitch),
                speed: Some(sl.params.speed),
                gene: Some(sl.params.gene),
                slide: Some(sl.params.slide),
                atk: Some(sl.params.atk),
                dec: Some(sl.params.dec),
                level: Some(sl.params.level),
            })
            .collect(),
    }
}

fn apply_params(s: &mut SamplerState, p: &state::SamplerParams) {
    if let Some(v) = p.kit {
        s.kit = v;
    }
    if let Some(v) = p.edit {
        s.edit = v.min(NUM_SLOTS - 1);
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.notes_src = parse(&p.notes_src);
    s.amp_src = parse(&p.amp_src);
    s.srcs = [
        parse(&p.pitch_src),
        parse(&p.speed_src),
        parse(&p.gene_src),
        parse(&p.slide_src),
        parse(&p.level_src),
    ];
    s.resolved = Default::default();
    for (i, sp) in p.slots.iter().enumerate().take(NUM_SLOTS) {
        let sl = &mut s.slots[i];
        if let Some(ref path) = sp.sample {
            sl.path = Some(path.clone());
            sl.name = std::path::Path::new(path)
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned());
            s.load_req.push((i, path.clone()));
        }
        let q = &mut sl.params;
        if let Some(ref m) = sp.mode {
            if let Some(m) = Mode::parse(m) {
                q.mode = m;
            }
        }
        macro_rules! f {
            ($field:ident, $lo:expr, $hi:expr) => {
                if let Some(v) = sp.$field {
                    q.$field = v.clamp($lo, $hi);
                }
            };
        }
        f!(start, 0.0, 1.0);
        f!(len, 0.01, 1.0);
        f!(pitch, -24.0, 24.0);
        f!(speed, -2.0, 2.0);
        f!(gene, 0.0, 1.0);
        f!(slide, 0.0, 1.0);
        f!(atk, 0.0, 1.0);
        f!(dec, 0.0, 1.0);
        f!(level, 0.0, 1.0);
    }
}

// ── audio thread ───────────────────────────────────────────────────────────

fn audio_thread(shared: Arc<Mutex<SamplerState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_sampler_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating sampler ring")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("sampler", instance, Some(&shm_name), 1)?;
    let mod_base = manifest.claimed_base();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();
    let consumer_id = crate::shm::consumer_id("sampler", instance);
    let mut events = EventRingbuf::open(consumer_id).ok();
    let transport = ShmTransport::open().ok();
    let sample_rate = transport
        .as_ref()
        .map(|t| t.sample_rate() as f32)
        .filter(|r| *r > 0.0)
        .unwrap_or(FALLBACK_RATE);

    let channels = ringbuf.channels() as usize;
    let slot_frames = ringbuf.slot_frames() as usize;
    let mut block = vec![0.0_f32; ringbuf.slot_len()];
    let mut mono = vec![0.0_f32; slot_frames];

    let mut eng = Engine::new(sample_rate);
    let mut reels: Vec<Option<Reel>> = (0..NUM_SLOTS).map(|_| None).collect();
    let mut note_filter: Option<u8> = None;
    let mut blocks: u64 = 0;

    loop {
        if blocks.is_multiple_of(128) {
            if events.is_none() {
                events = EventRingbuf::open(consumer_id).ok();
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
            // reel loads requested by the UI (decode OFF this thread? —
            // decode is afconvert+read, potentially slow; do it here but
            // only on the slow path, accepting a one-off block of lag on
            // explicit user action)
            let loads: Vec<(usize, String)> = s.load_req.drain(..).collect();
            let unloads: Vec<usize> = s.unload_req.drain(..).collect();
            drop(s);
            for slot in unloads {
                if slot < NUM_SLOTS {
                    reels[slot] = None;
                }
            }
            for (slot, path) in loads {
                if slot < NUM_SLOTS {
                    match fetch::load_reel(
                        std::path::Path::new(&path),
                        sample_rate,
                        engine::MAX_REEL_SECS,
                    ) {
                        Ok(r) => reels[slot] = Some(r),
                        Err(e) => eprintln!("[sampler] load failed: {e}"),
                    }
                }
            }
        }

        // note events
        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                if let Some(t) = note_filter {
                    if event.source != t {
                        continue;
                    }
                }
                let s = shared.lock().unwrap();
                let midi = if event.value > 0.0 {
                    (69.0 + 12.0 * (event.value / 440.0).log2()).round() as i32
                } else {
                    60
                };
                let midi = midi.clamp(0, 127) as u8;
                if event.is_note_on() {
                    let vel = event.param as f32 / 127.0;
                    let slot = if s.kit {
                        engine::kit_slot(midi)
                    } else {
                        Some(s.edit)
                    };
                    if let Some(slot) = slot {
                        let p = s.slots[slot].params;
                        let n = reels[slot].as_ref().map(|r| r.data.len()).unwrap_or(0);
                        eng.trigger(slot, midi, vel, &p, n);
                    }
                } else if event.is_note_off() {
                    let slot = if s.kit {
                        engine::kit_slot(midi)
                    } else {
                        Some(s.edit)
                    };
                    if let Some(slot) = slot {
                        eng.release(slot, midi);
                    }
                }
            }
        }

        // effective params: global mod bank overlays the slot knobs
        let (params, kit, amp) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            let get = |k: usize, s: &SamplerState| -> Option<f32> {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => Some(b.get(ch)),
                    _ => None,
                }
            };
            let overlay = [
                get(0, &s).map(|v| v.clamp(0.0, 1.0) * 48.0 - 24.0),
                get(1, &s).map(|v| v.clamp(0.0, 1.0) * 4.0 - 2.0),
                get(2, &s).map(|v| v.clamp(0.0, 1.0)),
                get(3, &s).map(|v| v.clamp(0.0, 1.0)),
                get(4, &s).map(|v| v.clamp(0.0, 1.0)),
            ];
            let params: Vec<SlotParams> = s
                .slots
                .iter()
                .map(|sl| {
                    let mut p = sl.params;
                    if let Some(v) = overlay[0] {
                        p.pitch = v;
                    }
                    if let Some(v) = overlay[1] {
                        p.speed = v;
                    }
                    if let Some(v) = overlay[2] {
                        p.gene = v;
                    }
                    if let Some(v) = overlay[3] {
                        p.slide = v;
                    }
                    if let Some(v) = overlay[4] {
                        p.level = v;
                    }
                    p
                })
                .collect();
            let amp = match (s.amp_src.is_some(), s.amp_resolved, bus) {
                (false, _, _) => 1.0,
                (true, Some(ch), Some(b)) => b.get(ch).clamp(0.0, 1.0),
                (true, _, _) => 0.0,
            };
            s.env_now = eng.env_out;
            (params, s.kit, amp)
        };

        mono.iter_mut().for_each(|v| *v = 0.0);
        eng.process(&mut mono, &reels, &params, kit);

        for f in 0..slot_frames {
            let v = mono[f] * amp;
            block[f * channels] = v;
            if channels > 1 {
                block[f * channels + 1] = v;
            }
        }
        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            eng = Engine::new(sample_rate);
        }
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            bus.set(base, eng.env_out);
        }
        while ringbuf.write(&block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }
        blocks += 1;
    }
}

// ── browser ────────────────────────────────────────────────────────────────

enum BrowseMsg {
    Results(Vec<Hit>),
    Fetched(usize, String, String), // slot, path, name
    Error(String),
}

#[derive(Default)]
struct Browser {
    open: bool,
    query: String,
    typing: bool,
    raw: bool,
    hits: Vec<Hit>,
    local: Vec<(std::path::PathBuf, String)>,
    sel: usize,
    busy: bool,
    status: String,
}

impl Browser {
    fn rows(&self) -> Vec<String> {
        if !self.online() {
            self.local
                .iter()
                .map(|(_, n)| format!("cache  {n}"))
                .collect()
        } else {
            self.hits.iter().map(|h| h.row()).collect()
        }
    }

    fn online(&self) -> bool {
        fetch::key().is_some()
    }
}

// ── ui helpers ─────────────────────────────────────────────────────────────

fn row_label(r: Row) -> &'static str {
    match r {
        Row::Slot => "slot",
        Row::Sample => "sample",
        Row::Mode => "mode",
        Row::Start => "start",
        Row::Len => "len",
        Row::Pitch => "pitch",
        Row::Speed => "speed",
        Row::Gene => "gene",
        Row::Slide => "slide",
        Row::Atk => "atk",
        Row::Dec => "dec",
        Row::Level => "level",
        Row::Kit => "kit",
        Row::Notes => "notes",
        Row::Amp => "amp",
    }
}

fn src_index(r: Row) -> Option<usize> {
    BINDABLE.iter().position(|b| *b == r)
}

fn slot_letter(i: usize) -> char {
    (b'a' + i as u8) as char
}

fn adjust(s: &mut SamplerState, steps: i32, coarse: bool) {
    use crate::keys::step_f32;
    let e = s.edit;
    let p = &mut s.slots[e].params;
    match ROWS[s.selected.min(ROWS.len() - 1)] {
        Row::Slot => s.edit = crate::keys::cycle(s.edit, steps, NUM_SLOTS),
        Row::Sample => {}
        Row::Mode => p.mode = p.mode.cycle(steps),
        Row::Start => p.start = step_f32(p.start, steps, 0.01, coarse, 0.0, 1.0),
        Row::Len => p.len = step_f32(p.len, steps, 0.01, coarse, 0.01, 1.0),
        Row::Pitch => {
            let st = if coarse { 12.0 } else { 1.0 };
            p.pitch = (p.pitch + steps as f32 * st).clamp(-24.0, 24.0);
        }
        Row::Speed => {
            let st = if coarse { 0.25 } else { 0.05 };
            p.speed = (p.speed + steps as f32 * st).clamp(-2.0, 2.0);
        }
        Row::Gene => p.gene = step_f32(p.gene, steps, 0.01, coarse, 0.0, 1.0),
        Row::Slide => p.slide = step_f32(p.slide, steps, 0.01, coarse, 0.0, 1.0),
        Row::Atk => p.atk = step_f32(p.atk, steps, 0.01, coarse, 0.0, 1.0),
        Row::Dec => p.dec = step_f32(p.dec, steps, 0.01, coarse, 0.0, 1.0),
        Row::Level => p.level = step_f32(p.level, steps, 0.01, coarse, 0.0, 1.0),
        Row::Kit => {
            if steps != 0 {
                s.kit = !s.kit;
            }
        }
        Row::Notes | Row::Amp => {}
    }
}

fn row_text(s: &SamplerState, r: Row) -> String {
    let p = &s.slots[s.edit].params;
    match r {
        Row::Slot => format!(
            "{} ({})",
            slot_letter(s.edit),
            s.slots
                .iter()
                .map(|sl| if sl.path.is_some() { '●' } else { '·' })
                .collect::<String>()
        ),
        Row::Sample => s.slots[s.edit]
            .name
            .clone()
            .unwrap_or_else(|| "(empty · / browses)".into()),
        Row::Mode => p.mode.name().into(),
        Row::Start => format!("{:.0}%", p.start * 100.0),
        Row::Len => format!("{:.0}%", p.len * 100.0),
        Row::Pitch => format!("{:+.0} st", p.pitch),
        Row::Speed => format!("{:+.2}×{}", p.speed, if p.speed < 0.0 { " ◀" } else { "" }),
        Row::Gene => {
            if p.gene <= 0.001 {
                "tape".into()
            } else {
                format!("{:.0} ms grains", engine::gene_secs(p.gene) * 1000.0)
            }
        }
        Row::Slide => format!("{:.0}%", p.slide * 100.0),
        Row::Atk => format!("{:.0} ms", engine::env_secs(p.atk) * 1000.0),
        Row::Dec => format!("{:.0} ms", engine::env_secs(p.dec) * 1000.0),
        Row::Level => format!("{:.0}%", p.level * 100.0),
        Row::Kit => if s.kit { "KIT (C→a … G→h)" } else { "single" }.into(),
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
    }
}

fn norm(s: &SamplerState, r: Row) -> Option<f32> {
    let p = &s.slots[s.edit].params;
    Some(match r {
        Row::Start => p.start,
        Row::Len => p.len,
        Row::Pitch => (p.pitch + 24.0) / 48.0,
        Row::Speed => (p.speed + 2.0) / 4.0,
        Row::Gene => p.gene,
        Row::Slide => p.slide,
        Row::Atk => p.atk,
        Row::Dec => p.dec,
        Row::Level => p.level,
        _ => return None,
    })
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &SamplerState,
    b: &Browser,
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
            "SAMPLER",
            &format!("reel {}", instance),
            "",
            w,
        ));
        let mut spans = vec![Span::styled("  env ".to_string(), theme::chrome())];
        spans.push(Span::styled(
            theme::meter_char(s.env_now).to_string(),
            theme::signal(theme::cv_ramp(s.env_now)),
        ));
        spans.push(Span::styled(
            format!(
                "  {}",
                if fetch::key().is_some() {
                    "a-u.supply ●"
                } else {
                    "local only ○"
                }
            ),
            theme::dim(),
        ));
        lines.push(Line::from(spans));
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
                vec![Span::styled(format!(" {:<7}", row_label(*r)), label_style)];
            let bound = src_index(*r).is_some_and(|k| s.srcs[k].is_some())
                || (*r == Row::Notes && s.notes_src.is_some())
                || (*r == Row::Amp && s.amp_src.is_some());
            let hue = src_index(*r)
                .and_then(|k| s.srcs[k].as_ref())
                .or(match r {
                    Row::Notes => s.notes_src.as_ref(),
                    Row::Amp => s.amp_src.as_ref(),
                    _ => None,
                })
                .map(|a| routing::cable_color(entries, a));
            if let Some(n) = norm(s, *r) {
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
            spans.push(Span::styled(
                format!(" {}{}", mark, row_text(s, *r)),
                vstyle,
            ));
            lines.push(Line::from(spans));
        }

        theme::anchor_bottom(&mut lines, h, 2);
        lines.push(theme::rule(w));
        let mode_hint = if b.busy { "searching…" } else { "" };
        lines.push(theme::status("NORMAL", overlay.unwrap_or(mode_hint), "", w));
        f.render_widget(Paragraph::new(lines), area);

        // the browser overlay
        if b.open {
            let rows = b.rows();
            let title = if !b.online() {
                " samples · local cache "
            } else if b.raw {
                " a-u.supply · raw reels "
            } else {
                " a-u.supply · drums "
            };
            let ph = (rows.len() as u16 + 4).clamp(8, area.height);
            let pw = (w as u16).saturating_sub(8).max(40);
            let rect = ratatui::layout::Rect::new(
                4,
                (area.height.saturating_sub(ph)) / 2,
                pw.min(area.width),
                ph,
            );
            f.render_widget(ratatui::widgets::Clear, rect);
            let mut blines = vec![Line::from(vec![
                Span::styled(" / ", theme::chrome_hi()),
                Span::styled(
                    format!("{}{}", b.query, if b.typing { "▌" } else { "" }),
                    theme::value(),
                ),
                Span::styled(
                    if b.busy {
                        "  …"
                    } else {
                        "  (Enter search · Tab drums/raw · Esc close)"
                    },
                    theme::dim(),
                ),
            ])];
            for (i, row) in rows.iter().enumerate() {
                let style = if i == b.sel && !b.typing {
                    theme::selected()
                } else {
                    theme::value()
                };
                blines.push(Line::from(Span::styled(format!(" {row}"), style)));
            }
            if !b.status.is_empty() {
                blines.push(Line::from(Span::styled(
                    format!(" {}", b.status),
                    theme::dim(),
                )));
            }
            let bl = Paragraph::new(blines)
                .style(Style::default().fg(theme::ink()).bg(theme::bg()))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(theme::chrome())
                        .title(Span::styled(title, theme::chrome_hi())),
                );
            f.render_widget(bl, rect);
        }

        if show_help {
            let help = Paragraph::new(vec![
                Line::from("━━━ SAMPLER · reels + microsound designer ━━━"),
                Line::from(""),
                Line::from("  slot row   h/l picks the edit/load slot (a–h)"),
                Line::from("  /          Browse a-u.supply (Tab: drums ↔ raw"),
                Line::from("             reels; Enter downloads into the slot)"),
                Line::from("  mode       oneshot · loop · gated · hold"),
                Line::from("  start/len  The splice window into the reel"),
                Line::from("  gene       0 = tape · up = grains (1 s → 10 ms)"),
                Line::from("  slide      Grain position — bind a drunk track"),
                Line::from("  speed      Varispeed; negative plays backwards"),
                Line::from("  kit        Notes map C→a … G→h: a drum kit on"),
                Line::from("             one track. single = edit slot plays"),
                Line::from("  @ / x      Bind / unbind (pitch speed gene slide"),
                Line::from("             level take CV; notes/amp as usual)"),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" SAMPLER ", theme::chrome_hi())),
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
    state::write_pid_file("sampler", instance);

    let shared = Arc::new(Mutex::new(SamplerState::new()));
    if let Ok(p) = state::load_module_state::<state::SamplerParams>("sampler", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let audio_builder = thread::Builder::new()
        .name(String::from("sampler-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = audio_builder.spawn(move || {
        if let Err(e) = audio_thread(audio_state, instance) {
            eprintln!("[sampler {}] audio thread error: {}", instance, e);
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
    let mut should_quit = false;
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();
    let manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let mut ui_entries: Vec<crate::shm::ManifestEntry> = Vec::new();
    let mut ui_entries_at: Option<Instant> = None;

    let mut browser = Browser::default();
    let (tx, rx) = mpsc::channel::<BrowseMsg>();

    loop {
        if state::check_save_signal() {
            let params = snapshot_params(&shared.lock().unwrap());
            let _ = state::save_module_state("sampler", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::SamplerParams>("sampler", instance) {
                apply_params(&mut shared.lock().unwrap(), &p);
            }
        }
        if ui_entries_at.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
            ui_entries = manifest.entries();
            ui_entries_at = Some(Instant::now());
        }
        // worker results
        while let Ok(msg) = rx.try_recv() {
            match msg {
                BrowseMsg::Results(hits) => {
                    browser.hits = hits;
                    browser.sel = 0;
                    browser.busy = false;
                    browser.status = format!("{} hits", browser.hits.len());
                }
                BrowseMsg::Fetched(slot, path, name) => {
                    browser.busy = false;
                    browser.status = format!("loaded {} → {}", name, slot_letter(slot));
                    let mut s = shared.lock().unwrap();
                    s.slots[slot].path = Some(path.clone());
                    s.slots[slot].name = Some(name);
                    s.load_req.push((slot, path));
                }
                BrowseMsg::Error(e) => {
                    browser.busy = false;
                    browser.status = e;
                }
            }
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
                &browser,
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

        // browser overlay eats the keyboard first
        if browser.open {
            if let Event::Key(key) = ev {
                match key.code {
                    KeyCode::Esc => {
                        if browser.typing {
                            browser.typing = false;
                        } else {
                            browser.open = false;
                        }
                    }
                    KeyCode::Tab => {
                        browser.raw = !browser.raw;
                        browser.hits.clear();
                        browser.status.clear();
                    }
                    KeyCode::Char(c) if browser.typing => browser.query.push(c),
                    KeyCode::Backspace if browser.typing => {
                        browser.query.pop();
                    }
                    KeyCode::Enter if browser.typing => {
                        browser.typing = false;
                        if browser.online() {
                            browser.busy = true;
                            let q = browser.query.clone();
                            let raw = browser.raw;
                            let txc = tx.clone();
                            thread::spawn(move || {
                                let msg = match fetch::search(&q, raw, 24) {
                                    Ok(hits) => BrowseMsg::Results(hits),
                                    Err(e) => BrowseMsg::Error(e.to_string()),
                                };
                                let _ = txc.send(msg);
                            });
                        } else {
                            browser.local = fetch::list_cache();
                            browser.status = format!("{} cached", browser.local.len());
                        }
                    }
                    KeyCode::Char('/') => {
                        browser.typing = true;
                        browser.query.clear();
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        let n = browser.rows().len();
                        if n > 0 {
                            browser.sel = (browser.sel + 1) % n;
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        let n = browser.rows().len();
                        if n > 0 {
                            browser.sel = (browser.sel + n - 1) % n;
                        }
                    }
                    KeyCode::Enter => {
                        let slot = shared.lock().unwrap().edit;
                        if !browser.online() {
                            if let Some((p, n)) = browser.local.get(browser.sel).cloned() {
                                let mut s = shared.lock().unwrap();
                                s.slots[slot].path = Some(p.to_string_lossy().into_owned());
                                s.slots[slot].name = Some(n.clone());
                                s.load_req.push((slot, p.to_string_lossy().into_owned()));
                                browser.status = format!("loaded {} → {}", n, slot_letter(slot));
                            }
                        } else if let Some(hit) = browser.hits.get(browser.sel).cloned() {
                            browser.busy = true;
                            browser.status = format!("fetching {}…", hit.filename);
                            let txc = tx.clone();
                            thread::spawn(move || {
                                let msg = match fetch::fetch(&hit) {
                                    Ok(p) => BrowseMsg::Fetched(
                                        slot,
                                        p.to_string_lossy().into_owned(),
                                        hit.filename.clone(),
                                    ),
                                    Err(e) => BrowseMsg::Error(e.to_string()),
                                };
                                let _ = txc.send(msg);
                            });
                        }
                    }
                    _ => {}
                }
            }
            continue;
        }

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
                let slot = match r {
                    Row::Notes => 100,
                    Row::Amp => 101,
                    _ => 110 + src_index(r).unwrap_or(0),
                };
                let old = s.get_param(slot);
                match r {
                    Row::Notes => s.notes_src = addr.clone(),
                    Row::Amp => s.amp_src = addr.clone(),
                    _ => {
                        if let Some(k) = src_index(r) {
                            s.srcs[k] = addr.clone();
                            s.resolved[k] = None;
                        }
                    }
                }
                if let Some(old) = old {
                    history.record(slot, "Bind", old, ParamValue::Src(addr.map(|a| a.to_string())));
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
                let mut baseline = String::new();
                match cmd {
                    ExCommand::Write(name) => {
                        let mut pn = None;
                        ex_msg = Some(
                            match crate::excmd::ex_write(name, &mut pn, &mut baseline, &params) {
                                Ok(m) | Err(m) => m,
                            },
                        );
                    }
                    ExCommand::Edit(name) => {
                        match state::load_patch::<state::SamplerParams>(&name) {
                            Ok(p) => {
                                apply_params(&mut shared.lock().unwrap(), &p);
                                ex_msg = Some(format!("Loaded {}", name));
                            }
                            Err(e) => ex_msg = Some(e.to_string()),
                        }
                    }
                    ExCommand::Quit { .. } => should_quit = true,
                    ExCommand::WriteQuit(name) => {
                        let mut pn = None;
                        let _ = crate::excmd::ex_write(name, &mut pn, &mut baseline, &params);
                        should_quit = true;
                    }
                    ExCommand::Set(k, v) => {
                        ex_msg = Some(ex_set(&mut shared.lock().unwrap(), &k, &v));
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
            let _ = state::save_module_state("sampler", instance, &params);
            ex_msg = Some(String::from("Saved"));
            continue;
        }

        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
            KeyCode::Char('/') => {
                count.clear();
                browser.open = true;
                browser.typing = true;
                browser.query.clear();
                if !browser.online() {
                    browser.local = fetch::list_cache();
                    browser.status = format!("{} cached (no API key)", browser.local.len());
                    browser.typing = false;
                }
            }
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
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
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
                let mut s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                let e = s.edit;
                let def = SlotParams::default();
                let p = &mut s.slots[e].params;
                match r {
                    Row::Mode => p.mode = def.mode,
                    Row::Start => p.start = def.start,
                    Row::Len => p.len = def.len,
                    Row::Pitch => p.pitch = def.pitch,
                    Row::Speed => p.speed = def.speed,
                    Row::Gene => p.gene = def.gene,
                    Row::Slide => p.slide = def.slide,
                    Row::Atk => p.atk = def.atk,
                    Row::Dec => p.dec = def.dec,
                    Row::Level => p.level = def.level,
                    _ => {}
                }
            }
            KeyCode::Char('@') => {
                count.clear();
                let s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                let current = match r {
                    Row::Notes => s.notes_src.clone(),
                    Row::Amp => s.amp_src.clone(),
                    _ => src_index(r).and_then(|k| s.srcs[k].clone()),
                };
                let bindable =
                    matches!(r, Row::Notes | Row::Amp) || src_index(r).is_some();
                drop(s);
                if bindable {
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
                let mut s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
                match r {
                    Row::Sample => {
                        let e = s.edit;
                        s.slots[e].path = None;
                        s.slots[e].name = None;
                        s.unload_req.push(e);
                    }
                    Row::Notes => s.notes_src = None,
                    Row::Amp => s.amp_src = None,
                    _ => {
                        if let Some(k) = src_index(r) {
                            s.srcs[k] = None;
                            s.resolved[k] = None;
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

// undo: rows 0..14 map by ROWS index against the EDIT slot; 100/101 the
// notes/amp bindings; 110+k the CV bank.
impl crate::undo::ParamUndo for SamplerState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        let p = &self.slots[self.edit].params;
        Some(match slot {
            0 => V::Usize(self.edit),
            2 => V::Usize(p.mode as usize),
            3 => V::F32(p.start),
            4 => V::F32(p.len),
            5 => V::F32(p.pitch),
            6 => V::F32(p.speed),
            7 => V::F32(p.gene),
            8 => V::F32(p.slide),
            9 => V::F32(p.atk),
            10 => V::F32(p.dec),
            11 => V::F32(p.level),
            12 => V::Bool(self.kit),
            100 => V::Src(self.notes_src.as_ref().map(|a| a.to_string())),
            101 => V::Src(self.amp_src.as_ref().map(|a| a.to_string())),
            k if (110..110 + N_SRC).contains(&k) => {
                V::Src(self.srcs[k - 110].as_ref().map(|a| a.to_string()))
            }
            _ => return None,
        })
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        let e = self.edit;
        match (slot, value) {
            (0, V::Usize(v)) => self.edit = v.min(NUM_SLOTS - 1),
            (2, V::Usize(v)) => {
                self.slots[e].params.mode =
                    [Mode::OneShot, Mode::Loop, Mode::Gated, Mode::Hold][v.min(3)]
            }
            (3, V::F32(v)) => self.slots[e].params.start = v.clamp(0.0, 1.0),
            (4, V::F32(v)) => self.slots[e].params.len = v.clamp(0.01, 1.0),
            (5, V::F32(v)) => self.slots[e].params.pitch = v.clamp(-24.0, 24.0),
            (6, V::F32(v)) => self.slots[e].params.speed = v.clamp(-2.0, 2.0),
            (7, V::F32(v)) => self.slots[e].params.gene = v.clamp(0.0, 1.0),
            (8, V::F32(v)) => self.slots[e].params.slide = v.clamp(0.0, 1.0),
            (9, V::F32(v)) => self.slots[e].params.atk = v.clamp(0.0, 1.0),
            (10, V::F32(v)) => self.slots[e].params.dec = v.clamp(0.0, 1.0),
            (11, V::F32(v)) => self.slots[e].params.level = v.clamp(0.0, 1.0),
            (12, V::Bool(v)) => self.kit = v,
            (100, V::Src(v)) => self.notes_src = v.as_deref().and_then(SourceAddr::parse),
            (101, V::Src(v)) => self.amp_src = v.as_deref().and_then(SourceAddr::parse),
            (k, V::Src(v)) if (110..110 + N_SRC).contains(&k) => {
                self.srcs[k - 110] = v.as_deref().and_then(SourceAddr::parse);
                self.resolved[k - 110] = None;
            }
            _ => {}
        }
    }
}

/// `:set <row> <value>` for the edit slot (`:set mode loop`,
/// `:set gene 0.4`, `:set kit on`).
fn ex_set(s: &mut SamplerState, key: &str, value: &str) -> String {
    let e = s.edit;
    let p = &mut s.slots[e].params;
    let ok = |what: String| what;
    match key {
        "mode" => match Mode::parse(value) {
            Some(m) => {
                p.mode = m;
                ok(format!("mode = {}", m.name()))
            }
            None => "mode: oneshot loop gated hold".into(),
        },
        "kit" => {
            s.kit = matches!(value, "on" | "1" | "true" | "kit");
            ok(format!("kit = {}", s.kit))
        }
        "slot" => {
            if let Some(c) = value.chars().next() {
                let i = (c as u8).wrapping_sub(b'a') as usize;
                if i < NUM_SLOTS {
                    s.edit = i;
                    return ok(format!("slot = {}", c));
                }
            }
            "slot: a–h".into()
        }
        _ => {
            let Ok(v) = value.parse::<f32>() else {
                return format!("{key}: not a number: {value}");
            };
            match key {
                "start" => p.start = v.clamp(0.0, 1.0),
                "len" => p.len = v.clamp(0.01, 1.0),
                "pitch" => p.pitch = v.clamp(-24.0, 24.0),
                "speed" => p.speed = v.clamp(-2.0, 2.0),
                "gene" => p.gene = v.clamp(0.0, 1.0),
                "slide" => p.slide = v.clamp(0.0, 1.0),
                "atk" => p.atk = v.clamp(0.0, 1.0),
                "dec" => p.dec = v.clamp(0.0, 1.0),
                "level" => p.level = v.clamp(0.0, 1.0),
                _ => return format!("Unknown setting: {key}"),
            }
            ok(format!("{key} = {v}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = SamplerState::new();
        s.kit = false;
        s.edit = 3;
        s.slots[3].params.mode = Mode::Gated;
        s.slots[3].params.gene = 0.4;
        s.slots[3].path = Some("/tmp/x.wav".into());
        s.notes_src = SourceAddr::parse("sequencer/0/t4");
        s.srcs[3] = SourceAddr::parse("sequencer/0/t2"); // slide
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::SamplerParams = toml::from_str(&toml).expect("parses");
        let mut s2 = SamplerState::new();
        apply_params(&mut s2, &back);
        assert!(!s2.kit);
        assert_eq!(s2.edit, 3);
        assert_eq!(s2.slots[3].params.mode, Mode::Gated);
        assert!((s2.slots[3].params.gene - 0.4).abs() < 1e-6);
        assert_eq!(s2.slots[3].path.as_deref(), Some("/tmp/x.wav"));
        assert!(!s2.load_req.is_empty(), "reload requests queued for loaded slots");
        assert_eq!(
            s2.srcs[3].as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t2".into())
        );
    }

    #[test]
    fn ex_set_covers_designer_rows() {
        let mut s = SamplerState::new();
        assert!(ex_set(&mut s, "mode", "loop").contains("loop"));
        assert!(ex_set(&mut s, "gene", "0.5").contains("0.5"));
        assert!(ex_set(&mut s, "slot", "c").contains("c"));
        assert_eq!(s.edit, 2);
        assert!(ex_set(&mut s, "kit", "off").contains("false"));
        assert!(ex_set(&mut s, "wow", "1").contains("Unknown"));
    }

    #[test]
    fn undo_slots_round_trip() {
        use crate::undo::{ParamUndo, ParamValue as V};
        let mut s = SamplerState::new();
        s.set_param(7, V::F32(0.7));
        assert!((s.slots[0].params.gene - 0.7).abs() < 1e-6);
        s.set_param(110 + 3, V::Src(Some("envelope/0/ch2".into())));
        assert_eq!(
            s.srcs[3].as_ref().map(|a| a.to_string()),
            Some("envelope/0/ch2".into())
        );
        s.set_param(100, V::Src(Some("sequencer/0/t5".into())));
        assert_eq!(
            s.notes_src.as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t5".into())
        );
    }
}
