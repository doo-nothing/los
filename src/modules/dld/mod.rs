//! # The DLD — dual looping delay (after the 4ms Dual Looping Delay)
//!
//! The clean, clock-locked counterpart to the textural 288 delay: two
//! identical channels around one time base, where delay/loop times are
//! *musical* — beats against the transport's BPM (Ping = the beat), or
//! a free time in ms when decoupled. Infinite Hold freezes a channel
//! into a loop you can window through memory; Reverse is crossfaded;
//! Feedback reaches 110% for blooming dub echoes. The tape engine and
//! its clickless-move invariants live in [`dsp`].
//!
//! Channel A consumes the module's claimed input; channel B is
//! normalized from A (the hardware's `In B` switch jack — patch a
//! second instance for four independent channels). Output: A left,
//! B right (`mono` sums). Loop clocks publish on the modbus as
//! `dld/N/clk·lpa·lpb` — bind an envelope's trigger to a loop clock
//! and the hardware's favorite patch works in two keystrokes.

pub mod dsp;

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
use dsp::{Channel, ChannelParams, TimeSwitch};

/// Per-channel tape length in seconds (~23 MB/channel at 48 k).
pub const MAX_SECS: f32 = 120.0;
const FALLBACK_RATE: f32 = 48_000.0;

// ── rows ───────────────────────────────────────────────────────────────────

/// The vertical row list: channel A, channel B, then globals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Time(usize),
    Switch(usize),
    Fdbk(usize),
    Feed(usize),
    Mix(usize),
    Hold(usize),
    Rev(usize),
    Win(usize),
    Ping,
    Input,
    Mono,
}

const CH_ROWS: usize = 8;
const N_ROWS: usize = CH_ROWS * 2 + 3;
const INPUT_SLOT: usize = CH_ROWS * 2 + 1;

fn row_at(i: usize) -> Row {
    let ch = i / CH_ROWS;
    if ch < 2 {
        match i % CH_ROWS {
            0 => Row::Time(ch),
            1 => Row::Switch(ch),
            2 => Row::Fdbk(ch),
            3 => Row::Feed(ch),
            4 => Row::Mix(ch),
            5 => Row::Hold(ch),
            6 => Row::Rev(ch),
            _ => Row::Win(ch),
        }
    } else {
        match i - CH_ROWS * 2 {
            0 => Row::Ping,
            1 => Row::Input,
            _ => Row::Mono,
        }
    }
}

/// Bindable mod inputs per channel, in srcs[] order: time, fdbk, feed,
/// win, hold trigger, rev trigger.
const N_SRC: usize = 6;

// ── shared state ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ChState {
    time: f32, // knob 1..16
    switch: TimeSwitch,
    fdbk: f32, // 0..1.1
    feed: f32,
    mix: f32,
    hold: bool,
    rev: bool,
    win: f32,
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    /// last trigger-input levels for edge detection (hold, rev)
    trig_last: [f32; 2],
    /// live loop phase for the UI meter
    phase: f32,
}

impl Default for ChState {
    fn default() -> Self {
        Self {
            time: 4.0,
            switch: TimeSwitch::Beats,
            fdbk: 0.5,
            feed: 1.0,
            mix: 0.5,
            hold: false,
            rev: false,
            win: 0.0,
            srcs: Default::default(),
            resolved: Default::default(),
            trig_last: [0.0; 2],
            phase: 0.0,
        }
    }
}

struct DldState {
    ch: [ChState; 2],
    /// 0.0 = Ping follows the transport beat; >0 = free Ping in ms.
    ping_ms: f32,
    mono: bool,
    /// Input selection ("module/instance"), the fx claim.
    input: Option<String>,
    input_live: bool,
    /// Channel pending a memory clear (UI → audio thread).
    clear_req: [bool; 2],
    selected: usize,
}

impl DldState {
    fn new() -> Self {
        Self {
            ch: [ChState::default(), ChState::default()],
            ping_ms: 0.0,
            mono: false,
            input: None,
            input_live: true,
            clear_req: [false; 2],
            selected: 0,
        }
    }
}

