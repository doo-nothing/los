//! # Marbles — the random-sampler shell
//!
//! A pure generator: the transport clock steps the t-generator and
//! the x/y output channels once per musical pulse. t2 is the master
//! (fires every pulse); t1/t3 fire stochastically per the model;
//! x1/x2/x3 draw fresh shaped voltages each pulse and y on a divided
//! clock. The three t-gates emit note events (a note source, band
//! 220+instance·3) and publish as gates; the four CVs publish on the
//! bus as marbles/N/{x1,x2,x3,y}. CV+gate, no audio ring.

// max/min, not clamp, where modbus values land: clamp(NaN) is NaN and
// a stale channel must die at the boundary.
#![allow(clippy::manual_clamp)]

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

use super::dsp::{
    OutputChannel, RandomSequence, RandomStream, TGenerator, TModel,
};
use crate::ipc::routing::{self, SourceAddr};
use crate::shm::{AudioEvent, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;
use crate::theory::scales;

const NOTE_SOURCE_BASE: u8 = 220;

pub const MODEL_NAMES: [&str; 7] = [
    "bernoulli", "clusters", "drums", "independent", "divider", "three_states", "markov",
];
pub const RANGE_NAMES: [&str; 3] = ["0.25x", "1x", "4x"];
pub const Y_DIV_NAMES: [&str; 5] = ["1", "2", "4", "8", "16"];
pub const Y_DIV_VALUES: [u64; 5] = [1, 2, 4, 8, 16];
pub const RANGE_MULT: [f64; 3] = [0.25, 1.0, 4.0];

fn model_from(i: usize) -> TModel {
    match i {
        0 => TModel::ComplementaryBernoulli,
        1 => TModel::Clusters,
        2 => TModel::Drums,
        3 => TModel::IndependentBernoulli,
        4 => TModel::Divider,
        5 => TModel::ThreeStates,
        _ => TModel::Markov,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    TModel,
    TRange,
    TBias,
    TJitter,
    TPw,
    XSpread,
    XBias,
    XSteps,
    XDejaVu,
    XLength,
    XScale,
    YSpread,
    YSteps,
    YDiv,
}

const ROWS: [Row; 14] = [
    Row::TModel,
    Row::TRange,
    Row::TBias,
    Row::TJitter,
    Row::TPw,
    Row::XSpread,
    Row::XBias,
    Row::XSteps,
    Row::XDejaVu,
    Row::XLength,
    Row::XScale,
    Row::YSpread,
    Row::YSteps,
    Row::YDiv,
];

/// The bindable continuous knobs (srcs[] order). Every value knob
/// takes a cable; the discrete rows (model/range/length/scale/div) do not.
const BINDABLE: [Row; 9] = [
    Row::TBias,
    Row::TJitter,
    Row::TPw,
    Row::XSpread,
    Row::XBias,
    Row::XSteps,
    Row::XDejaVu,
    Row::YSpread,
    Row::YSteps,
];
const N_SRC: usize = BINDABLE.len();
const SRC_SLOT_BASE: usize = 40;

fn src_index(r: Row) -> Option<usize> {
    BINDABLE.iter().position(|b| *b == r)
}

struct MarblesState {
    t_model: usize,
    t_range: usize,
    t_bias: f32,
    t_jitter: f32,
    t_pw: f32,
    x_spread: f32,
    x_bias: f32,
    x_steps: f32,
    x_deja_vu: f32,
    x_length: usize, // 1..16
    x_scale: usize,  // index into scale_names()
    y_spread: f32,
    y_steps: f32,
    y_div: usize,
    srcs: [Option<SourceAddr>; N_SRC],
    resolved: [Option<usize>; N_SRC],
    eff: [f32; N_SRC],
    out_now: [f32; 7], // t1,t2,t3,x1,x2,x3,y meters
    selected: usize,
}

fn scale_names() -> &'static [&'static str] {
    // a short, musical subset of the los scale library
    &[
        "chromatic",
        "major",
        "minor",
        "dorian",
        "gong",
        "hirajoshi",
        "whole tone",
        "pelog",
    ]
}

impl MarblesState {
    fn new() -> Self {
        Self {
            t_model: 0,
            t_range: 1,
            t_bias: 0.5,
            t_jitter: 0.0,
            t_pw: 0.5,
            x_spread: 0.5,
            x_bias: 0.5,
            x_steps: 0.5,
            x_deja_vu: 0.0,
            x_length: 8,
            x_scale: 1,
            y_spread: 0.4,
            y_steps: 0.7,
            y_div: 2,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.5; N_SRC],
            out_now: [0.0; 7],
            selected: 0,
        }
    }

    fn get(&self, r: Row) -> f32 {
        match r {
            Row::TBias => self.t_bias,
            Row::TJitter => self.t_jitter,
            Row::TPw => self.t_pw,
            Row::XSpread => self.x_spread,
            Row::XBias => self.x_bias,
            Row::XSteps => self.x_steps,
            Row::XDejaVu => self.x_deja_vu,
            Row::YSpread => self.y_spread,
            Row::YSteps => self.y_steps,
            _ => 0.0,
        }
    }

    fn set(&mut self, r: Row, v: f32) {
        let v = v.max(0.0).min(1.0);
        match r {
            Row::TBias => self.t_bias = v,
            Row::TJitter => self.t_jitter = v,
            Row::TPw => self.t_pw = v,
            Row::XSpread => self.x_spread = v,
            Row::XBias => self.x_bias = v,
            Row::XSteps => self.x_steps = v,
            Row::XDejaVu => self.x_deja_vu = v,
            Row::YSpread => self.y_spread = v,
            Row::YSteps => self.y_steps = v,
            _ => {}
        }
    }
}

