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
    widgets::{Block, Borders, Gauge, Paragraph},
    Terminal,
};

use crate::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

#[derive(Clone)]
struct VoiceState {
    shape: f32,
    sub: f32,
    fm: f32,
    output: u8,
    freq: f32,
    gate: bool,
    level: f32,
    velocity: f32, // 0.0-1.0 from last note_on
    // Receiver-side bindings: each input names its modulation source
    // (docs/keybindings.md, routing.rs). None = use the local param value.
    shape_src: Option<SourceAddr>,
    sub_src: Option<SourceAddr>,
    fm_src: Option<SourceAddr>,
    level_src: Option<SourceAddr>,
    /// Amplitude control (replaces the old hardwired modbus ch 0).
    /// None or unresolvable = 1.0 — an unpatched voice is audible.
    amp_src: Option<SourceAddr>,
    /// Which sequencer track's notes this voice plays. None = all tracks.
    notes_src: Option<SourceAddr>,
}

impl Default for VoiceState {
    fn default() -> Self {
        Self {
            shape: 0.5,
            sub: 0.0,
            fm: 0.0,
            output: 0,
            freq: 440.0,
            gate: false,
            level: 0.0,
            velocity: 0.0,
            shape_src: None,
            sub_src: None,
            fm_src: None,
            level_src: None,
            amp_src: SourceAddr::parse("envelope/0/ch1"),
            notes_src: None,
        }
    }
}