// ── undo ───────────────────────────────────────────────────────────────────
// Slots: 0..19 rows in order; 20+k channel A bindings; 30+k channel B.

const SRC_SLOT_BASE: usize = 20;

impl crate::undo::ParamUndo for DldState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            let (c, k) = (i / 10, i % 10);
            if c < 2 && k < N_SRC {
                return Some(V::Src(self.ch[c].srcs[k].as_ref().map(|a| a.to_string())));
            }
            return None;
        }
        Some(match row_at(slot.min(N_ROWS - 1)) {
            Row::Time(c) => V::F32(self.ch[c].time),
            Row::Switch(c) => V::Usize(self.ch[c].switch as usize),
            Row::Fdbk(c) => V::F32(self.ch[c].fdbk),
            Row::Feed(c) => V::F32(self.ch[c].feed),
            Row::Mix(c) => V::F32(self.ch[c].mix),
            Row::Hold(c) => V::Bool(self.ch[c].hold),
            Row::Rev(c) => V::Bool(self.ch[c].rev),
            Row::Win(c) => V::F32(self.ch[c].win),
            Row::Ping => V::F32(self.ping_ms),
            Row::Input => V::Src(self.input.clone()),
            Row::Mono => V::Bool(self.mono),
        })
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            let (c, k) = (i / 10, i % 10);
            if c < 2 && k < N_SRC {
                if let V::Src(v) = value {
                    self.ch[c].srcs[k] = v.as_deref().and_then(SourceAddr::parse);
                    self.ch[c].resolved[k] = None;
                }
            }
            return;
        }
        match (row_at(slot.min(N_ROWS - 1)), value) {
            (Row::Time(c), V::F32(v)) => self.ch[c].time = v.clamp(1.0, 16.0),
            (Row::Switch(c), V::Usize(v)) => {
                self.ch[c].switch =
                    [TimeSwitch::Eighth, TimeSwitch::Beats, TimeSwitch::Plus16][v.min(2)]
            }
            (Row::Fdbk(c), V::F32(v)) => self.ch[c].fdbk = v.clamp(0.0, 1.1),
            (Row::Feed(c), V::F32(v)) => self.ch[c].feed = v.clamp(0.0, 1.0),
            (Row::Mix(c), V::F32(v)) => self.ch[c].mix = v.clamp(0.0, 1.0),
            (Row::Hold(c), V::Bool(v)) => self.ch[c].hold = v,
            (Row::Rev(c), V::Bool(v)) => self.ch[c].rev = v,
            (Row::Win(c), V::F32(v)) => self.ch[c].win = v.clamp(0.0, 1.0),
            (Row::Ping, V::F32(v)) => self.ping_ms = v.clamp(0.0, 10_000.0),
            (Row::Input, V::Src(v)) => self.input = v,
            (Row::Mono, V::Bool(v)) => self.mono = v,
            _ => {}
        }
    }
}

// ── persistence ────────────────────────────────────────────────────────────

fn snapshot_params(s: &DldState) -> state::DldParams {
    let ch = |c: &ChState| state::DldChannelParams {
        time: Some(c.time),
        switch: Some(c.switch.name().to_string()),
        fdbk: Some(c.fdbk),
        feed: Some(c.feed),
        mix: Some(c.mix),
        hold: Some(c.hold),
        rev: Some(c.rev),
        win: Some(c.win),
        time_src: c.srcs[0].as_ref().map(|a| a.to_string()),
        fdbk_src: c.srcs[1].as_ref().map(|a| a.to_string()),
        feed_src: c.srcs[2].as_ref().map(|a| a.to_string()),
        win_src: c.srcs[3].as_ref().map(|a| a.to_string()),
        hold_src: c.srcs[4].as_ref().map(|a| a.to_string()),
        rev_src: c.srcs[5].as_ref().map(|a| a.to_string()),
    };
    state::DldParams {
        format: state::STATE_FORMAT,
        ping_ms: Some(s.ping_ms),
        mono: Some(s.mono),
        input: s.input.clone(),
        a: Some(ch(&s.ch[0])),
        b: Some(ch(&s.ch[1])),
    }
}

