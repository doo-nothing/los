//! The DPO module shell: rows, bindings, events, the audio thread.
//!
//! VCO B plays the notes; `ratio` places VCO A against it (follow lag
//! makes the tracking humanly late); the FM bus, mod bus, and timbre
//! chain are all rows with `@`-bindable CV; `strike` takes a trigger
//! source and snaps the folder open through a vactrol, exactly the
//! hardware's favorite percussion trick. In `lfo` mode VCO A slows to
//! LFO rates and its sine publishes as `dpo/N/lfo`.

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

use super::dsp::{AMode, Dpo, DpoParams};
use crate::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

const FALLBACK_RATE: f32 = 48_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Ratio,
    Follow,
    Index,
    FmA,
    FmB,
    Mode,
    Shape,
    Angle,
    Fold,
    Mod,
    Mix,
    Level,
    Strike,
    Amp,
    Notes,
}

const ROWS: [Row; 15] = [
    Row::Ratio,
    Row::Follow,
    Row::Index,
    Row::FmA,
    Row::FmB,
    Row::Mode,
    Row::Shape,
    Row::Angle,
    Row::Fold,
    Row::Mod,
    Row::Mix,
    Row::Level,
    Row::Strike,
    Row::Amp,
    Row::Notes,
];

/// CV-bindable knobs, srcs[] order.
const BINDABLE: [Row; 7] = [
    Row::Ratio,
    Row::Index,
    Row::Shape,
    Row::Angle,
    Row::Fold,
    Row::Mod,
    Row::Follow,
];
const N_SRC: usize = 7;

struct DpoState {
    ratio: f32,
    follow: f32,
    index: f32,
    fm_a: f32,
    fm_b: f32,
    mode: AMode,
    shape: f32,
    angle: f32,
    fold: f32,
    mod_index: f32,
    mix: f32,
    level: f32,
    freq: f32,
    gate: bool,
    velocity: f32,
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    strike_src: Option<SourceAddr>,
    strike_resolved: Option<usize>,
    strike_last: f32,
    strike_until: u64,
    amp_src: Option<SourceAddr>,
    amp_resolved: Option<usize>,
    notes_src: Option<SourceAddr>,
    a_now: f32,
    b_now: f32,
    selected: usize,
}

impl DpoState {
    fn new() -> Self {
        Self {
            ratio: 1.0,
            follow: 1.0,
            index: 0.0,
            fm_a: 0.5,
            fm_b: 0.5,
            mode: AMode::Free,
            shape: 0.0,
            angle: 0.0,
            fold: 0.25,
            mod_index: 0.0,
            mix: 0.0,
            level: 0.8,
            freq: 110.0,
            gate: false,
            velocity: 0.0,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [1.0, 0.0, 0.0, 0.0, 0.25, 0.0, 1.0],
            strike_src: None,
            strike_resolved: None,
            strike_last: 0.0,
            strike_until: 0,
            amp_src: None,
            amp_resolved: None,
            notes_src: None,
            a_now: 0.0,
            b_now: 0.0,
            selected: 0,
        }
    }
}

const SRC_SLOT_BASE: usize = 20;
const STRIKE_SLOT: usize = 40;
const AMP_SLOT: usize = 41;
const NOTES_SLOT: usize = 42;

