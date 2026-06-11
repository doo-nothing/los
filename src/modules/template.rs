//! # The template module — a worked example for writing your own
//!
//! This file is a complete, fully-wired los module kept deliberately small
//! so you can read it top to bottom in one sitting. It is real — `los add
//! template` spawns it and it makes sound — but every section exists to
//! teach the pattern, and the comments say *why*, not just *what*.
//!
//! ## What it does
//!
//! A little LFO you can hear: a sine drone (so the audio path is audible
//! immediately) with the LFO applied as tremolo, plus the LFO published as
//! a modulation source other modules can patch in with `@`.
//!
//! ## The shape of every los module
//!
//! A module is **one OS process in one tmux pane**. There is no plugin API
//! and no trait to implement — a module is a `pub fn run(instance) ->
//! Result<()>` that follows the conventions below (DESIGN.md §9 has the
//! formal lifecycle):
//!
//! 1. **Announce yourself.** Write a PID file (so the conductor can signal
//!    you) and register in the shared-memory *manifest* (so every other
//!    module can discover you).
//! 2. **Claim your I/O.** Audio out is a per-module SHM ringbuffer the
//!    mixer discovers and consumes; modulation out is a claimed range of
//!    channels on the 64-channel *modbus*; modulation *in* is a stored
//!    source address (`"envelope/0/ch1"`) resolved to a live modbus
//!    channel through the manifest.
//! 3. **Split real-time from UI.** A background thread produces audio
//!    blocks on its own clock; the main thread runs the ratatui event
//!    loop. They share one `Arc<Mutex<State>>` — locks are held only for
//!    a 64-frame block (~1.3 ms of audio), which is the house style
//!    (the mixer does the same inside its cpal callback).
//! 4. **Speak the editing grammar.** vi keys per docs/keybindings.md:
//!    j/k navigate, h/l adjust (the axis rule), counts, `H`/`L` coarse,
//!    `0` reset, `@` bind / `x` unbind, `u`/`Ctrl-r` undo, `:` ex line,
//!    `?` help, `Space` transport, `Ctrl-s` save.
//! 5. **Persist on demand.** SIGUSR1 = save state, SIGUSR2 = reload; the
//!    conductor orchestrates whole-session saves through these.
//!
//! ## The integration checklist
//!
//! Beyond this file, a new module touches five places (all marked with
//! `template` so you can grep for the full set):
//!
//! - `src/modules.rs` — declare the module
//! - `src/lib.rs` — re-export it at the crate root
//! - `src/main.rs` — `dispatch_module` arm + usage text
//! - `src/modules/conductor.rs` — `canonical_module` + `ADDABLE_MODULES`
//! - `src/ipc/routing.rs` — `output_labels` for your claimed mod outputs
//! - `src/session/state.rs` — your `Params` struct (all fields optional
//!   or defaulted, so old save files keep loading)
//!
//! docs/writing-a-module.md walks the checklist; docs/writing-dsp.md
//! covers writing the audio-rate core in a DSP language instead of Rust.

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

// ── parameters ─────────────────────────────────────────────────────────────
//
// Each module defines its own param enum. The enum is the single source of
// truth for labels, ranges, step sizes, defaults, and how a patched modbus
// value maps onto the range — keeping all of that in one place is what
// makes the key handler and the renderer impossible to desynchronize.

/// The rows of the param list, top to bottom. The UI is a vertical list,
/// so by the axis rule j/k selects a row and h/l turns the knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Param {
    /// LFO rate in Hz.
    Rate,
    /// LFO waveshape (sine / triangle / saw / square) — an enum-ish param.
    Shape,
    /// Tremolo depth: how hard the LFO works the drone's amplitude.
    Depth,
    /// Drone pitch in Hz (audible carrier so the LFO is heard, not imagined).
    Pitch,
    /// Drone output level.
    Level,
    /// Published LFO polarity: bipolar ±1 (classic CV) or unipolar 0..1
    /// (plays nicer with receivers that clamp to 0..1, like a fader).
    Polar,
}

/// Row order — also the undo-slot order (see `ParamUndo` below).
const ROWS: [Param; 6] = [
    Param::Rate,
    Param::Shape,
    Param::Depth,
    Param::Pitch,
    Param::Level,
    Param::Polar,
];

/// Which params accept a mod-source binding, in `srcs[]` index order.
/// Shape and Polar stay manual-only on purpose: switching a waveform from
/// a control signal needs a deliberate mapping story, and a template is
/// the wrong place to invent one. This is also your example of *not*
/// making something bindable.
const BINDABLE: [Param; 4] = [Param::Rate, Param::Depth, Param::Pitch, Param::Level];

const RATE_MIN: f32 = 0.05;
const RATE_MAX: f32 = 20.0;
const PITCH_MIN: f32 = 55.0;
const PITCH_MAX: f32 = 1760.0;