impl crate::undo::ParamUndo for MarblesState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(i) = slot.checked_sub(SRC_SLOT_BASE) {
            let s = self.srcs.get(i)?;
            return Some(V::Src(s.as_ref().map(|a| a.to_string())));
        }
        let r = *ROWS.get(slot)?;
        Some(match r {
            Row::TModel => V::Usize(self.t_model),
            Row::TRange => V::Usize(self.t_range),
            Row::XLength => V::Usize(self.x_length),
            Row::XScale => V::Usize(self.x_scale),
            Row::YDiv => V::Usize(self.y_div),
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
            (Row::TModel, V::Usize(v)) => self.t_model = v.min(MODEL_NAMES.len() - 1),
            (Row::TRange, V::Usize(v)) => self.t_range = v.min(2),
            (Row::XLength, V::Usize(v)) => self.x_length = v.clamp(1, 16),
            (Row::XScale, V::Usize(v)) => self.x_scale = v.min(scale_names().len() - 1),
            (Row::YDiv, V::Usize(v)) => self.y_div = v.min(Y_DIV_NAMES.len() - 1),
            (_, V::F32(v)) => self.set(r, v),
            _ => {}
        }
    }
}

fn snapshot_params(s: &MarblesState) -> state::MarblesParams {
    let src = |i: usize| s.srcs[i].as_ref().map(|a| a.to_string());
    state::MarblesParams {
        format: state::STATE_FORMAT,
        t_model: Some(MODEL_NAMES[s.t_model].to_string()),
        t_range: Some(RANGE_NAMES[s.t_range].to_string()),
        t_bias: Some(s.t_bias),
        t_jitter: Some(s.t_jitter),
        t_pw: Some(s.t_pw),
        x_spread: Some(s.x_spread),
        x_bias: Some(s.x_bias),
        x_steps: Some(s.x_steps),
        x_deja_vu: Some(s.x_deja_vu),
        x_length: Some(s.x_length),
        x_scale: Some(scale_names()[s.x_scale].to_string()),
        y_spread: Some(s.y_spread),
        y_steps: Some(s.y_steps),
        y_div: Some(Y_DIV_NAMES[s.y_div].to_string()),
        t_bias_src: src(0),
        t_jitter_src: src(1),
        t_pw_src: src(2),
        x_spread_src: src(3),
        x_bias_src: src(4),
        x_steps_src: src(5),
        x_deja_vu_src: src(6),
        y_spread_src: src(7),
        y_steps_src: src(8),
    }
}

