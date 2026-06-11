//! filterbank — a spectral processor, after the Buchla 296e
//! (docs/plans/filterbank-296e.md).
//!
//! The second fx module: sixteen fixed bands (the 296e's analysis
//! curve), each with a fader, a CV input, and an envelope follower
//! published on the modbus (`filterbank/N/b1…b16`). Two stored spectra
//! (A and B) morph under a knob or CV; the odd↔even **spectral
//! transfer** is the vocoder; **freeze** latches the followers into a
//! spectral hold; the swept center/width **window** is the original
//! 296's programmed spectrum; **spread** staggers the bands in time
//! (the software-only wildcard); **split** pans odd|even into a stereo
//! field. The filters are an all-Faust core (bank16.dsp); everything
//! else is Rust (filterbank/dsp.rs) — see docs/writing-dsp.md.

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

pub mod dsp;

/// The committed Faust codegen for the sixteen filters. Regenerate with
/// `just dsp`; never edit the _gen file.
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
pub mod bank16 {
    use crate::faust::*;
    include!("filterbank/bank16_gen.rs");
}

use dsp::{Xfer, BANDS};

/// Used only until the transport answers with the device's real rate.
const FALLBACK_RATE: f32 = 48_000.0;

const GLOBAL_STRIP: usize = BANDS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalRow {
    /// Which audio source the bank consumes ("send/1", "voice/0", …).
    Input,
    /// A↔B spectrum crossfade.
    Morph,
    /// Spectral transfer: off / o→e / e→o / both (the vocoder switch).
    Transfer,
    /// Latch the followers (spectral hold).
    Freeze,
    /// Programmed-spectrum window center (0..1 across the bank).
    WinCenter,
    /// …and width (1 = wide open).
    WinWidth,
    /// Per-band time stagger.
    Spread,
    /// Odd|even stereo split.
    Split,
    /// Instantaneous input in the mix.
    Dry,
    /// Follower decay (50 ms … 40 s).
    Decay,
}
const GLOBAL_ROWS: [GlobalRow; 10] = [
    GlobalRow::Input,
    GlobalRow::Morph,
    GlobalRow::Transfer,
    GlobalRow::Freeze,
    GlobalRow::WinCenter,
    GlobalRow::WinWidth,
    GlobalRow::Spread,
    GlobalRow::Split,
    GlobalRow::Dry,
    GlobalRow::Decay,
];

const XFERS: [&str; 4] = ["off", "o→e", "e→o", "both"];
fn xfer_from(idx: usize) -> Xfer {
    match idx {
        1 => Xfer::OddToEven,
        2 => Xfer::EvenToOdd,
        3 => Xfer::Both,
        _ => Xfer::Off,
    }
}

/// Global bindable srcs order.
const GSRC: usize = 8;
fn gsrc_index(r: GlobalRow) -> Option<usize> {
    match r {
        GlobalRow::Morph => Some(0),
        GlobalRow::Freeze => Some(1),
        GlobalRow::WinCenter => Some(2),
        GlobalRow::WinWidth => Some(3),
        GlobalRow::Spread => Some(4),
        GlobalRow::Split => Some(5),
        GlobalRow::Dry => Some(6),
        GlobalRow::Decay => Some(7),
        GlobalRow::Input | GlobalRow::Transfer => None,
    }
}

// ── state ──────────────────────────────────────────────────────────────────

struct BankState {
    /// The two stored spectra. Faders edit the one selected by
    /// `edit_bank`; morph crossfades between them live.
    bank_a: [f32; BANDS],
    bank_b: [f32; BANDS],
    /// false = editing A, true = editing B (the `b` key).
    edit_bank: bool,
    /// Per-band CV ins: a bound source replaces the morphed fader (the
    /// 296e's 16 CV inputs).
    band_srcs: [Option<SourceAddr>; BANDS],
    band_resolved: [Option<usize>; BANDS],
    /// Live effective gains (post morph/CV/window), for ghost display.
    band_eff: [f32; BANDS],
    morph: f32,
    /// Index into XFERS.
    xfer: usize,
    freeze: bool,
    wcent: f32,
    wwidth: f32,
    spread: f32,
    split: f32,
    dry: f32,
    decay: f32,
    input: Option<String>,
    input_live: bool,
    gsrcs: [Option<SourceAddr>; GSRC],
    gresolved: [Option<usize>; GSRC],
    geff: [f32; GSRC],
    followers: [f32; BANDS],
    selected: usize,
    sel_row: usize,
}

impl BankState {
    fn new() -> Self {
        Self {
            bank_a: [0.8; BANDS],
            // B ships as an odd/even comb so morph is audible out of
            // the box — sweep it and the spectrum breathes.
            bank_b: std::array::from_fn(|i| if i % 2 == 0 { 0.9 } else { 0.2 }),
            edit_bank: false,
            band_srcs: Default::default(),
            band_resolved: Default::default(),
            band_eff: [0.8; BANDS],
            morph: 0.0,
            xfer: 0,
            freeze: false,
            wcent: 0.5,
            wwidth: 1.0,
            spread: 0.0,
            split: 0.3,
            dry: 0.0,
            decay: 0.3,
            input: None,
            input_live: true,
            gsrcs: Default::default(),
            gresolved: Default::default(),
            geff: [0.0, 0.0, 0.5, 1.0, 0.0, 0.3, 0.0, 0.3],
            followers: [0.0; BANDS],
            selected: GLOBAL_STRIP,
            sel_row: 0,
        }
    }

    fn rows_in(&self, strip: usize) -> usize {
        if strip == GLOBAL_STRIP {
            GLOBAL_ROWS.len()
        } else {
            1
        }
    }

    fn gget(&self, r: GlobalRow) -> f32 {
        match r {
            GlobalRow::Morph => self.morph,
            GlobalRow::Freeze => self.freeze as u8 as f32,
            GlobalRow::WinCenter => self.wcent,
            GlobalRow::WinWidth => self.wwidth,
            GlobalRow::Spread => self.spread,
            GlobalRow::Split => self.split,
            GlobalRow::Dry => self.dry,
            GlobalRow::Decay => self.decay,
            GlobalRow::Input | GlobalRow::Transfer => 0.0,
        }
    }