impl crate::undo::ParamUndo for DpoState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        match slot {
            STRIKE_SLOT => {
                return Some(V::Src(self.strike_src.as_ref().map(|a| a.to_string())))
            }
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
            Row::Mode => V::Usize(self.mode as usize),
            Row::Ratio => V::F32(self.ratio),
            Row::Follow => V::F32(self.follow),
            Row::Index => V::F32(self.index),
            Row::FmA => V::F32(self.fm_a),
            Row::FmB => V::F32(self.fm_b),
            Row::Shape => V::F32(self.shape),
            Row::Angle => V::F32(self.angle),
            Row::Fold => V::F32(self.fold),
            Row::Mod => V::F32(self.mod_index),
            Row::Mix => V::F32(self.mix),
            Row::Level => V::F32(self.level),
            Row::Strike | Row::Amp | Row::Notes => return None,
        })
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        let parse = |v: &Option<String>| v.as_deref().and_then(SourceAddr::parse);
        match (slot, &value) {
            (STRIKE_SLOT, V::Src(v)) => {
                self.strike_src = parse(v);
                self.strike_resolved = None;
                return;
            }
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
        let Some(r) = ROWS.get(slot).copied() else {
            return;
        };
        match (r, value) {
            (Row::Mode, V::Usize(v)) => {
                self.mode = [AMode::Free, AMode::Lock, AMode::Sync, AMode::Lfo][v.min(3)]
            }
            (Row::Ratio, V::F32(v)) => self.ratio = v.clamp(0.25, 8.0),
            (Row::Follow, V::F32(v)) => self.follow = v.clamp(0.0, 1.0),
            (Row::Index, V::F32(v)) => self.index = v.clamp(0.0, 1.0),
            (Row::FmA, V::F32(v)) => self.fm_a = v.clamp(0.0, 1.0),
            (Row::FmB, V::F32(v)) => self.fm_b = v.clamp(0.0, 1.0),
            (Row::Shape, V::F32(v)) => self.shape = v.clamp(0.0, 1.0),
            (Row::Angle, V::F32(v)) => self.angle = v.clamp(0.0, 1.0),
            (Row::Fold, V::F32(v)) => self.fold = v.clamp(0.0, 1.0),
            (Row::Mod, V::F32(v)) => self.mod_index = v.clamp(0.0, 1.0),
            (Row::Mix, V::F32(v)) => self.mix = v.clamp(0.0, 1.0),
            (Row::Level, V::F32(v)) => self.level = v.clamp(0.0, 1.0),
            _ => {}
        }
    }
}

fn snapshot_params(s: &DpoState) -> state::DpoParamsState {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::DpoParamsState {
        format: state::STATE_FORMAT,
        ratio: Some(s.ratio),
        follow: Some(s.follow),
        index: Some(s.index),
        fm_a: Some(s.fm_a),
        fm_b: Some(s.fm_b),
        mode: Some(s.mode.name().to_string()),
        shape: Some(s.shape),
        angle: Some(s.angle),
        fold: Some(s.fold),
        mod_index: Some(s.mod_index),
        mix: Some(s.mix),
        level: Some(s.level),
        freq: Some(s.freq),
        gate: Some(s.gate),
        ratio_src: src(0),
        index_src: src(1),
        shape_src: src(2),
        angle_src: src(3),
        fold_src: src(4),
        mod_src: src(5),
        follow_src: src(6),
        strike_src: s.strike_src.as_ref().map(|a| a.to_string()),
        amp_src: s.amp_src.as_ref().map(|a| a.to_string()),
        notes_src: s.notes_src.as_ref().map(|a| a.to_string()),
    }
}

fn apply_params(s: &mut DpoState, p: &state::DpoParamsState) {
    macro_rules! f {
        ($field:ident, $lo:expr, $hi:expr) => {
            if let Some(v) = p.$field {
                s.$field = v.clamp($lo, $hi);
            }
        };
    }
    f!(ratio, 0.25, 8.0);
    f!(follow, 0.0, 1.0);
    f!(index, 0.0, 1.0);
    f!(fm_a, 0.0, 1.0);
    f!(fm_b, 0.0, 1.0);
    f!(shape, 0.0, 1.0);
    f!(angle, 0.0, 1.0);
    f!(fold, 0.0, 1.0);
    f!(mod_index, 0.0, 1.0);
    f!(mix, 0.0, 1.0);
    f!(level, 0.0, 1.0);
    if let Some(ref m) = p.mode {
        if let Some(m) = AMode::parse(m) {
            s.mode = m;
        }
    }
    if let Some(v) = p.freq {
        s.freq = v;
    }
    if let Some(v) = p.gate {
        s.gate = v;
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.ratio_src),
        parse(&p.index_src),
        parse(&p.shape_src),
        parse(&p.angle_src),
        parse(&p.fold_src),
        parse(&p.mod_src),
        parse(&p.follow_src),
    ];
    s.strike_src = parse(&p.strike_src);
    s.amp_src = parse(&p.amp_src);
    s.notes_src = parse(&p.notes_src);
    s.resolved = Default::default();
    s.strike_resolved = None;
    s.amp_resolved = None;
}