fn apply_params(s: &mut DldState, p: &state::DldParams) {
    let apply_ch = |c: &mut ChState, q: &state::DldChannelParams| {
        if let Some(v) = q.time {
            c.time = v.clamp(1.0, 16.0);
        }
        if let Some(ref n) = q.switch {
            if let Some(sw) = TimeSwitch::parse(n) {
                c.switch = sw;
            }
        }
        if let Some(v) = q.fdbk {
            c.fdbk = v.clamp(0.0, 1.1);
        }
        if let Some(v) = q.feed {
            c.feed = v.clamp(0.0, 1.0);
        }
        if let Some(v) = q.mix {
            c.mix = v.clamp(0.0, 1.0);
        }
        if let Some(v) = q.hold {
            c.hold = v;
        }
        if let Some(v) = q.rev {
            c.rev = v;
        }
        if let Some(v) = q.win {
            c.win = v.clamp(0.0, 1.0);
        }
        let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
        c.srcs = [
            parse(&q.time_src),
            parse(&q.fdbk_src),
            parse(&q.feed_src),
            parse(&q.win_src),
            parse(&q.hold_src),
            parse(&q.rev_src),
        ];
        c.resolved = Default::default();
    };
    if let Some(ref q) = p.a {
        apply_ch(&mut s.ch[0], q);
    }
    if let Some(ref q) = p.b {
        apply_ch(&mut s.ch[1], q);
    }
    if let Some(v) = p.ping_ms {
        s.ping_ms = v.clamp(0.0, 10_000.0);
    }
    if let Some(v) = p.mono {
        s.mono = v;
    }
    if p.input.is_some() {
        s.input = p.input.clone();
    }
}

// ── audio thread ───────────────────────────────────────────────────────────