    /// Effective global value (bound source replaces the knob; every
    /// bindable global is a plain 0..1).
    fn geffective(&self, r: GlobalRow, bus: Option<&ModulationBus>) -> f32 {
        match (gsrc_index(r).and_then(|i| self.gresolved[i]), bus) {
            (Some(ch), Some(bus)) => bus.get(ch).clamp(0.0, 1.0),
            _ => self.gget(r),
        }
    }
}

// ── undo ───────────────────────────────────────────────────────────────────
//
// Slots: band i × 4 + {0 level-A, 1 level-B, 2 src}; globals at
// 1000 + {0 morph, 1 xfer, 2 freeze, 3 wcent, 4 wwidth, 5 spread,
// 6 split, 7 dry, 8 decay, 9 input}; global bindings at 1020 + i.

const GLOBAL_SLOT: usize = 1000;
const GSRC_SLOT: usize = 1020;
const BAND_STRIDE: usize = 4;

impl crate::undo::ParamUndo for BankState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(GSRC_SLOT) {
            let s = self.gsrcs.get(i)?;
            return Some(V::Src(s.as_ref().map(|a| a.to_string())));
        }
        if let Some(k) = slot.checked_sub(GLOBAL_SLOT) {
            return match k {
                0 => Some(V::F32(self.morph)),
                1 => Some(V::Usize(self.xfer)),
                2 => Some(V::Bool(self.freeze)),
                3 => Some(V::F32(self.wcent)),
                4 => Some(V::F32(self.wwidth)),
                5 => Some(V::F32(self.spread)),
                6 => Some(V::F32(self.split)),
                7 => Some(V::F32(self.dry)),
                8 => Some(V::F32(self.decay)),
                9 => Some(V::Src(self.input.clone())),
                _ => None,
            };
        }
        let (b, k) = (slot / BAND_STRIDE, slot % BAND_STRIDE);
        if b >= BANDS {
            return None;
        }
        match k {
            0 => Some(V::F32(self.bank_a[b])),
            1 => Some(V::F32(self.bank_b[b])),
            2 => Some(V::Src(self.band_srcs[b].as_ref().map(|a| a.to_string()))),
            _ => None,
        }
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(GSRC_SLOT) {
            if let (Some(s), V::Src(v)) = (self.gsrcs.get_mut(i), value) {
                *s = v.as_deref().and_then(SourceAddr::parse);
                self.gresolved[i] = None;
            }
            return;
        }
        if let Some(k) = slot.checked_sub(GLOBAL_SLOT) {
            match (k, value) {
                (0, V::F32(v)) => self.morph = v.clamp(0.0, 1.0),
                (1, V::Usize(v)) => self.xfer = v.min(XFERS.len() - 1),
                (2, V::Bool(v)) => self.freeze = v,
                (3, V::F32(v)) => self.wcent = v.clamp(0.0, 1.0),
                (4, V::F32(v)) => self.wwidth = v.clamp(0.0, 1.0),
                (5, V::F32(v)) => self.spread = v.clamp(0.0, 1.0),
                (6, V::F32(v)) => self.split = v.clamp(0.0, 1.0),
                (7, V::F32(v)) => self.dry = v.clamp(0.0, 1.0),
                (8, V::F32(v)) => self.decay = v.clamp(0.0, 1.0),
                (9, V::Src(v)) => self.input = v,
                _ => {}
            }
            return;
        }
        let (b, k) = (slot / BAND_STRIDE, slot % BAND_STRIDE);
        if b >= BANDS {
            return;
        }
        match (k, value) {
            (0, V::F32(v)) => self.bank_a[b] = v.clamp(0.0, 1.0),
            (1, V::F32(v)) => self.bank_b[b] = v.clamp(0.0, 1.0),
            (2, V::Src(v)) => {
                self.band_srcs[b] = v.as_deref().and_then(SourceAddr::parse);
                self.band_resolved[b] = None;
            }
            _ => {}
        }
    }
}

fn slot_at(s: &BankState) -> usize {
    if s.selected == GLOBAL_STRIP {
        GLOBAL_SLOT + s.sel_row.min(GLOBAL_ROWS.len() - 1)
    } else {
        s.selected * BAND_STRIDE + s.edit_bank as usize
    }
}

fn src_slot_at(s: &BankState) -> Option<usize> {
    if s.selected == GLOBAL_STRIP {
        match GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)] {
            GlobalRow::Input => Some(GLOBAL_SLOT + 9),
            r => gsrc_index(r).map(|i| GSRC_SLOT + i),
        }
    } else {
        Some(s.selected * BAND_STRIDE + 2)
    }
}

// ── editing ────────────────────────────────────────────────────────────────

fn adjust(s: &mut BankState, steps: i32, coarse: bool) -> Option<String> {
    use crate::keys::step_f32;
    // fine = 1% of the range, coarse = 5%
    let u = |fine: f32, coarse_u: f32| if coarse { coarse_u } else { fine };
    if s.selected == GLOBAL_STRIP {
        match GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)] {
            GlobalRow::Input => return Some(String::from("@ picks the input source")),
            GlobalRow::Morph => s.morph = step_f32(s.morph, steps, u(0.01, 0.05), false, 0.0, 1.0),
            GlobalRow::Transfer => s.xfer = crate::keys::cycle(s.xfer, steps, XFERS.len()),
            GlobalRow::Freeze => {
                if steps != 0 {
                    s.freeze = !s.freeze;
                }
            }
            GlobalRow::WinCenter => {
                s.wcent = step_f32(s.wcent, steps, u(0.01, 0.05), false, 0.0, 1.0)
            }
            GlobalRow::WinWidth => {
                s.wwidth = step_f32(s.wwidth, steps, u(0.01, 0.05), false, 0.0, 1.0)
            }
            GlobalRow::Spread => {
                s.spread = step_f32(s.spread, steps, u(0.01, 0.05), false, 0.0, 1.0)
            }
            GlobalRow::Split => s.split = step_f32(s.split, steps, u(0.01, 0.05), false, 0.0, 1.0),
            GlobalRow::Dry => s.dry = step_f32(s.dry, steps, u(0.01, 0.05), false, 0.0, 1.0),
            GlobalRow::Decay => s.decay = step_f32(s.decay, steps, u(0.01, 0.05), false, 0.0, 1.0),
        }
    } else {
        let b = s.selected;
        let bank = if s.edit_bank {
            &mut s.bank_b
        } else {
            &mut s.bank_a
        };
        bank[b] = step_f32(bank[b], steps, u(0.01, 0.05), false, 0.0, 1.0);
    }
    None
}