fn apply_params(s: &mut MarblesState, p: &state::MarblesParams) {
    if let Some(m) = p.t_model.as_deref() {
        if let Some(i) = MODEL_NAMES.iter().position(|n| *n == m) {
            s.t_model = i;
        }
    }
    if let Some(r) = p.t_range.as_deref() {
        if let Some(i) = RANGE_NAMES.iter().position(|n| *n == r) {
            s.t_range = i;
        }
    }
    if let Some(v) = p.t_bias {
        s.t_bias = v;
    }
    if let Some(v) = p.t_jitter {
        s.t_jitter = v;
    }
    if let Some(v) = p.t_pw {
        s.t_pw = v;
    }
    if let Some(v) = p.x_spread {
        s.x_spread = v;
    }
    if let Some(v) = p.x_bias {
        s.x_bias = v;
    }
    if let Some(v) = p.x_steps {
        s.x_steps = v;
    }
    if let Some(v) = p.x_deja_vu {
        s.x_deja_vu = v;
    }
    if let Some(v) = p.x_length {
        s.x_length = v.clamp(1, 16);
    }
    if let Some(sc) = p.x_scale.as_deref() {
        if let Some(i) = scale_names().iter().position(|n| *n == sc) {
            s.x_scale = i;
        }
    }
    if let Some(v) = p.y_spread {
        s.y_spread = v;
    }
    if let Some(v) = p.y_steps {
        s.y_steps = v;
    }
    if let Some(d) = p.y_div.as_deref() {
        if let Some(i) = Y_DIV_NAMES.iter().position(|n| *n == d) {
            s.y_div = i;
        }
    }
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    s.srcs = [
        parse(&p.t_bias_src),
        parse(&p.t_jitter_src),
        parse(&p.t_pw_src),
        parse(&p.x_spread_src),
        parse(&p.x_bias_src),
        parse(&p.x_steps_src),
        parse(&p.x_deja_vu_src),
        parse(&p.y_spread_src),
        parse(&p.y_steps_src),
    ];
    s.resolved = Default::default();
}

/// Build a 0..1-per-octave degree set from a los scale name.
fn scale_degrees(name: &str) -> Vec<f32> {
    if name == "chromatic" {
        return Vec::new();
    }
    scales::lookup(name)
        .map(|sc| {
            sc.degrees
                .iter()
                .map(|c| (c / sc.period) as f32)
                .collect()
        })
        .unwrap_or_default()
}

