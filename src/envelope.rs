use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{enable_raw_mode, EnterAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use crate::routing::{self, SourceAddr};
use crate::shm::{EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

const NUM_CHANNELS: usize = 4;
const SAMPLE_RATE: f64 = 48000.0;
const BLOCK_SIZE: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Off,
    Rise,
    Sustain,
    Fall,
}

#[derive(Clone, Copy)]
struct EnvelopeChannel {
    stage: Stage,
    phase: f32,
    output: f32,
    eor_fired: bool,
    eoc_fired: bool,
}

impl Default for EnvelopeChannel {
    fn default() -> Self {
        Self {
            stage: Stage::Off,
            phase: 0.0,
            output: 0.0,
            eor_fired: false,
            eoc_fired: false,
        }
    }
}

/// What fires a channel: any note event, nothing (manual `t`/cycle only),
/// or one specific sequencer track's notes.
#[derive(Clone, Debug, PartialEq)]
enum Trigger {
    Any,
    Off,
    Source(SourceAddr),
}

impl Trigger {
    /// Stable string form used in params/undo: absent = Any, "off" = Off.
    fn to_param(&self) -> Option<String> {
        match self {
            Trigger::Any => None,
            Trigger::Off => Some(String::from("off")),
            Trigger::Source(a) => Some(a.to_string()),
        }
    }

    fn from_param(s: Option<&str>) -> Self {
        match s {
            None => Trigger::Any,
            Some("off") => Trigger::Off,
            Some(a) => SourceAddr::parse(a).map(Trigger::Source).unwrap_or(Trigger::Any),
        }
    }
}

#[derive(Clone)]
struct ChannelParams {
    rise_param: f32,   // 0.0-1.0 → 1ms to 10s (exponential)
    fall_param: f32,   // 0.0-1.0 → 1ms to 10s (exponential)
    shape_param: f32,  // 0.0-1.0 → curve exponent 0.05 to 20.0
    loop_mode: bool,
    attenuverter: f32, // -1.0 to 1.0
    // Receiver-side bindings (routing.rs source addresses).
    trigger: Trigger,
    rise_src: Option<SourceAddr>,
    fall_src: Option<SourceAddr>,
    shape_src: Option<SourceAddr>,
    atten_src: Option<SourceAddr>,
}

/// Exponential parameter → real time in seconds.
/// 0.0 → 1ms, 0.5 → ~100ms, 1.0 → 10s
fn param_to_time(param: f32) -> f32 {
    let t = param.clamp(0.0, 1.0);
    0.001 * (10000.0f32).powf(t)
}

/// Time in seconds → exponential parameter.
#[allow(dead_code)]
fn time_to_param(time: f32) -> f32 {
    let t = time.clamp(0.001, 10.0);
    (t / 0.001).log10() / 10000.0f32.log10()
}

/// Display a time value with appropriate units.
fn format_time(t: f32) -> String {
    if t < 0.01 {
        format!("{:.2}ms", t * 1000.0)
    } else if t < 1.0 {
        format!("{:.1}ms", t * 1000.0)
    } else {
        format!("{:.2}s", t)
    }
}

/// Exponential shape parameter → curve exponent.
/// 0.0 → 0.05 (very concave / fast attack-like), 0.5 → 1.0 (linear), 1.0 → 20.0 (very convex)
fn param_to_shape(param: f32) -> f32 {
    let t = param.clamp(0.0, 1.0);
    0.05 * (400.0f32).powf(t)
}

impl Default for ChannelParams {
    fn default() -> Self {
        Self {
            rise_param: 0.5,   // ~100ms
            fall_param: 0.5,   // ~100ms
            shape_param: 0.5,  // ~1.0 (linear-ish)
            loop_mode: false,
            attenuverter: 1.0,
            trigger: Trigger::Any, // ch0 = sequencer/0/t1 in EnvelopeState::default
            rise_src: None,
            fall_src: None,
            shape_src: None,
            atten_src: None,
        }
    }
}

#[derive(Clone)]
struct EnvelopeState {
    channels: Vec<EnvelopeChannel>,
    params: Vec<ChannelParams>,
    current_channel: usize,
    gate: bool,
    events_received: u64,
    /// Modbus base channel claimed at registration (outputs write here).
    mod_base: Option<usize>,
}

impl Default for EnvelopeState {
    fn default() -> Self {
        let mut params = vec![ChannelParams::default(); NUM_CHANNELS];
        // Channel 1 defaults to sequencer track 1; channels 2-4 to any-note
        if let Some(a) = SourceAddr::parse("sequencer/0/t1") {
            params[0].trigger = Trigger::Source(a);
        }
        Self {
            channels: vec![EnvelopeChannel::default(); NUM_CHANNELS],
            params,
            current_channel: 0,
            gate: false,
            mod_base: None,
            events_received: 0,
        }
    }
}

fn curve(t: f32, shape: f32) -> f32 {
    if shape <= 0.0 {
        return t;
    }
    let exp = if shape < 0.5 {
        0.1 + shape * 1.8
    } else {
        1.0 + (shape - 0.5) * 18.0
    };
    t.powf(exp)
}

fn env_thread(
    state: Arc<Mutex<EnvelopeState>>,
    shutdown: std::sync::mpsc::Receiver<()>,
    instance: usize,
) -> Result<()> {
    let consumer_id = crate::shm::consumer_id("envelope", instance);
    let mut events = EventRingbuf::open(consumer_id).ok();
    let mut modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();
    let manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let _transport = ShmTransport::open().ok();

    let dt = 1.0 / SAMPLE_RATE as f32;

    // Per-channel resolved bindings: (trigger mode, rise/fall/shape/atten
    // modbus channels). Refreshed periodically through the manifest.
    // Trigger resolves to: None = any note, Some(None) = off / non-note
    // source, Some(Some(t)) = notes from sequencer track t.
    type ResolvedTrigger = Option<Option<u8>>;
    let mut resolved: Vec<(ResolvedTrigger, [Option<usize>; 4])> = vec![(None, [None; 4]); NUM_CHANNELS];
    let mut refresh_in = 0u32;

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        if events.is_none() {
            events = EventRingbuf::open(consumer_id).ok();
        }
        if modbus.is_none() {
            modbus = ModulationBus::open().ok();
        }

        if refresh_in == 0 {
            refresh_in = 256;
            let entries = manifest.entries();
            let s = state.lock().unwrap();
            for (i, r) in resolved.iter_mut().enumerate().take(s.params.len()) {
                let p = &s.params[i];
                r.0 = match &p.trigger {
                    Trigger::Any => None,
                    Trigger::Off => Some(None),
                    Trigger::Source(a) => Some(routing::note_source_track(a)),
                };
                r.1 = [
                    p.rise_src.as_ref().and_then(|a| routing::resolve(&entries, a)),
                    p.fall_src.as_ref().and_then(|a| routing::resolve(&entries, a)),
                    p.shape_src.as_ref().and_then(|a| routing::resolve(&entries, a)),
                    p.atten_src.as_ref().and_then(|a| routing::resolve(&entries, a)),
                ];
            }
        }
        refresh_in -= 1;

        // Read events (triggers)
        let mut triggers = [false; NUM_CHANNELS];

        let mut track_trigger: Option<u8> = None;
        let mut release_track: Option<u8> = None;

        let mut event_count = 0u32;
        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                event_count += 1;
                match event.event_type {
                    0 => { // note_on
                        track_trigger = Some(event.source);
                    }
                    1 => { // note_off
                        release_track = Some(event.source);
                    }
                    4 => { // trigger
                        let ch = (event.target as usize).min(NUM_CHANNELS - 1);
                        triggers[ch] = true;
                    }
                    _ => {}
                }
            }
        }

        let mut s = state.lock().unwrap();
        s.events_received += event_count as u64;

        // Process all channels
        let mut ch_outputs = [0.0f32; NUM_CHANNELS];

        for i in 0..NUM_CHANNELS {
            let params = s.params[i].clone();
            let ch = &mut s.channels[i];
            let (trigger_filter, mod_chans) = resolved[i];

            let should_trigger = match (&trigger_filter, track_trigger) {
                (None, got) => got.is_some() || triggers[i], // any note
                (Some(None), _) => triggers[i],              // off: manual only
                (Some(Some(want)), Some(got)) => got == *want || triggers[i],
                (Some(Some(_)), None) => triggers[i],
            };

            let should_release = match (&trigger_filter, release_track) {
                (None, got) => got.is_some(),
                (Some(None), _) => false,
                (Some(Some(want)), Some(got)) => got == *want,
                (Some(Some(_)), None) => false,
            };

            // Apply trigger/release
            // Release can happen from ANY stage (Rise, Sustain, even Off — no-op there)
            if should_release && ch.stage != Stage::Off && ch.stage != Stage::Fall {
                ch.stage = Stage::Fall;
                ch.phase = 0.0;
            }
            if should_trigger {
                ch.stage = Stage::Rise;
                ch.phase = 0.0;
                ch.eor_fired = false;
                ch.eoc_fired = false;
            }

            // Param modulation: bound + resolvable -> modbus value
            let chan_val = |ch: Option<usize>| -> Option<f32> {
                ch.and_then(|c| modbus.as_ref().map(|m| m.get(c)))
            };

            let rp = chan_val(mod_chans[0]).map(|v| v.clamp(0.0, 1.0)).unwrap_or(params.rise_param);
            let fp = chan_val(mod_chans[1]).map(|v| v.clamp(0.0, 1.0)).unwrap_or(params.fall_param);
            let sp = chan_val(mod_chans[2]).map(|v| v.clamp(0.0, 1.0)).unwrap_or(params.shape_param);
            let att = chan_val(mod_chans[3]).map(|v| v.clamp(-1.0, 1.0)).unwrap_or(params.attenuverter);

            let rise_time = param_to_time(rp);
            let fall_time = param_to_time(fp);
            let shape = param_to_shape(sp);

            // Process one block worth of samples (update per sample for smoothness)
            for _ in 0..BLOCK_SIZE {
                match ch.stage {
                    Stage::Off => {
                        ch.output = 0.0;
                    }
                    Stage::Rise => {
                        ch.phase += dt / rise_time.max(0.0005);
                        if ch.phase >= 1.0 {
                            ch.phase = 1.0;
                            ch.output = 1.0;
                            if !ch.eor_fired {
                                ch.eor_fired = true;
                            }
                            if params.loop_mode {
                                ch.stage = Stage::Fall;
                                ch.phase = 0.0;
                            } else {
                                ch.stage = Stage::Sustain;
                            }
                        } else {
                            ch.output = curve(ch.phase, shape);
                        }
                    }
                    Stage::Sustain => {
                        ch.output = 1.0;
                    }
                    Stage::Fall => {
                        ch.phase += dt / fall_time.max(0.0005);
                        if ch.phase >= 1.0 {
                            ch.phase = 1.0;
                            ch.output = 0.0;
                            if !ch.eoc_fired {
                                ch.eoc_fired = true;
                            }
                            if params.loop_mode {
                                ch.stage = Stage::Rise;
                                ch.phase = 0.0;
                                ch.eor_fired = false;
                            } else {
                                ch.stage = Stage::Off;
                            }
                        } else {
                            ch.output = 1.0 - curve(ch.phase, shape);
                        }
                    }
                }
            }

            // Apply attenuverter (already modulated above if track assigned)
            ch_outputs[i] = ch.output * att;
        }

        // Compute logic outputs
        let sum = ch_outputs.iter().sum::<f32>().clamp(-1.0, 1.0);
        let or_val = ch_outputs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let and_val = ch_outputs.iter().copied().fold(f32::INFINITY, f32::min);
        let invert = -ch_outputs[0];

        let mod_base = s.mod_base;
        drop(s);

        // Write outputs to our claimed modbus range:
        // base+0..3 = channels, base+4..7 = sum/or/and/inv
        if let (Some(ref mut bus), Some(base)) = (modbus.as_mut(), mod_base) {
            for (i, &val) in ch_outputs.iter().enumerate().take(NUM_CHANNELS) {
                bus.set(base + i, val);
            }
            bus.set(base + 4, sum);
            bus.set(base + 5, or_val);
            bus.set(base + 6, and_val);
            bus.set(base + 7, invert);
        }

        // Sleep to maintain real-time pacing: 64 samples @ 48kHz ≈ 1.33ms
        std::thread::sleep(Duration::from_millis(1));
    }

    Ok(())
}

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &EnvelopeState,
    selected: usize,
    show_help: bool,
    overlay: Option<&str>,
    picker: Option<(Vec<String>, usize)>,
) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        let tab_row = 1u16;
        let content_height = area.height.saturating_sub(tab_row + 1);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(tab_row),
                Constraint::Length(content_height),
                Constraint::Length(1),
            ])
            .split(area);

        // Channel tabs
        let mut tab_text = String::new();
        for i in 0..NUM_CHANNELS {
            let label = format!("Ch{}", i + 1);
            if i == state.current_channel {
                tab_text.push_str(&format!(" [{}] ", label));
            } else {
                tab_text.push_str(&format!("  {}  ", label));
            }
        }
        // Logic outputs tab
        let logic_label = "Logic";
        if state.current_channel >= NUM_CHANNELS {
            tab_text.push_str(&format!(" [{}] ", logic_label));
        } else {
            tab_text.push_str(&format!("  {}  ", logic_label));
        }

        let tab_style = Style::default().fg(Color::Cyan);
        let tab_widget = Paragraph::new(tab_text).style(tab_style);
        f.render_widget(tab_widget, chunks[0]);

        let ch = state.current_channel.min(NUM_CHANNELS - 1);
        let params = state.params[ch].clone();
        let env_ch = state.channels[ch];

        let src_label = |a: &Option<SourceAddr>| -> String {
            a.as_ref().map(|a| format!(" @{}", a)).unwrap_or_default()
        };

        let trigger_str = match &params.trigger {
            Trigger::Any => String::from("any note"),
            Trigger::Off => String::from("off"),
            Trigger::Source(a) => a.to_string(),
        };

        let rise_time = param_to_time(params.rise_param);
        let fall_time = param_to_time(params.fall_param);
        let shape_exp = param_to_shape(params.shape_param);

        let param_labels = [
            format!("Rise:  {}{}", format_time(rise_time), src_label(&params.rise_src)),
            format!("Fall:  {}{}", format_time(fall_time), src_label(&params.fall_src)),
            format!("Shape: {:.2}{}", shape_exp, src_label(&params.shape_src)),
            format!("Atten: {:+.2}{}", params.attenuverter, src_label(&params.atten_src)),
            format!("Trig:  {}", trigger_str),
        ];

        let mut content_lines = vec![
            Line::from(format!("Channel {} | Loop: {} | Stage: {:?} | Events: {}",
                ch + 1,
                if params.loop_mode { "ON" } else { "OFF" },
                env_ch.stage,
                state.events_received,
            )),
            Line::from(""),
        ];

        for (i, label) in param_labels.iter().enumerate() {
            let marker = if selected == i { "> " } else { "  " };
            content_lines.push(Line::from(format!("{}{}", marker, label)));
        }

        content_lines.push(Line::from(""));
        content_lines.push(Line::from(format!(
            "Output: {:.3} | EOR:{} EOC:{}",
            env_ch.output,
            if env_ch.eor_fired { "●" } else { "○" },
            if env_ch.eoc_fired { "●" } else { "○" },
        )));

        if state.current_channel >= NUM_CHANNELS {
            // Logic outputs view
            content_lines = vec![
                Line::from("Logic Outputs"),
                Line::from(""),
                Line::from(format!("SUM:   {:.3}", 0.0)),
                Line::from(format!("OR:    {:.3}", 0.0)),
                Line::from(format!("AND:   {:.3}", 0.0)),
                Line::from(format!("INV:   {:.3}", 0.0)),
            ];
        }

        let content = Paragraph::new(content_lines)
            .style(Style::default().fg(Color::White))
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(content, chunks[1]);

        let status = format!(
            "Ch{}/{} | j/k params | h/l adjust | [ ] switch | t trigger | T track | c loop | ? help",
            state.current_channel + 1,
            NUM_CHANNELS
        );
        let status_widget = Paragraph::new(status).style(Style::default().fg(Color::Cyan));
        f.render_widget(status_widget, chunks[2]);

        if show_help {
            let help_text = vec![
                Line::from("━━━ Envelope Help ━━━"),
                Line::from(""),
                Line::from("Navigation:"),
                Line::from("  [ / ]      Previous/next channel (counts)"),
                Line::from("  gg / G     First / last channel"),
                Line::from("  j/k, ↑/↓   Select parameter"),
                Line::from("  h/l, ←/→   Adjust value"),
                Line::from("  H/L        Coarse adjust (10x)"),
                Line::from("  #j/#l ...  Count prefix repeats"),
                Line::from(""),
                Line::from("Actions:"),
                Line::from("  t          Trigger envelope manually"),
                Line::from("  @          Bind selected param/trigger to a source"),
                Line::from("  c          Toggle cycle/loop mode"),
                Line::from("  o          Toggle gate on/off (sustain)"),
                Line::from(""),
                Line::from("  u / ^r     Undo / redo (counts; sweeps coalesce)"),
                Line::from("  :w/:e/:q   Patch save/load, quit (:x save+quit)"),
                Line::from("  space      Play/pause (global)"),
                Line::from("  ?          Toggle this help"),
                Line::from("  Close pane: tmux prefix + x"),
            ];
            let help = Paragraph::new(help_text)
                .style(Style::default().fg(Color::White).bg(Color::Black))
                .block(Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title("Help"));
            f.render_widget(help, area);
        }

        if let Some(text) = overlay {
            let r = ratatui::layout::Rect::new(0, area.height.saturating_sub(1), area.width, 1);
            f.render_widget(
                Paragraph::new(text.to_string()).style(Style::default().fg(Color::Yellow)),
                r,
            );
        }

        // Source picker overlay (@): list of live modulation sources
        if let Some((rows, sel)) = picker {
            let h = (rows.len() as u16 + 2).min(area.height);
            let w = rows.iter().map(|r| r.len()).max().unwrap_or(10).max(20) as u16 + 4;
            let r = ratatui::layout::Rect::new(
                (area.width.saturating_sub(w)) / 2,
                (area.height.saturating_sub(h)) / 2,
                w.min(area.width),
                h,
            );
            f.render_widget(ratatui::widgets::Clear, r);
            let items: Vec<ratatui::widgets::ListItem> = rows
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    let style = if i == sel {
                        Style::default().fg(Color::Black).bg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    ratatui::widgets::ListItem::new(row.clone()).style(style)
                })
                .collect();
            let list = ratatui::widgets::List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title("Bind source (Enter binds, x unbinds, Esc cancels)"),
            );
            f.render_widget(list, r);
        }
    })?;

    Ok(())
}