// ── audio thread ───────────────────────────────────────────────────────────

fn audio_thread(shared: Arc<Mutex<DpoState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_dpo_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating dpo ring")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    // one claim: the LFO-mode sine
    manifest.register("dpo", instance, Some(&shm_name), 1)?;
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
    let mut mono = vec![0.0_f32; slot_frames];

    let mut core = Dpo::new(sample_rate);
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
            s.strike_resolved = s
                .strike_src
                .as_ref()
                .and_then(|a| routing::resolve(&entries, a));
            s.amp_resolved = s
                .amp_src
                .as_ref()
                .and_then(|a| routing::resolve(&entries, a));
            note_filter = s.notes_src.as_ref().and_then(routing::note_source_track);
            let mask = s
                .resolved
                .iter()
                .flatten()
                .chain(s.strike_resolved.iter())
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

        let (p, amp, vel) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            // max/min, not clamp: NaN from a stale channel dies here
            #[allow(clippy::manual_clamp)]
            let cv = |k: usize, manual: f32, lo: f32, hi: f32, s: &DpoState| -> f32 {
                match (s.resolved[k], bus) {
                    (Some(ch), Some(b)) => lo + b.get(ch).max(0.0).min(1.0) * (hi - lo),
                    _ => manual,
                }
            };
            let vals = [
                cv(0, s.ratio, 0.25, 8.0, &s),
                cv(1, s.index, 0.0, 1.0, &s),
                cv(2, s.shape, 0.0, 1.0, &s),
                cv(3, s.angle, 0.0, 1.0, &s),
                cv(4, s.fold, 0.0, 1.0, &s),
                cv(5, s.mod_index, 0.0, 1.0, &s),
                cv(6, s.follow, 0.0, 1.0, &s),
            ];
            s.eff = vals;
            // strike: rising edge on the bound source opens the vactrol
            // for ~30 ms of gate
            if let (Some(ch), Some(b)) = (s.strike_resolved, bus) {
                let v = b.get(ch);
                if v > 0.5 && s.strike_last <= 0.5 {
                    s.strike_until = blocks + 24; // ~32 ms of gate
                }
                s.strike_last = v;
            }
            let strike = blocks < s.strike_until;
            let amp = match (s.amp_src.is_some(), s.amp_resolved, bus) {
                (false, _, _) => 1.0,
                (true, Some(ch), Some(b)) => b.get(ch).clamp(0.0, 1.0),
                (true, _, _) => 0.0,
            };
            let vel = if s.gate && s.velocity < 0.001 {
                1.0
            } else {
                s.velocity
            };
            s.a_now = core.a_out;
            s.b_now = core.b_out;
            (
                DpoParams {
                    freq_b: s.freq,
                    ratio: vals[0],
                    follow: vals[6],
                    index: vals[1],
                    fm_a: s.fm_a,
                    fm_b: s.fm_b,
                    mode: s.mode,
                    shape: vals[2],
                    angle: vals[3],
                    fold: vals[4],
                    mod_index: vals[5],
                    strike,
                    mix: s.mix,
                    level: s.level,
                },
                amp,
                vel,
            )
        };

        let lfo = core.process(&mut mono, &p);

        let gain_target = amp * vel * 0.5;
        let g_alpha = 1.0 - (-1.0 / (0.0007 * sample_rate)).exp();
        for f in 0..slot_frames {
            gain_smooth += (gain_target - gain_smooth) * g_alpha;
            let v = mono[f] * gain_smooth;
            block[f * channels] = v;
            if channels > 1 {
                block[f * channels + 1] = v;
            }
        }
        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            core = Dpo::new(sample_rate);
        }
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            bus.set(base, 0.5 + 0.5 * lfo);
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
        Row::Ratio => "ratio",
        Row::Follow => "follow",
        Row::Index => "index",
        Row::FmA => "fm→a",
        Row::FmB => "fm→b",
        Row::Mode => "mode",
        Row::Shape => "shape",
        Row::Angle => "angle",
        Row::Fold => "fold",
        Row::Mod => "mod",
        Row::Mix => "mix",
        Row::Level => "level",
        Row::Strike => "strike",
        Row::Amp => "amp",
        Row::Notes => "notes",
    }
}