fn audio_thread(shared: Arc<Mutex<DldState>>, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_dld_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating dld ring")?;

    // Three modbus channels: clk, lpa, lpb (gates).
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("dld", instance, Some(&shm_name), 3)?;
    let mod_base = manifest.claimed_base();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();

    let mut transport = ShmTransport::open().ok();
    let rate_of = |t: &Option<ShmTransport>| {
        t.as_ref()
            .map(|t| t.sample_rate() as f32)
            .filter(|r| *r > 0.0)
            .unwrap_or(FALLBACK_RATE)
    };
    let bpm_of = |t: &Option<ShmTransport>| {
        t.as_ref()
            .map(|t| t.bpm())
            .filter(|b| *b > 0.0)
            .unwrap_or(120.0)
    };
    let mut sample_rate = rate_of(&transport);

    let channels = ringbuf.channels() as usize;
    let slot_frames = ringbuf.slot_frames() as usize;
    let mut block = vec![0.0_f32; ringbuf.slot_len()];
    let mut scratch = vec![0.0_f32; ringbuf.slot_len()];
    let mut mono_in = vec![0.0_f32; slot_frames];
    let mut out_a = vec![0.0_f32; slot_frames];
    let mut out_b = vec![0.0_f32; slot_frames];

    let max_samples = (MAX_SECS * sample_rate) as usize;
    let mut tape_a = Channel::new(max_samples);
    let mut tape_b = Channel::new(max_samples);

    let mut input: Option<AudioRingbuf> = None;
    let mut input_shm: Option<String> = None;
    let mut blocks: u64 = 0;

    loop {
        // slow path: bindings, claim, rate
        if blocks.is_multiple_of(64) {
            if transport.is_none() {
                transport = ShmTransport::open().ok();
            }
            let now_rate = rate_of(&transport);
            if (now_rate - sample_rate).abs() > 0.5 {
                sample_rate = now_rate;
                let n = (MAX_SECS * sample_rate) as usize;
                tape_a = Channel::new(n);
                tape_b = Channel::new(n);
            }
            let entries = manifest.entries();
            let desired: Option<String> = {
                let mut s = shared.lock().unwrap();
                for c in 0..2 {
                    for k in 0..N_SRC {
                        s.ch[c].resolved[k] = s.ch[c].srcs[k]
                            .as_ref()
                            .and_then(|a| routing::resolve(&entries, a));
                    }
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

        // acquire one input block (or fall to silence)
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
            mono_in[f] = 0.5 * (block[f * channels] + block[f * channels + 1]);
        }

        // params under one short lock (mod-bound values replace knobs,
        // trigger bindings edge-toggle hold/rev)
        let (pa, pb, mono, clears) = {
            let mut s = shared.lock().unwrap();
            let beat_secs = if s.ping_ms > 0.0 {
                s.ping_ms / 1000.0
            } else {
                60.0 / bpm_of(&transport)
            };
            let bus = modbus.as_ref();
            let mk = |c: usize, s: &mut DldState| -> ChannelParams {
                // trigger edges first (they mutate)
                for (slot, t) in [(4usize, 0usize), (5, 1)] {
                    if let (Some(ch), Some(b)) = (s.ch[c].resolved[slot], bus) {
                        let v = b.get(ch);
                        let last = s.ch[c].trig_last[t];
                        if v > 0.5 && last <= 0.5 {
                            if t == 0 {
                                s.ch[c].hold = !s.ch[c].hold;
                            } else {
                                s.ch[c].rev = !s.ch[c].rev;
                            }
                        }
                        s.ch[c].trig_last[t] = v;
                    }
                }
                let st = &s.ch[c];
                let cv = |k: usize, manual: f32, lo: f32, hi: f32| -> f32 {
                    match (st.resolved[k], bus) {
                        (Some(ch), Some(b)) => lo + b.get(ch).clamp(0.0, 1.0) * (hi - lo),
                        _ => manual,
                    }
                };
                let knob = cv(0, st.time, 1.0, 16.0);
                ChannelParams {
                    delay_samples: dsp::delay_samples(knob, st.switch, beat_secs, sample_rate)
                        .min(MAX_SECS * sample_rate - 512.0),
                    feedback: cv(1, st.fdbk, 0.0, 1.1),
                    feed: cv(2, st.feed, 0.0, 1.0),
                    mix: st.mix,
                    hold: st.hold,
                    reverse: st.rev,
                    window: cv(3, st.win, 0.0, 1.0),
                }
            };
            let pa = mk(0, &mut s);
            let pb = mk(1, &mut s);
            let clears = s.clear_req;
            s.clear_req = [false, false];
            s.ch[0].phase = tape_a.loop_phase;
            s.ch[1].phase = tape_b.loop_phase;
            (pa, pb, s.mono, clears)
        };
        if clears[0] {
            tape_a.clear();
        }
        if clears[1] {
            tape_b.clear();
        }

        tape_a.process(&mono_in, &mut out_a, &pa);
        tape_b.process(&mono_in, &mut out_b, &pb);

        for f in 0..slot_frames {
            let (l, r) = if mono {
                let m = 0.5 * (out_a[f] + out_b[f]);
                (m, m)
            } else {
                (out_a[f], out_b[f])
            };
            block[f * channels] = l;
            if channels > 1 {
                block[f * channels + 1] = r;
            }
        }
        // NaN watchdog — ship silence, rebuild tapes
        if block.iter().any(|v| !v.is_finite()) {
            block.fill(0.0);
            let n = (MAX_SECS * sample_rate) as usize;
            tape_a = Channel::new(n);
            tape_b = Channel::new(n);
        }

        // clocks: gates high for the first 15% of each period
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            let beat = transport
                .as_ref()
                .map(|t| {
                    let spb = 60.0 / t.bpm().max(1.0) * sample_rate;
                    f32::from(u8::from((t.clock() as f32 / spb).fract() < 0.15))
                })
                .unwrap_or(0.0);
            bus.set(base, beat);
            bus.set(base + 1, f32::from(u8::from(tape_a.loop_phase < 0.15)));
            bus.set(base + 2, f32::from(u8::from(tape_b.loop_phase < 0.15)));
        }

        while ringbuf.write(&block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }
        blocks += 1;
    }
}

// ── ui helpers ─────────────────────────────────────────────────────────────

fn ab(c: usize) -> &'static str {
    if c == 0 {
        "A"
    } else {
        "B"
    }
}