fn reset_current(s: &mut BankState) {
    if s.selected == GLOBAL_STRIP {
        match GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)] {
            GlobalRow::Input => s.input = None,
            GlobalRow::Morph => s.morph = 0.0,
            GlobalRow::Transfer => s.xfer = 0,
            GlobalRow::Freeze => s.freeze = false,
            GlobalRow::WinCenter => s.wcent = 0.5,
            GlobalRow::WinWidth => s.wwidth = 1.0,
            GlobalRow::Spread => s.spread = 0.0,
            GlobalRow::Split => s.split = 0.3,
            GlobalRow::Dry => s.dry = 0.0,
            GlobalRow::Decay => s.decay = 0.3,
        }
    } else if s.edit_bank {
        s.bank_b[s.selected] = if s.selected.is_multiple_of(2) {
            0.9
        } else {
            0.2
        };
    } else {
        s.bank_a[s.selected] = 0.8;
    }
}

// ── persistence ────────────────────────────────────────────────────────────

fn snapshot_params(s: &BankState) -> state::FilterbankParams {
    let src = |o: &Option<SourceAddr>| o.as_ref().map(|a| a.to_string());
    state::FilterbankParams {
        format: state::STATE_FORMAT,
        bank_a: s.bank_a.to_vec(),
        bank_b: s.bank_b.to_vec(),
        morph: Some(s.morph),
        xfer: Some(XFERS[s.xfer.min(XFERS.len() - 1)].to_string()),
        freeze: Some(s.freeze),
        wcent: Some(s.wcent),
        wwidth: Some(s.wwidth),
        spread: Some(s.spread),
        split: Some(s.split),
        dry: Some(s.dry),
        decay: Some(s.decay),
        input: s.input.clone(),
        band_srcs: s
            .band_srcs
            .iter()
            .map(|o| o.as_ref().map(|a| a.to_string()).unwrap_or_default())
            .collect(),
        morph_src: src(&s.gsrcs[0]),
        freeze_src: src(&s.gsrcs[1]),
        wcent_src: src(&s.gsrcs[2]),
        wwidth_src: src(&s.gsrcs[3]),
        spread_src: src(&s.gsrcs[4]),
        split_src: src(&s.gsrcs[5]),
        dry_src: src(&s.gsrcs[6]),
        decay_src: src(&s.gsrcs[7]),
    }
}