fn src_index(r: Row) -> Option<usize> {
    BINDABLE.iter().position(|b| *b == r)
}

fn binding_slot(r: Row) -> Option<usize> {
    match r {
        Row::Strike => Some(STRIKE_SLOT),
        Row::Amp => Some(AMP_SLOT),
        Row::Notes => Some(NOTES_SLOT),
        _ => src_index(r).map(|i| SRC_SLOT_BASE + i),
    }
}

fn adjust(s: &mut DpoState, steps: i32, coarse: bool) {
    use crate::keys::step_f32;
    match ROWS[s.selected.min(ROWS.len() - 1)] {
        Row::Ratio => {
            let st = if coarse { 0.5 } else { 0.01 };
            s.ratio = (s.ratio + steps as f32 * st).clamp(0.25, 8.0);
        }
        Row::Follow => s.follow = step_f32(s.follow, steps, 0.01, coarse, 0.0, 1.0),
        Row::Index => s.index = step_f32(s.index, steps, 0.01, coarse, 0.0, 1.0),
        Row::FmA => s.fm_a = step_f32(s.fm_a, steps, 0.01, coarse, 0.0, 1.0),
        Row::FmB => s.fm_b = step_f32(s.fm_b, steps, 0.01, coarse, 0.0, 1.0),
        Row::Mode => s.mode = s.mode.cycle(steps),
        Row::Shape => s.shape = step_f32(s.shape, steps, 0.01, coarse, 0.0, 1.0),
        Row::Angle => s.angle = step_f32(s.angle, steps, 0.01, coarse, 0.0, 1.0),
        Row::Fold => s.fold = step_f32(s.fold, steps, 0.01, coarse, 0.0, 1.0),
        Row::Mod => s.mod_index = step_f32(s.mod_index, steps, 0.01, coarse, 0.0, 1.0),
        Row::Mix => s.mix = step_f32(s.mix, steps, 0.01, coarse, 0.0, 1.0),
        Row::Level => s.level = step_f32(s.level, steps, 0.01, coarse, 0.0, 1.0),
        Row::Strike | Row::Amp | Row::Notes => {}
    }
}

fn row_text(s: &DpoState, r: Row) -> String {
    match r {
        Row::Ratio => format!("{:.2}×", s.ratio),
        Row::Follow => format!("{:.0}%", s.follow * 100.0),
        Row::Index => format!("{:.0}%", s.index * 100.0),
        Row::FmA => format!("{:.0}%", s.fm_a * 100.0),
        Row::FmB => format!("{:.0}%", s.fm_b * 100.0),
        Row::Mode => s.mode.name().into(),
        Row::Shape => format!("{:.0}%", s.shape * 100.0),
        Row::Angle => format!("{:.0}%", s.angle * 100.0),
        Row::Fold => format!("{:.0}%", s.fold * 100.0),
        Row::Mod => format!("{:.0}%", s.mod_index * 100.0),
        Row::Mix => format!("B {:.0}·{:.0} A", (1.0 - s.mix) * 100.0, s.mix * 100.0),
        Row::Level => format!("{:.0}%", s.level * 100.0),
        Row::Strike => s
            .strike_src
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(unbound)".into()),
        Row::Amp => s
            .amp_src
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(unbound · drone)".into()),
        Row::Notes => s
            .notes_src
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(all tracks)".into()),
    }
}