impl Param {
    fn label(self) -> &'static str {
        match self {
            Param::Rate => "rate",
            Param::Shape => "shape",
            Param::Depth => "depth",
            Param::Pitch => "pitch",
            Param::Level => "level",
            Param::Polar => "polar",
        }
    }

    fn default_value(self) -> f32 {
        match self {
            Param::Rate => 2.0,
            Param::Shape => 0.0, // sine
            Param::Depth => 0.5,
            Param::Pitch => 220.0,
            Param::Level => 0.5,
            Param::Polar => 0.0, // bipolar
        }
    }

    /// `srcs[]` index for a bindable param, None for manual-only rows.
    fn src_index(self) -> Option<usize> {
        BINDABLE.iter().position(|p| *p == self)
    }

    /// Map a raw modbus value onto this param's range. A bound source
    /// REPLACES the manual value (los-wide convention — the knob becomes
    /// a display while the cable is plugged in). Modbus values are raw
    /// f32s with no inherent range, so every receiver decides its own
    /// mapping; keep it predictable: clamp, then scale.
    fn map_mod(self, v: f32) -> f32 {
        match self {
            Param::Rate => RATE_MIN + v.clamp(0.0, 1.0) * (RATE_MAX - RATE_MIN),
            // 0..1 spans the five octaves 55 Hz → 1760 Hz, exponentially —
            // equal CV steps are equal musical intervals.
            Param::Pitch => PITCH_MIN * (PITCH_MAX / PITCH_MIN).powf(v.clamp(0.0, 1.0)),
            Param::Depth | Param::Level => v.clamp(0.0, 1.0),
            // unreachable in practice (not bindable), but exhaustive match
            // is house style — a new variant must be handled to compile.
            Param::Shape | Param::Polar => v,
        }
    }
}

/// The LFO waveshapes, cycled with h/l on the Shape row.
const SHAPES: [&str; 4] = ["sine", "tri", "saw", "sqr"];

// ── shared state ───────────────────────────────────────────────────────────

/// Everything the UI edits and the audio thread reads. One mutex, two
/// threads, short critical sections.
struct TemplateState {
    rate: f32,
    /// Index into SHAPES.
    shape: usize,
    depth: f32,
    pitch: f32,
    level: f32,
    /// true = publish the LFO unipolar (0..1).
    unipolar: bool,
    /// Stored bindings, one per BINDABLE param. These are *addresses*
    /// ("envelope/0/ch1"), not channel numbers — addresses survive the
    /// source module restarting (it re-claims fresh channels; resolution
    /// re-finds them through the manifest).
    srcs: [Option<SourceAddr>; 4],
    /// Live modbus channels for `srcs`, re-resolved periodically by the
    /// audio thread. Split from `srcs` so the hot loop never parses
    /// strings or walks the manifest.
    resolved: [Option<usize>; 4],
    /// Effective values as the audio thread last used them — what the UI
    /// shows on bound rows (the "ghost": you see the cable working).
    eff: [f32; 4],
    /// Live LFO value, for the little meter in the UI.
    lfo_now: f32,
    /// Cursor row (index into ROWS).
    selected: usize,
}

impl TemplateState {
    fn new() -> Self {
        Self {
            rate: Param::Rate.default_value(),
            shape: 0,
            depth: Param::Depth.default_value(),
            pitch: Param::Pitch.default_value(),
            level: Param::Level.default_value(),
            unipolar: false,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [
                Param::Rate.default_value(),
                Param::Depth.default_value(),
                Param::Pitch.default_value(),
                Param::Level.default_value(),
            ],
            lfo_now: 0.0,
            selected: 0,
        }
    }

    fn get(&self, p: Param) -> f32 {
        match p {
            Param::Rate => self.rate,
            Param::Shape => self.shape as f32,
            Param::Depth => self.depth,
            Param::Pitch => self.pitch,
            Param::Level => self.level,
            Param::Polar => self.unipolar as u8 as f32,
        }
    }

    fn set(&mut self, p: Param, v: f32) {
        match p {
            Param::Rate => self.rate = v.clamp(RATE_MIN, RATE_MAX),
            Param::Shape => self.shape = (v as usize).min(SHAPES.len() - 1),
            Param::Depth => self.depth = v.clamp(0.0, 1.0),
            Param::Pitch => self.pitch = v.clamp(PITCH_MIN, PITCH_MAX),
            Param::Level => self.level = v.clamp(0.0, 1.0),
            Param::Polar => self.unipolar = v >= 0.5,
        }
    }

    /// The value the audio thread should use right now: the live mod
    /// source when bound and resolvable, else the manual knob.
    fn effective(&self, p: Param, bus: Option<&ModulationBus>) -> f32 {
        match (p.src_index().and_then(|i| self.resolved[i]), bus) {
            (Some(ch), Some(bus)) => p.map_mod(bus.get(ch)),
            _ => self.get(p),
        }
    }