fn snapshot_params(s: &EnvelopeState) -> state::EnvelopeParams {
    state::EnvelopeParams {
        format: state::STATE_FORMAT,
        channels: s.params.iter().map(|p| state::EnvelopeChannelParams {
            rise: p.rise_param,
            fall: p.fall_param,
            shape: p.shape_param,
            loop_mode: p.loop_mode,
            attenuverter: p.attenuverter,
            trigger_src: p.trigger.to_param(),
            rise_src: p.rise_src.as_ref().map(|a| a.to_string()),
            fall_src: p.fall_src.as_ref().map(|a| a.to_string()),
            shape_src: p.shape_src.as_ref().map(|a| a.to_string()),
            atten_src: p.atten_src.as_ref().map(|a| a.to_string()),
        }).collect(),
        logic_outputs: state::LogicOutputConfig {
            sum_enabled: true,
            or_enabled: true,
            and_enabled: true,
        },
    }
}

fn apply_params(s: &mut EnvelopeState, params: &state::EnvelopeParams) {
    for (i, ch) in params.channels.iter().enumerate().take(s.params.len()) {
        s.params[i].rise_param = ch.rise;
        s.params[i].fall_param = ch.fall;
        s.params[i].shape_param = ch.shape;
        s.params[i].loop_mode = ch.loop_mode;
        s.params[i].attenuverter = ch.attenuverter;
        // Binding fields only exist in format-2 files; older files keep the
        // defaults (ch1 triggered by sequencer/0/t1) instead of unbinding.
        if params.format >= state::STATE_FORMAT {
            s.params[i].trigger = Trigger::from_param(ch.trigger_src.as_deref());
            s.params[i].rise_src = ch.rise_src.as_deref().and_then(SourceAddr::parse);
            s.params[i].fall_src = ch.fall_src.as_deref().and_then(SourceAddr::parse);
            s.params[i].shape_src = ch.shape_src.as_deref().and_then(SourceAddr::parse);
            s.params[i].atten_src = ch.atten_src.as_deref().and_then(SourceAddr::parse);
        }
    }
}