fn norm(s: &DpoState, r: Row) -> Option<f32> {
    Some(match r {
        Row::Ratio => (s.ratio - 0.25) / 7.75,
        Row::Follow => s.follow,
        Row::Index => s.index,
        Row::FmA => s.fm_a,
        Row::FmB => s.fm_b,
        Row::Shape => s.shape,
        Row::Angle => s.angle,
        Row::Fold => s.fold,
        Row::Mod => s.mod_index,
        Row::Mix => s.mix,
        Row::Level => s.level,
        _ => return None,
    })
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &DpoState,
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
        lines.push(theme::header("DPO", &format!("complex {}", instance), "", w));
        lines.push(Line::from(vec![
            Span::styled("  A ".to_string(), theme::chrome()),
            Span::styled(
                theme::meter_char(0.5 + 0.5 * s.a_now).to_string(),
                theme::signal(theme::cv_ramp(0.5 + 0.5 * s.a_now)),
            ),
            Span::styled("  B ".to_string(), theme::chrome()),
            Span::styled(
                theme::meter_char(0.5 + 0.5 * s.b_now).to_string(),
                theme::signal(theme::cv_ramp(0.5 + 0.5 * s.b_now)),
            ),
            Span::styled(
                format!("  {}", if s.gate { "●" } else { "○" }),
                if s.gate { theme::value() } else { theme::dim() },
            ),
        ]));
        lines.push(Line::from(""));

        let bar_w = theme::bar_width(w, 30);
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
                || (*r == Row::Strike && s.strike_src.is_some())
                || (*r == Row::Amp && s.amp_src.is_some())
                || (*r == Row::Notes && s.notes_src.is_some());
            let hue = src_index(*r)
                .and_then(|k| s.srcs[k].as_ref())
                .or(match r {
                    Row::Strike => s.strike_src.as_ref(),
                    Row::Amp => s.amp_src.as_ref(),
                    Row::Notes => s.notes_src.as_ref(),
                    _ => None,
                })
                .map(|a| routing::cable_color(entries, a));
            if let Some(n) = norm(s, *r) {
                let shown = match src_index(*r) {
                    Some(k) if s.srcs[k].is_some() => {
                        // bound rows show the live value, normalized into
                        // the same bar space
                        match *r {
                            Row::Ratio => (s.eff[k] - 0.25) / 7.75,
                            _ => s.eff[k],
                        }
                    }
                    _ => n,
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
                Line::from("━━━ DPO · complex oscillator (after the Make Noise) ━━━"),
                Line::from(""),
                Line::from("  B plays the notes; ratio places A against it"),
                Line::from("  follow     <100% lags A's tracking (loose ratios)"),
                Line::from("  index      The FM bus, both directions at once"),
                Line::from("  fm→a/fm→b  Per-direction attenuators"),
                Line::from("  mode       free · lock · sync · lfo (A as LFO)"),
                Line::from("  shape      Sine → spike → glitched triangle"),
                Line::from("  angle      Tilts harmonics across the cycle"),
                Line::from("  fold       The folder; strike snaps it open"),
                Line::from("  mod        A's sine into shape/angle/fold"),
                Line::from("  strike     Bind a trigger — vactrol percussion"),
                Line::from(""),
                Line::from("LFO-mode sine publishes as dpo/N/lfo."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" DPO ", theme::chrome_hi())),
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
    state::write_pid_file("dpo", instance);

    let shared = Arc::new(Mutex::new(DpoState::new()));
    if let Ok(p) = state::load_module_state::<state::DpoParamsState>("dpo", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let audio_builder = thread::Builder::new()
        .name(String::from("dpo-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = audio_builder.spawn(move || {
        if let Err(e) = audio_thread(audio_state, instance) {
            eprintln!("[dpo {}] audio thread error: {}", instance, e);
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
            let _ = state::save_module_state("dpo", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::DpoParamsState>("dpo", instance) {
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
                if let Some(slot) = binding_slot(r) {
                    let old = s.get_param(slot);
                    let text = addr.as_ref().map(|a| a.to_string());
                    match r {
                        Row::Strike => {
                            s.strike_src = addr.clone();
                            s.strike_resolved = None;
                        }
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
                        match state::load_patch::<state::DpoParamsState>(&name) {
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
            let _ = state::save_module_state("dpo", instance, &params);
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
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                if matches!(
                    ROWS[s.selected.min(ROWS.len() - 1)],
                    Row::Strike | Row::Amp | Row::Notes
                ) {
                    ex_msg = Some("routing row: @ binds, x unbinds".into());
                    continue;
                }
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
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                if matches!(
                    ROWS[s.selected.min(ROWS.len() - 1)],
                    Row::Strike | Row::Amp | Row::Notes
                ) {
                    continue;
                }
                let slot = s.selected;
                let old = s.get_param(slot);
                let def = DpoState::new();
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
                        Row::Strike => s.strike_src.clone(),
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
                        Row::Strike => s.strike_src.is_some(),
                        Row::Amp => s.amp_src.is_some(),
                        Row::Notes => s.notes_src.is_some(),
                        _ => src_index(r).is_some_and(|k| s.srcs[k].is_some()),
                    };
                    if had {
                        let old = s.get_param(slot);
                        match r {
                            Row::Strike => {
                                s.strike_src = None;
                                s.strike_resolved = None;
                            }
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
    s: &mut DpoState,
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
    let parsed: Result<V, String> = match r {
        Row::Mode => AMode::parse(value)
            .map(|m| V::Usize(m as usize))
            .ok_or_else(|| "mode: free lock sync lfo".into()),
        Row::Strike | Row::Amp | Row::Notes => {
            if value == "-" {
                Ok(V::Src(None))
            } else {
                Ok(V::Src(Some(value.to_string())))
            }
        }
        _ => value
            .parse::<f32>()
            .map(V::F32)
            .map_err(|_| format!("{key}: not a number: {value}")),
    };
    match parsed {
        Ok(v) => {
            // value rows write the row slot; only the routing rows
            // redirect to their binding slot
            let slot = if matches!(r, Row::Strike | Row::Amp | Row::Notes) {
                binding_slot(r).unwrap_or(slot)
            } else {
                slot
            };
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
    fn undo_slots_round_trip() {
        use crate::undo::{ParamUndo, ParamValue as V};
        let mut s = DpoState::new();
        s.set_param(0, V::F32(2.5));
        assert_eq!(s.ratio, 2.5);
        s.set_param(5, V::Usize(2));
        assert_eq!(s.mode, AMode::Sync);
        s.set_param(STRIKE_SLOT, V::Src(Some("sequencer/0/t4".into())));
        assert_eq!(
            s.strike_src.as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t4".into())
        );
        s.set_param(SRC_SLOT_BASE + 4, V::Src(Some("envelope/0/ch2".into())));
        assert_eq!(
            s.srcs[4].as_ref().map(|a| a.to_string()),
            Some("envelope/0/ch2".into())
        );
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = DpoState::new();
        s.ratio = 3.0;
        s.mode = AMode::Sync;
        s.fold = 0.7;
        s.strike_src = SourceAddr::parse("sequencer/0/t4");
        s.notes_src = SourceAddr::parse("sequencer/0/t1");
        s.srcs[5] = SourceAddr::parse("envelope/1/ch1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::DpoParamsState = toml::from_str(&toml).expect("parses");
        let mut s2 = DpoState::new();
        apply_params(&mut s2, &back);
        assert_eq!(s2.ratio, 3.0);
        assert_eq!(s2.mode, AMode::Sync);
        assert!((s2.fold - 0.7).abs() < 1e-6);
        assert_eq!(
            s2.strike_src.as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t4".into())
        );
        assert_eq!(
            s2.srcs[5].as_ref().map(|a| a.to_string()),
            Some("envelope/1/ch1".into())
        );
        let empty: state::DpoParamsState = toml::from_str("").expect("parses");
        let mut s3 = DpoState::new();
        apply_params(&mut s3, &empty);
        assert_eq!(s3.ratio, 1.0);
    }

    #[test]
    fn ex_set_parses() {
        let mut s = DpoState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "ratio", "2").contains("2.00"));
        assert!(ex_set(&mut s, &mut h, "mode", "sync").contains("sync"));
        assert!(ex_set(&mut s, &mut h, "strike", "sequencer/0/t4").contains("t4"));
        assert!(ex_set(&mut s, &mut h, "wow", "1").contains("Unknown"));
    }
}