    fn current(&self) -> Param {
        ROWS[self.selected.min(ROWS.len() - 1)]
    }
}

// ── undo ───────────────────────────────────────────────────────────────────
//
// Undo is the shared `ParamHistory` from src/ui/undo.rs: you map your
// editable fields onto numbered slots, record old/new pairs around every
// mutation, and get vi-correct behavior (sweep coalescing, counts, redo
// truncation) for free. Slots are arbitrary as long as they're stable:
// here, 0..6 are the rows in ROWS order and 10+i are the four bindings.

const SRC_SLOT_BASE: usize = 10;

impl crate::undo::ParamUndo for TemplateState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            let src = self.srcs.get(i)?;
            return Some(V::Src(src.as_ref().map(|a| a.to_string())));
        }
        let p = *ROWS.get(slot)?;
        Some(match p {
            Param::Shape => V::Usize(self.shape),
            Param::Polar => V::Bool(self.unipolar),
            _ => V::F32(self.get(p)),
        })
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            if let (Some(s), V::Src(v)) = (self.srcs.get_mut(i), value) {
                *s = v.as_deref().and_then(SourceAddr::parse);
                self.resolved[i] = None; // re-resolved on the next refresh
            }
            return;
        }
        let Some(p) = ROWS.get(slot).copied() else {
            return;
        };
        match (p, value) {
            (Param::Shape, V::Usize(v)) => self.shape = v.min(SHAPES.len() - 1),
            (Param::Polar, V::Bool(v)) => self.unipolar = v,
            (_, V::F32(v)) => self.set(p, v),
            _ => {}
        }
    }
}

// ── persistence ────────────────────────────────────────────────────────────
//
// Two snapshot/apply functions bridge live state ↔ the serde struct in
// src/session/state.rs. The same pair serves all three persistence paths:
// SIGUSR1/2 session saves, Ctrl-s, and `:w`/`:e` patches.

fn snapshot_params(s: &TemplateState) -> state::TemplateParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::TemplateParams {
        format: state::STATE_FORMAT,
        rate: Some(s.rate),
        shape: Some(SHAPES[s.shape].to_string()),
        depth: Some(s.depth),
        pitch: Some(s.pitch),
        level: Some(s.level),
        unipolar: Some(s.unipolar),
        rate_src: src(0),
        depth_src: src(1),
        pitch_src: src(2),
        level_src: src(3),
    }
}

fn apply_params(s: &mut TemplateState, p: &state::TemplateParams) {
    // Every field is Optional so a save from an older build (missing
    // fields) loads cleanly instead of clobbering defaults with zeros.
    if let Some(v) = p.rate {
        s.set(Param::Rate, v);
    }
    if let Some(ref name) = p.shape {
        if let Some(i) = SHAPES.iter().position(|n| n == name) {
            s.shape = i;
        }
    }
    if let Some(v) = p.depth {
        s.set(Param::Depth, v);
    }
    if let Some(v) = p.pitch {
        s.set(Param::Pitch, v);
    }
    if let Some(v) = p.level {
        s.set(Param::Level, v);
    }
    if let Some(v) = p.unipolar {
        s.unipolar = v;
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.rate_src),
        parse(&p.depth_src),
        parse(&p.pitch_src),
        parse(&p.level_src),
    ];
    s.resolved = Default::default();
}

// ── the audio thread ───────────────────────────────────────────────────────
//
// Free-running block producer, modeled on voice.rs. It owns every
// real-time resource: the audio ringbuffer, the manifest registration
// (keeping the handle alive keeps the manifest slot alive — dropping it
// unregisters us), and the modbus connection.
//
// los's audio graph in one sentence: producers each write 64-frame
// stereo blocks into their own SHM ringbuffer at their own pace, and the
// mixer — the only process talking to the OS audio device — consumes
// every ringbuffer it finds in the manifest and sums them. There is no
// global callback into your code and no hard deadline: if you fall
// behind, your channel drops out, nothing else does.

/// One ringbuffer slot is 64 frames ≈ 1.3 ms at 48 kHz. We sleep for one
/// slot per iteration; the ringbuffer (16 slots deep) absorbs the jitter.
/// Used only until the transport answers with the device's real rate.
const FALLBACK_RATE: f32 = 48_000.0;