/// Undo slots: ch*32 + row for values (0–3) and loop (5); ch*32 + 8 + row
/// for the five per-row bindings (rise/fall/shape/atten/trigger).
const CH_SLOT_STRIDE: usize = 32;
const BIND_OFF: usize = 8;

impl crate::undo::ParamUndo for EnvelopeState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        let (ch, row) = (slot / CH_SLOT_STRIDE, slot % CH_SLOT_STRIDE);
        let p = self.params.get(ch)?;
        match row {
            0 => Some(V::F32(p.rise_param)),
            1 => Some(V::F32(p.fall_param)),
            2 => Some(V::F32(p.shape_param)),
            3 => Some(V::F32(p.attenuverter)),
            5 => Some(V::Bool(p.loop_mode)),
            r if (BIND_OFF..BIND_OFF + 5).contains(&r) => {
                let b = match r - BIND_OFF {
                    0 => p.rise_src.as_ref().map(|a| a.to_string()),
                    1 => p.fall_src.as_ref().map(|a| a.to_string()),
                    2 => p.shape_src.as_ref().map(|a| a.to_string()),
                    3 => p.atten_src.as_ref().map(|a| a.to_string()),
                    _ => p.trigger.to_param(),
                };
                Some(V::Src(b))
            }
            _ => None,
        }
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        let (ch, row) = (slot / CH_SLOT_STRIDE, slot % CH_SLOT_STRIDE);
        let Some(p) = self.params.get_mut(ch) else { return };
        match (row, value) {
            (0, V::F32(v)) => p.rise_param = v,
            (1, V::F32(v)) => p.fall_param = v,
            (2, V::F32(v)) => p.shape_param = v,
            (3, V::F32(v)) => p.attenuverter = v,
            (5, V::Bool(v)) => p.loop_mode = v,
            (r, V::Src(a)) if (BIND_OFF..BIND_OFF + 5).contains(&r) => {
                if r - BIND_OFF == 4 {
                    p.trigger = Trigger::from_param(a.as_deref());
                } else {
                    let addr = a.as_deref().and_then(SourceAddr::parse);
                    match r - BIND_OFF {
                        0 => p.rise_src = addr,
                        1 => p.fall_src = addr,
                        2 => p.shape_src = addr,
                        _ => p.atten_src = addr,
                    }
                }
            }
            _ => {}
        }
    }
}