fn row_label(r: Row) -> String {
    match r {
        Row::Time(c) => format!("{} time", ab(c)),
        Row::Switch(c) => format!("{} sw", ab(c)),
        Row::Fdbk(c) => format!("{} fdbk", ab(c)),
        Row::Feed(c) => format!("{} feed", ab(c)),
        Row::Mix(c) => format!("{} mix", ab(c)),
        Row::Hold(c) => format!("{} hold", ab(c)),
        Row::Rev(c) => format!("{} rev", ab(c)),
        Row::Win(c) => format!("{} win", ab(c)),
        Row::Ping => "ping".into(),
        Row::Input => "input".into(),
        Row::Mono => "mono".into(),
    }
}

/// (channel, srcs index) for a bindable row.
fn src_index(r: Row) -> Option<(usize, usize)> {
    match r {
        Row::Time(c) => Some((c, 0)),
        Row::Fdbk(c) => Some((c, 1)),
        Row::Feed(c) => Some((c, 2)),
        Row::Win(c) => Some((c, 3)),
        Row::Hold(c) => Some((c, 4)),
        Row::Rev(c) => Some((c, 5)),
        _ => None,
    }
}

fn adjust(s: &mut DldState, steps: i32, coarse: bool) {
    use crate::keys::step_f32;
    let st = steps as f32;
    match row_at(s.selected) {
        Row::Time(c) => {
            let step = if coarse { 1.0 } else { 0.25 };
            s.ch[c].time = (s.ch[c].time + st * step).clamp(1.0, 16.0);
        }
        Row::Switch(c) => {
            let cur = s.ch[c].switch as usize;
            let n = crate::keys::cycle(cur, steps, 3);
            s.ch[c].switch = [TimeSwitch::Eighth, TimeSwitch::Beats, TimeSwitch::Plus16][n];
        }
        Row::Fdbk(c) => s.ch[c].fdbk = step_f32(s.ch[c].fdbk, steps, 0.01, coarse, 0.0, 1.1),
        Row::Feed(c) => s.ch[c].feed = step_f32(s.ch[c].feed, steps, 0.01, coarse, 0.0, 1.0),
        Row::Mix(c) => s.ch[c].mix = step_f32(s.ch[c].mix, steps, 0.01, coarse, 0.0, 1.0),
        Row::Hold(c) => {
            if steps != 0 {
                s.ch[c].hold = !s.ch[c].hold;
            }
        }
        Row::Rev(c) => {
            if steps != 0 {
                s.ch[c].rev = !s.ch[c].rev;
            }
        }
        Row::Win(c) => s.ch[c].win = step_f32(s.ch[c].win, steps, 0.01, coarse, 0.0, 1.0),
        Row::Ping => {
            let step = if coarse { 50.0 } else { 5.0 };
            s.ping_ms = (s.ping_ms + st * step).clamp(0.0, 10_000.0);
        }
        Row::Input => {}
        Row::Mono => {
            if steps != 0 {
                s.mono = !s.mono;
            }
        }
    }
}