fn voice_thread(
    state: Arc<Mutex<VoiceState>>,
    shutdown: std::sync::mpsc::Receiver<()>,
    instance: usize,
) -> Result<()> {
    let shm_name = format!("/los_audio_voice_{}", instance);
    let mut ringbuf = AudioRingbuf::open(&shm_name)
        .or_else(|_| AudioRingbuf::create(&shm_name))?;

    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let _ = manifest.register("voice", instance, Some(&shm_name), 0);

    let consumer_id = crate::shm::consumer_id("voice", instance);
    let mut events = EventRingbuf::open(consumer_id).ok();
    let mut modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();

    let _transport = ShmTransport::open()
        .or_else(|_| ShmTransport::create(48000))?;

    let mut phase = 0.0f64;
    let mut sub_phase = 0.0f64;

    let sample_rate = 48000.0;
    let block_size = 64;

    // Resolved modbus channels for each binding; refreshed periodically so a
    // restarted source module (fresh channel claim) keeps working.
    let mut ch_shape: Option<usize> = None;
    let mut ch_sub: Option<usize> = None;
    let mut ch_fm: Option<usize> = None;
    let mut ch_amp: Option<usize> = None;
    let mut note_filter: Option<u8> = None;
    let mut refresh_in = 0u32;

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        // Reconnect to shared resources if disconnected
        if events.is_none() {
            events = EventRingbuf::open(consumer_id).ok();
        }
        if modbus.is_none() {
            modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();
        }

        // Re-resolve bindings through the manifest (~every 256 blocks)
        if refresh_in == 0 {
            refresh_in = 256;
            let entries = manifest.entries();
            let s = state.lock().unwrap();
            ch_shape = s.shape_src.as_ref().and_then(|a| routing::resolve(&entries, a));
            ch_sub = s.sub_src.as_ref().and_then(|a| routing::resolve(&entries, a));
            ch_fm = s.fm_src.as_ref().and_then(|a| routing::resolve(&entries, a));
            ch_amp = s.amp_src.as_ref().and_then(|a| routing::resolve(&entries, a));
            note_filter = s.notes_src.as_ref().and_then(routing::note_source_track);
        }
        refresh_in -= 1;

        // Read events (note_on sets pitch + velocity, note_off sets gate=false).
        // With a notes binding, only events from that sequencer track apply.
        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                if let Some(t) = note_filter {
                    if event.source != t {
                        continue;
                    }
                }
                let mut s = state.lock().unwrap();
                match event.event_type {
                    0 => { // Note on
                        s.freq = event.value; // frequency from note
                        s.velocity = event.param as f32 / 127.0;
                        s.gate = true;
                    }
                    1 => { // Note off
                        s.gate = false;
                        // velocity stays as last value for release tail
                    }
                    _ => {}
                }
            }
        }

        // Generate audio
        let s = state.lock().unwrap();
        let freq = s.freq as f64;
        let output_mode = s.output;
        // If gate is on but velocity hasn't been set (no note_on received),
        // default to full velocity so the voice produces sound immediately
        // on session load or when the sequencer hasn't started yet.
        let velocity = if s.gate && s.velocity < 0.001 { 1.0 } else { s.velocity };

        let chan_val = |ch: Option<usize>| -> Option<f32> {
            ch.and_then(|c| modbus.as_ref().map(|m| m.get(c)))
        };

        // Amplitude: bound + resolvable -> modbus value; otherwise 1.0 so an
        // unpatched (or orphaned) voice stays audible.
        let amp = if s.amp_src.is_some() {
            chan_val(ch_amp).unwrap_or(1.0)
        } else {
            1.0
        };

        let shape = chan_val(ch_shape).unwrap_or(s.shape).clamp(0.0, 1.0);
        let sub_mix = chan_val(ch_sub).unwrap_or(s.sub).clamp(0.0, 1.0);
        let fm_amount = chan_val(ch_fm).unwrap_or(s.fm).clamp(0.0, 1.0);

        // Final amplitude: amp source (usually an envelope) × step velocity
        let level = amp * velocity;

        let mut block = vec![0.0f32; block_size * 2];

        for i in 0..block_size {
            // FM
            let fm_mod = (phase * fm_amount as f64 * 2.0 * std::f64::consts::PI).sin() * 0.1;

            // Main oscillator with shape morphing
            let main_phase = (phase + fm_mod).fract();
            let sine = (main_phase * 2.0 * std::f64::consts::PI).sin() as f32;
            let saw = (main_phase * 2.0 - 1.0) as f32;
            let square = if main_phase < 0.5 { 1.0f32 } else { -1.0f32 };

            let main = if shape < 0.5 {
                sine * (1.0 - shape * 2.0) + saw * (shape * 2.0)
            } else {
                saw * (1.0 - (shape - 0.5) * 2.0) + square * ((shape - 0.5) * 2.0)
            };

            // Sub oscillator (square, one octave down)
            let sub = if sub_phase < 0.5 { 1.0f32 } else { -1.0f32 };

            // Mix
            let sample = match output_mode {
                0 => main,
                1 => main * (1.0 - sub_mix) + sub * sub_mix,
                _ => main * (1.0 - sub_mix) + sub * sub_mix * 0.5,
            };

            let output = sample * level * 0.5;
            block[i * 2] = output;
            block[i * 2 + 1] = output;

            phase = (phase + freq / sample_rate).fract();
            sub_phase = (sub_phase + freq / (sample_rate * 2.0)).fract();
        }

        drop(s);

        // Update level meter for TUI
        {
            let mut s = state.lock().unwrap();
            s.level = level;
        }

        // Write to ringbuffer — retry when full, don't drop blocks
        loop {
            match ringbuf.write(&block) {
                Ok(()) => break,
                Err(_) => {
                    std::thread::yield_now();
                }
            }
        }
    }

    Ok(())
}

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &VoiceState,
    selected: usize,
    show_help: bool,
    overlay: Option<&str>,
    picker: Option<(Vec<String>, usize)>,
) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),  // Shape
                Constraint::Length(1),  // Sub
                Constraint::Length(1),  // FM
                Constraint::Length(1),  // Output
                Constraint::Length(1),  // Amp binding
                Constraint::Length(1),  // Notes binding
                Constraint::Length(1),  // Level meter
                Constraint::Min(0),
                Constraint::Length(1),  // Status
            ])
            .split(area);

        // Status
        let gate_str = if state.gate { "●" } else { "○" };
        let status = format!(
            "{} {:.1} Hz | Output: {} | Env: {:.0}% | Vel: {:.0}% | Level: {:.0}%",
            gate_str,
            state.freq,
            match state.output { 0 => "Main", 1 => "Main+Sub", _ => "Mix" },
            state.level / state.velocity.max(0.001) * 100.0, // env = level / velocity
            state.velocity * 100.0,
            state.level * 100.0
        );
        let status_widget = Paragraph::new(status).style(Style::default().fg(Color::Cyan));
        f.render_widget(status_widget, chunks[8]);

        // Parameters (gauges show the @source when bound)
        let src_label = |a: &Option<SourceAddr>| -> String {
            a.as_ref().map(|a| format!(" @{}", a)).unwrap_or_default()
        };
        let param_srcs = [&state.shape_src, &state.sub_src, &state.fm_src];
        let params = [
            ("Shape", state.shape, selected == 0),
            ("Sub", state.sub, selected == 1),
            ("FM", state.fm, selected == 2),
        ];

        for (i, (name, value, is_selected)) in params.iter().enumerate() {
            let style = if *is_selected {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::White)
            };

            let gauge = Gauge::default()
                .gauge_style(style)
                .ratio(*value as f64)
                .label(format!("{}: {:.2}{}", name, value, src_label(param_srcs[i])));
            f.render_widget(gauge, chunks[i]);
        }

        // Binding-only rows: amp (amplitude source) and notes (track filter)
        let amp_text = format!(
            "Amp:   {}",
            state.amp_src.as_ref().map(|a| a.to_string()).unwrap_or_else(|| String::from("(unbound = 1.0)"))
        );
        let amp_style = if selected == 4 { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::White) };
        f.render_widget(Paragraph::new(amp_text).style(amp_style), chunks[4]);

        let notes_text = format!(
            "Notes: {}",
            state.notes_src.as_ref().map(|a| a.to_string()).unwrap_or_else(|| String::from("all tracks"))
        );
        let notes_style = if selected == 5 { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::White) };
        f.render_widget(Paragraph::new(notes_text).style(notes_style), chunks[5]);

        // Output mode
        let output_style = if selected == 3 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };
        let output_text = format!(
            "Output: [{}] Main  [{}] Main+Sub  [{}] Mix",
            if state.output == 0 { "●" } else { "○" },
            if state.output == 1 { "●" } else { "○" },
            if state.output == 2 { "●" } else { "○" },
        );
        let output_widget = Paragraph::new(output_text).style(output_style);
        f.render_widget(output_widget, chunks[3]);

        // Level meter
        let level_gauge = Gauge::default()
            .gauge_style(Style::default().fg(Color::Green))
            .ratio(state.level as f64)
            .label(format!("Level: {:.0}%", state.level * 100.0));
        f.render_widget(level_gauge, chunks[6]);

        // Help overlay
        if show_help {
            let help_text = vec![
                Line::from("━━━ Voice Help ━━━"),
                Line::from(""),
                Line::from("Parameters:"),
                Line::from("  j/k, ↑/↓  Select parameter"),
                Line::from("  h/l, ←/→  Adjust value"),
                Line::from("  H/L        Coarse adjust (10x)"),
                Line::from("  #j/#l ...  Count prefix repeats"),
                Line::from("  gg / G     First / last param"),
                Line::from(""),
                Line::from("Output row: Main / Main+Sub / Mix"),
                Line::from(""),
                Line::from("Routing:"),
                Line::from("  @          Bind selected param to a source (picker)"),
                Line::from("  Amp row    Amplitude source (default env ch1)"),
                Line::from("  Notes row  Which seq track's notes to play"),
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

fn snapshot_params(s: &VoiceState) -> state::VoiceParams {
    state::VoiceParams {
        format: state::STATE_FORMAT,
        shape: Some(s.shape),
        sub: Some(s.sub),
        fm: Some(s.fm),
        output: Some(s.output),
        freq: Some(s.freq),
        gate: Some(s.gate),
        level: Some(s.level),
        velocity: Some(s.velocity),
        shape_src: s.shape_src.as_ref().map(|a| a.to_string()),
        sub_src: s.sub_src.as_ref().map(|a| a.to_string()),
        fm_src: s.fm_src.as_ref().map(|a| a.to_string()),
        level_src: s.level_src.as_ref().map(|a| a.to_string()),
        amp_src: s.amp_src.as_ref().map(|a| a.to_string()),
        notes_src: s.notes_src.as_ref().map(|a| a.to_string()),
    }
}

fn apply_params(s: &mut VoiceState, params: &state::VoiceParams) {
    if let Some(v) = params.shape { s.shape = v; }
    if let Some(v) = params.sub { s.sub = v; }
    if let Some(v) = params.fm { s.fm = v; }
    if let Some(v) = params.output { s.output = v; }
    if let Some(v) = params.freq { s.freq = v; }
    if let Some(v) = params.gate { s.gate = v; }
    if let Some(v) = params.level { s.level = v; }
    if let Some(v) = params.velocity { s.velocity = v; }
    // Binding fields only exist in format-2 files. An old file simply lacks
    // them — keep the defaults (e.g. amp -> envelope/0/ch1) rather than
    // unbinding everything.
    if params.format >= state::STATE_FORMAT {
        s.shape_src = params.shape_src.as_deref().and_then(SourceAddr::parse);
        s.sub_src = params.sub_src.as_deref().and_then(SourceAddr::parse);
        s.fm_src = params.fm_src.as_deref().and_then(SourceAddr::parse);
        s.level_src = params.level_src.as_deref().and_then(SourceAddr::parse);
        s.amp_src = params.amp_src.as_deref().and_then(SourceAddr::parse);
        s.notes_src = params.notes_src.as_deref().and_then(SourceAddr::parse);
    }
}

/// The binding slot for each param row (rows 0-2 modulate values; 4-5 are
/// binding-only rows).
fn row_binding(s: &VoiceState, row: usize) -> Option<&Option<SourceAddr>> {
    match row {
        0 => Some(&s.shape_src),
        1 => Some(&s.sub_src),
        2 => Some(&s.fm_src),
        4 => Some(&s.amp_src),
        5 => Some(&s.notes_src),
        _ => None, // output row has no binding
    }
}

fn set_row_binding(s: &mut VoiceState, row: usize, addr: Option<SourceAddr>) {
    match row {
        0 => s.shape_src = addr,
        1 => s.sub_src = addr,
        2 => s.fm_src = addr,
        4 => s.amp_src = addr,
        5 => s.notes_src = addr,
        _ => {}
    }
}

/// Undo slots: 0–3 = row values (shape/sub/fm/output); 10+row = bindings.
const BIND_SLOT: usize = 10;

impl crate::undo::ParamUndo for VoiceState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        match slot {
            0 => Some(V::F32(self.shape)),
            1 => Some(V::F32(self.sub)),
            2 => Some(V::F32(self.fm)),
            3 => Some(V::U8(self.output)),
            s if s >= BIND_SLOT => row_binding(self, s - BIND_SLOT)
                .map(|b| V::Src(b.as_ref().map(|a| a.to_string()))),
            _ => None,
        }
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        match (slot, value) {
            (0, V::F32(v)) => self.shape = v,
            (1, V::F32(v)) => self.sub = v,
            (2, V::F32(v)) => self.fm = v,
            (3, V::U8(v)) => self.output = v,
            (s, V::Src(a)) if s >= BIND_SLOT => {
                set_row_binding(self, s - BIND_SLOT, a.as_deref().and_then(SourceAddr::parse));
            }
            _ => {}
        }
    }
}