const NUM_ROWS: usize = 5; // rise, fall, shape, atten, trigger-track

/// Adjust a param row on the current channel (doctrine: h/l fine, H/L coarse ×10).
fn adjust(s: &mut EnvelopeState, row: usize, steps: i32, coarse: bool) {
    use crate::keys::step_f32;
    let ch = s.current_channel;
    let p = &mut s.params[ch];
    match row {
        0 => p.rise_param = step_f32(p.rise_param, steps, 0.005, coarse, 0.0, 1.0),
        1 => p.fall_param = step_f32(p.fall_param, steps, 0.005, coarse, 0.0, 1.0),
        2 => p.shape_param = step_f32(p.shape_param, steps, 0.005, coarse, 0.0, 1.0),
        3 => p.attenuverter = step_f32(p.attenuverter, steps, 0.05, coarse, -1.0, 1.0),
        4 => {} // trigger row is binding-only (@ opens the source picker)
        _ => {}
    }
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("envelope", instance);
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let _ = manifest.register("envelope", instance, None, 8);
    let claimed_base = manifest.claimed_base();

    for attempt in 0..20 {
        match enable_raw_mode() {
            Ok(()) => break,
            Err(e) => {
                if attempt < 19 {
                    std::thread::sleep(Duration::from_millis(200));
                } else {
                    return Err(anyhow::anyhow!("Failed to enable raw mode after 20 attempts: {}", e));
                }
            }
        }
    }

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let state = Arc::new(Mutex::new(EnvelopeState::default()));
    state.lock().unwrap().mod_base = claimed_base;

    // Load saved state if available
    if let Ok(params) = state::load_module_state::<state::EnvelopeParams>("envelope", instance) {
        apply_params(&mut state.lock().unwrap(), &params);
    }

    let state_clone = Arc::clone(&state);
    let (_tx, rx) = std::sync::mpsc::channel();

    let _env_handle = std::thread::spawn(move || {
        if let Err(e) = env_thread(state_clone, rx, instance) {
            eprintln!("Envelope thread error: {}", e);
        }
    });

    let mut selected = 0usize;
    let mut show_help = false;
    // Global transport handle for Space = play/pause (lazily reopened)
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();
    let mut picker = crate::picker::Picker::default();
    let mut history = crate::undo::ParamHistory::default();
    let mut count = crate::keys::Count::default();
    let mut pending_g = false;
    let mut ex = crate::excmd::ExLine::default();
    let mut ex_msg: Option<String> = None;
    let mut patch_name: Option<String> = None;
    let mut baseline = state::to_toml_string(&snapshot_params(&state.lock().unwrap())).unwrap_or_default();
    let mut should_quit = false;

    loop {
        if state::check_save_signal() {
            let params = snapshot_params(&state.lock().unwrap());
            let _ = state::save_module_state("envelope", instance, &params);
        }

        if state::check_reload_signal() {
            if let Ok(params) = state::load_module_state::<state::EnvelopeParams>("envelope", instance) {
                apply_params(&mut state.lock().unwrap(), &params);
            }
        }

        let current_state = state.lock().unwrap().clone();
        let overlay = if ex.is_active() {
            Some(ex.display())
        } else {
            ex_msg.clone()
        };
        let picker_rows = if picker.is_active() { Some(picker.rows()) } else { None };
        draw_ui(&mut terminal, &current_state, selected, show_help, overlay.as_deref(), picker_rows)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                ex_msg = None;
                if picker.is_active() {
                    use crate::picker::PickerEvent;
                    let chosen: Option<Option<String>> = match picker.handle_key(key.code) {
                        // first special: unbind (params rows) / any-note (trigger row)
                        PickerEvent::Chosen(addr) => Some(addr.map(|a| a.to_string())),
                        // second special on the trigger row: off
                        PickerEvent::ChosenSpecial(1) => Some(Some(String::from("off"))),
                        _ => None,
                    };
                    if let Some(value) = chosen {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = state.lock().unwrap();
                        let ch = s.current_channel;
                        let slot = ch * CH_SLOT_STRIDE + BIND_OFF + selected.min(4);
                        let old = s.get_param(slot);
                        s.set_param(slot, ParamValue::Src(value));
                        let new = s.get_param(slot);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(slot, "Bind", old, new);
                        }
                    }
                    continue;
                }
                if ex.is_active() {
                    let candidates = crate::excmd::patch_names(&state::patches_dir());
                    if let crate::excmd::ExEvent::Submit(cmd) = ex.handle_key(key.code, &candidates) {
                        use crate::excmd::ExCommand;
                        let params = snapshot_params(&state.lock().unwrap());
                        match cmd {
                            ExCommand::Write(name) => {
                                ex_msg = Some(match crate::excmd::ex_write(name, &mut patch_name, &mut baseline, &params) {
                                    Ok(m) | Err(m) => m,
                                });
                            }
                            ExCommand::Edit(name) => match state::load_patch::<state::EnvelopeParams>(&name) {
                                Ok(p) => {
                                    apply_params(&mut state.lock().unwrap(), &p);
                                    baseline = state::to_toml_string(&snapshot_params(&state.lock().unwrap())).unwrap_or_default();
                                    patch_name = Some(name.clone());
                                    ex_msg = Some(format!("Loaded {}", name));
                                }
                                Err(e) => ex_msg = Some(e.to_string()),
                            },
                            ExCommand::Quit { force } => {
                                if !force && crate::excmd::is_dirty(&params, &baseline) {
                                    ex_msg = Some(String::from("Unsaved changes (:q! to discard, :w <name> to save)"));
                                } else {
                                    should_quit = true;
                                }
                            }
                            ExCommand::WriteQuit(name) => {
                                match crate::excmd::ex_write(name, &mut patch_name, &mut baseline, &params) {
                                    Ok(_) => should_quit = true,
                                    Err(m) => ex_msg = Some(m),
                                }
                            }
                            ExCommand::Set(k, _) => ex_msg = Some(format!("Unknown setting: {}", k)),
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
                    let mut s = state.lock().unwrap();
                    ex_msg = Some(crate::undo::history_status("Redo", n, || history.redo(&mut *s)));
                    continue;
                }
                // Ctrl-s: save
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let params = snapshot_params(&state.lock().unwrap());
                    let _ = state::save_module_state("envelope", instance, &params);
                    continue;
                }

                match key.code {
                    KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
                    KeyCode::Char('j') | KeyCode::Down => {
                        selected = crate::keys::cycle(selected, count.take() as i32, NUM_ROWS);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        selected = crate::keys::cycle(selected, -(count.take() as i32), NUM_ROWS);
                    }
                    KeyCode::Char('h' | 'l' | 'H' | 'L') | KeyCode::Left | KeyCode::Right => {
                        let c = match key.code {
                            KeyCode::Char(c) => c,
                            KeyCode::Left => 'h',
                            _ => 'l',
                        };
                        let n = count.take() as i32;
                        let (steps, coarse) = match c {
                            'h' => (-n, false),
                            'l' => (n, false),
                            'H' => (-n, true),
                            _ => (n, true),
                        };
                        use crate::undo::ParamUndo;
                        let mut s = state.lock().unwrap();
                        let slot = s.current_channel * CH_SLOT_STRIDE + selected;
                        let old = s.get_param(slot);
                        adjust(&mut s, selected, steps, coarse);
                        let new = s.get_param(slot);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(slot, "Adjust", old, new);
                        }
                    }
                    KeyCode::Char('[') => {
                        let n = count.take();
                        let mut s = state.lock().unwrap();
                        s.current_channel = s.current_channel.saturating_sub(n);
                        selected = 0;
                    }
                    KeyCode::Char(']') => {
                        let n = count.take();
                        let mut s = state.lock().unwrap();
                        s.current_channel = (s.current_channel + n).min(NUM_CHANNELS - 1);
                        selected = 0;
                    }
                    KeyCode::Char('g') => {
                        count.clear();
                        if pending_g {
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            s.current_channel = 0;
                            selected = 0;
                        } else {
                            pending_g = true;
                        }
                    }
                    KeyCode::Char('G') => {
                        count.clear();
                        let mut s = state.lock().unwrap();
                        s.current_channel = NUM_CHANNELS - 1;
                        selected = 0;
                    }
                    KeyCode::Char('t') => {
                        let mut s = state.lock().unwrap();
                        let ch = s.current_channel;
                        s.channels[ch].stage = Stage::Rise;
                        s.channels[ch].phase = 0.0;
                        s.channels[ch].eor_fired = false;
                        s.channels[ch].eoc_fired = false;
                    }
                    KeyCode::Char('u') => {
                        let n = count.take();
                        let mut s = state.lock().unwrap();
                        ex_msg = Some(crate::undo::history_status("Undo", n, || history.undo(&mut *s)));
                    }
                    KeyCode::Char('@') => {
                        count.clear();
                        let sources = Manifest::open()
                            .map(|m| crate::routing::live_sources(&m.entries()))
                            .unwrap_or_default();
                        let s = state.lock().unwrap();
                        let ch = s.current_channel;
                        if selected == 4 {
                            // trigger row: any-note / off / specific source
                            let (current, special) = match &s.params[ch].trigger {
                                Trigger::Any => (None, 0),
                                Trigger::Off => (None, 1),
                                Trigger::Source(a) => (Some(a.clone()), 0),
                            };
                            drop(s);
                            picker.open_with(
                                vec![String::from("— any note —"), String::from("— off —")],
                                sources,
                                current.as_ref(),
                                special,
                            );
                        } else {
                            let current = match selected {
                                0 => s.params[ch].rise_src.clone(),
                                1 => s.params[ch].fall_src.clone(),
                                2 => s.params[ch].shape_src.clone(),
                                _ => s.params[ch].atten_src.clone(),
                            };
                            drop(s);
                            picker.open(sources, current.as_ref());
                        }
                    }
                    KeyCode::Char('c') => {
                        count.clear();
                        use crate::undo::ParamValue;
                        let mut s = state.lock().unwrap();
                        let ch = s.current_channel;
                        let was = s.params[ch].loop_mode;
                        s.params[ch].loop_mode = !was;
                        history.record(
                            ch * CH_SLOT_STRIDE + 5,
                            "Cycle mode",
                            ParamValue::Bool(was),
                            ParamValue::Bool(!was),
                        );
                    }
                    KeyCode::Char('o') => {
                        count.clear();
                        let mut s = state.lock().unwrap();
                        s.gate = !s.gate;
                        if !s.gate {
                            for ch in s.channels.iter_mut() {
                                if ch.stage == Stage::Sustain {
                                    ch.stage = Stage::Fall;
                                    ch.phase = 0.0;
                                }
                            }
                        } else {
                            for ch in s.channels.iter_mut() {
                                if ch.stage == Stage::Off || ch.stage == Stage::Fall {
                                    ch.stage = Stage::Rise;
                                    ch.phase = 0.0;
                                    ch.eor_fired = false;
                                    ch.eoc_fired = false;
                                }
                            }
                        }
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
                        show_help = !show_help;
                    }
                    _ => {
                        count.clear();
                    }
                }
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

#[cfg(test)]
mod envelope_tests {
    use super::*;

    #[test]
    fn curve_linear_at_shape_half() {
        // shape = 0.5 should be approximately linear
        assert!((curve(0.0, 0.5) - 0.0).abs() < 0.01);
        assert!((curve(0.5, 0.5) - 0.5).abs() < 0.01);
        assert!((curve(1.0, 0.5) - 1.0).abs() < 0.01);
    }

    #[test]
    fn curve_convex_at_high_shape() {
        // shape > 0.5: slow start, fast end (t^exp with exp > 1)
        let v = curve(0.5, 0.8);
        assert!(v < 0.5, "convex curve at 0.5 should be below linear: got {}", v);
        // Near t=1 it finally approaches 1.0
        assert!((curve(1.0, 0.8) - 1.0).abs() < 0.001, "convex curve at 1 should be 1");
    }

    #[test]
    fn curve_concave_at_low_shape() {
        // shape < 0.5: fast start, slow end
        let v = curve(0.5, 0.2);
        assert!(v > 0.5, "concave curve at 0.5 should be above linear: got {}", v);
    }

    #[test]
    fn curve_edge_cases() {
        assert_eq!(curve(0.0, 0.0), 0.0);
        assert_eq!(curve(1.0, 0.0), 1.0);
        assert_eq!(curve(0.0, 1.0), 0.0);
        assert!((curve(1.0, 1.0) - 1.0).abs() < 0.01);
    }

    #[test]
    fn envelope_rise_reaches_one() {
        let mut ch = EnvelopeChannel { stage: Stage::Rise, phase: 0.0, ..Default::default() };

        let params = ChannelParams {
            rise_param: 0.0, // 1ms = very fast
            fall_param: 0.5,
            shape_param: 0.5,
            loop_mode: false,
            attenuverter: 1.0,
            trigger: Trigger::Any,
            rise_src: None,
            fall_src: None,
            shape_src: None,
            atten_src: None,
        };
        let rise_time = param_to_time(params.rise_param);
        let shape = param_to_shape(params.shape_param);

        let dt = 1.0 / 48000.0;
        // Simulate ~100 samples
        for _ in 0..100 {
            ch.phase += dt / rise_time.max(0.0005);
            if ch.phase >= 1.0 {
                ch.phase = 1.0;
                ch.output = 1.0;
                ch.stage = Stage::Sustain;
                break;
            } else {
                ch.output = curve(ch.phase, shape);
            }
        }

        assert_eq!(ch.stage, Stage::Sustain, "should reach sustain");
        assert!((ch.output - 1.0).abs() < 0.01, "output should reach ~1.0");
    }

    #[test]
    fn envelope_fall_reaches_zero() {
        let mut ch = EnvelopeChannel { stage: Stage::Fall, phase: 0.0, ..Default::default() };

        let params = ChannelParams {
            rise_param: 0.5,
            fall_param: 0.0, // 1ms = very fast
            shape_param: 0.5,
            loop_mode: false,
            attenuverter: 1.0,
            trigger: Trigger::Any,
            rise_src: None,
            fall_src: None,
            shape_src: None,
            atten_src: None,
        };
        let fall_time = param_to_time(params.fall_param);
        let shape = param_to_shape(params.shape_param);

        let dt = 1.0 / 48000.0;
        for _ in 0..200 {
            ch.phase += dt / fall_time.max(0.0005);
            if ch.phase >= 1.0 {
                ch.phase = 1.0;
                ch.output = 0.0;
                ch.stage = Stage::Off;
                break;
            } else {
                ch.output = 1.0 - curve(ch.phase, shape);
            }
        }

        assert_eq!(ch.stage, Stage::Off, "should reach off");
        assert!((ch.output - 0.0).abs() < 0.01, "output should reach ~0.0");
    }

    #[test]
    fn attenuverter_inverts() {
        let ch = EnvelopeChannel { output: 0.75, ..Default::default() };
        let att = -1.0;
        let out = ch.output * att;
        assert!((out - (-0.75)).abs() < 0.001);
    }

    #[test]
    fn attenuverter_scales_down() {
        let ch = EnvelopeChannel { output: 1.0, ..Default::default() };
        let att = 0.5;
        let out = ch.output * att;
        assert!((out - 0.5).abs() < 0.001);
    }

    #[test]
    fn logic_outputs_computed_correctly() {
        let ch_outputs = [0.25f32, 0.5, 0.75, 1.0];
        let sum = ch_outputs.iter().sum::<f32>().clamp(-1.0, 1.0);
        let or_val = ch_outputs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let and_val = ch_outputs.iter().copied().fold(f32::INFINITY, f32::min);
        let invert = -ch_outputs[0];

        // Sum is clamped to [-1, 1]; 2.5 clamps to 1.0
        assert!((sum - 1.0).abs() < 0.01, "sum of 2.5 clamped to 1.0");
        assert!((or_val - 1.0).abs() < 0.01, "OR of max = 1.0");
        assert!((and_val - 0.25).abs() < 0.01, "AND of min = 0.25");
        assert!((invert - (-0.25)).abs() < 0.01, "invert of 0.25 = -0.25");
    }

    #[test]
    fn channel_params_default() {
        let p = ChannelParams::default();
        assert_eq!(p.rise_param, 0.5);
        assert_eq!(p.fall_param, 0.5);
        assert_eq!(p.shape_param, 0.5);
        assert!(!p.loop_mode);
        assert_eq!(p.attenuverter, 1.0);
    }

    #[test]
    fn envelope_state_default() {
        let s = EnvelopeState::default();
        assert_eq!(s.channels.len(), NUM_CHANNELS);
        assert_eq!(s.params.len(), NUM_CHANNELS);
        assert_eq!(s.current_channel, 0);
    }

    #[test]
    fn exponential_param_mapping() {
        // param=0.0 → 1ms
        assert!((param_to_time(0.0) - 0.001).abs() < 0.0001);
        // param=0.5 → ~100ms
        let t = param_to_time(0.5);
        assert!(t > 0.09 && t < 0.11, "mid param should be ~100ms, got {}", t);
        // param=1.0 → 10s
        assert!((param_to_time(1.0) - 10.0).abs() < 0.01);

        // Round-trip
        assert!((time_to_param(param_to_time(0.0)) - 0.0).abs() < 0.001);
        assert!((time_to_param(param_to_time(0.5)) - 0.5).abs() < 0.001);
        assert!((time_to_param(param_to_time(1.0)) - 1.0).abs() < 0.001);
    }

    #[test]
    fn envelope_release_from_rise() {
        // Bug fix: note_off during Rise should immediately transition to Fall
        // partially through rise
        let mut ch = EnvelopeChannel { stage: Stage::Rise, phase: 0.3, ..Default::default() };

        // Simulate receiving a release while in Rise
        let should_release = true;
        if should_release && ch.stage != Stage::Off && ch.stage != Stage::Fall {
            ch.stage = Stage::Fall;
            ch.phase = 0.0;
        }

        assert_eq!(ch.stage, Stage::Fall, "should transition from Rise to Fall on release");
        assert_eq!(ch.phase, 0.0, "phase should reset to 0");
    }

    #[test]
    fn envelope_no_release_when_already_off() {
        let mut ch = EnvelopeChannel { stage: Stage::Off, ..Default::default() };

        let should_release = true;
        let original_stage = ch.stage;
        if should_release && ch.stage != Stage::Off && ch.stage != Stage::Fall {
            ch.stage = Stage::Fall;
        }

        assert_eq!(ch.stage, original_stage, "Off should stay Off on release");
    }
}

#[cfg(test)]
mod doctrine_tests {
    use super::*;

    #[test]
    fn adjust_steps_params_on_current_channel() {
        let mut s = EnvelopeState { current_channel: 1, ..Default::default() };
        let rise0 = s.params[1].rise_param;
        adjust(&mut s, 0, 2, false);
        assert!((s.params[1].rise_param - (rise0 + 0.01)).abs() < 1e-6);
        assert_eq!(s.params[0].rise_param, rise0, "other channels untouched");
    }

    #[test]
    fn adjust_clamps_and_coarse() {
        let mut s = EnvelopeState::default();
        adjust(&mut s, 3, -100, false);
        assert_eq!(s.params[0].attenuverter, -1.0);
        adjust(&mut s, 2, 1, true);
        let expected = 0.5 + 0.05; // default shape 0.5 + coarse step
        assert!((s.params[0].shape_param - expected).abs() < 1e-6);
    }

    #[test]
    fn trigger_row_is_binding_only() {
        let mut s = EnvelopeState::default();
        let before = s.params[0].trigger.clone();
        adjust(&mut s, 4, -10, false);
        adjust(&mut s, 4, 100, true);
        assert_eq!(s.params[0].trigger, before, "h/l must not touch the binding");
    }

    #[test]
    fn trigger_param_roundtrip_includes_off() {
        assert_eq!(Trigger::Any.to_param(), None);
        assert_eq!(Trigger::Off.to_param().as_deref(), Some("off"));
        let a = SourceAddr::parse("sequencer/0/t2").unwrap();
        assert_eq!(Trigger::Source(a.clone()).to_param().as_deref(), Some("sequencer/0/t2"));

        assert_eq!(Trigger::from_param(None), Trigger::Any);
        assert_eq!(Trigger::from_param(Some("off")), Trigger::Off);
        assert_eq!(Trigger::from_param(Some("sequencer/0/t2")), Trigger::Source(a));
    }

    #[test]
    fn old_format_envelope_state_keeps_default_trigger() {
        let mut s = EnvelopeState::default();
        assert!(matches!(s.params[0].trigger, Trigger::Source(_)));
        let old = state::EnvelopeParams {
            channels: vec![state::EnvelopeChannelParams::default(); NUM_CHANNELS],
            ..Default::default()
        };
        apply_params(&mut s, &old);
        assert!(
            matches!(s.params[0].trigger, Trigger::Source(_)),
            "old file must not reset the default trigger"
        );
    }
}