// ── control thread ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
fn control_thread(shared: Arc<Mutex<MarblesState>>, instance: usize) -> Result<()> {
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    // 7 claims: t1,t2,t3 gates + x1,x2,x3,y CVs
    manifest.register("marbles", instance, None, 7)?;
    let mod_base = manifest.claimed_base();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();
    let mut events = EventRingbuf::open_producer().ok();
    let transport = ShmTransport::open().ok();
    let note_base = NOTE_SOURCE_BASE + (instance as u8).saturating_mul(3);

    let mut stream = RandomStream::new(0x4a1b_c0de ^ (instance as u32).wrapping_mul(2_654_435_761));
    let mut t_gen = TGenerator::new(&mut stream);
    let mut x_seq: [RandomSequence; 3] = [
        RandomSequence::new(&mut stream),
        RandomSequence::new(&mut stream),
        RandomSequence::new(&mut stream),
    ];
    let mut y_seq = RandomSequence::new(&mut stream);
    let mut x_ch: [OutputChannel; 3] = Default::default();
    let mut y_ch = OutputChannel::default();
    let mut last_scale_name = String::new();
    let mut cur_scale: Vec<f32> = Vec::new();

    let mut last_step: Option<u64> = None;
    let mut gate_off_at: [Option<u64>; 3] = [None; 3];
    let mut gate_level = [0.0_f32; 3];
    let mut step_counter: u64 = 0;
    let hashes = [0u32, 0xbeca_55e5, 0xf0ca_cc1a];
    let mut ticks: u64 = 0;

    loop {
        if ticks.is_multiple_of(64) {
            let entries = manifest.entries();
            let mut s = shared.lock().unwrap();
            for k in 0..N_SRC {
                s.resolved[k] = s.srcs[k]
                    .as_ref()
                    .and_then(|a| routing::resolve(&entries, a));
            }
            let mask = s
                .resolved
                .iter()
                .flatten()
                .filter(|&&c| c < 64)
                .fold(0u64, |m, &c| m | (1 << c));
            manifest.publish_consumes(mask, 0);
        }
        ticks += 1;

        let Some(ref t) = transport else {
            thread::sleep(Duration::from_millis(5));
            continue;
        };
        let playing = t.playing();
        let clock = t.clock();
        let bpm = f64::from(t.bpm()).clamp(20.0, 300.0);
        let sr = f64::from(t.sample_rate()).max(1.0);

        // resolve knob values (cv replaces knob)
        let (
            model,
            t_bias,
            t_jitter,
            t_pw,
            x_spread,
            x_bias,
            x_steps,
            x_deja_vu,
            x_length,
            scale_name,
            y_spread,
            y_steps,
            range,
            y_div,
        ) = {
            let mut s = shared.lock().unwrap();
            let bus = modbus.as_ref();
            let cv = |k: usize, manual: f32, s: &MarblesState| -> f32 {
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
            (
                s.t_model,
                vals[0],
                vals[1],
                vals[2],
                vals[3],
                vals[4],
                vals[5],
                vals[6],
                s.x_length,
                scale_names()[s.x_scale].to_string(),
                vals[7],
                vals[8],
                s.t_range,
                Y_DIV_VALUES[s.y_div],
            )
        };

        if scale_name != last_scale_name {
            cur_scale = scale_degrees(&scale_name);
            last_scale_name = scale_name.clone();
        }

        let div = RANGE_MULT[range];
        // marbles: like grids, 8 pulses per beat, scaled by the range
        let samples_per_step = (60.0 / bpm * sr / (8.0 * div)).max(1.0);
        let step_index = (clock as f64 / samples_per_step) as u64;

        if playing && last_step != Some(step_index) {
            last_step = Some(step_index);
            step_counter = step_counter.wrapping_add(1);

            t_gen.model = model_from(model);
            t_gen.bias = t_bias;
            t_gen.jitter = t_jitter;
            t_gen.pulse_width_mean = t_pw;
            t_gen.sequence.set_deja_vu(x_deja_vu);
            t_gen.sequence.set_length(x_length);
            let mask = t_gen.step(&mut stream);

            // t2 (master) always fires; t1 = bit0, t3 = bit1
            let fires = [mask & 1 != 0, true, mask & 2 != 0];

            // x voltages: ch0 from seq0; ch1/ch2 replay seq0 pseudo-random
            // (the firmware's "same clock → shifted constants" behavior)
            x_seq[0].record();
            x_seq[0].set_deja_vu(x_deja_vu);
            x_seq[0].set_length(x_length);
            let seq0 = x_seq[0].clone();
            x_seq[1].clone_from_seq(&seq0);
            x_seq[1].replay_pseudo_random(hashes[1]);
            x_seq[2].clone_from_seq(&seq0);
            x_seq[2].replay_pseudo_random(hashes[2]);

            let mut x_out = [0.0_f32; 3];
            #[allow(clippy::needless_range_loop)] // i strides x_ch + x_seq + x_out
            for i in 0..3usize {
                x_ch[i].spread = x_spread;
                x_ch[i].bias = x_bias;
                x_ch[i].steps = x_steps;
                x_ch[i].scale = cur_scale.clone();
                x_out[i] = x_ch[i].process_step(&mut x_seq[i], &mut stream, true);
            }

            // y on a divided clock
            let y_now = step_counter.is_multiple_of(y_div);
            if y_now {
                y_seq.record();
                y_seq.set_deja_vu(x_deja_vu);
                y_seq.set_length(x_length);
                y_ch.spread = y_spread;
                y_ch.bias = x_bias;
                y_ch.steps = y_steps;
                y_ch.scale = cur_scale.clone();
                let v = y_ch.process_step(&mut y_seq, &mut stream, true);
                if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
                    bus.set(base + 6, v.max(0.0).min(1.0));
                }
                shared.lock().unwrap().out_now[6] = v.max(0.0).min(1.0);
            }

            // emit notes + publish gates/CVs
            let pw_samples = (samples_per_step * f64::from(t_gen.pulse_width())) as u64;
            #[allow(clippy::needless_range_loop)] // i strides fires/x_out/gate arrays
            for i in 0..3 {
                if fires[i] {
                    // pitch = x voltage mapped to a MIDI span; t2 uses x2
                    let v = x_out[i.min(2)];
                    let note = (36.0 + v * 48.0) as u8; // 4-octave span
                    let hz = 440.0 * 2.0_f32.powf((f32::from(note) - 69.0) / 12.0);
                    if let Some(ref mut pr) = events {
                        let _ = pr.write_event(&AudioEvent::note_on_hz(
                            hz,
                            100,
                            note_base + i as u8,
                            clock as u32,
                        ));
                    }
                    gate_level[i] = 1.0;
                    gate_off_at[i] = Some(clock.wrapping_add(pw_samples.max(1)));
                }
            }
            if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
                for (i, v) in x_out.iter().enumerate() {
                    bus.set(base + 3 + i, v.max(0.0).min(1.0));
                }
            }
            let mut s = shared.lock().unwrap();
            for (i, v) in x_out.iter().enumerate() {
                s.out_now[3 + i] = v.max(0.0).min(1.0);
            }
        }

        // gate-off scheduling + gate meters
        #[allow(clippy::needless_range_loop)] // i strides gate_off_at + gate_level
        for i in 0..3 {
            if let Some(off) = gate_off_at[i] {
                if clock >= off {
                    if let Some(ref mut pr) = events {
                        let _ = pr.write_event(&AudioEvent::note_off_source(
                            0,
                            note_base + i as u8,
                            clock as u32,
                        ));
                    }
                    gate_level[i] = 0.0;
                    gate_off_at[i] = None;
                }
            }
        }
        if let (Some(base), Some(bus)) = (mod_base, modbus.as_mut()) {
            for (i, g) in gate_level.iter().enumerate() {
                bus.set(base + i, *g);
            }
        }
        shared.lock().unwrap().out_now[..3].copy_from_slice(&gate_level);

        thread::sleep(Duration::from_micros(700));
    }
}