fn audio_thread(state: Arc<Mutex<TemplateState>>, instance: usize) -> Result<()> {
    // 1. Create our audio-out ringbuffer. The name pattern is load-bearing:
    //    the mixer labels the strip from it, and DESIGN.md §7.1 reserves
    //    the /los_audio_<module>_<instance> namespace.
    let shm_name = format!("/los_audio_template_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating SHM audio ringbuffer")?;

    // 2. Register in the manifest: name, instance, our audio SHM, and a
    //    claim of 1 modbus channel for the LFO output. The claim is a
    //    contiguous range sized at registration — claim your maximum up
    //    front, you can't grow it later. routing::output_labels() must
    //    list exactly this many labels for "template".
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("template", instance, Some(&shm_name), 1)?;
    let mod_base = manifest.claimed_base();

    // 3. The modbus: 64 shared f32 channels. We write ours, we read the
    //    ones our bindings resolve to. Open-or-create because module
    //    start order is undefined.
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();

    let channels = ringbuf.channels() as usize;
    let slot_frames = ringbuf.slot_frames() as usize;
    let mut block = vec![0.0_f32; ringbuf.slot_len()];

    // The mixer owns the audio device and publishes its real sample rate
    // on the transport (it may not be 48 k!). Read it here and refresh in
    // the slow path below — the transport might not exist yet if we
    // started before the mixer.
    let mut transport = ShmTransport::open().ok();
    let rate_of = |t: &Option<ShmTransport>| {
        t.as_ref()
            .map(|t| t.sample_rate() as f32)
            .filter(|r| *r > 0.0)
            .unwrap_or(FALLBACK_RATE)
    };
    let mut sample_rate = rate_of(&transport);
    let mut slot_duration =
        Duration::from_nanos((slot_frames as f64 / sample_rate as f64 * 1e9) as u64);

    let mut lfo_phase = 0.0_f32; // 0..1
    let mut carrier_phase = 0.0_f32; // 0..1
    let mut blocks: u64 = 0;

    loop {
        let tick = Instant::now();

        // Re-resolve bindings every ~128 blocks (~170 ms): address →
        // channel lookups walk the manifest, so they stay out of the
        // per-block path, but a restarted source is picked up within a
        // couple of UI frames. (voice.rs uses the same cadence.)
        if blocks.is_multiple_of(128) {
            if transport.is_none() {
                transport = ShmTransport::open().ok();
            }
            let now_rate = rate_of(&transport);
            if (now_rate - sample_rate).abs() > 0.5 {
                sample_rate = now_rate;
                slot_duration =
                    Duration::from_nanos((slot_frames as f64 / sample_rate as f64 * 1e9) as u64);
            }
            let entries = manifest.entries();
            let mut s = state.lock().unwrap();
            for i in 0..s.srcs.len() {
                s.resolved[i] = s.srcs[i]
                    .as_ref()
                    .and_then(|a| routing::resolve(&entries, a));
            }
        }

        // Snapshot the effective params under one short lock, then render
        // the block without holding it. `effective()` is where a plugged
        // cable replaces the knob.
        let (rate, shape, depth, pitch, level, unipolar) = {
            let mut s = state.lock().unwrap();
            let bus = modbus.as_ref();
            let eff = (
                s.effective(Param::Rate, bus),
                s.shape,
                s.effective(Param::Depth, bus),
                s.effective(Param::Pitch, bus),
                s.effective(Param::Level, bus),
                s.unipolar,
            );
            s.eff = [eff.0, eff.2, eff.3, eff.4]; // ghost display
            eff
        };

        // ── the DSP ────────────────────────────────────────────────────
        // Hand-rolled here because it's eight lines; for anything bigger,
        // docs/writing-dsp.md shows the Faust path (write a .dsp file,
        // commit the generated Rust, call it from exactly this spot).
        let lfo_inc = rate / sample_rate;
        let car_inc = pitch / sample_rate;
        let mut lfo = 0.0;
        for frame in 0..slot_frames {
            lfo = match SHAPES[shape] {
                "tri" => 1.0 - 4.0 * (lfo_phase - 0.5).abs(),
                "saw" => 2.0 * lfo_phase - 1.0,
                "sqr" => {
                    if lfo_phase < 0.5 {
                        1.0
                    } else {
                        -1.0
                    }
                }
                _ => (lfo_phase * std::f32::consts::TAU).sin(),
            };
            // Tremolo: LFO at +1 leaves the drone alone, at -1 it cuts up
            // to `depth` of the amplitude. Audible proof the LFO runs.
            let gain = level * (1.0 - depth * (0.5 - 0.5 * lfo));
            let sample = (carrier_phase * std::f32::consts::TAU).sin() * gain * 0.5;
            for ch in 0..channels {
                block[frame * channels + ch] = sample;
            }
            lfo_phase = (lfo_phase + lfo_inc).fract();
            carrier_phase = (carrier_phase + car_inc).fract();
        }

        // Publish the LFO on our claimed channel — this is what other
        // modules see in the `@` picker as `template/<i>/lfo`. Once per
        // block (~750 Hz) is the modbus's native resolution.
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            bus.set(base, if unipolar { 0.5 + 0.5 * lfo } else { lfo });
        }
        if let Ok(mut s) = state.lock() {
            s.lfo_now = lfo;
        }

        // Write the block. Full ring = the mixer is gone or stalled; spin
        // gently rather than erroring out, exactly like tone.rs.
        while ringbuf.write(&block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }

        blocks += 1;
        let elapsed = tick.elapsed();
        if elapsed < slot_duration {
            thread::sleep(slot_duration - elapsed);
        }
    }
}

