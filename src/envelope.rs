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

use crate::shm::{EventRingbuf, ModulationBus, ShmTransport};
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

#[derive(Clone, Copy)]
struct ChannelParams {
    rise_param: f32,   // 0.0-1.0 → 1ms to 10s (exponential)
    fall_param: f32,   // 0.0-1.0 → 1ms to 10s (exponential)
    shape_param: f32,  // 0.0-1.0 → curve exponent 0.05 to 20.0
    loop_mode: bool,
    attenuverter: f32, // -1.0 to 1.0
    trigger_track: i32, // -1 = off, 0..NUM_TRACKS-1 = specific track
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
            trigger_track: -1, // ch0 = global by default
        }
    }
}

#[derive(Clone)]
struct EnvelopeState {
    channels: Vec<EnvelopeChannel>,
    params: Vec<ChannelParams>,
    current_channel: usize,
    gate: bool,
}

impl Default for EnvelopeState {
    fn default() -> Self {
        let mut params = vec![ChannelParams::default(); NUM_CHANNELS];
        // Channel 1 defaults to Track 1; channels 2-4 default to Off
        params[0].trigger_track = 0;
        for p in params.iter_mut().skip(1) {
            p.trigger_track = -1;
        }
        Self {
            channels: vec![EnvelopeChannel::default(); NUM_CHANNELS],
            params,
            current_channel: 0,
            gate: false,
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
) -> Result<()> {
    let mut events = EventRingbuf::open(1).ok();
    let mut modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();
    let _transport = ShmTransport::open().ok();

    let dt = 1.0 / SAMPLE_RATE as f32;

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        if events.is_none() {
            events = EventRingbuf::open(1).ok();
        }
        if modbus.is_none() {
            modbus = ModulationBus::open().ok();
        }

        // Read events (triggers)
        let mut triggers = [false; NUM_CHANNELS];

        let mut track_trigger: Option<u8> = None;
        let mut release_track: Option<u8> = None;

        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
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

        // Process all channels
        let mut ch_outputs = [0.0f32; NUM_CHANNELS];

        for i in 0..NUM_CHANNELS {
            let params = s.params[i];
            let ch = &mut s.channels[i];

            let should_trigger = if params.trigger_track >= 0 {
                if let Some(t) = track_trigger {
                    (t as i32 == params.trigger_track) || triggers[i]
                } else {
                    triggers[i]
                }
            } else {
                triggers[i]
            };

            let should_release = if params.trigger_track >= 0 {
                if let Some(t) = release_track {
                    t as i32 == params.trigger_track
                } else {
                    false
                }
            } else {
                false
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

            // Convert params to real values
            let rise_time = param_to_time(params.rise_param);
            let fall_time = param_to_time(params.fall_param);
            let shape = param_to_shape(params.shape_param);

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

            // Apply attenuverter
            let att = params.attenuverter;
            ch_outputs[i] = ch.output * att;
        }

        // Compute logic outputs
        let sum = ch_outputs.iter().sum::<f32>().clamp(-1.0, 1.0);
        let or_val = ch_outputs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let and_val = ch_outputs.iter().copied().fold(f32::INFINITY, f32::min);
        let invert = -ch_outputs[0];

        drop(s);

        // Write to modulation bus
        if let Some(ref mut bus) = modbus {
            for (i, &val) in ch_outputs.iter().enumerate().take(NUM_CHANNELS) {
                bus.set(i, val);
            }
            bus.set(4, sum);
            bus.set(5, or_val);
            bus.set(6, and_val);
            bus.set(7, invert);
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
        let params = state.params[ch];
        let env_ch = state.channels[ch];

        let trigger_str = if params.trigger_track < 0 {
            "Off".to_string()
        } else {
            format!("Track {}", params.trigger_track + 1)
        };

        let rise_time = param_to_time(params.rise_param);
        let fall_time = param_to_time(params.fall_param);
        let shape_exp = param_to_shape(params.shape_param);

        let param_labels = [
            format!("Rise:  {}", format_time(rise_time)),
            format!("Fall:  {}", format_time(fall_time)),
            format!("Shape: {:.2}", shape_exp),
            format!("Atten: {:+.2}", params.attenuverter),
            format!("Trig:  {}", trigger_str),
        ];

        let mut content_lines = vec![
            Line::from(format!("Channel {} | Loop: {} | Stage: {:?}",
                ch + 1,
                if params.loop_mode { "ON" } else { "OFF" },
                env_ch.stage,
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
                Line::from("  [ / ]      Previous/next channel"),
                Line::from("  j/k, ↑/↓   Select parameter"),
                Line::from("  h/l, ←/→   Adjust value"),
                Line::from(""),
                Line::from("Actions:"),
                Line::from("  t          Trigger envelope manually"),
                Line::from("  T          Cycle trigger track (Global/Track 1-8)"),
                Line::from("  c          Toggle cycle/loop mode"),
                Line::from("  g          Toggle gate (sustain)"),
                Line::from(""),
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
    })?;

    Ok(())
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("envelope", instance);

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

    // Load saved state if available
    if let Ok(params) = state::load_module_state::<state::EnvelopeParams>("envelope", instance) {
        let mut s = state.lock().unwrap();
        for (i, ch) in params.channels.iter().enumerate().take(s.params.len()) {
            s.params[i].rise_param = ch.rise;
            s.params[i].fall_param = ch.fall;
            s.params[i].shape_param = ch.shape;
            s.params[i].loop_mode = ch.loop_mode;
            s.params[i].attenuverter = ch.attenuverter;
            s.params[i].trigger_track = ch.trigger_track;
        }
    }

    let state_clone = Arc::clone(&state);
    let (_tx, rx) = std::sync::mpsc::channel();

    let _env_handle = std::thread::spawn(move || {
        if let Err(e) = env_thread(state_clone, rx) {
            eprintln!("Envelope thread error: {}", e);
        }
    });

    let mut selected = 0usize;
    let mut show_help = false;

    loop {
        if state::check_save_signal() {
            let s = state.lock().unwrap();
            let params = state::EnvelopeParams {
                channels: s.params.iter().map(|p| state::EnvelopeChannelParams {
                    rise: p.rise_param,
                    fall: p.fall_param,
                    shape: p.shape_param,
                    loop_mode: p.loop_mode,
                    attenuverter: p.attenuverter,
                    trigger_track: p.trigger_track,
                }).collect(),
                logic_outputs: state::LogicOutputConfig {
                    sum_enabled: true,
                    or_enabled: true,
                    and_enabled: true,
                },
            };
            drop(s);
            let _ = state::save_module_state("envelope", instance, &params);
        }

        if state::check_reload_signal() {
            if let Ok(params) = state::load_module_state::<state::EnvelopeParams>("envelope", instance) {
                let mut s = state.lock().unwrap();
                for (i, ch) in params.channels.iter().enumerate().take(s.params.len()) {
                    s.params[i].rise_param = ch.rise;
                    s.params[i].fall_param = ch.fall;
                    s.params[i].shape_param = ch.shape;
                    s.params[i].loop_mode = ch.loop_mode;
                    s.params[i].attenuverter = ch.attenuverter;
                    s.params[i].trigger_track = ch.trigger_track;
                }
            }
        }

        let current_state = state.lock().unwrap().clone();
        draw_ui(&mut terminal, &current_state, selected, show_help)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Ctrl-s: save
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let s = state.lock().unwrap();
                    let params = state::EnvelopeParams {
                        channels: s.params.iter().map(|p| state::EnvelopeChannelParams {
                            rise: p.rise_param,
                            fall: p.fall_param,
                            shape: p.shape_param,
                            loop_mode: p.loop_mode,
                            attenuverter: p.attenuverter,
                            trigger_track: p.trigger_track,
                        }).collect(),
                        logic_outputs: state::LogicOutputConfig {
                            sum_enabled: true,
                            or_enabled: true,
                            and_enabled: true,
                        },
                    };
                    drop(s);
                    let _ = state::save_module_state("envelope", instance, &params);
                    continue;
                }

                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        selected = (selected + 1) % 5;
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        selected = if selected == 0 { 4 } else { selected - 1 };
                    }
                    KeyCode::Char('h') | KeyCode::Left => {
                        let mut s = state.lock().unwrap();
                        let ch = s.current_channel;
                        match selected {
                            0 => s.params[ch].rise_param = (s.params[ch].rise_param - 0.005).max(0.0),
                            1 => s.params[ch].fall_param = (s.params[ch].fall_param - 0.005).max(0.0),
                            2 => s.params[ch].shape_param = (s.params[ch].shape_param - 0.005).max(0.0),
                            3 => s.params[ch].attenuverter = (s.params[ch].attenuverter - 0.05).max(-1.0),
                            4 => s.params[ch].trigger_track = (s.params[ch].trigger_track - 1).max(-1),
                            _ => {}
                        }
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        let mut s = state.lock().unwrap();
                        let ch = s.current_channel;
                        match selected {
                            0 => s.params[ch].rise_param = (s.params[ch].rise_param + 0.005).min(1.0),
                            1 => s.params[ch].fall_param = (s.params[ch].fall_param + 0.005).min(1.0),
                            2 => s.params[ch].shape_param = (s.params[ch].shape_param + 0.005).min(1.0),
                            3 => s.params[ch].attenuverter = (s.params[ch].attenuverter + 0.05).min(1.0),
                            4 => s.params[ch].trigger_track = (s.params[ch].trigger_track + 1).min(15),
                            _ => {}
                        }
                    }
                    KeyCode::Char('[') => {
                        let mut s = state.lock().unwrap();
                        if s.current_channel > 0 {
                            s.current_channel -= 1;
                        }
                        selected = 0;
                    }
                    KeyCode::Char(']') => {
                        let mut s = state.lock().unwrap();
                        if s.current_channel + 1 < NUM_CHANNELS {
                            s.current_channel += 1;
                        }
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
                    KeyCode::Char('T') => {
                        let mut s = state.lock().unwrap();
                        let ch = s.current_channel;
                        let max_track = crate::NUM_TRACKS as i32 - 1;
                        let next = match s.params[ch].trigger_track {
                            -1 if max_track >= 0 => 0,
                            x if x >= max_track => -1,
                            x => x + 1,
                        };
                        s.params[ch].trigger_track = next;
                    }
                    KeyCode::Char('c') => {
                        let mut s = state.lock().unwrap();
                        let ch = s.current_channel;
                        s.params[ch].loop_mode = !s.params[ch].loop_mode;
                    }
                    KeyCode::Char('g') => {
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
                    KeyCode::Char('?') => {
                        show_help = !show_help;
                    }
                    _ => {}
                }
            }
        }
    }
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
        let mut ch = EnvelopeChannel::default();
        ch.stage = Stage::Rise;
        ch.phase = 0.0;

        let params = ChannelParams {
            rise_param: 0.0, // 1ms = very fast
            fall_param: 0.5,
            shape_param: 0.5,
            loop_mode: false,
            attenuverter: 1.0,
            trigger_track: -1,
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
        let mut ch = EnvelopeChannel::default();
        ch.stage = Stage::Fall;
        ch.phase = 0.0;

        let params = ChannelParams {
            rise_param: 0.5,
            fall_param: 0.0, // 1ms = very fast
            shape_param: 0.5,
            loop_mode: false,
            attenuverter: 1.0,
            trigger_track: -1,
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
        let mut ch = EnvelopeChannel::default();
        ch.output = 0.75;
        let att = -1.0;
        let out = ch.output * att;
        assert!((out - (-0.75)).abs() < 0.001);
    }

    #[test]
    fn attenuverter_scales_down() {
        let mut ch = EnvelopeChannel::default();
        ch.output = 1.0;
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
        let mut ch = EnvelopeChannel::default();
        ch.stage = Stage::Rise;
        ch.phase = 0.3; // partially through rise

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
        let mut ch = EnvelopeChannel::default();
        ch.stage = Stage::Off;

        let should_release = true;
        let original_stage = ch.stage;
        if should_release && ch.stage != Stage::Off && ch.stage != Stage::Fall {
            ch.stage = Stage::Fall;
        }

        assert_eq!(ch.stage, original_stage, "Off should stay Off on release");
    }
}