fn row_text(s: &DldState, r: Row) -> String {
    match r {
        Row::Time(c) => format!(
            "{:.2} × {} = {:.2} beats",
            s.ch[c].time,
            s.ch[c].switch.name(),
            s.ch[c].switch.beats(s.ch[c].time)
        ),
        Row::Switch(c) => s.ch[c].switch.name().to_string(),
        Row::Fdbk(c) => format!("{:.0}%", s.ch[c].fdbk * 100.0),
        Row::Feed(c) => format!("{:.0}%", s.ch[c].feed * 100.0),
        Row::Mix(c) => format!("{:.0}%", s.ch[c].mix * 100.0),
        Row::Hold(c) => if s.ch[c].hold { "∞ LOOP" } else { "delay" }.into(),
        Row::Rev(c) => if s.ch[c].rev { "◀ rev" } else { "▶ fwd" }.into(),
        Row::Win(c) => format!("{:.0}%", s.ch[c].win * 100.0),
        Row::Ping => {
            if s.ping_ms > 0.0 {
                format!("free {:.0} ms", s.ping_ms)
            } else {
                "♪ transport".into()
            }
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
        Row::Mono => if s.mono { "Σ mono" } else { "A|B stereo" }.into(),
    }
}

fn norm(s: &DldState, r: Row) -> Option<f32> {
    Some(match r {
        Row::Time(c) => (s.ch[c].time - 1.0) / 15.0,
        Row::Fdbk(c) => s.ch[c].fdbk / 1.1,
        Row::Feed(c) => s.ch[c].feed,
        Row::Mix(c) => s.ch[c].mix,
        Row::Win(c) => s.ch[c].win,
        Row::Ping => s.ping_ms / 10_000.0,
        _ => return None,
    })
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &DldState,
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
        lines.push(theme::header("DLD", &format!("loop {}", instance), "", w));

        // loop-phase sweep per channel
        let mut spans = vec![Span::styled("  ".to_string(), theme::chrome())];
        for c in 0..2 {
            spans.push(Span::styled(format!("{} ", ab(c)), theme::chrome()));
            spans.push(Span::styled(
                theme::meter_char(1.0 - s.ch[c].phase).to_string(),
                theme::signal(theme::cv_ramp(s.ch[c].phase)),
            ));
            spans.push(Span::styled(
                if s.ch[c].hold { " ∞   " } else { "     " }.to_string(),
                theme::value(),
            ));
        }
        lines.push(Line::from(spans));
        lines.push(Line::from(""));

        let bar_w = theme::bar_width(w, 34);
        for i in 0..N_ROWS {
            let r = row_at(i);
            let selected = i == s.selected;
            let label_style = if selected {
                theme::selected()
            } else {
                theme::chrome()
            };
            let mut spans: Vec<Span> =
                vec![Span::styled(format!(" {:<7}", row_label(r)), label_style)];
            let bound = src_index(r)
                .map(|(c, k)| s.ch[c].srcs[k].is_some())
                .unwrap_or(false);
            let hue = src_index(r)
                .and_then(|(c, k)| s.ch[c].srcs[k].as_ref())
                .map(|a| routing::cable_color(entries, a));
            if let Some(n) = norm(s, r) {
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
            spans.push(Span::styled(format!(" {}{}", mark, row_text(s, r)), vstyle));
            if let Some(addr) = src_index(r).and_then(|(c, k)| s.ch[c].srcs[k].as_ref()) {
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
                Line::from("━━━ DLD · dual looping delay (after the 4ms) ━━━"),
                Line::from(""),
                Line::from("  j/k h/l    Rows / adjust (H/L coarse, counts)"),
                Line::from("  time·sw    Beats against the Ping (/8 · = · +16)"),
                Line::from("  ping       0 = transport beat · else free ms"),
                Line::from("  hold       Infinite Hold: loop the last `time`"),
                Line::from("  win        Scroll the held loop through memory"),
                Line::from("  rev        Reverse (always crossfaded)"),
                Line::from("  X          Clear this channel's tape (globals: both)"),
                Line::from("  @ / x      Bind / unbind (hold+rev take triggers;"),
                Line::from("             input row picks the audio source)"),
                Line::from(""),
                Line::from("Clocks on the modbus: dld/N/clk·lpa·lpb — bind an"),
                Line::from("envelope trigger to a loop clock and VCA something."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" DLD ", theme::chrome_hi())),
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
                    .title(Span::styled(" bind ", theme::chrome_hi())),
            );
            f.render_widget(list, r);
        }
    })?;
    Ok(())
}