// ── rendering ──────────────────────────────────────────────────────────────

/// Normalized 0..1 position for a param value, for drawing its bar.
fn norm(p: Param, v: f32) -> f32 {
    match p {
        Param::Rate => (v - RATE_MIN) / (RATE_MAX - RATE_MIN),
        Param::Pitch => (v / PITCH_MIN).log2() / (PITCH_MAX / PITCH_MIN).log2(),
        Param::Depth | Param::Level => v,
        Param::Shape => v / (SHAPES.len() - 1) as f32,
        Param::Polar => v,
    }
}

/// Value text for a row ("2.0 Hz", "220 Hz", "50%", "sine", "±").
fn param_text(p: Param, v: f32) -> String {
    match p {
        Param::Rate => format!("{:.2} Hz", v),
        Param::Pitch => format!("{:.0} Hz", v),
        Param::Depth | Param::Level => format!("{:.0}%", v * 100.0),
        Param::Shape => SHAPES[(v as usize).min(SHAPES.len() - 1)].to_string(),
        Param::Polar => if v >= 0.5 { "+ uni" } else { "± bi" }.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &TemplateState,
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

        // House chrome: header line, body, then rule + status anchored to
        // the bottom. Every theme:: call here is shared with the other
        // modules — never hardcode colors (docs/plans/design-language.md).
        lines.push(theme::header(
            "TEMPLATE",
            &format!("lfo {}", instance),
            "",
            w,
        ));

        // A live one-character meter of the published LFO, because a
        // modulation source you can't see is a modulation source you
        // don't trust. (The scope does this properly; this is the cheap
        // in-module version.)
        let m = 0.5 + 0.5 * s.lfo_now;
        lines.push(Line::from(vec![
            Span::styled("  lfo ".to_string(), theme::chrome()),
            Span::styled(
                theme::meter_char(m).to_string(),
                theme::signal(theme::cv_ramp(m)),
            ),
            Span::styled(
                format!(
                    " {} {:+.2}",
                    if s.unipolar { "uni" } else { "bi " },
                    s.lfo_now
                ),
                theme::dim(),
            ),
        ]));
        lines.push(Line::from(""));

        // The param rows. Reserve width for "  label  [bar] value▸" and
        // size the bar from what's left, via the shared helper so mouse
        // hit-tests (if you add them) use identical geometry.
        let bar_w = theme::bar_width(w, 26);
        for (row, p) in ROWS.iter().enumerate() {
            let selected = row == s.selected;
            let bound = p.src_index().is_some_and(|i| s.srcs[i].is_some());
            // Bound rows show the live (modulated) value, manual rows the
            // knob — same rule as the mixer's ▸ display.
            let shown = match p.src_index() {
                Some(i) if bound => s.eff[i],
                _ => s.get(*p),
            };
            let label_style = if selected {
                theme::selected()
            } else {
                theme::chrome()
            };
            let mut spans: Vec<Span> =
                vec![Span::styled(format!(" {:<6}", p.label()), label_style)];
            // Cable color: a bound param wears its connection's hue, so
            // you can trace a patch across panes by color alone.
            let hue = p
                .src_index()
                .and_then(|i| s.srcs[i].as_ref())
                .map(|a| routing::cable_color(entries, a));
            spans.extend(theme::bar(
                norm(*p, shown),
                None,
                bar_w,
                hue.unwrap_or_else(theme::cv),
            ));
            let vstyle = if selected {
                theme::selected()
            } else if let Some(hue) = hue {
                theme::signal(hue)
            } else {
                theme::value()
            };
            let mark = if bound { theme::BIND } else { ' ' };
            spans.push(Span::styled(
                format!(" {}{}", mark, param_text(*p, shown)),
                vstyle,
            ));
            if let Some(addr) = p.src_index().and_then(|i| s.srcs[i].as_ref()) {
                spans.push(Span::styled(format!("  ◂ {}", addr), theme::dim()));
            }
            lines.push(Line::from(spans));
        }

        theme::anchor_bottom(&mut lines, h, 2);
        lines.push(theme::rule(w));
        lines.push(theme::status("NORMAL", overlay.unwrap_or(""), "", w));
        f.render_widget(Paragraph::new(lines), area);

        // Overlays render on top of the page: `?` help and the `@` picker.
        if show_help {
            let help = Paragraph::new(vec![
                Line::from("━━━ TEMPLATE · the worked example ━━━"),
                Line::from(""),
                Line::from("  j/k        Select param (counts, wraps)"),
                Line::from("  h/l        Adjust (H/L coarse, counts)"),
                Line::from("  0          Reset param to default"),
                Line::from("  @          Bind a mod source · x unbinds"),
                Line::from("  gg / G     First / last param"),
                Line::from("  u / ^r     Undo / redo"),
                Line::from("  :w/:e/:q   Patches / quit · Space transport"),
                Line::from(""),
                Line::from("A sine drone with an LFO on its amplitude."),
                Line::from("The LFO is published as template/N/lfo — patch"),
                Line::from("it anywhere with @. Source: modules/template.rs,"),
                Line::from("the guided tour for writing your own module."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" TEMPLATE ", theme::chrome_hi())),
            );
            f.render_widget(help, area);
        }

        if let Some((rows, sel)) = picker {
            let ph = (rows.len() as u16 + 2).min(area.height);
            let pw = rows.iter().map(|r| r.len()).max().unwrap_or(10).max(20) as u16 + 4;
            let r = ratatui::layout::Rect::new(
                (area.width.saturating_sub(pw)) / 2,
                (area.height.saturating_sub(ph)) / 2,
                pw.min(area.width),
                ph,
            );
            f.render_widget(ratatui::widgets::Clear, r);
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
                    .title(Span::styled(" bind param ", theme::chrome_hi())),
            );
            f.render_widget(list, r);
        }
    })?;
    Ok(())
}