fn apply_params(s: &mut BankState, p: &state::FilterbankParams) {
    for (i, v) in p.bank_a.iter().enumerate().take(BANDS) {
        s.bank_a[i] = v.clamp(0.0, 1.0);
    }
    for (i, v) in p.bank_b.iter().enumerate().take(BANDS) {
        s.bank_b[i] = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.morph {
        s.morph = v.clamp(0.0, 1.0);
    }
    if let Some(ref name) = p.xfer {
        if let Some(i) = XFERS.iter().position(|n| n == name) {
            s.xfer = i;
        }
    }
    if let Some(v) = p.freeze {
        s.freeze = v;
    }
    if let Some(v) = p.wcent {
        s.wcent = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.wwidth {
        s.wwidth = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.spread {
        s.spread = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.split {
        s.split = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.dry {
        s.dry = v.clamp(0.0, 1.0);
    }
    if let Some(v) = p.decay {
        s.decay = v.clamp(0.0, 1.0);
    }
    s.input = p.input.clone();
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    for (i, src) in p.band_srcs.iter().enumerate().take(BANDS) {
        s.band_srcs[i] = if src.is_empty() {
            None
        } else {
            SourceAddr::parse(src)
        };
    }
    s.band_resolved = Default::default();
    s.gsrcs = [
        parse(&p.morph_src),
        parse(&p.freeze_src),
        parse(&p.wcent_src),
        parse(&p.wwidth_src),
        parse(&p.spread_src),
        parse(&p.split_src),
        parse(&p.dry_src),
        parse(&p.decay_src),
    ];
    s.gresolved = Default::default();
}

// ── the audio thread ───────────────────────────────────────────────────────
//
// Same shape as the delay's: input-clocked when patched (one block out
// per block in), self-paced silence otherwise so the spread lines and
// followers keep breathing.

fn audio_thread(shared: Arc<Mutex<BankState>>, instance: usize) -> Result<()> {
    let out_name = format!("/los_audio_filterbank_{}", instance);
    let mut out_rb = AudioRingbuf::create(&out_name).context("creating output ringbuffer")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("filterbank", instance, Some(&out_name), BANDS as u32)?;
    let mod_base = manifest.claimed_base();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();

    let slot_frames = out_rb.slot_frames() as usize;
    let slot_len = out_rb.slot_len();

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

    let mut core = dsp::FilterCore::new(sample_rate, slot_frames);
    let mut fx = bank16::Bank16::new();
    fx.init(sample_rate as i32);

    let mut input: Option<AudioRingbuf> = None;
    let mut input_shm: Option<String> = None;
    let mut block = vec![0.0_f32; slot_len];
    let mut scratch = vec![0.0_f32; slot_len];
    let mut blocks: u64 = 0;

    loop {
        if blocks.is_multiple_of(64) {
            if transport.is_none() {
                transport = ShmTransport::open().ok();
            }
            let now_rate = rate_of(&transport);
            if (now_rate - sample_rate).abs() > 0.5 {
                sample_rate = now_rate;
                slot_duration =
                    Duration::from_nanos((slot_frames as f64 / sample_rate as f64 * 1e9) as u64);
                core = dsp::FilterCore::new(sample_rate, slot_frames);
                fx = bank16::Bank16::new();
                fx.init(sample_rate as i32);
            }
            let entries = manifest.entries();
            let desired: Option<String> = {
                let mut s = shared.lock().unwrap();
                for i in 0..GSRC {
                    s.gresolved[i] = s.gsrcs[i]
                        .as_ref()
                        .and_then(|a| routing::resolve(&entries, a));
                }
                for i in 0..BANDS {
                    s.band_resolved[i] = s.band_srcs[i]
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

        let p = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            let morph = s.geffective(GlobalRow::Morph, bus);
            let wcent = s.geffective(GlobalRow::WinCenter, bus);
            let wwidth = s.geffective(GlobalRow::WinWidth, bus);
            let spread = s.geffective(GlobalRow::Spread, bus);
            let split = s.geffective(GlobalRow::Split, bus);
            let dry = s.geffective(GlobalRow::Dry, bus);
            let decay = s.geffective(GlobalRow::Decay, bus);
            let freeze = match (s.gresolved[1], bus) {
                (Some(ch), Some(bus)) => bus.get(ch) >= 0.5,
                _ => s.freeze,
            };
            s.geff = [
                morph,
                freeze as u8 as f32,
                wcent,
                wwidth,
                spread,
                split,
                dry,
                decay,
            ];
            let mut gain = [0.0_f32; BANDS];
            for (i, g) in gain.iter_mut().enumerate() {
                // a band CV replaces the morphed fader (the 296e's CV
                // ins drive the VCAs directly); the window multiplies
                // either way
                let base = match (s.band_resolved[i], bus) {
                    (Some(ch), Some(bus)) => bus.get(ch).clamp(0.0, 1.0),
                    _ => s.bank_a[i] + (s.bank_b[i] - s.bank_a[i]) * morph,
                };
                *g = base * dsp::window_mask(i, wcent, wwidth);
            }
            s.band_eff = gain;
            dsp::BlockParams {
                gain,
                xfer: xfer_from(s.xfer),
                freeze,
                decay,
                spread,
                split,
                dry,
            }
        };

        core.process_block(&mut block, &p, &mut fx);

        let f = core.followers();
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            for (i, v) in f.iter().enumerate() {
                bus.set(base + i, *v);
            }
        }
        if let Ok(mut s) = shared.lock() {
            s.followers = f;
        }

        while out_rb.write(&block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }

        blocks += 1;
        if !got {
            let elapsed = tick.elapsed();
            if elapsed < slot_duration {
                thread::sleep(slot_duration - elapsed);
            }
        }
    }
}

// ── rendering ──────────────────────────────────────────────────────────────

const BAND_W: usize = 4;
const PANEL_W: usize = 26;
const CONSOLE_MIN_H: usize = 14;
/// First fader row in console mode: header + names.
const FADER_TOP: usize = 2;

/// Fader geometry from the pane height — one function for the renderer
/// and the mouse hit-test.
fn fader_rows_for(h: usize) -> usize {
    h.saturating_sub(5).clamp(9, 14)
}

/// Each band's center as a MIDI note, for the pitch-wheel tint: the
/// slider wall reads as a spectrum (terracotta lows → plum highs,
/// brightness rising with octave — the same wheel notes wear).
fn band_pitch(i: usize) -> u8 {
    let hz = dsp::CENTERS[i.min(dsp::BANDS - 1)];
    (69.0 + 12.0 * (hz / 440.0).log2()).round().clamp(0.0, 127.0) as u8
}

fn global_label(r: GlobalRow) -> &'static str {
    match r {
        GlobalRow::Input => "input",
        GlobalRow::Morph => "morph",
        GlobalRow::Transfer => "xfer",
        GlobalRow::Freeze => "frz",
        GlobalRow::WinCenter => "wcent",
        GlobalRow::WinWidth => "wwide",
        GlobalRow::Spread => "sprd",
        GlobalRow::Split => "split",
        GlobalRow::Dry => "dry",
        GlobalRow::Decay => "decay",
    }
}

fn global_text(s: &BankState, r: GlobalRow) -> String {
    let bound = gsrc_index(r).is_some_and(|i| s.gsrcs[i].is_some());
    let shown = |i: usize, manual: f32| if bound { s.geff[i] } else { manual };
    match r {
        GlobalRow::Input => match (&s.input, s.input_live) {
            (None, _) => String::from("— none —"),
            (Some(sel), true) => sel.replace('/', " "),
            (Some(sel), false) => format!("{} ✗", sel.replace('/', " ")),
        },
        GlobalRow::Morph => {
            format!(
                "{:.0}% {}",
                shown(0, s.morph) * 100.0,
                if s.edit_bank { "·B" } else { "·A" }
            )
        }
        GlobalRow::Transfer => XFERS[s.xfer.min(XFERS.len() - 1)].to_string(),
        GlobalRow::Freeze => {
            if shown(1, s.freeze as u8 as f32) >= 0.5 {
                String::from("HOLD")
            } else {
                String::from("run")
            }
        }
        GlobalRow::WinCenter => format!("{:.0}%", shown(2, s.wcent) * 100.0),
        GlobalRow::WinWidth => format!("{:.0}%", shown(3, s.wwidth) * 100.0),
        GlobalRow::Spread => format!("{:.0}ms", shown(4, s.spread) * dsp::SPREAD_MAX * 1000.0),
        GlobalRow::Split => format!("{:.0}%", shown(5, s.split) * 100.0),
        GlobalRow::Dry => format!("{:.0}%", shown(6, s.dry) * 100.0),
        GlobalRow::Decay => {
            let secs = dsp::decay_secs(shown(7, s.decay));
            if secs >= 1.0 {
                format!("{:.1}s", secs)
            } else {
                format!("{:.0}ms", secs * 1000.0)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &BankState,
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

        let ctx = format!(
            "{} · bank {} · {}",
            instance,
            if s.edit_bank { "B" } else { "A" },
            global_text(s, GlobalRow::Input)
        );
        lines.push(theme::header("BANK", &ctx, "", w));

        let panel_line = |row: usize| -> Vec<Span<'static>> {
            let Some(r) = GLOBAL_ROWS.get(row).copied() else {
                return vec![Span::raw(" ".repeat(PANEL_W))];
            };
            let cursor = s.selected == GLOBAL_STRIP && s.sel_row.min(GLOBAL_ROWS.len() - 1) == row;
            let bound = gsrc_index(r).is_some_and(|i| s.gsrcs[i].is_some());
            let mark = if bound { theme::BIND } else { ' ' };
            let mut txt = format!(" {:<5}{}{}", global_label(r), mark, global_text(s, r));
            txt.truncate(PANEL_W);
            while txt.chars().count() < PANEL_W {
                txt.push(' ');
            }
            let style = if cursor {
                theme::selected()
            } else if bound {
                let cable = gsrc_index(r)
                    .and_then(|i| s.gsrcs[i].as_ref())
                    .map(|a| routing::cable_color(entries, a))
                    .unwrap_or_else(theme::clock);
                theme::signal(cable)
            } else if (r == GlobalRow::Input && s.input.is_some() && !s.input_live)
                || (r == GlobalRow::Freeze && s.freeze)
            {
                theme::signal(theme::alert())
            } else {
                theme::value()
            };
            vec![Span::styled(txt, style)]
        };

        let console = h >= CONSOLE_MIN_H && w >= BANDS * BAND_W + 3 + PANEL_W;
        if console {
            let fader_rows = fader_rows_for(h);
            let sep = || Span::styled(" │", theme::chrome());
            let edited = if s.edit_bank { &s.bank_b } else { &s.bank_a };

            // names row
            let mut spans: Vec<Span> = Vec::new();
            for i in 0..BANDS {
                let mut nm = if i < 9 {
                    format!("  b{}", i + 1)
                } else {
                    format!(" b{}", i + 1)
                };
                while nm.chars().count() < BAND_W {
                    nm.push(' ');
                }
                let style = if i == s.selected {
                    theme::selected()
                } else if s.band_srcs[i].is_some() {
                    let cable = s.band_srcs[i]
                        .as_ref()
                        .map(|a| routing::cable_color(entries, a))
                        .unwrap_or_else(theme::clock);
                    theme::signal(cable)
                } else {
                    theme::signal(theme::pitch_color(band_pitch(i)))
                };
                spans.push(Span::styled(nm, style));
            }
            spans.push(sep());
            spans.extend(panel_line(0));
            lines.push(Line::from(spans));

            // the slider wall: edited-bank tick, follower meter fill,
            // ghost at the live effective gain
            let row_of =
                |v: f32| ((1.0 - v.clamp(0.0, 1.0)) * (fader_rows - 1) as f32).round() as usize;
            #[allow(clippy::explicit_counter_loop)] // panel rows continue past the fader loop
            for fr in 0..fader_rows {
                let mut spans: Vec<Span> = Vec::new();
                for (i, &lvl) in edited.iter().enumerate() {
                    // ghost whenever the live gain (morph, CV, window —
                    // bound or not) has left the edited fader behind
                    let ghost =
                        fr == row_of(s.band_eff[i]) && (s.band_eff[i] - lvl).abs() > 0.02;
                    let meter = theme::meter_frac(s.followers[i]);
                    spans.push(Span::raw(" "));
                    // fader column: ghost (the live gain after morph /
                    // CV / window) over knob over rail
                    if ghost {
                        let hue = s.band_srcs[i]
                            .as_ref()
                            .map(|a| routing::cable_color(entries, a))
                            .unwrap_or_else(theme::cv);
                        spans.push(Span::styled(theme::GHOST.to_string(), theme::signal(hue)));
                    } else if let Some(knob) = theme::knob_cell(lvl, fr, fader_rows) {
                        let style = if i == s.selected {
                            theme::selected()
                        } else {
                            theme::value()
                        };
                        spans.push(Span::styled(knob.to_string(), style));
                    } else {
                        spans.push(Span::styled(theme::RAIL.to_string(), theme::chrome()));
                    }
                    // that band's follower as an LED ladder, snug to its
                    // fader — the analysis half of the 296e at a glance
                    let (mc, mstyle) = theme::meter_cell(meter, fr, fader_rows);
                    spans.push(Span::styled(mc.to_string(), mstyle));
                    spans.push(Span::raw(" "));
                }
                spans.push(sep());
                spans.extend(panel_line(fr + 1));
                lines.push(Line::from(spans));
            }
        } else {
            // compact: globals on two lines, a spectrum line, detail
            for chunk in GLOBAL_ROWS.chunks(5) {
                let mut g: Vec<Span> = vec![Span::raw(" ")];
                for r in chunk {
                    let row = GLOBAL_ROWS.iter().position(|x| x == r).unwrap_or(0);
                    let cursor =
                        s.selected == GLOBAL_STRIP && s.sel_row.min(GLOBAL_ROWS.len() - 1) == row;
                    let style = if cursor {
                        theme::selected()
                    } else {
                        theme::value()
                    };
                    g.push(Span::styled(
                        format!("{} {}", global_label(*r), global_text(s, *r)),
                        style,
                    ));
                    g.push(Span::styled(theme::SEP.to_string(), theme::chrome()));
                }
                lines.push(Line::from(g));
            }
            let mut m: Vec<Span> = vec![Span::raw(" ")];
            for i in 0..BANDS {
                let style = if i == s.selected {
                    theme::selected()
                } else {
                    theme::signal(theme::audio())
                };
                m.push(Span::styled(
                    theme::meter_char(theme::meter_frac(s.followers[i])).to_string(),
                    style,
                ));
            }
            if s.selected < BANDS {
                let edited = if s.edit_bank {
                    s.bank_b[s.selected]
                } else {
                    s.bank_a[s.selected]
                };
                m.push(Span::styled(
                    format!(
                        "  › b{} ({:.0} Hz) {} {:.0}%",
                        s.selected + 1,
                        dsp::CENTERS[s.selected],
                        if s.edit_bank { "B" } else { "A" },
                        edited * 100.0
                    ),
                    theme::chrome_hi(),
                ));
            }
            lines.push(Line::from(m));
        }

        theme::anchor_bottom(&mut lines, h, 2);
        lines.push(theme::rule(w));
        lines.push(theme::status("NORMAL", overlay.unwrap_or(""), "", w));
        f.render_widget(Paragraph::new(lines), area);

        if show_help {
            let help = Paragraph::new(vec![
                Line::from("━━━ BANK · spectral processor (296e) ━━━"),
                Line::from(""),
                Line::from("  h/l        Select band (b1–b16, then GLOBAL)"),
                Line::from("  j/k        Global row · K/J or =/- adjust 1% (_/+ 5%)"),
                Line::from("  b          Edit bank A ↔ B (morph blends them)"),
                Line::from("  f          Freeze (latch the followers)"),
                Line::from("  0          Reset · @ bind (band: CV in; input"),
                Line::from("             row: pick the audio source) · x unbind"),
                Line::from("  gg / G     Band 1 / GLOBAL · u/^r undo · : · Space"),
                Line::from(""),
                Line::from("16 fixed bands (100 Hz – 10 kHz). xfer is the"),
                Line::from("vocoder: odd followers drive even VCAs. wcent/"),
                Line::from("wwide sweep the 296's programmed window; sprd"),
                Line::from("staggers bands in time; split pans odd|even."),
                Line::from("Followers publish as filterbank/N/b1…b16."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" BANK ", theme::chrome_hi())),
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
                    .title(Span::styled(" patch ", theme::chrome_hi())),
            );
            f.render_widget(list, r);
        }
    })?;
    Ok(())
}

// ── entry point ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Picking {
    ModSource,
    Input,
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("filterbank", instance);

    let shared = Arc::new(Mutex::new(BankState::new()));
    if let Ok(p) = state::load_module_state::<state::FilterbankParams>("filterbank", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    // The audio thread owns every RT resource. Its setup races the
    // session's boot storm (a dozen modules registering into the CAS
    // manifest at once), so a failure retries instead of dying silent —
    // and persistent errors land in a tmp file the user can find.
    let audio_state = Arc::clone(&shared);
    // A named builder with a roomy stack: generated Faust cores hold
    // their delay lines as big inline arrays (tap8fx ≈ 800 KB), and a
    // debug build materializes extra copies constructing them — the
    // default 2 MB thread stack overflowed and took the whole process
    // (and its pane) down.
    let audio_builder = thread::Builder::new()
        .name(String::from("filterbank-audio"))
        .stack_size(8 * 1024 * 1024);
    let _ = audio_builder.spawn(move || {
        let err_path = state::tmp_dir().join(format!("filterbank_{}.err", instance));
        let _ = std::fs::remove_file(&err_path);
        loop {
            match audio_thread(Arc::clone(&audio_state), instance) {
                Ok(()) => break,
                Err(e) => {
                    let _ = std::fs::write(&err_path, format!("{}", e));
                    eprintln!(
                        "[filterbank {}] audio thread error (retrying): {}",
                        instance, e
                    );
                    thread::sleep(Duration::from_millis(500));
                }
            }
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
    // the band fader currently held by the mouse
    let mut grabbed: Option<usize> = None;
    let mut baseline =
        state::to_toml_string(&snapshot_params(&shared.lock().unwrap())).unwrap_or_default();

    loop {
        if state::check_save_signal() {
            let params = snapshot_params(&shared.lock().unwrap());
            let _ = state::save_module_state("filterbank", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) =
                state::load_module_state::<state::FilterbankParams>("filterbank", instance)
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
            match m.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let steps = if m.kind == MouseEventKind::ScrollUp {
                        1
                    } else {
                        -1
                    };
                    use crate::undo::ParamUndo;
                    let mut s = shared.lock().unwrap();
                    let slot = slot_at(&s);
                    let old = s.get_param(slot);
                    let msg = adjust(&mut s, steps, false);
                    let new = s.get_param(slot);
                    if let (Some(old), Some(new)) = (old, new) {
                        history.record(slot, "Adjust", old, new);
                    }
                    if msg.is_some() {
                        ex_msg = msg;
                    }
                }
                MouseEventKind::Down(_) => {
                    // click selects; clicking the wall grabs that band —
                    // dragging then paints ONLY the grabbed band
                    let h = terminal.size().map(|r| r.height as usize).unwrap_or(0);
                    let mut s = shared.lock().unwrap();
                    let col = m.column as usize;
                    let strip = if col < BANDS * BAND_W { col / BAND_W } else { GLOBAL_STRIP };
                    s.selected = strip;
                    s.sel_row = s.sel_row.min(s.rows_in(s.selected) - 1);
                    let rows = fader_rows_for(h);
                    if strip < BANDS
                        && h >= CONSOLE_MIN_H
                        && (FADER_TOP..FADER_TOP + rows).contains(&(m.row as usize))
                    {
                        grabbed = Some(strip);
                    }
                }
                MouseEventKind::Drag(_) => {
                    let Some(strip) = grabbed else { continue };
                    use crate::undo::{ParamUndo, ParamValue};
                    let h = terminal.size().map(|r| r.height as usize).unwrap_or(0);
                    let rows = fader_rows_for(h);
                    if h < CONSOLE_MIN_H || rows < 2 {
                        continue;
                    }
                    let row = (m.row as usize).clamp(FADER_TOP, FADER_TOP + rows - 1);
                    let value = 1.0 - (row - FADER_TOP) as f32 / (rows - 1) as f32;
                    let mut s = shared.lock().unwrap();
                    let slot = strip * BAND_STRIDE + s.edit_bank as usize;
                    let old = s.get_param(slot);
                    if s.edit_bank {
                        s.bank_b[strip] = value;
                    } else {
                        s.bank_a[strip] = value;
                    }
                    if let Some(old) = old {
                        history.record(slot, "Fader", old, ParamValue::F32(value));
                    }
                }
                MouseEventKind::Up(_) => {
                    grabbed = None;
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
                        if let Some(slot) = src_slot_at(&s) {
                            let old = s.get_param(slot);
                            s.set_param(
                                slot,
                                ParamValue::Src(addr.as_ref().map(|a| a.to_string())),
                            );
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
                        let slot = GLOBAL_SLOT + 9;
                        let old = s.get_param(slot);
                        s.input = None;
                        if let Some(old) = old {
                            history.record(slot, "Unpatch", old, ParamValue::Src(None));
                        }
                    }
                },
                crate::picker::PickerEvent::ChosenSpecial(i) if picking == Picking::Input => {
                    use crate::undo::{ParamUndo, ParamValue};
                    if let Some(sel) = input_options.get(i.saturating_sub(1)).cloned() {
                        let mut s = shared.lock().unwrap();
                        let slot = GLOBAL_SLOT + 9;
                        let old = s.get_param(slot);
                        s.input = Some(sel.clone());
                        s.input_live = true;
                        if let Some(old) = old {
                            history.record(slot, "Patch", old, ParamValue::Src(Some(sel)));
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
                    ExCommand::Edit(name) => {
                        match state::load_patch::<state::FilterbankParams>(&name) {
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
            let _ = state::save_module_state("filterbank", instance, &params);
            ex_msg = Some(String::from("Saved"));
            continue;
        }

        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
            KeyCode::Char('h') | KeyCode::Left => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, -n, BANDS + 1);
                s.sel_row = s.sel_row.min(s.rows_in(s.selected) - 1);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, n, BANDS + 1);
                s.sel_row = s.sel_row.min(s.rows_in(s.selected) - 1);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                let rows = s.rows_in(s.selected);
                s.sel_row = crate::keys::cycle(s.sel_row.min(rows - 1), n, rows);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                let rows = s.rows_in(s.selected);
                s.sel_row = crate::keys::cycle(s.sel_row.min(rows - 1), -n, rows);
            }
            KeyCode::Char(c @ ('-' | '=' | '_' | '+' | 'J' | 'K')) => {
                let n = count.take() as i32;
                let (steps, coarse) = match c {
                    '-' | 'J' => (-n, false),
                    '=' | 'K' => (n, false),
                    '_' => (-n, true),
                    _ => (n, true),
                };
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                let slot = slot_at(&s);
                let old = s.get_param(slot);
                let msg = adjust(&mut s, steps, coarse);
                let new = s.get_param(slot);
                if let (Some(old), Some(new)) = (old, new) {
                    history.record(slot, "Adjust", old, new);
                }
                if msg.is_some() {
                    ex_msg = msg;
                }
            }
            KeyCode::Char('0') => {
                count.clear();
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                let slot = slot_at(&s);
                let old = s.get_param(slot);
                reset_current(&mut s);
                let new = s.get_param(slot);
                if let (Some(old), Some(new)) = (old, new) {
                    history.record(slot, "Reset", old, new);
                }
            }
            // b flips which spectrum the faders edit; f is the freeze.
            KeyCode::Char('b') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                s.edit_bank = !s.edit_bank;
                ex_msg = Some(format!(
                    "editing bank {}",
                    if s.edit_bank { "B" } else { "A" }
                ));
            }
            KeyCode::Char('f') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let slot = GLOBAL_SLOT + 2;
                let old = s.get_param(slot);
                s.freeze = !s.freeze;
                if let Some(old) = old {
                    history.record(slot, "Freeze", old, ParamValue::Bool(s.freeze));
                }
            }
            KeyCode::Char('@') | KeyCode::Enter => {
                count.clear();
                let s = shared.lock().unwrap();
                let on_input = s.selected == GLOBAL_STRIP
                    && GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)] == GlobalRow::Input;
                if on_input {
                    let current = s.input.clone();
                    drop(s);
                    let entries = Manifest::open().map(|m| m.entries()).unwrap_or_default();
                    input_options = entries
                        .iter()
                        .filter(|e| e.audio_shm.is_some())
                        .filter(|e| !(e.module_name == "filterbank" && e.instance == instance))
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
                } else {
                    let bindable = src_slot_at(&s).is_some()
                        && !(s.selected == GLOBAL_STRIP
                            && GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)]
                                == GlobalRow::Transfer);
                    if bindable {
                        let current: Option<SourceAddr> = if s.selected == GLOBAL_STRIP {
                            gsrc_index(GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)])
                                .and_then(|i| s.gsrcs[i].clone())
                        } else {
                            s.band_srcs[s.selected].clone()
                        };
                        drop(s);
                        let sources = Manifest::open()
                            .map(|m| routing::live_sources(&m.entries()))
                            .unwrap_or_default();
                        picking = Picking::ModSource;
                        picker.open(sources, current.as_ref());
                    } else {
                        ex_msg = Some(String::from("not bindable"));
                    }
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let on_input = s.selected == GLOBAL_STRIP
                    && GLOBAL_ROWS[s.sel_row.min(GLOBAL_ROWS.len() - 1)] == GlobalRow::Input;
                let slot = if on_input {
                    Some(GLOBAL_SLOT + 9)
                } else {
                    src_slot_at(&s)
                };
                if let Some(slot) = slot {
                    let old = s.get_param(slot);
                    if !matches!(old, Some(ParamValue::Src(None))) {
                        s.set_param(slot, ParamValue::Src(None));
                        if let Some(old) = old {
                            let desc = if on_input { "Unpatch" } else { "Unbind" };
                            history.record(slot, desc, old, ParamValue::Src(None));
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
                shared.lock().unwrap().selected = GLOBAL_STRIP;
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

/// `:set morph 50 · :set xfer o→e|oe · :set freeze on · :set input send/1`.
fn ex_set(
    s: &mut BankState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    let record = |s: &mut BankState, h: &mut crate::undo::ParamHistory, slot: usize, old| {
        if let (Some(old), Some(new)) = (old, s.get_param(slot)) {
            h.record(slot, "Set", old, new);
        }
    };
    let pct_rows: [(&str, usize); 7] = [
        ("morph", 0),
        ("wcent", 3),
        ("wwide", 4),
        ("sprd", 5),
        ("split", 6),
        ("dry", 7),
        ("decay", 8),
    ];
    if let Some((_, k)) = pct_rows.iter().find(|(n, _)| *n == key) {
        let Ok(pct) = value.trim_end_matches('%').parse::<f32>() else {
            return format!("{}: not a number: {}", key, value);
        };
        let v = (pct / 100.0).clamp(0.0, 1.0);
        let slot = GLOBAL_SLOT + k;
        let old = s.get_param(slot);
        s.set_param(slot, crate::undo::ParamValue::F32(v));
        record(s, history, slot, old);
        return format!("{} = {:.0}%", key, v * 100.0);
    }
    match key {
        "xfer" => {
            let idx = match value {
                "off" => Some(0),
                "oe" | "o→e" | "o>e" => Some(1),
                "eo" | "e→o" | "e>o" => Some(2),
                "both" => Some(3),
                _ => None,
            };
            let Some(idx) = idx else {
                return String::from("xfer: off | oe | eo | both");
            };
            let slot = GLOBAL_SLOT + 1;
            let old = s.get_param(slot);
            s.xfer = idx;
            record(s, history, slot, old);
            format!("xfer = {}", XFERS[idx])
        }
        "freeze" => {
            let v = matches!(value, "on" | "true" | "1" | "hold");
            let slot = GLOBAL_SLOT + 2;
            let old = s.get_param(slot);
            s.freeze = v;
            record(s, history, slot, old);
            format!("freeze = {}", if v { "HOLD" } else { "run" })
        }
        "input" => {
            let slot = GLOBAL_SLOT + 9;
            let old = s.get_param(slot);
            if value == "none" || value.is_empty() {
                s.input = None;
            } else if value.contains('/') {
                s.input = Some(value.to_string());
            } else {
                return String::from("input: module/instance (e.g. send/1) or none");
            }
            record(s, history, slot, old);
            format!("input = {}", s.input.as_deref().unwrap_or("none"))
        }
        _ => format!(
            "Unknown setting: {} (morph xfer freeze wcent wwide sprd split dry decay input)",
            key
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bank16_core_shape() {
        assert_eq!(bank16::FAUST_INPUTS, 1);
        assert_eq!(bank16::FAUST_OUTPUTS, 16);
        let mut map = crate::faust::ParamMap::default();
        bank16::Bank16::build_user_interface_static(&mut map);
        assert!(
            map.params.is_empty(),
            "bank16 declares no widgets: {:?}",
            map.params
        );
    }

    #[test]
    fn adjust_edits_the_selected_bank() {
        let mut s = BankState::new();
        s.selected = 3;
        adjust(&mut s, -4, false);
        assert!((s.bank_a[3] - 0.76).abs() < 1e-6, "fine = 1%");
        adjust(&mut s, -2, true);
        assert!((s.bank_a[3] - 0.66).abs() < 1e-6, "coarse = 5%");
        assert!((s.bank_b[3] - 0.2).abs() < 1e-6, "B untouched");
        s.edit_bank = true;
        adjust(&mut s, 100, true);
        assert_eq!(s.bank_b[3], 1.0);
        assert!((s.bank_a[3] - 0.66).abs() < 1e-6, "A untouched");
        // globals
        s.selected = GLOBAL_STRIP;
        s.sel_row = 2; // xfer cycles
        adjust(&mut s, 1, false);
        assert_eq!(s.xfer, 1);
        s.sel_row = 3; // freeze toggles
        adjust(&mut s, 1, false);
        assert!(s.freeze);
        s.sel_row = 0; // input: hint
        assert!(adjust(&mut s, 1, false).is_some());
    }

    #[test]
    fn undo_slots_round_trip() {
        use crate::undo::{ParamUndo, ParamValue as V};
        let mut s = BankState::new();
        s.set_param(5 * BAND_STRIDE, V::F32(0.1));
        assert_eq!(s.bank_a[5], 0.1);
        s.set_param(5 * BAND_STRIDE + 1, V::F32(0.7));
        assert_eq!(s.bank_b[5], 0.7);
        s.set_param(5 * BAND_STRIDE + 2, V::Src(Some("delay/0/t3".into())));
        assert_eq!(
            s.band_srcs[5].as_ref().map(|a| a.to_string()),
            Some("delay/0/t3".into())
        );
        s.set_param(GLOBAL_SLOT + 1, V::Usize(3));
        assert_eq!(s.xfer, 3);
        s.set_param(GLOBAL_SLOT + 9, V::Src(Some("send/1".into())));
        assert_eq!(s.input.as_deref(), Some("send/1"));
        s.set_param(GSRC_SLOT, V::Src(Some("envelope/1/ch1".into())));
        assert_eq!(
            s.gsrcs[0].as_ref().map(|a| a.to_string()),
            Some("envelope/1/ch1".into())
        );
        // cursor mapping: band slot follows the edited bank
        s.selected = 5;
        assert_eq!(slot_at(&s), 5 * BAND_STRIDE);
        s.edit_bank = true;
        assert_eq!(slot_at(&s), 5 * BAND_STRIDE + 1);
        assert_eq!(src_slot_at(&s), Some(5 * BAND_STRIDE + 2));
        s.selected = GLOBAL_STRIP;
        s.sel_row = 1; // morph
        assert_eq!(src_slot_at(&s), Some(GSRC_SLOT));
        s.sel_row = 2; // xfer: manual only
        assert_eq!(src_slot_at(&s), None);
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = BankState::new();
        s.bank_a[0] = 0.11;
        s.bank_b[15] = 0.99;
        s.morph = 0.4;
        s.xfer = 2;
        s.freeze = true;
        s.input = Some("send/1".into());
        s.band_srcs[7] = SourceAddr::parse("sequencer/1/t1");
        s.gsrcs[0] = SourceAddr::parse("envelope/1/ch1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::FilterbankParams = toml::from_str(&toml).expect("parses");
        let mut s2 = BankState::new();
        apply_params(&mut s2, &back);
        assert!((s2.bank_a[0] - 0.11).abs() < 1e-6);
        assert!((s2.bank_b[15] - 0.99).abs() < 1e-6);
        assert!((s2.morph - 0.4).abs() < 1e-6);
        assert_eq!(s2.xfer, 2);
        assert!(s2.freeze);
        assert_eq!(s2.input.as_deref(), Some("send/1"));
        assert_eq!(
            s2.band_srcs[7].as_ref().map(|a| a.to_string()),
            Some("sequencer/1/t1".into())
        );
        assert_eq!(
            s2.gsrcs[0].as_ref().map(|a| a.to_string()),
            Some("envelope/1/ch1".into())
        );
        // empty save leaves defaults
        let empty: state::FilterbankParams = toml::from_str("").expect("parses");
        let mut s3 = BankState::new();
        apply_params(&mut s3, &empty);
        assert_eq!(s3.bank_a[0], 0.8);
        assert!(s3.input.is_none());
    }

    #[test]
    fn ex_set_parses_and_rejects() {
        let mut s = BankState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert_eq!(ex_set(&mut s, &mut h, "morph", "50"), "morph = 50%");
        assert_eq!(ex_set(&mut s, &mut h, "xfer", "oe"), "xfer = o→e");
        assert_eq!(ex_set(&mut s, &mut h, "freeze", "on"), "freeze = HOLD");
        assert_eq!(ex_set(&mut s, &mut h, "input", "send/1"), "input = send/1");
        assert!(ex_set(&mut s, &mut h, "input", "send").contains("module/instance"));
        assert!(ex_set(&mut s, &mut h, "xfer", "sideways").contains("off | oe"));
        assert!(ex_set(&mut s, &mut h, "qq", "1").contains("Unknown setting"));
    }
}