const NUM_ROWS: usize = 6; // shape, sub, fm, output, amp, notes

/// Adjust a param row by `steps` (doctrine: h/l fine, H/L coarse ×10).
fn adjust(s: &mut VoiceState, row: usize, steps: i32, coarse: bool) {
    use crate::keys::{cycle, step_f32};
    match row {
        0 => s.shape = step_f32(s.shape, steps, 0.05, coarse, 0.0, 1.0),
        1 => s.sub = step_f32(s.sub, steps, 0.05, coarse, 0.0, 1.0),
        2 => s.fm = step_f32(s.fm, steps, 0.05, coarse, 0.0, 1.0),
        3 => s.output = cycle(s.output as usize, steps, 3) as u8,
        _ => {}
    }
}

pub fn run(instance: usize) -> Result<()> {
    // Initialize terminal with retry logic (handles tmux PTY race)
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("voice", instance);
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

    let state = Arc::new(Mutex::new(VoiceState::default()));
    
    // Load saved state if available
    if let Ok(params) = state::load_module_state::<state::VoiceParams>("voice", instance) {
        apply_params(&mut state.lock().unwrap(), &params);
    }
    
    let state_clone = Arc::clone(&state);

    let (_tx, rx) = std::sync::mpsc::channel();

    let _voice_handle = std::thread::spawn(move || {
        if let Err(e) = voice_thread(state_clone, rx, instance) {
            eprintln!("Voice thread error: {}", e);
        }
    });

    let mut selected = 0usize;
    let mut show_help = false;
    let mut count = crate::keys::Count::default();
    // Global transport handle for Space = play/pause (lazily reopened)
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();
    let mut picker = crate::picker::Picker::default();
    let mut history = crate::undo::ParamHistory::default();
    let mut pending_g = false;
    let mut ex = crate::excmd::ExLine::default();
    let mut ex_msg: Option<String> = None;
    let mut patch_name: Option<String> = None;
    let mut baseline = state::to_toml_string(&snapshot_params(&state.lock().unwrap())).unwrap_or_default();
    let mut should_quit = false;

    loop {
        // Check for save-on-signal
        if state::check_save_signal() {
            let params = snapshot_params(&state.lock().unwrap());
            let _ = state::save_module_state("voice", instance, &params);
        }
        
        // Check for reload-on-signal
        if state::check_reload_signal() {
            if let Ok(params) = state::load_module_state::<state::VoiceParams>("voice", instance) {
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
                    if let crate::picker::PickerEvent::Chosen(addr) = picker.handle_key(key.code) {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = state.lock().unwrap();
                        let slot = BIND_SLOT + selected;
                        let old = s.get_param(slot);
                        set_row_binding(&mut s, selected, addr.clone());
                        if let Some(old) = old {
                            history.record(slot, "Bind", old, ParamValue::Src(addr.map(|a| a.to_string())));
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
                            ExCommand::Edit(name) => match state::load_patch::<state::VoiceParams>(&name) {
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
                // Ctrl-s: save module state
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let params = snapshot_params(&state.lock().unwrap());
                    let _ = state::save_module_state("voice", instance, &params);
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
                        let old = s.get_param(selected);
                        adjust(&mut s, selected, steps, coarse);
                        let new = s.get_param(selected);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(selected, "Adjust", old, new);
                        }
                    }
                    KeyCode::Char('g') => {
                        count.clear();
                        if pending_g {
                            pending_g = false;
                            selected = 0;
                        } else {
                            pending_g = true;
                        }
                    }
                    KeyCode::Char('G') => {
                        count.clear();
                        selected = NUM_ROWS - 1;
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
                        let current = row_binding(&s, selected).cloned().flatten();
                        drop(s);
                        if row_binding(&VoiceState::default(), selected).is_some() {
                            picker.open(sources, current.as_ref());
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
                        pending_g = false;
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
mod tests {
    use super::*;

    #[test]
    fn adjust_steps_and_clamps_params() {
        let mut s = VoiceState::default();
        let shape0 = s.shape;
        adjust(&mut s, 0, 1, false);
        assert!((s.shape - (shape0 + 0.05)).abs() < 1e-6);
        adjust(&mut s, 0, -100, false);
        assert_eq!(s.shape, 0.0, "shape clamps at 0");
        adjust(&mut s, 1, 100, true);
        assert_eq!(s.sub, 1.0, "sub clamps at 1");
    }

    #[test]
    fn adjust_output_cycles() {
        let mut s = VoiceState::default();
        assert_eq!(s.output, 0);
        adjust(&mut s, 3, 1, false);
        assert_eq!(s.output, 1);
        adjust(&mut s, 3, 2, false);
        assert_eq!(s.output, 0, "output wraps");
        adjust(&mut s, 3, -1, false);
        assert_eq!(s.output, 2, "output wraps backward");
    }

    #[test]
    fn coarse_adjust_is_ten_times() {
        let mut s = VoiceState::default();
        adjust(&mut s, 2, 1, true);
        assert!((s.fm - 0.5).abs() < 1e-6);
    }

    #[test]
    fn old_format_state_keeps_default_bindings() {
        let mut s = VoiceState::default();
        assert!(s.amp_src.is_some(), "default amp binding present");
        // simulate a pre-v2 state file: format 0, no binding fields
        let old = state::VoiceParams {
            shape: Some(0.7),
            ..Default::default()
        };
        apply_params(&mut s, &old);
        assert_eq!(s.shape, 0.7, "values still apply");
        assert!(s.amp_src.is_some(), "old file must not unbind amp");
    }

    #[test]
    fn v2_state_unbind_is_honored() {
        let mut s = VoiceState::default();
        let p = state::VoiceParams {
            format: state::STATE_FORMAT,
            ..Default::default()
        };
        apply_params(&mut s, &p);
        assert!(s.amp_src.is_none(), "format-2 file with no amp_src = unbound");
    }
}