// ── editing helpers ────────────────────────────────────────────────────────

/// Adjust the selected param by doctrine steps. Each param picks its own
/// fine step; coarse is the shared ×10 rule (keys::step_f32).
fn adjust_param(s: &mut TemplateState, steps: i32, coarse: bool) {
    use crate::keys::step_f32;
    let p = s.current();
    let v = s.get(p);
    let new = match p {
        Param::Rate => step_f32(v, steps, 0.05, coarse, RATE_MIN, RATE_MAX),
        Param::Depth | Param::Level => step_f32(v, steps, 0.05, coarse, 0.0, 1.0),
        Param::Pitch => step_f32(v, steps, 1.0, coarse, PITCH_MIN, PITCH_MAX),
        // Enum-ish rows cycle; coarse is meaningless, counts still work.
        Param::Shape => crate::keys::cycle(s.shape, steps, SHAPES.len()) as f32,
        Param::Polar => {
            if steps != 0 {
                (!s.unipolar) as u8 as f32
            } else {
                v
            }
        }
    };
    s.set(p, new);
}

// ── entry point ────────────────────────────────────────────────────────────

pub fn run(instance: usize) -> Result<()> {
    // Startup order matters and is the same for every module
    // (DESIGN.md §9.1): signals first so a session-save can't catch us
    // half-initialized, PID file so the conductor can reach us, then
    // resources, then state, then threads, then UI.
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("template", instance);

    let shared = Arc::new(Mutex::new(TemplateState::new()));

    // Reload state from a previous run of this instance, if any. Errors
    // here mean "no saved state yet" — that's fine, defaults stand.
    if let Ok(p) = state::load_module_state::<state::TemplateParams>("template", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    // The audio thread owns the manifest registration; if it dies the
    // slot is reaped and the mixer drops our strip — exactly what should
    // happen. The UI keeps its own read-only manifest handle.
    let audio_state = Arc::clone(&shared);
    thread::spawn(move || {
        if let Err(e) = audio_thread(audio_state, instance) {
            eprintln!("[template {}] audio thread error: {}", instance, e);
        }
    });

    // Terminal setup. The retry loop matters in practice: tmux respawns
    // panes faster than the old process releases the tty.
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

    // The standard editing kit, all shared components: count prefixes,
    // undo history, the `:` line, the `@` picker.
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
    // The dirty baseline for `:q`'s "unsaved changes" check: the params
    // as last written, serialized — comparison by TOML string keeps the
    // check honest as fields grow.
    let mut baseline =
        state::to_toml_string(&snapshot_params(&shared.lock().unwrap())).unwrap_or_default();

    loop {
        // Conductor signals, polled once per frame. SIGUSR1 = snapshot to
        // the tmp state file (a whole-session save is in progress);
        // SIGUSR2 = someone rewrote that file, load it.
        if state::check_save_signal() {
            let params = snapshot_params(&shared.lock().unwrap());
            let _ = state::save_module_state("template", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::TemplateParams>("template", instance) {
                apply_params(&mut shared.lock().unwrap(), &p);
            }
        }

        // Manifest entries for cable colors, refreshed ~1s — cheap enough
        // to poll, fresh enough to track module restarts.
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

        // 50 ms poll = 20 fps idle redraw; key events render immediately
        // on the next loop. The house event-loop rhythm.
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let ev = event::read()?;

        // Mouse follows the keyboard grammar, never extends it
        // (docs/keybindings.md §Mouse): wheel = h/l on the hovered state,
        // click = select.
        if let Event::Mouse(m) = ev {
            if picker.is_active() || ex.is_active() {
                continue;
            }
            match m.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let steps = if m.kind == MouseEventKind::ScrollUp {
                        1
                    } else {
                        -1
                    };
                    use crate::undo::ParamUndo;
                    let mut s = shared.lock().unwrap();
                    let slot = s.selected;
                    let old = s.get_param(slot);
                    adjust_param(&mut s, steps, false);
                    let new = s.get_param(slot);
                    if let (Some(old), Some(new)) = (old, new) {
                        history.record(slot, "Adjust", old, new);
                    }
                }
                MouseEventKind::Down(_) => {
                    // Param rows start under the header + meter + blank.
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

        // Modal overlays eat the keyboard first: picker, then ex line.
        if picker.is_active() {
            if let crate::picker::PickerEvent::Chosen(addr) = picker.handle_key(key.code) {
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                if let Some(i) = s.current().src_index() {
                    let slot = SRC_SLOT_BASE + i;
                    let old = s.get_param(slot);
                    s.srcs[i] = addr.clone();
                    s.resolved[i] = None;
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
                        match state::load_patch::<state::TemplateParams>(&name) {
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
                    // `:set <param> <value>` — the spoken-word twin of h/l.
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

        // vi two-key sequences: `gg`. Any non-g key cancels the pending g.
        if !matches!(key.code, KeyCode::Char('g')) {
            pending_g = false;
        }
        // Ctrl-bindings before the char match (Ctrl-r arrives as 'r' +
        // CONTROL, which a bare match on Char('r') would shadow).
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
            let _ = state::save_module_state("template", instance, &params);
            ex_msg = Some(String::from("Saved"));
            continue;
        }

        match key.code {
            // Digits accumulate into the count prefix (`5l`, `3j`). The
            // guard order matters: '0' when no count is pending falls
            // through to the reset binding below.
            KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}

            // Axis rule for a vertical list: j/k navigate…
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
            // …h/l adjust, H/L coarse. Every adjustment records an undo
            // edit; the history coalesces a held-down sweep into one entry.
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
                adjust_param(&mut s, steps, coarse);
                let new = s.get_param(slot);
                if let (Some(old), Some(new)) = (old, new) {
                    history.record(slot, "Adjust", old, new);
                }
            }
            KeyCode::Char('0') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let p = s.current();
                let slot = s.selected;
                let old = s.get_param(slot);
                s.set(p, p.default_value());
                if let Some(old) = old {
                    let new = s
                        .get_param(slot)
                        .unwrap_or(ParamValue::F32(p.default_value()));
                    history.record(slot, "Reset", old, new);
                }
            }
            // `@` opens the picker on a bindable row. The picker is a
            // shared component; we only supply the live sources (from the
            // manifest) and the current binding (so the cursor starts on
            // it).
            KeyCode::Char('@') => {
                count.clear();
                let s = shared.lock().unwrap();
                if let Some(i) = s.current().src_index() {
                    let current = s.srcs[i].clone();
                    drop(s);
                    let sources = Manifest::open()
                        .map(|m| routing::live_sources(&m.entries()))
                        .unwrap_or_default();
                    picker.open(sources, current.as_ref());
                } else {
                    ex_msg = Some(format!("{} is not bindable", s.current().label()));
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                if let Some(i) = s.current().src_index() {
                    if s.srcs[i].is_some() {
                        let slot = SRC_SLOT_BASE + i;
                        let old = s.get_param(slot);
                        s.srcs[i] = None;
                        s.resolved[i] = None;
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
            // Space toggles the global transport flag. The drone itself
            // free-runs (like the voices); transport gates the sequencer,
            // not the audio graph.
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
            // Unhandled keys clear a pending count: `5` then `q` is
            // nothing, not a queued five.
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

/// `:set <key> <value>` — accepts every row by label, with the same
/// clamping as the keys. Returns the status-line message.
fn ex_set(
    s: &mut TemplateState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    let Some(p) = ROWS.iter().find(|p| p.label() == key).copied() else {
        return format!(
            "Unknown setting: {} (rate shape depth pitch level polar)",
            key
        );
    };
    let parsed = match p {
        Param::Shape => SHAPES
            .iter()
            .position(|n| *n == value)
            .map(|i| i as f32)
            .ok_or_else(|| format!("shape: one of {}", SHAPES.join(" "))),
        Param::Polar => match value {
            "uni" | "unipolar" | "+" => Ok(1.0),
            "bi" | "bipolar" | "±" => Ok(0.0),
            _ => Err(String::from("polar: uni or bi")),
        },
        _ => value
            .parse::<f32>()
            .map_err(|_| format!("{}: not a number: {}", key, value)),
    };
    match parsed {
        Ok(v) => {
            let slot = ROWS.iter().position(|r| *r == p).unwrap_or(0);
            let old = s.get_param(slot);
            s.set(p, v);
            let new = s.get_param(slot);
            if let (Some(old), Some(new)) = (old, new) {
                history.record(slot, "Set", old, new);
            }
            format!("{} = {}", p.label(), param_text(p, s.get(p)))
        }
        Err(m) => m,
    }
}

// ── tests ──────────────────────────────────────────────────────────────────
//
// Module tests are plain unit tests over the pure parts: param math,
// undo slots, persistence round-trips. The SHM/audio plumbing is covered
// by the ipc tests; don't reach for a live session in `cargo test`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjust_respects_ranges_and_steps() {
        let mut s = TemplateState::new();
        s.selected = 0; // rate
        adjust_param(&mut s, 2, false);
        assert!((s.rate - 2.1).abs() < 1e-4, "fine step is 0.05 Hz");
        adjust_param(&mut s, 1000, true);
        assert_eq!(s.rate, RATE_MAX, "clamps at the top");
        s.selected = 1; // shape cycles
        adjust_param(&mut s, 5, false);
        assert_eq!(s.shape, 1, "5 steps around a 4-cycle lands on tri");
        s.selected = 5; // polar toggles
        adjust_param(&mut s, 1, false);
        assert!(s.unipolar);
    }

    #[test]
    fn mod_mapping_replaces_manual() {
        // rate: 0..1 spans the full range
        assert!((Param::Rate.map_mod(0.0) - RATE_MIN).abs() < 1e-6);
        assert!((Param::Rate.map_mod(1.0) - RATE_MAX).abs() < 1e-6);
        assert_eq!(Param::Rate.map_mod(-5.0), RATE_MIN, "clamped below");
        // pitch: exponential, so 0.5 lands an octave-and-a-half… no —
        // exactly halfway in octaves: 55 * 32^0.5 ≈ 311 Hz
        let mid = Param::Pitch.map_mod(0.5);
        assert!((mid - 311.1).abs() < 1.0, "got {}", mid);
        // unbound effective() falls back to the manual knob
        let s = TemplateState::new();
        assert_eq!(
            s.effective(Param::Level, None),
            Param::Level.default_value()
        );
    }

    #[test]
    fn undo_slots_round_trip_every_param() {
        use crate::undo::{ParamUndo, ParamValue as V};
        let mut s = TemplateState::new();
        s.set_param(0, V::F32(5.0));
        assert_eq!(s.get_param(0), Some(V::F32(5.0)), "rate");
        s.set_param(1, V::Usize(2));
        assert_eq!(s.shape, 2, "shape");
        s.set_param(5, V::Bool(true));
        assert!(s.unipolar, "polar");
        s.set_param(SRC_SLOT_BASE, V::Src(Some("envelope/0/ch1".into())));
        assert_eq!(
            s.srcs[0].as_ref().map(|a| a.to_string()),
            Some("envelope/0/ch1".into())
        );
        s.set_param(SRC_SLOT_BASE, V::Src(None));
        assert!(s.srcs[0].is_none(), "unbind via undo");
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = TemplateState::new();
        s.rate = 7.5;
        s.shape = 3;
        s.unipolar = true;
        s.srcs[2] = SourceAddr::parse("sequencer/0/t4");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::TemplateParams = toml::from_str(&toml).expect("parses");
        let mut s2 = TemplateState::new();
        apply_params(&mut s2, &back);
        assert_eq!(s2.rate, 7.5);
        assert_eq!(s2.shape, 3);
        assert!(s2.unipolar);
        assert_eq!(
            s2.srcs[2].as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t4".into())
        );
        // an empty params file (old save) leaves the defaults standing
        let empty: state::TemplateParams = toml::from_str("").expect("parses");
        let mut s3 = TemplateState::new();
        apply_params(&mut s3, &empty);
        assert_eq!(s3.rate, Param::Rate.default_value());
    }

    #[test]
    fn ex_set_parses_and_rejects() {
        let mut s = TemplateState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert_eq!(ex_set(&mut s, &mut h, "rate", "4"), "rate = 4.00 Hz");
        assert_eq!(ex_set(&mut s, &mut h, "shape", "sqr"), "shape = sqr");
        assert_eq!(ex_set(&mut s, &mut h, "polar", "uni"), "polar = + uni");
        assert!(ex_set(&mut s, &mut h, "rate", "fast").contains("not a number"));
        assert!(ex_set(&mut s, &mut h, "wow", "1").contains("Unknown setting"));
        // out-of-range :set clamps like the keys do
        ex_set(&mut s, &mut h, "rate", "999");
        assert_eq!(s.rate, RATE_MAX);
    }

    #[test]
    fn norm_and_text_cover_every_row() {
        for p in ROWS {
            let v = p.default_value();
            let n = norm(p, v);
            assert!((0.0..=1.0).contains(&n), "{:?} norm {} out of range", p, n);
            assert!(!param_text(p, v).is_empty());
        }
        assert_eq!(param_text(Param::Depth, 0.5), "50%");
        assert_eq!(param_text(Param::Shape, 0.0), "sine");
    }
}
