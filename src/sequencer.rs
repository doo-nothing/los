use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use crate::shm::{AudioEvent, EventRingbuf, ShmTransport};
use crate::state::{self, TrackParam};

const NUM_STEPS: usize = 16;

#[derive(Clone)]
struct Step {
    active: bool,
    note: u8,
    velocity: u8,
}

impl Default for Step {
    fn default() -> Self {
        Self { active: false, note: 60, velocity: 100 }
    }
}

#[derive(Clone)]
struct Track {
    steps: Vec<Step>,
    length: usize,
    pulses: usize,
    rotation: usize,
    muted: bool,
}

impl Track {
    fn new() -> Self {
        let mut steps = vec![Step::default(); NUM_STEPS];
        for i in (0..NUM_STEPS).step_by(4) {
            steps[i].active = true;
        }
        Self { steps, length: 16, pulses: 5, rotation: 0, muted: false }
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
        let track_count = 1;
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

    let mut events = match EventRingbuf::open() {
        Ok(e) => e,
        Err(_) => EventRingbuf::create()?,
    };

    let transport = match ShmTransport::open() {
        Ok(t) => t,
        Err(_) => ShmTransport::create(sample_rate as u32)?,
    };

    let mut last_steps: Vec<i32> = vec![-1];

    loop {
        if shutdown.try_recv().is_ok() {
            break;
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

            for t in 0..s.tracks.len() {
                let len = s.tracks[t].length;
                let current_step = if samples_per_step > 0 && playing {
                    (clock / samples_per_step) as usize % len
                } else {
                    last_steps[t].max(0) as usize
                };

                if current_step as i32 != last_steps[t] {
                    s.current_steps[t] = current_step;

                    if playing && !s.tracks[t].muted {
                        if let Some(n) = s.last_notes[t] {
                            let _ = events.write_event(&AudioEvent::note_off(n, last_steps[t] as u32));
                        }
                        if s.tracks[t].steps[current_step].active {
                            let note = s.tracks[t].steps[current_step].note;
                            let vel = s.tracks[t].steps[current_step].velocity;
                            let _ = events.write_event(&AudioEvent::note_on(note, vel, current_step as u32));
                            s.last_notes[t] = Some(note);
                        } else {
                            s.last_notes[t] = None;
                        }
                    }
                    last_steps[t] = current_step as i32;
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
        for i in 0..len {
            bucket += pulses;
            if bucket >= len {
                bucket -= len;
                pattern[i] = true;
            }
        }
    }
    let rot = rotation % len;
    for i in 0..len {
        let src = (i + len - rot) % len;
        steps[i].active = pattern[src];
    }
    // Deactivate any steps beyond the set length
    for i in len..steps.len() {
        steps[i].active = false;
    }
}

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &SequencerState,
    input_mode: &str,
    input_buffer: &str,
    show_help: bool,
) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),  // Track tabs
                Constraint::Length(1),  // Status line
                Constraint::Length(3),  // Step grid
                Constraint::Length(1),  // Note display
                Constraint::Min(0),     // Help or empty
            ])
            .split(area);

        // Track tabs
        let mut tabs = String::new();
        for (i, t) in state.tracks.iter().enumerate() {
            let sel = if i == state.current_track { "*" } else { " " };
            let m = if t.muted { "m" } else { "" };
            tabs.push_str(&format!("│{}{}{}│", sel, i + 1, m));
        }
        let tabs_widget = Paragraph::new(tabs)
            .style(Style::default().fg(Color::Cyan));
        f.render_widget(tabs_widget, chunks[0]);

        // Status line
        let len = state.track().length;
        let play_str = if state.playing { "▶" } else { "■" };
        let cstep = state.current_steps[state.current_track];
        let status = format!(
            "{} {} BPM | Step {}/{} | Sel {} | P:{} L:{} R:{} | {}",
            play_str, state.bpm as u32, cstep, len, state.selected,
            state.track().pulses, len, state.track().rotation,
            input_mode
        );
        let status_widget = Paragraph::new(status)
            .style(Style::default().fg(Color::Cyan));
        f.render_widget(status_widget, chunks[1]);

        let step_width = area.width as usize / len;
        let step_width = step_width.max(3).min(10);
        
        for i in 0..len {
            let step = &state.track().steps[i];
            let is_current = i == cstep;
            let is_selected = i == state.selected;

            let x = (i * step_width) as u16;
            let rect = Rect::new(x, chunks[2].y, step_width as u16, 3);

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

            let step_text = format!(
                "{}{}\n{}\n{}",
                marker,
                i,
                if step.active { "●" } else { "○" },
                midi_note_name(step.note)
            );

            let step_widget = Paragraph::new(step_text)
                .style(Style::default().fg(fg).bg(bg))
                .block(Block::default().borders(Borders::NONE));
            f.render_widget(step_widget, rect);
        }

        let sel_step = &state.track().steps[state.selected];
        let note_info = format!(
            "Step {}: {} | Note: {} ({}) | Vel: {} | {}",
            state.selected,
            if sel_step.active { "ON" } else { "OFF" },
            sel_step.note,
            midi_note_name(sel_step.note),
            sel_step.velocity,
            if !input_buffer.is_empty() { format!("Input: {}", input_buffer) } else { String::new() }
        );
        let note_widget = Paragraph::new(note_info)
            .style(Style::default().fg(Color::Yellow));
        f.render_widget(note_widget, chunks[3]);

        if show_help {
            let help_text = vec![
                Line::from("━━━ Sequencer Help ━━━"),
                Line::from(""),
                Line::from("Navigation:"),
                Line::from("  h/l, ←/→  Move left/right"),
                Line::from("  0          First step"),
                Line::from("  $          Last step"),
                Line::from("  w          Next active step"),
                Line::from("  b          Previous active step"),
                Line::from("  gg         Go to step 0"),
                Line::from(""),
                Line::from("Tracks:"),
                Line::from("  n          New track"),
                Line::from("  [ / ]      Previous/next track"),
                Line::from("  m          Toggle mute track"),
                Line::from(""),
                Line::from("Editing:"),
                Line::from("  Enter      Toggle step on/off"),
                Line::from("  x          Delete step (copies to clipboard)"),
                Line::from("  p          Paste step from clipboard"),
                Line::from("  k/K        Raise note (semitone/octave)"),
                Line::from("  j/J        Lower note (semitone/octave)"),
                Line::from("  N<NUM>     Set note (e.g. N60 for C4)"),
                Line::from("  t<NUM>     Set BPM (e.g. t140)"),
                Line::from(""),
                Line::from("Euclidean:"),
                Line::from("  <N>P       Set pulses and fill"),
                Line::from("  <N>L       Set pattern length"),
                Line::from("  <N>R       Set rotation"),
                Line::from("  R          Rotate by 1"),
                Line::from("  (e.g. 5P, 16L, 3R)"),
                Line::from(""),
                Line::from("Transport:"),
                Line::from("  space      Play/pause"),
                Line::from("  s          Stop"),
                Line::from(""),
                Line::from("  ?          Close this help"),
                Line::from("  q          Quit"),
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
    use std::fs::OpenOptions;
    use std::io::Write;
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/los-sequencer.log")
        .ok();
    
    if let Some(ref mut f) = log {
        writeln!(f, "Sequencer starting at {:?}", std::time::SystemTime::now()).ok();
        unsafe {
            writeln!(f, "isatty(stdin)={} isatty(stdout)={}", 
                libc::isatty(libc::STDIN_FILENO), 
                libc::isatty(libc::STDOUT_FILENO)).ok();
            let tty_path = std::ffi::CString::new("/dev/tty").unwrap();
            let dev = libc::open(tty_path.as_ptr(), libc::O_RDWR);
            if dev >= 0 {
                writeln!(f, "/dev/tty opened, isatty={}", libc::isatty(dev)).ok();
                libc::close(dev);
            } else {
                writeln!(f, "/dev/tty failed: {}", std::io::Error::last_os_error()).ok();
            }
        }
    }
    
    // Setup SIGUSR1 handler for save-on-signal
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("sequencer", instance);
    
    let mut last_err = String::new();
    for attempt in 0..20 {
        match enable_raw_mode() {
            Ok(()) => {
                if let Some(ref mut f) = log {
                    writeln!(f, "Raw mode enabled on attempt {}", attempt + 1).ok();
                }
                break;
            }
            Err(e) => {
                last_err = format!("{}", e);
                if let Some(ref mut f) = log {
                    writeln!(f, "Attempt {} failed: {}", attempt + 1, e).ok();
                }
                if attempt < 19 {
                    std::thread::sleep(Duration::from_millis(200));
                } else {
                    if let Some(ref mut f) = log {
                        writeln!(f, "All 20 attempts failed, giving up").ok();
                    }
                    return Err(anyhow::anyhow!("Failed to enable raw mode after 20 attempts: {}", last_err));
                }
            }
        }
    }
    
    if let Some(ref mut f) = log {
        writeln!(f, "Raw mode enabled").ok();
    }
    
    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        if let Some(ref mut f) = log {
            writeln!(f, "Failed to enter alternate screen: {}", e).ok();
        }
        return Err(anyhow::anyhow!("Failed to enter alternate screen: {}", e));
    }
    
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            return Err(anyhow::anyhow!("Failed to create terminal: {}", e));
        }
    };
    
    if let Some(ref mut f) = log {
        writeln!(f, "Terminal created, starting main loop").ok();
    }

    let state = Arc::new(Mutex::new(SequencerState::default()));
    
    // Load saved state if available
    if let Ok(params) = state::load_module_state::<state::SequencerParams>("sequencer", instance) {
        let mut s = state.lock().unwrap();
        if let Some(bpm) = params.bpm { s.bpm = bpm; }
        if let Some(playing) = params.playing { s.playing = playing; }
        // Load from tracks, falling back to flat fields for backward compat
        if !params.tracks.is_empty() {
            for (ti, tp) in params.tracks.iter().enumerate() {
                if ti >= s.tracks.len() {
                    s.tracks.push(Track {
                        steps: vec![Step::default(); NUM_STEPS],
                        length: tp.length.unwrap_or(16),
                        pulses: tp.pulses.unwrap_or(5),
                        rotation: tp.rotation.unwrap_or(0),
                        muted: tp.muted,
                    });
                    s.current_steps.push(0);
                    s.last_notes.push(None);
                }
                let trk = &mut s.tracks[ti];
                if let Some(l) = tp.length { trk.length = l; }
                if let Some(p) = tp.pulses { trk.pulses = p; }
                if let Some(r) = tp.rotation { trk.rotation = r; }
                trk.muted = tp.muted;
                for (i, step) in tp.steps.iter().enumerate().take(trk.steps.len()) {
                    trk.steps[i] = Step { active: step.active, note: step.note, velocity: step.velocity };
                }
            }
        } else {
            // Fallback: load flat fields into first track
            let trk = &mut s.tracks[0];
            if let Some(p) = params.euclidean_pulses { trk.pulses = p; }
            if let Some(l) = params.euclidean_length { trk.length = l; }
            if let Some(r) = params.euclidean_rotation { trk.rotation = r; }
            for (i, step) in params.steps.iter().enumerate().take(trk.steps.len()) {
                trk.steps[i] = Step { active: step.active, note: step.note, velocity: step.velocity };
            }
        }
    }
    
    let state_clone = Arc::clone(&state);

    let (tx, rx) = std::sync::mpsc::channel();

    let seq_handle = std::thread::spawn(move || {
        if let Err(e) = sequencer_thread(state_clone, rx) {
            eprintln!("Sequencer thread error: {}", e);
        }
    });

    let mut input_mode = String::from("normal");
    let mut input_buffer = String::new();
    let mut pending_g = false;
    let mut pending_d = false;
    let mut pending_y = false;
    let mut show_help = false;
    let mut pending_count: Option<String> = None;

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
                    }).collect(),
                    length: Some(trk.length),
                    pulses: Some(trk.pulses),
                    rotation: Some(trk.rotation),
                    muted: trk.muted,
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
                    let ti = 0.min(s.tracks.len() - 1);
                    let trk = &mut s.tracks[ti];
                    if let Some(l) = params.tracks[0].length { trk.length = l; }
                    if let Some(p) = params.tracks[0].pulses { trk.pulses = p; }
                    if let Some(r) = params.tracks[0].rotation { trk.rotation = r; }
                    for (i, step) in params.tracks[0].steps.iter().enumerate().take(trk.steps.len()) {
                        trk.steps[i] = Step { active: step.active, note: step.note, velocity: step.velocity };
                    }
                }
            }
        }
        
        let current_state = state.lock().unwrap().clone();
        draw_ui(&mut terminal, &current_state, &input_mode, &input_buffer, show_help)?;

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
                            }).collect(),
                            length: Some(trk.length),
                            pulses: Some(trk.pulses),
                            rotation: Some(trk.rotation),
                            muted: trk.muted,
                        }).collect(),
                    };
                    drop(s);
                    let _ = state::save_module_state("sequencer", 0, &params);
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc if input_mode == "normal" => break,
                    
                    // Digits accumulate into pending_count
                    KeyCode::Char(c) if input_mode == "normal" && c.is_ascii_digit() => {
                        if c == '0' && pending_count.is_none() {
                            let mut s = state.lock().unwrap();
                            s.selected = 0;
                        } else {
                            pending_count.get_or_insert(String::new()).push(c);
                        }
                    }
                    
                    // Count-prefixed P, L, R
                    KeyCode::Char('P') if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(0);
                        let mut s = state.lock().unwrap();
                let tidx = s.current_track;
                        if count > 0 { s.track_mut().pulses = count.min(16); }
                        let (p, l, r) = (s.track_mut().pulses, s.track_mut().length, s.track_mut().rotation);
                        euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                    }
                    KeyCode::Char('L') if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(0);
                        let mut s = state.lock().unwrap();
                let tidx = s.current_track;
                        if count > 0 { s.track_mut().length = count.min(16).max(1); }
                        s.selected = s.selected.min(s.track_mut().length - 1);
                        let (p, l, r) = (s.track_mut().pulses, s.track_mut().length, s.track_mut().rotation);
                        euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
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
                    }
                    // Count-prefixed navigation
                    KeyCode::Char('l') | KeyCode::Right if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        s.selected = (s.selected + count) % len;
                    }
                    KeyCode::Char('h') | KeyCode::Left if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        s.selected = s.selected.saturating_sub(count).min(len - 1);
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
                    }
                    
                    // Non-count commands (clear pending_count)
                    KeyCode::Char('P') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                let tidx = s.current_track;
                        let (p, l, r) = (s.track_mut().pulses, s.track_mut().length, s.track_mut().rotation);
                        euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                    }
                    KeyCode::Char('L') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                let tidx = s.current_track;
                        s.selected = s.selected.min(s.track_mut().length - 1);
                        let (p, l, r) = (s.track_mut().pulses, s.track_mut().length, s.track_mut().rotation);
                        euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                    }
                    KeyCode::Char('R') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                let tidx = s.current_track;
                        s.track_mut().rotation = (s.track_mut().rotation + 1) % s.track_mut().length;
                        let (p, l, r) = (s.track_mut().pulses, s.track_mut().length, s.track_mut().rotation);
                        euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                    }
                    
                    // Clear pending_count on any other action key
                    KeyCode::Char(' ') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        s.playing = !s.playing;
                    }
                    KeyCode::Enter if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        s.track_mut().steps[sel].active = !s.track_mut().steps[sel].active;
                    }
                    KeyCode::Char('x') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        s.clipboard = Some(s.track_mut().steps[sel].clone());
                        s.track_mut().steps[sel].active = false;
                    }
                    KeyCode::Char('p') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        if let Some(ref clip) = s.clipboard {
                            s.track_mut().steps[sel] = clip.clone();
                            s.track_mut().steps[sel].active = true;
                        }
                    }
                    KeyCode::Char('k') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        s.track_mut().steps[sel].note = (s.track_mut().steps[sel].note + 1).min(127);
                    }
                    KeyCode::Char('j') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        s.track_mut().steps[sel].note = s.track_mut().steps[sel].note.saturating_sub(1).max(0);
                    }
                    KeyCode::Char('K') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        s.track_mut().steps[sel].note = (s.track_mut().steps[sel].note + 12).min(127);
                    }
                    KeyCode::Char('J') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        s.track_mut().steps[sel].note = s.track_mut().steps[sel].note.saturating_sub(12).max(0);
                    }
                    KeyCode::Char('s') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        s.playing = false;
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') if input_mode == "normal" => {
                        pending_count = None;
                    }
                    KeyCode::Char('-') if input_mode == "normal" => {
                        pending_count = None;
                    }
                    KeyCode::Char('$') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        s.selected = s.track_mut().length - 1;
                    }
                    KeyCode::Char('l') | KeyCode::Right if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        s.selected = (s.selected + 1) % len;
                    }
                    KeyCode::Char('h') | KeyCode::Left if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        s.selected = s.selected.saturating_sub(1).min(len - 1);
                    }
                    KeyCode::Char('w') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        for i in 1..=len {
                            let idx = (s.selected + i) % len;
                            if s.track_mut().steps[idx].active {
                                s.selected = idx;
                                break;
                            }
                        }
                    }
                    KeyCode::Char('b') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        for i in 1..=len {
                            let idx = (s.selected + len - i) % len;
                            if s.track_mut().steps[idx].active {
                                s.selected = idx;
                                break;
                            }
                        }
                    }
                    KeyCode::Char('g') if input_mode == "normal" => {
                        pending_count = None;
                        if pending_g {
                            let mut s = state.lock().unwrap();
                            s.selected = 0;
                            pending_g = false;
                        } else {
                            pending_g = true;
                        }
                    }
                    KeyCode::Char('N') if input_mode == "normal" => {
                        pending_count = None;
                        input_mode = String::from("note");
                        input_buffer.clear();
                    }
                    KeyCode::Char('t') if input_mode == "normal" => {
                        pending_count = None;
                        input_mode = String::from("bpm");
                        input_buffer.clear();
                    }
                    // Track switching
                    KeyCode::Char('[') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        if s.current_track > 0 {
                            s.current_track -= 1;
                        }
                        s.selected = 0;
                    }
                    KeyCode::Char(']') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        if s.current_track + 1 < s.tracks.len() {
                            s.current_track += 1;
                        }
                        s.selected = 0;
                    }
                    KeyCode::Char('n') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        s.tracks.push(Track::new());
                        s.current_steps.push(0);
                        s.last_notes.push(None);
                        s.current_track = s.tracks.len() - 1;
                        s.selected = 0;
                    }
                    KeyCode::Char('d') if input_mode == "normal" => {
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
                    KeyCode::Char('y') if input_mode == "normal" => {
                        if pending_y {
                            pending_y = false;
                            let mut s = state.lock().unwrap();
                            s.track_clipboard = Some(s.tracks[s.current_track].clone());
                        } else {
                            pending_y = true;
                            pending_d = false;
                        }
                    }
                    KeyCode::Char('P') if input_mode == "normal" => {
                        pending_count = None;
                        pending_d = false;
                        pending_y = false;
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
                    KeyCode::Char('m') if input_mode == "normal" => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        let tidx = s.current_track;
                        s.tracks[tidx].muted = !s.tracks[tidx].muted;
                    }
                    KeyCode::Char('?') if input_mode == "normal" => {
                        pending_count = None;
                        show_help = !show_help;
                    }
                    KeyCode::Char(c) if input_mode != "normal" => {
                        if c.is_ascii_digit() || c == '.' {
                            input_buffer.push(c);
                        }
                    }
                    KeyCode::Enter if input_mode != "normal" => {
                        let mut s = state.lock().unwrap();
                        match input_mode.as_str() {
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
                            _ => {}
                        }
                        input_mode = String::from("normal");
                        input_buffer.clear();
                    }
                    KeyCode::Esc if input_mode != "normal" => {
                        input_mode = String::from("normal");
                        input_buffer.clear();
                    }
                    
                    // Any other key clears pending_count
                    _ if pending_count.is_some() => {
                        pending_count = None;
                    }
                    _ if pending_d || pending_y => {
                        pending_d = false;
                        pending_y = false;
                    }
                    _ => {}
                }
            }
        }
    }

    let _ = tx.send(());
    seq_handle.join().unwrap();

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