// ── entry point ────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum Picking {
    ModSource,
    Input,
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("dld", instance);

    let shared = Arc::new(Mutex::new(DldState::new()));
    if let Ok(p) = state::load_module_state::<state::DldParams>("dld", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let audio_state = Arc::clone(&shared);
    let audio_builder = thread::Builder::new()
        .name(String::from("dld-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = audio_builder.spawn(move || {
        if let Err(e) = audio_thread(audio_state, instance) {
            eprintln!("[dld {}] audio thread error: {}", instance, e);
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
            let _ = state::save_module_state("dld", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::DldParams>("dld", instance) {
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
                    if row < N_ROWS {
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
                        if let Some((c, k)) = src_index(row_at(s.selected)) {
                            let slot = SRC_SLOT_BASE + c * 10 + k;
                            let old = s.get_param(slot);
                            s.ch[c].srcs[k] = addr.clone();
                            s.ch[c].resolved[k] = None;
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
                    ExCommand::Edit(name) => match state::load_patch::<state::DldParams>(&name) {
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
                        ex_msg = Some(ex_set(&mut shared.lock().unwrap(), &mut history, &k, &v));
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
            let _ = state::save_module_state("dld", instance, &params);
            ex_msg = Some(String::from("Saved"));
            continue;
        }

        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
            KeyCode::Char('j') | KeyCode::Down => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, n, N_ROWS);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, -n, N_ROWS);
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
            KeyCode::Char('X') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                match row_at(s.selected) {
                    Row::Ping | Row::Input | Row::Mono => {
                        s.clear_req = [true, true];
                        ex_msg = Some("cleared A+B".into());
                    }
                    Row::Time(c) | Row::Switch(c) | Row::Fdbk(c) | Row::Feed(c)
                    | Row::Mix(c) | Row::Hold(c) | Row::Rev(c) | Row::Win(c) => {
                        s.clear_req[c] = true;
                        ex_msg = Some(format!("cleared {}", ab(c)));
                    }
                }
            }
            KeyCode::Char('0') => {
                count.clear();
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                let slot = s.selected;
                let old = s.get_param(slot);
                let def = DldState::new();
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
                let r = row_at(s.selected);
                if matches!(r, Row::Input) {
                    let current = s.input.clone();
                    drop(s);
                    let entries = Manifest::open().map(|m| m.entries()).unwrap_or_default();
                    input_options = entries
                        .iter()
                        .filter(|e| e.audio_shm.is_some())
                        .filter(|e| !(e.module_name == "dld" && e.instance == instance))
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
                } else if let Some((c, k)) = src_index(r) {
                    let current = s.ch[c].srcs[k].clone();
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
                let r = row_at(s.selected);
                if matches!(r, Row::Input) {
                    let old = s.get_param(INPUT_SLOT);
                    s.input = None;
                    if let Some(old) = old {
                        history.record(INPUT_SLOT, "Unpatch", old, ParamValue::Src(None));
                    }
                } else if let Some((c, k)) = src_index(r) {
                    if s.ch[c].srcs[k].is_some() {
                        let slot = SRC_SLOT_BASE + c * 10 + k;
                        let old = s.get_param(slot);
                        s.ch[c].srcs[k] = None;
                        s.ch[c].resolved[k] = None;
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
                shared.lock().unwrap().selected = N_ROWS - 1;
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

/// `:set <row> <value>` — rows by dotted label (`a.time 4`, `b.fdbk
/// 0.9`, `ping 350`, `input voice/0`, `input -` to unpatch).
fn ex_set(
    s: &mut DldState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    let labels = [
        "a.time", "a.sw", "a.fdbk", "a.feed", "a.mix", "a.hold", "a.rev", "a.win", "b.time",
        "b.sw", "b.fdbk", "b.feed", "b.mix", "b.hold", "b.rev", "b.win", "ping", "input", "mono",
    ];
    let Some(slot) = labels.iter().position(|l| *l == key) else {
        return format!("Unknown setting: {key} (a.time a.sw … b.win ping input mono)");
    };
    let r = row_at(slot);
    let parsed: Result<V, String> = match r {
        Row::Switch(_) => TimeSwitch::parse(value)
            .map(|sw| V::Usize(sw as usize))
            .ok_or_else(|| "sw: /8 = +16".into()),
        Row::Hold(_) | Row::Rev(_) | Row::Mono => match value {
            "on" | "1" | "true" => Ok(V::Bool(true)),
            "off" | "0" | "false" => Ok(V::Bool(false)),
            _ => Err("on or off".into()),
        },
        Row::Input => {
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
mod modtests {
    use super::*;

    #[test]
    fn rows_cover_and_labels_exist() {
        for i in 0..N_ROWS {
            assert!(!row_label(row_at(i)).is_empty());
        }
    }

    #[test]
    fn undo_slots_round_trip() {
        use crate::undo::{ParamUndo, ParamValue as V};
        let mut s = DldState::new();
        s.set_param(0, V::F32(7.5));
        assert_eq!(s.ch[0].time, 7.5);
        s.set_param(9, V::Usize(2));
        assert_eq!(s.ch[1].switch, TimeSwitch::Plus16);
        s.set_param(13, V::Bool(true));
        assert!(s.ch[1].hold);
        s.set_param(SRC_SLOT_BASE + 1, V::Src(Some("envelope/0/ch1".into())));
        assert_eq!(
            s.ch[0].srcs[1].as_ref().map(|a| a.to_string()),
            Some("envelope/0/ch1".into())
        );
        s.set_param(SRC_SLOT_BASE + 10 + 5, V::Src(Some("sequencer/0/t2".into())));
        assert_eq!(
            s.ch[1].srcs[5].as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t2".into())
        );
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = DldState::new();
        s.ch[0].time = 3.0;
        s.ch[0].switch = TimeSwitch::Eighth;
        s.ch[1].fdbk = 1.05;
        s.ch[1].hold = true;
        s.ping_ms = 250.0;
        s.input = Some("voice/2".into());
        s.ch[0].srcs[4] = SourceAddr::parse("sequencer/0/t8");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::DldParams = toml::from_str(&toml).expect("parses");
        let mut s2 = DldState::new();
        apply_params(&mut s2, &back);
        assert_eq!(s2.ch[0].time, 3.0);
        assert_eq!(s2.ch[0].switch, TimeSwitch::Eighth);
        assert!((s2.ch[1].fdbk - 1.05).abs() < 1e-6);
        assert!(s2.ch[1].hold);
        assert_eq!(s2.ping_ms, 250.0);
        assert_eq!(s2.input.as_deref(), Some("voice/2"));
        assert_eq!(
            s2.ch[0].srcs[4].as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t8".into())
        );
        // empty file leaves defaults standing
        let empty: state::DldParams = toml::from_str("").expect("parses");
        let mut s3 = DldState::new();
        apply_params(&mut s3, &empty);
        assert_eq!(s3.ch[0].time, 4.0);
    }

    #[test]
    fn ex_set_speaks_both_channels() {
        let mut s = DldState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "a.time", "8").contains("8.00"));
        assert!(ex_set(&mut s, &mut h, "b.sw", "+16").contains("+16"));
        assert!(ex_set(&mut s, &mut h, "a.hold", "on").contains("LOOP"));
        assert!(ex_set(&mut s, &mut h, "ping", "350").contains("350"));
        assert!(ex_set(&mut s, &mut h, "input", "voice/0").contains("voice/0"));
        assert!(ex_set(&mut s, &mut h, "wow", "1").contains("Unknown"));
    }
}
