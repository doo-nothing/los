use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{self, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{enable_raw_mode, EnterAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use crate::shm::{AudioEvent, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state::{self, TrackMode, TrackParam};

const NUM_STEPS: usize = 16;

#[derive(Clone)]
struct Step {
    active: bool,
    note: u8,
    velocity: u8,
    mod_value: f32,
}

impl Default for Step {
    fn default() -> Self {
        Self { active: false, note: 60, velocity: 100, mod_value: 0.0 }
    }
}

#[derive(Clone)]
struct Track {
    steps: Vec<Step>,
    length: usize,
    pulses: usize,
    rotation: usize,
    muted: bool,
    mode: state::TrackMode,
}

impl Track {
    fn new() -> Self {
        let mut steps = vec![Step::default(); NUM_STEPS];
        for i in (0..NUM_STEPS).step_by(4) {
            steps[i].active = true;
        }
        Self { steps, length: 16, pulses: 5, rotation: 0, muted: false, mode: state::TrackMode::Note }
    }
}

#[derive(Clone)]
struct SequencerState {
    tracks: Vec<Track>,
    current_track: usize,
    bpm: f64,
    playing: bool,
    current_steps: Vec<usize>,
    selected: usize,
    last_notes: Vec<Option<u8>>,
    clipboard: Option<Step>,
    track_clipboard: Option<Track>,
}

impl Default for SequencerState {
    fn default() -> Self {
        let track_count = crate::NUM_TRACKS;
        Self {
            tracks: vec![Track::new(); track_count],
            current_track: 0,
            bpm: 120.0,
            playing: true,
            current_steps: vec![0; track_count],
            selected: 0,
            last_notes: vec![None; track_count],
            clipboard: None,
            track_clipboard: None,
        }
    }
}

impl SequencerState {
    fn track(&self) -> &Track {
        &self.tracks[self.current_track]
    }
    fn track_mut(&mut self) -> &mut Track {
        &mut self.tracks[self.current_track]
    }
}

fn sequencer_thread(
    state: Arc<Mutex<SequencerState>>,
    shutdown: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let sample_rate = 48000.0;

    let mut events = match EventRingbuf::open_producer() {
        Ok(e) => e,
        Err(_) => EventRingbuf::create()?,
    };

    let mut modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();

    let transport = match ShmTransport::open() {
        Ok(t) => t,
        Err(_) => ShmTransport::create(sample_rate as u32)?,
    };

    let mut last_steps: Vec<i32> = vec![-1];

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        if modbus.is_none() {
            modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();
        }

        let (bpm, playing) = {
            let s = state.lock().unwrap();
            (s.bpm, s.playing)
        };

        let clock = transport.clock();
        let samples_per_step = (60.0 / bpm * sample_rate / 4.0) as u64;

        // Scope the lock so it's not held during sleep
        {
            let mut s = state.lock().unwrap();
            
            // Grow tracking vectors as tracks are added
            while last_steps.len() < s.tracks.len() {
                last_steps.push(-1);
            }
            while s.current_steps.len() < s.tracks.len() {
                s.current_steps.push(0);
            }
            while s.last_notes.len() < s.tracks.len() {
                s.last_notes.push(None);
            }

            for (t, last_step) in last_steps.iter_mut().enumerate().take(s.tracks.len()) {
                let len = s.tracks[t].length;
                let current_step = if samples_per_step > 0 && playing {
                    (clock / samples_per_step) as usize % len
                } else {
                    (*last_step).max(0) as usize
                };

                if current_step as i32 != *last_step {
                    s.current_steps[t] = current_step;

                    if playing && !s.tracks[t].muted {
                        let track = &s.tracks[t];
                        let step = &track.steps[current_step];

                        // Always write step value to modbus per-track channel
                        let mod_val = match track.mode {
                            TrackMode::Note => {
                                if step.active {
                                    step.velocity as f32 / 127.0
                                } else {
                                    0.0
                                }
                            }
                            TrackMode::Modulation => step.mod_value,
                        };
                        if let Some(ref mut bus) = modbus {
                            bus.set(8 + t, mod_val);
                        }

                        // Note mode: send note-on/note-off events
                        if track.mode == TrackMode::Note {
                            if let Some(n) = s.last_notes[t] {
                                let _ = events.write_event(&AudioEvent::note_off_source(n, t as u8, *last_step as u32));
                            }
                            if step.active {
                                let _ = events.write_event(&AudioEvent::note_on_source(step.note, step.velocity, t as u8, current_step as u32));
                                s.last_notes[t] = Some(step.note);
                            } else {
                                s.last_notes[t] = None;
                            }
                        }
                    }
                    *last_step = current_step as i32;
                }
            }
        } // lock released here, before sleep

        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(())
}

fn midi_note_name(note: u8) -> String {
    let names = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    let octave = (note / 12).saturating_sub(1);
    let idx = (note % 12) as usize;
    format!("{}{}", names[idx], octave)
}

fn euclidean_apply(steps: &mut [Step], pulses: usize, length: usize, rotation: usize) {
    let len = length.min(steps.len());
    let mut pattern = vec![false; len];
    if pulses > 0 && pulses <= len {
        let mut bucket = 0usize;
        for pat in pattern.iter_mut().take(len) {
            bucket += pulses;
            if bucket >= len {
                bucket -= len;
                *pat = true;
            }
        }
    }
    let rot = rotation % len;
    for (i, step) in steps.iter_mut().enumerate().take(len) {
        let src = (i + len - rot) % len;
        step.active = pattern[src];
    }
    // Deactivate any steps beyond the set length
    for step in steps.iter_mut().skip(len) {
        step.active = false;
    }
}

fn compact_track_row(track: &Track, track_idx: usize, current_track: usize, selected: usize) -> String {
    let is_current = track_idx == current_track;
    let mut row = format!("{}T{} ", if is_current { "▶" } else { " " }, track_idx + 1);
    if track.muted {
        row.push_str("[mute]   ");
    } else {
        let mode_str = match track.mode {
            state::TrackMode::Note => "[NOTE]",
            state::TrackMode::Modulation => "[MOD] ",
        };
        row.push_str(&format!("{:<8} ", mode_str));
    }
    for (i, step) in track.steps.iter().enumerate().take(track.length) {
        let ch = if i == selected && is_current {
            '▽'
        } else if step.active {
            '●'
        } else {
            '○'
        };
        row.push(ch);
    }
    // Pad to 16 chars if shorter
    for _ in track.length..16 {
        row.push(' ');
    }
    row.push_str(&format!("  P:{} L:{} R:{}", track.pulses, track.length, track.rotation));
    row
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &SequencerState,
    mode: &str,
    submode: &str,
    input_buffer: &str,
    pending_count: &Option<String>,
    gt_target: &Option<String>,
    gt_input: &str,
    show_help: bool,
) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        let track_rows = state.tracks.len().clamp(1, 6) as u16;
        
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(track_rows), // Stacked track list
                Constraint::Length(3),            // Step grid
                Constraint::Length(1),            // Note display
                Constraint::Min(0),               // Help or empty
                Constraint::Length(1),            // Status line
            ])
            .split(area);

        // Stacked track rows
        for (ti, trk) in state.tracks.iter().enumerate() {
            let sel = if ti == state.current_track { state.selected } else { 0 };
            let row_text = compact_track_row(trk, ti, state.current_track, sel);
            let style = if ti == state.current_track {
                Style::default().fg(Color::Yellow)
            } else if trk.muted {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Cyan)
            };
            let row_widget = Paragraph::new(row_text).style(style);
            if ti < chunks[0].height as usize {
                let row_rect = Rect::new(chunks[0].x, chunks[0].y + ti as u16, chunks[0].width, 1);
                f.render_widget(row_widget, row_rect);
            }
        }

        let len = state.track().length;
        let play_str = if state.playing { "▶" } else { "■" };
        let cstep = state.current_steps[state.current_track];
        
        let step_width = area.width as usize / len;
        let step_width = step_width.clamp(3, 10);
        
        for i in 0..len {
            let step = &state.track().steps[i];
            let is_current = i == cstep;
            let is_selected = i == state.selected;

            let x = (i * step_width) as u16;
            let rect = Rect::new(x, chunks[1].y, step_width as u16, 3);

            let (bg, fg, marker) = if is_current && is_selected {
                (Color::Yellow, Color::Black, "▶")
            } else if is_current {
                (Color::Green, Color::Black, "▶")
            } else if is_selected {
                (Color::Blue, Color::White, "*")
            } else if step.active {
                (Color::DarkGray, Color::White, " ")
            } else {
                (Color::Reset, Color::DarkGray, " ")
            };

            let third_line = match state.track().mode {
                state::TrackMode::Note => midi_note_name(step.note),
                state::TrackMode::Modulation => format!("{:+.2}", step.mod_value),
            };
            let step_text = format!(
                "{}{}\n{}\n{}",
                marker,
                i,
                if step.active { "●" } else { "○" },
                third_line,
            );

            let step_widget = Paragraph::new(step_text)
                .style(Style::default().fg(fg).bg(bg))
                .block(Block::default().borders(Borders::NONE));
            f.render_widget(step_widget, rect);
        }

        let sel_step = &state.track().steps[state.selected];
        let note_info = match state.track().mode {
            state::TrackMode::Note => format!(
                "Step {}: {} | Note: {} ({}) | Vel: {} | {}",
                state.selected,
                if sel_step.active { "ON" } else { "OFF" },
                sel_step.note,
                midi_note_name(sel_step.note),
                sel_step.velocity,
                if !input_buffer.is_empty() { format!("Input: {}", input_buffer) } else { String::new() }
            ),
            state::TrackMode::Modulation => format!(
                "Step {}: {} | Value: {:+.2} | {}",
                state.selected,
                if sel_step.active { "ON" } else { "OFF" },
                sel_step.mod_value,
                if !input_buffer.is_empty() { format!("Input: {}", input_buffer) } else { String::new() }
            ),
        };
        let note_widget = Paragraph::new(note_info)
            .style(Style::default().fg(Color::Yellow));
        f.render_widget(note_widget, chunks[2]);

        // Bottom status bar
        let mode_label = if mode == "insert" {
            if !submode.is_empty() {
                format!(" INSERT[{}] ", submode)
            } else {
                " INSERT ".to_string()
            }
        } else {
            " NORMAL ".to_string()
        };
        let mode_str = match state.track().mode {
            state::TrackMode::Note => "NOTE",
            state::TrackMode::Modulation => "MOD",
        };
        let mut status_parts = vec![
            mode_label,
            format!("T{}/{}", state.current_track + 1, state.tracks.len()),
            mode_str.to_string(),
            format!("{} {} BPM", play_str, state.bpm as u32),
            format!("Step {}/{}", cstep, len),
            format!("Sel {}", state.selected),
            format!("P:{} L:{} R:{}", state.track().pulses, len, state.track().rotation),
        ];
        if let Some(ref count) = pending_count {
            status_parts.push(format!("Count:{}", count));
        }
        if !submode.is_empty() {
            status_parts.push(format!("{}:{}", submode.to_uppercase(), input_buffer));
        }
        if let Some(ref target) = gt_target {
            status_parts.push(format!("gt{}:{}", target, gt_input));
        }
        let status = status_parts.join(" | ");
        let status_style = if mode == "insert" {
            Style::default().fg(Color::Green).bg(Color::Black)
        } else {
            Style::default().fg(Color::Cyan).bg(Color::Black)
        };
        let status_widget = Paragraph::new(status).style(status_style);
        f.render_widget(status_widget, chunks[4]);

        if show_help {
            let help_text = vec![
                Line::from("━━━ Sequencer Help ━━━"),
                Line::from(""),
                Line::from("Navigation (both modes):"),
                Line::from("  h/l, ←/→  Move left/right"),
                Line::from("  gg         First step"),
                Line::from("  G          Last step"),
                Line::from("  w          Next active step"),
                Line::from("  b          Previous active step"),
                Line::from("  [ / ]      Previous/next track"),
                Line::from("  #j/#k      Jump # tracks down/up"),
                Line::from("  1..9       Jump to track"),
                Line::from(""),
                Line::from("Normal mode:"),
                Line::from("  Enter/i    Enter insert mode"),
                Line::from("  n          New track"),
                Line::from("  dd         Delete track"),
                Line::from("  yy         Yank track"),
                Line::from("  P          Paste track after current"),
                Line::from("  m          Toggle mute"),
                Line::from("  gt#        Go to track #"),
                Line::from("  space      Play/pause"),
                Line::from("  s          Stop"),
                Line::from("  t<NUM>     Set BPM"),
                Line::from(""),
                Line::from("Insert mode:"),
                Line::from("  Esc        Return to normal"),
                Line::from("  Enter/space Toggle step"),
                Line::from("  x          Cut step"),
                Line::from("  y          Yank step"),
                Line::from("  p          Paste step"),
                Line::from("  k/K        Raise note (st/oct)"),
                Line::from("  j/J        Lower note (st/oct)"),
                Line::from("  N<NUM>     Set note (e.g. N60)"),
                Line::from("  gt#        Go to step #"),
                Line::from("  <N>P/L/R   Euclidean pulses/length/rotation"),
                Line::from("  R          Rotate by 1"),
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
    // Setup SIGUSR1 handler for save-on-signal
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("sequencer", instance);
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let _ = manifest.register("sequencer", instance, None);
    
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

    let state = Arc::new(Mutex::new(SequencerState::default()));
    
    // Load saved state if available
    if let Ok(params) = state::load_module_state::<state::SequencerParams>("sequencer", instance) {
        let mut s = state.lock().unwrap();
        if let Some(bpm) = params.bpm { s.bpm = bpm; }
        if let Some(playing) = params.playing { s.playing = playing; }
        if !params.tracks.is_empty() {
            // Rebuild tracks from saved state exactly — clear defaults first
            s.tracks.clear();
            s.current_steps.clear();
            s.last_notes.clear();
            for tp in &params.tracks {
                let mut steps = vec![Step::default(); NUM_STEPS];
                for (i, step) in tp.steps.iter().enumerate().take(steps.len()) {
                    steps[i] = Step { active: step.active, note: step.note, velocity: step.velocity, mod_value: step.mod_value };
                }
                s.tracks.push(Track {
                    steps,
                    length: tp.length.unwrap_or(16),
                    pulses: tp.pulses.unwrap_or(5),
                    rotation: tp.rotation.unwrap_or(0),
                    muted: tp.muted,
                    mode: tp.mode,
                });
                s.current_steps.push(0);
                s.last_notes.push(None);
            }
        } else {
            // Fallback: load flat fields into first track
            let trk = &mut s.tracks[0];
            if let Some(p) = params.euclidean_pulses { trk.pulses = p; }
            if let Some(l) = params.euclidean_length { trk.length = l; }
            if let Some(r) = params.euclidean_rotation { trk.rotation = r; }
            for (i, step) in params.steps.iter().enumerate().take(trk.steps.len()) {
                trk.steps[i] = Step { active: step.active, note: step.note, velocity: step.velocity, mod_value: step.mod_value };
            }
        }
    }
    
    let state_clone = Arc::clone(&state);

    let (_tx, rx) = std::sync::mpsc::channel();

    let _seq_handle = std::thread::spawn(move || {
        if let Err(e) = sequencer_thread(state_clone, rx) {
            eprintln!("Sequencer thread error: {}", e);
        }
    });

    let mut mode = String::from("normal");
    let mut submode = String::new();
    let mut input_buffer = String::new();
    let mut show_help = false;
    let mut pending_count: Option<String> = None;
    let mut pending_d = false;
    let mut pending_y = false;
    let mut pending_g = false;
    let mut gt_target: Option<String> = None;
    let mut gt_input = String::new();
    let mut gt_last_key: Option<Instant> = None;

    loop {
        
        // Check for save-on-signal
        if state::check_save_signal() {
            let s = state.lock().unwrap();
            let params = state::SequencerParams {
                bpm: Some(s.bpm),
                playing: Some(s.playing),
                euclidean_pulses: None,
                euclidean_length: None,
                euclidean_rotation: None,
                steps: vec![],
                tracks: s.tracks.iter().map(|trk| state::TrackParam {
                    steps: trk.steps.iter().map(|step| state::StepParam {
                        active: step.active,
                        note: step.note,
                        velocity: step.velocity,
                        mod_value: step.mod_value,
                    }).collect(),
                    length: Some(trk.length),
                    pulses: Some(trk.pulses),
                    rotation: Some(trk.rotation),
                    muted: trk.muted,
                    mode: trk.mode,
                }).collect(),
            };
            drop(s);
            let _ = state::save_module_state("sequencer", instance, &params);
        }
        
        // Check for reload-on-signal
        if state::check_reload_signal() {
            if let Ok(params) = state::load_module_state::<state::SequencerParams>("sequencer", instance) {
                let mut s = state.lock().unwrap();
                if let Some(bpm) = params.bpm { s.bpm = bpm; }
                if let Some(playing) = params.playing { s.playing = playing; }
                if !params.tracks.is_empty() {
                    s.tracks.clear();
                    s.current_steps.clear();
                    s.last_notes.clear();
                    for tp in &params.tracks {
                        let mut steps = vec![Step::default(); NUM_STEPS];
                        for (i, step) in tp.steps.iter().enumerate().take(steps.len()) {
                            steps[i] = Step { active: step.active, note: step.note, velocity: step.velocity, mod_value: step.mod_value };
                        }
                        s.tracks.push(Track {
                            steps,
                            length: tp.length.unwrap_or(16),
                            pulses: tp.pulses.unwrap_or(5),
                            rotation: tp.rotation.unwrap_or(0),
                            muted: tp.muted,
                            mode: tp.mode,
                        });
                        s.current_steps.push(0);
                        s.last_notes.push(None);
                    }
                    s.current_track = s.current_track.min(s.tracks.len().saturating_sub(1));
                }
            }
        }
        
        // Auto-execute gt on timeout (only when digits were collected)
        if let Some(ref target) = gt_target {
            if gt_last_key.as_ref().is_some_and(|t| t.elapsed() > Duration::from_millis(300)) && !gt_input.is_empty() {
                let mut s = state.lock().unwrap();
                match target.as_str() {
                    "track" => {
                        if let Ok(tnum) = gt_input.parse::<usize>() {
                            let tidx = tnum.saturating_sub(1).min(s.tracks.len().saturating_sub(1));
                            s.current_track = tidx;
                            s.selected = 0;
                        }
                    }
                    "step" => {
                        if let Ok(step) = gt_input.parse::<usize>() {
                            let len = s.track_mut().length;
                            s.selected = step.min(len - 1);
                        }
                    }
                    _ => {}
                }
                gt_target = None;
                gt_input.clear();
                gt_last_key = None;
            }
        }

        let current_state = state.lock().unwrap().clone();
        draw_ui(&mut terminal, &current_state, &mode, &submode, &input_buffer, &pending_count, &gt_target, &gt_input, show_help)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Ctrl-s: save module state
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let s = state.lock().unwrap();
                    let params = state::SequencerParams {
                        bpm: Some(s.bpm),
                        playing: Some(s.playing),
                        euclidean_pulses: None,
                        euclidean_length: None,
                        euclidean_rotation: None,
                        steps: vec![],
                        tracks: s.tracks.iter().map(|trk| TrackParam {
                            steps: trk.steps.iter().map(|step| state::StepParam {
                                active: step.active,
                                note: step.note,
                                velocity: step.velocity,
                                mod_value: step.mod_value,
                            }).collect(),
                            length: Some(trk.length),
                            pulses: Some(trk.pulses),
                            rotation: Some(trk.rotation),
                            muted: trk.muted,
                            mode: trk.mode,
                        }).collect(),
                    };
                    drop(s);
                    let _ = state::save_module_state("sequencer", 0, &params);
                    continue;
                }

                // gt<#> inline collection (mode-independent)
                if let Some(ref target) = gt_target {
                    match key.code {
                        KeyCode::Char(c) if c.is_ascii_digit() => {
                            gt_input.push(c);
                            gt_last_key = Some(Instant::now());
                            continue;
                        }
                        KeyCode::Enter => {
                            if !gt_input.is_empty() {
                                let mut s = state.lock().unwrap();
                                match target.as_str() {
                                    "track" => {
                                        if let Ok(tnum) = gt_input.parse::<usize>() {
                                            let tidx = tnum.saturating_sub(1).min(s.tracks.len().saturating_sub(1));
                                            s.current_track = tidx;
                                            s.selected = 0;
                                        }
                                    }
                                    "step" => {
                                        if let Ok(step) = gt_input.parse::<usize>() {
                                            let len = s.track_mut().length;
                                            s.selected = step.min(len - 1);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            gt_target = None;
                            gt_input.clear();
                            gt_last_key = None;
                            continue;
                        }
                        _ => {
                            // Execute pending gt before handling this key
                            if !gt_input.is_empty() {
                                let mut s = state.lock().unwrap();
                                match target.as_str() {
                                    "track" => {
                                        if let Ok(tnum) = gt_input.parse::<usize>() {
                                            let tidx = tnum.saturating_sub(1).min(s.tracks.len().saturating_sub(1));
                                            s.current_track = tidx;
                                            s.selected = 0;
                                        }
                                    }
                                    "step" => {
                                        if let Ok(step) = gt_input.parse::<usize>() {
                                            let len = s.track_mut().length;
                                            s.selected = step.min(len - 1);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            gt_target = None;
                            gt_input.clear();
                            gt_last_key = None;
                            // Fall through to normal key processing
                        }
                    }
                }

                // Input submode handling (mode-independent)
                if !submode.is_empty() {
                    match key.code {
                        KeyCode::Enter => {
                            let mut s = state.lock().unwrap();
                            match submode.as_str() {
                                "note" => {
                                    if let Ok(note) = input_buffer.parse::<u8>() {
                                        let sel = s.selected;
                                        s.track_mut().steps[sel].note = note.clamp(0, 127);
                                    }
                                }
                                "bpm" => {
                                    if let Ok(bpm) = input_buffer.parse::<f64>() {
                                        s.bpm = bpm.clamp(20.0, 300.0);
                                    }
                                }
                                "track" => {
                                    if let Ok(tnum) = input_buffer.parse::<usize>() {
                                        let tidx = tnum.saturating_sub(1).min(s.tracks.len().saturating_sub(1));
                                        s.current_track = tidx;
                                        s.selected = 0;
                                    }
                                }
                                "step" => {
                                    if let Ok(step) = input_buffer.parse::<usize>() {
                                        let len = s.track_mut().length;
                                        s.selected = step.min(len - 1);
                                    }
                                }
                                _ => {}
                            }
                            submode.clear();
                            input_buffer.clear();
                        }
                        KeyCode::Char(c) if c.is_ascii_digit() || c == '.' => {
                            input_buffer.push(c);
                        }
                        _ => {
                            submode.clear();
                            input_buffer.clear();
                        }
                    }
                    continue;
                }

                // Help toggle (mode-independent)
                if key.code == KeyCode::Char('?') {
                    pending_count = None;
                    pending_d = false;
                    pending_y = false;
                    show_help = !show_help;
                    continue;
                }

                // Mode switch: Esc in insert → normal
                if key.code == KeyCode::Esc && mode == "insert" {
                    mode = String::from("normal");
                    submode.clear();
                    input_buffer.clear();
                    pending_count = None;
                    continue;
                }

                // Digits accumulate into pending_count (both modes)
                if let KeyCode::Char(c) = key.code {
                    if c.is_ascii_digit() {
                        if c == '0' && pending_count.is_none() {
                            let mut s = state.lock().unwrap();
                            s.selected = 0;
                        } else {
                            pending_count.get_or_insert(String::new()).push(c);
                        }
                        pending_d = false;
                        pending_y = false;
                        continue;
                    }
                }

                // Mode-independent navigation
                match key.code {
                    KeyCode::Char('l') | KeyCode::Right if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        s.selected = (s.selected + count) % len;
                        continue;
                    }
                    KeyCode::Char('h') | KeyCode::Left if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        s.selected = s.selected.saturating_sub(count).min(len - 1);
                        continue;
                    }
                    KeyCode::Char('w') if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        for _ in 0..count {
                            for i in 1..=len {
                                let idx = (s.selected + i) % len;
                                if s.track_mut().steps[idx].active {
                                    s.selected = idx;
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                    KeyCode::Char('b') if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        for _ in 0..count {
                            for i in 1..=len {
                                let idx = (s.selected + len - i) % len;
                                if s.track_mut().steps[idx].active {
                                    s.selected = idx;
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        pending_count = None;
                        pending_d = false;
                        pending_y = false;
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        s.selected = (s.selected + 1) % len;
                        continue;
                    }
                    KeyCode::Char('h') | KeyCode::Left => {
                        pending_count = None;
                        pending_d = false;
                        pending_y = false;
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        s.selected = s.selected.saturating_sub(1).min(len - 1);
                        continue;
                    }
                    KeyCode::Char('w') => {
                        pending_count = None;
                        pending_d = false;
                        pending_y = false;
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        for i in 1..=len {
                            let idx = (s.selected + i) % len;
                            if s.track_mut().steps[idx].active {
                                s.selected = idx;
                                break;
                            }
                        }
                        continue;
                    }
                    KeyCode::Char('b') => {
                        pending_count = None;
                        pending_d = false;
                        pending_y = false;
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        for i in 1..=len {
                            let idx = (s.selected + len - i) % len;
                            if s.track_mut().steps[idx].active {
                                s.selected = idx;
                                break;
                            }
                        }
                        continue;
                    }
                    KeyCode::Char('0') => {
                        pending_count = None;
                        pending_d = false;
                        pending_y = false;
                        let mut s = state.lock().unwrap();
                        s.selected = 0;
                        continue;
                    }
                    KeyCode::Char('$') => {
                        pending_count = None;
                        pending_d = false;
                        pending_y = false;
                        let mut s = state.lock().unwrap();
                        s.selected = s.track_mut().length - 1;
                        continue;
                    }
                    _ => {}
                }

                // Count-prefixed P, L, R (both modes)
                match key.code {
                    KeyCode::Char('P') if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(0);
                        let mut s = state.lock().unwrap();
                        let tidx = s.current_track;
                        if count > 0 { s.track_mut().pulses = count.min(16); }
                        let (p, l, r) = (s.track_mut().pulses, s.track_mut().length, s.track_mut().rotation);
                        euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                        pending_d = false; pending_y = false;
                        continue;
                    }
                    KeyCode::Char('L') if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(0);
                        let mut s = state.lock().unwrap();
                        let tidx = s.current_track;
                        if count > 0 { s.track_mut().length = count.clamp(1, 16); }
                        s.selected = s.selected.min(s.track_mut().length - 1);
                        let (p, l, r) = (s.track_mut().pulses, s.track_mut().length, s.track_mut().rotation);
                        euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                        pending_d = false; pending_y = false;
                        continue;
                    }
                    KeyCode::Char('R') if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(0);
                        let mut s = state.lock().unwrap();
                        let tidx = s.current_track;
                        if count > 0 {
                            s.track_mut().rotation = count.min(255);
                        } else {
                            s.track_mut().rotation = (s.track_mut().rotation + 1) % s.track_mut().length;
                        }
                        let (p, l, r) = (s.track_mut().pulses, s.track_mut().length, s.track_mut().rotation);
                        euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                        pending_d = false; pending_y = false;
                        continue;
                    }
                    _ => {}
                }

                // Count-prefixed track jump (both modes)
                if let KeyCode::Char(c) = key.code {
                    if c.is_ascii_digit() && pending_count.is_some() {
                        // Already handled above
                    }
                }

                if mode == "normal" {
                    match key.code {
                        // Enter insert mode
                        KeyCode::Enter | KeyCode::Char('i') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            mode = String::from("insert");
                        }

                        // Count-prefixed track jump
                        KeyCode::Char(c) if c.is_ascii_digit() && pending_count.is_some() => {
                            let count: usize = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            let tidx = count.saturating_sub(1).min(s.tracks.len().saturating_sub(1));
                            s.current_track = tidx;
                            s.selected = 0;
                            pending_g = false;
                        }

                        // Track ops
                        KeyCode::Char('n') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            s.tracks.push(Track::new());
                            s.current_steps.push(0);
                            s.last_notes.push(None);
                            s.current_track = s.tracks.len() - 1;
                            s.selected = 0;
                        }
                        KeyCode::Char('d') => {
                            pending_count = None;
                            pending_g = false;
                            if pending_d {
                                pending_d = false;
                                let mut s = state.lock().unwrap();
                                if s.tracks.len() > 1 {
                                    let was = s.current_track;
                                    s.track_clipboard = Some(s.tracks.remove(was));
                                    s.current_steps.remove(was);
                                    s.last_notes.remove(was);
                                    if s.current_track >= s.tracks.len() {
                                        s.current_track = s.tracks.len() - 1;
                                    }
                                    s.selected = 0;
                                }
                            } else {
                                pending_d = true;
                                pending_y = false;
                            }
                        }
                        KeyCode::Char('y') => {
                            pending_count = None;
                            pending_g = false;
                            if pending_y {
                                pending_y = false;
                                let mut s = state.lock().unwrap();
                                s.track_clipboard = Some(s.tracks[s.current_track].clone());
                            } else {
                                pending_y = true;
                                pending_d = false;
                            }
                        }
                        KeyCode::Char('P') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let clip = state.lock().unwrap().track_clipboard.clone();
                            if let Some(track) = clip {
                                let mut s = state.lock().unwrap();
                                let insert_at = s.current_track + 1;
                                s.tracks.insert(insert_at, track);
                                s.current_steps.insert(insert_at, 0);
                                s.last_notes.insert(insert_at, None);
                                s.current_track = insert_at;
                                s.selected = 0;
                            }
                        }
                        KeyCode::Char('m') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            let tidx = s.current_track;
                            s.tracks[tidx].muted = !s.tracks[tidx].muted;
                        }
                        KeyCode::Char('@') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            let tidx = s.current_track;
                            s.tracks[tidx].mode = match s.tracks[tidx].mode {
                                state::TrackMode::Note => state::TrackMode::Modulation,
                                state::TrackMode::Modulation => state::TrackMode::Note,
                            };
                        }
                        KeyCode::Char('k') | KeyCode::Up if pending_count.is_some() => {
                            let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            for _ in 0..count {
                                if s.current_track > 0 {
                                    s.current_track -= 1;
                                }
                            }
                            s.selected = 0;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                        }
                        KeyCode::Char('j') | KeyCode::Down if pending_count.is_some() => {
                            let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            for _ in 0..count {
                                if s.current_track + 1 < s.tracks.len() {
                                    s.current_track += 1;
                                }
                            }
                            s.selected = 0;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                        }
                        KeyCode::Char('[') | KeyCode::Char('k') | KeyCode::Up => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            if s.current_track > 0 {
                                s.current_track -= 1;
                            }
                            s.selected = 0;
                        }
                        KeyCode::Char(']') | KeyCode::Char('j') | KeyCode::Down => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            if s.current_track + 1 < s.tracks.len() {
                                s.current_track += 1;
                            }
                            s.selected = 0;
                        }
                        KeyCode::Char('g') => {
                            pending_count = None;
                            if pending_g {
                                // gg = go to first track
                                pending_g = false;
                                let mut s = state.lock().unwrap();
                                s.current_track = 0;
                                s.selected = 0;
                            } else {
                                pending_g = true;
                                pending_d = false;
                                pending_y = false;
                            }
                        }
                        KeyCode::Char('t') if pending_g => {
                            pending_g = false;
                            gt_target = Some(String::from("track"));
                            gt_input.clear();
                            gt_last_key = Some(Instant::now());
                        }
                        KeyCode::Char('G') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            s.current_track = s.tracks.len() - 1;
                            s.selected = 0;
                        }

                        // Transport
                        KeyCode::Char(' ') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            s.playing = !s.playing;
                        }
                        KeyCode::Char('s') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            s.playing = false;
                        }
                        KeyCode::Char('t') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            submode = String::from("bpm");
                            input_buffer.clear();
                        }

                        _ => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                        }
                    }
                } else {
                    // Insert mode
                    match key.code {
                        KeyCode::Enter | KeyCode::Char(' ') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            s.track_mut().steps[sel].active = !s.track_mut().steps[sel].active;
                        }
                        KeyCode::Char('x') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            s.clipboard = Some(s.track_mut().steps[sel].clone());
                            s.track_mut().steps[sel].active = false;
                        }
                        KeyCode::Char('y') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            s.clipboard = Some(s.track_mut().steps[sel].clone());
                        }
                        KeyCode::Char('p') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            if let Some(ref clip) = s.clipboard {
                                s.track_mut().steps[sel] = clip.clone();
                                s.track_mut().steps[sel].active = true;
                            }
                        }
                        KeyCode::Char('k') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            if s.track().mode == state::TrackMode::Modulation {
                                let v = s.track_mut().steps[sel].mod_value + 0.01;
                                s.track_mut().steps[sel].mod_value = v.min(1.0);
                            } else {
                                s.track_mut().steps[sel].note = (s.track_mut().steps[sel].note + 1).min(127);
                            }
                        }
                        KeyCode::Char('j') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            if s.track().mode == state::TrackMode::Modulation {
                                let v = s.track_mut().steps[sel].mod_value - 0.01;
                                s.track_mut().steps[sel].mod_value = v.max(-1.0);
                            } else {
                                s.track_mut().steps[sel].note = s.track_mut().steps[sel].note.saturating_sub(1);
                            }
                        }
                        KeyCode::Char('K') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            if s.track().mode == state::TrackMode::Modulation {
                                let v = s.track_mut().steps[sel].mod_value + 0.1;
                                s.track_mut().steps[sel].mod_value = v.min(1.0);
                            } else {
                                s.track_mut().steps[sel].note = (s.track_mut().steps[sel].note + 12).min(127);
                            }
                        }
                        KeyCode::Char('J') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            if s.track().mode == state::TrackMode::Modulation {
                                let v = s.track_mut().steps[sel].mod_value - 0.1;
                                s.track_mut().steps[sel].mod_value = v.max(-1.0);
                            } else {
                                s.track_mut().steps[sel].note = s.track_mut().steps[sel].note.saturating_sub(12);
                            }
                        }
                        KeyCode::Char('g') => {
                            pending_count = None;
                            if pending_g {
                                // gg = go to step 0
                                pending_g = false;
                                let mut s = state.lock().unwrap();
                                s.selected = 0;
                            } else {
                                pending_g = true;
                            }
                        }
                        KeyCode::Char('t') if pending_g => {
                            pending_g = false;
                            gt_target = Some(String::from("step"));
                            gt_input.clear();
                            gt_last_key = Some(Instant::now());
                        }
                        KeyCode::Char('t') => {
                            pending_count = None;
                            submode = String::from("bpm");
                            input_buffer.clear();
                        }
                        KeyCode::Char('N') => {
                            pending_count = None;
                            submode = String::from("note");
                            input_buffer.clear();
                        }
                        // Euclidean (no count)
                        KeyCode::Char('P') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let tidx = s.current_track;
                            let (p, l, r) = (s.track_mut().pulses, s.track_mut().length, s.track_mut().rotation);
                            euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                        }
                        KeyCode::Char('L') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let tidx = s.current_track;
                            s.selected = s.selected.min(s.track_mut().length - 1);
                            let (p, l, r) = (s.track_mut().pulses, s.track_mut().length, s.track_mut().rotation);
                            euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                        }
                        KeyCode::Char('R') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let tidx = s.current_track;
                            s.track_mut().rotation = (s.track_mut().rotation + 1) % s.track_mut().length;
                            let (p, l, r) = (s.track_mut().pulses, s.track_mut().length, s.track_mut().rotation);
                            euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                        }
                        _ => {
                            pending_count = None;
                            pending_g = false;
                        }
                    }
                }
            }
        }
    }
}