// ── ui ──────────────────────────────────────────────────────────────────────

fn row_label(r: Row) -> &'static str {
    match r {
        Row::TModel => "t model",
        Row::TRange => "t range",
        Row::TBias => "· t bias",
        Row::TJitter => "· t jitter",
        Row::TPw => "· t pulse",
        Row::XSpread => "x spread",
        Row::XBias => "· x bias",
        Row::XSteps => "· x steps",
        Row::XDejaVu => "· déjà vu",
        Row::XLength => "· length",
        Row::XScale => "· scale",
        Row::YSpread => "y spread",
        Row::YSteps => "· y steps",
        Row::YDiv => "· y ÷",
    }
}

fn row_text(s: &MarblesState, r: Row) -> String {
    match r {
        Row::TModel => MODEL_NAMES[s.t_model].to_string(),
        Row::TRange => RANGE_NAMES[s.t_range].to_string(),
        Row::XLength => format!("{}", s.x_length),
        Row::XScale => scale_names()[s.x_scale].to_string(),
        Row::YDiv => format!("÷{}", Y_DIV_NAMES[s.y_div]),
        _ => format!("{:.0}%", s.get(r) * 100.0),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &MarblesState,
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
            "MARBLES",
            &format!("random {}", instance),
            "",
            w,
        ));
        let labels = ["t1", "t2", "t3", "x1", "x2", "x3", "y"];
        let mut meter = vec![Span::styled("  ".to_string(), theme::chrome())];
        for (i, l) in labels.iter().enumerate() {
            meter.push(Span::styled(format!("{} ", l), theme::dim()));
            meter.push(Span::styled(
                theme::meter_char(s.out_now[i]).to_string(),
                theme::signal(theme::cv_ramp(s.out_now[i])),
            ));
            meter.push(Span::raw(" "));
        }
        lines.push(Line::from(meter));
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
                Line::from("━━━ MARBLES · random sampler (Mutable Instruments port) ━━━"),
                Line::from(""),
                Line::from("  t model    bernoulli·clusters·drums·independent"),
                Line::from("             divider·three_states·markov"),
                Line::from("  t bias     which of t1/t3 fires more (t2 = master)"),
                Line::from("  x spread   narrow (constant) → wide (bernoulli)"),
                Line::from("  x bias     the centre of the random voltages"),
                Line::from("  x steps    smooth glide ↔ quantized to the scale"),
                Line::from("  déjà vu    fresh ↔ looped (the signature knob)"),
                Line::from("  length     déjà-vu loop length (1–16)"),
                Line::from(""),
                Line::from("Emits t1/t2/t3 as note sources; publishes"),
                Line::from("marbles/N/{t1,t2,t3,x1,x2,x3,y} on the bus."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" MARBLES ", theme::chrome_hi())),
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
    state::write_pid_file("marbles", instance);

    let shared = Arc::new(Mutex::new(MarblesState::new()));
    if let Ok(p) = state::load_module_state::<state::MarblesParams>("marbles", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }

    let ctl_state = Arc::clone(&shared);
    let builder = thread::Builder::new()
        .name(String::from("marbles-ctl"))
        .stack_size(4 * 1024 * 1024);
    let _ = builder.spawn(move || {
        if let Err(e) = control_thread(ctl_state, instance) {
            eprintln!("[marbles {}] control thread error: {}", instance, e);
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
            let _ = state::save_module_state("marbles", instance, &params);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::MarblesParams>("marbles", instance) {
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
                        match state::load_patch::<state::MarblesParams>(&name) {
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
            let _ = state::save_module_state("marbles", instance, &params);
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
                let cyc = |cur: usize, len: usize| -> usize {
                    (cur as i32 + steps).rem_euclid(len as i32) as usize
                };
                match r {
                    Row::TModel => {
                        let old = s.get_param(slot);
                        let v = cyc(s.t_model, MODEL_NAMES.len());
                        s.t_model = v;
                        if let Some(old) = old {
                            history.record(slot, "Model", old, ParamValue::Usize(v));
                        }
                    }
                    Row::TRange => {
                        let old = s.get_param(slot);
                        let v = cyc(s.t_range, RANGE_NAMES.len());
                        s.t_range = v;
                        if let Some(old) = old {
                            history.record(slot, "Range", old, ParamValue::Usize(v));
                        }
                    }
                    Row::XLength => {
                        let old = s.get_param(slot);
                        let v = (s.x_length as i32 + steps).clamp(1, 16) as usize;
                        s.x_length = v;
                        if let Some(old) = old {
                            history.record(slot, "Length", old, ParamValue::Usize(v));
                        }
                    }
                    Row::XScale => {
                        let old = s.get_param(slot);
                        let v = cyc(s.x_scale, scale_names().len());
                        s.x_scale = v;
                        if let Some(old) = old {
                            history.record(slot, "Scale", old, ParamValue::Usize(v));
                        }
                    }
                    Row::YDiv => {
                        let old = s.get_param(slot);
                        let v = cyc(s.y_div, Y_DIV_NAMES.len());
                        s.y_div = v;
                        if let Some(old) = old {
                            history.record(slot, "YDiv", old, ParamValue::Usize(v));
                        }
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
                let def = MarblesState::new();
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
                if let Some(k) = src_index(r) {
                    let current = s.srcs[k].clone();
                    drop(s);
                    let sources = Manifest::open()
                        .map(|m| routing::live_sources(&m.entries()))
                        .unwrap_or_default();
                    picker.open(sources, current.as_ref());
                } else {
                    ex_msg = Some("discrete row — h/l cycles".into());
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let r = ROWS[s.selected.min(ROWS.len() - 1)];
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
    s: &mut MarblesState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    use crate::undo::ParamValue as V;
    let Some(slot) = ROWS.iter().position(|r| {
        row_label(*r).trim_start_matches("· ") == key
            || row_label(*r).trim_start_matches('·').trim() == key
    }) else {
        return format!(
            "Unknown setting: {key} (t_model t_range t_bias t_jitter x_spread x_bias x_steps deja length scale y_spread y_steps y_div — or use the row name)"
        );
    };
    let r = ROWS[slot];
    let parsed: Result<V, String> = match r {
        Row::TModel => MODEL_NAMES
            .iter()
            .position(|n| *n == value)
            .map(V::Usize)
            .ok_or_else(|| format!("{key}: one of {}", MODEL_NAMES.join(" "))),
        Row::TRange => RANGE_NAMES
            .iter()
            .position(|n| *n == value)
            .map(V::Usize)
            .ok_or_else(|| format!("{key}: one of {}", RANGE_NAMES.join(" "))),
        Row::XLength => value
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=16).contains(n))
            .map(V::Usize)
            .ok_or_else(|| format!("{key}: 1–16")),
        Row::XScale => scale_names()
            .iter()
            .position(|n| *n == value)
            .map(V::Usize)
            .ok_or_else(|| format!("{key}: one of {}", scale_names().join(" "))),
        Row::YDiv => Y_DIV_NAMES
            .iter()
            .position(|n| *n == value)
            .map(V::Usize)
            .ok_or_else(|| format!("{key}: one of {}", Y_DIV_NAMES.join(" "))),
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
    fn every_continuous_knob_takes_a_cable() {
        for r in ROWS {
            let continuous = !matches!(
                r,
                Row::TModel | Row::TRange | Row::XLength | Row::XScale | Row::YDiv
            );
            if continuous {
                assert!(src_index(r).is_some(), "{r:?} must be bindable");
            }
        }
        assert_eq!(N_SRC, 9);
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = MarblesState::new();
        s.t_model = 2;
        s.t_range = 2;
        s.x_spread = 0.8;
        s.x_length = 12;
        s.x_scale = 4;
        s.y_div = 3;
        s.srcs[3] = SourceAddr::parse("lfo/0/s1");
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::MarblesParams = toml::from_str(&toml).expect("parses");
        let mut s2 = MarblesState::new();
        apply_params(&mut s2, &back);
        assert_eq!(s2.t_model, 2);
        assert_eq!(s2.t_range, 2);
        assert!((s2.x_spread - 0.8).abs() < 1e-6);
        assert_eq!(s2.x_length, 12);
        assert_eq!(s2.x_scale, 4);
        assert_eq!(s2.y_div, 3);
        assert_eq!(
            s2.srcs[3].as_ref().map(|a| a.to_string()),
            Some("lfo/0/s1".into())
        );
    }

    #[test]
    fn scale_degrees_are_octave_fractions() {
        let maj = scale_degrees("major");
        assert_eq!(maj.len(), 7);
        assert!((maj[0]).abs() < 1e-6);
        assert!(maj.iter().all(|d| (0.0..1.0).contains(d)));
        assert!(scale_degrees("chromatic").is_empty());
    }

    #[test]
    fn ex_set_parses() {
        let mut s = MarblesState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert!(ex_set(&mut s, &mut h, "t model", "drums").contains("drums"));
        assert!(ex_set(&mut s, &mut h, "x spread", "0.9").contains('%'));
        assert!(ex_set(&mut s, &mut h, "déjà vu", "lfo/0/s1").contains("lfo/0/s1"));
        assert_eq!(s.t_model, 2);
        assert!(s.srcs[6].is_some());
    }
}
