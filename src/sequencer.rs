use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use crate::shm::{AudioEvent, EventRingbuf, ShmTransport};

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
struct SequencerState {
    steps: Vec<Step>,
    bpm: f64,
    playing: bool,
    current_step: usize,
    selected: usize,
    last_note: Option<u8>,
    clipboard: Option<Step>,
    euclidean_pulses: usize,
    euclidean_length: usize,
    euclidean_rotation: usize,
}

impl Default for SequencerState {
    fn default() -> Self {
        let mut steps = vec![Step::default(); NUM_STEPS];
        for i in (0..NUM_STEPS).step_by(4) {
            steps[i].active = true;
        }
        Self {
            steps,
            bpm: 120.0,
            playing: true,
            current_step: 0,
            selected: 0,
            last_note: None,
            clipboard: None,
            euclidean_pulses: 5,
            euclidean_length: 16,
            euclidean_rotation: 0,
        }
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

    let mut last_step: i32 = -1;

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
        let current_step = if samples_per_step > 0 && playing {
            (clock / samples_per_step) as usize % NUM_STEPS
        } else {
            last_step.max(0) as usize
        };

        if current_step as i32 != last_step {
            let mut s = state.lock().unwrap();
            s.current_step = current_step;

            if playing {
                if let Some(n) = s.last_note {
                    let _ = events.write_event(&AudioEvent::note_off(n, last_step as u32));
                }
                if s.steps[current_step].active {
                    let note = s.steps[current_step].note;
                    let vel = s.steps[current_step].velocity;
                    let _ = events.write_event(&AudioEvent::note_on(note, vel, current_step as u32));
                    s.last_note = Some(note);
                } else {
                    s.last_note = None;
                }
            }
            last_step = current_step as i32;
        }

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
    // Apply rotation
    let rot = rotation % len;
    for i in 0..len {
        let src = (i + len - rot) % len;
        steps[i].active = pattern[src];
    }
}

fn next_char_ms(timeout_ms: u64) -> Option<char> {
    if event::poll(Duration::from_millis(timeout_ms)).ok()? {
        if let Event::Key(key) = event::read().ok()? {
            if let KeyCode::Char(c) = key.code {
                return Some(c);
            }
        }
    }
    None
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
        
        // Minimal layout: grid + status line
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),  // Status line
                Constraint::Length(3),  // Step grid
                Constraint::Length(1),  // Note display
                Constraint::Min(0),     // Empty space for help
            ])
            .split(area);

        // Status line
        let play_str = if state.playing { "▶" } else { "■" };
        let status = format!(
            "{} {} BPM | Step {}/{} | Sel {} | P:{} L:{} R:{} | {}",
            play_str, state.bpm as u32, state.current_step, NUM_STEPS, state.selected,
            state.euclidean_pulses, state.euclidean_length, state.euclidean_rotation,
            input_mode
        );
        let status_widget = Paragraph::new(status)
            .style(Style::default().fg(Color::Cyan));
        f.render_widget(status_widget, chunks[0]);

        // Step grid - horizontal layout
        let step_width = area.width as usize / NUM_STEPS;
        let step_width = step_width.max(3).min(8);
        
        for i in 0..NUM_STEPS {
            let step = &state.steps[i];
            let is_current = i == state.current_step;
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

        // Note display for selected step
        let sel_step = &state.steps[state.selected];
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
        f.render_widget(note_widget, chunks[2]);

        // Help overlay
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
                Line::from("Editing:"),
                Line::from("  Enter      Toggle step on/off"),
                Line::from("  x          Delete step (copies to clipboard)"),
                Line::from("  p          Paste step from clipboard"),
                Line::from("  k/K        Raise note (semitone/octave)"),
                Line::from("  j/J        Lower note (semitone/octave)"),
                Line::from("  n<NUM>     Set note (e.g. n60 for C4)"),
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

pub fn run(_instance: usize) -> Result<()> {
    // Log to file for debugging
    use std::fs::OpenOptions;
    use std::io::Write;
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/los-sequencer.log")
        .ok();
    
    if let Some(ref mut f) = log {
        writeln!(f, "Sequencer starting at {:?}", std::time::SystemTime::now()).ok();
        // Log TTY status for debugging
        unsafe {
            writeln!(f, "isatty(stdin)={} isatty(stdout)={}", 
                libc::isatty(libc::STDIN_FILENO), 
                libc::isatty(libc::STDOUT_FILENO)).ok();
            // Try /dev/tty
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
    
    // Initialize terminal with retry logic (handles tmux PTY race)
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
    
    if let Some(ref mut f) = log {
        writeln!(f, "Alternate screen entered").ok();
    }
    
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            if let Some(ref mut f) = log {
                writeln!(f, "Failed to create terminal: {}", e).ok();
            }
            return Err(anyhow::anyhow!("Failed to create terminal: {}", e));
        }
    };
    
    if let Some(ref mut f) = log {
        writeln!(f, "Terminal created, starting main loop").ok();
    }

    let state = Arc::new(Mutex::new(SequencerState::default()));
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
    let mut show_help = false;

    loop {
        let current_state = state.lock().unwrap().clone();
        draw_ui(&mut terminal, &current_state, &input_mode, &input_buffer, show_help)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc if input_mode == "normal" => break,
                    // Count prefix for P/L/R/h/l/w/b
                    KeyCode::Char(c) if input_mode == "normal" && c.is_ascii_digit() && c != '0' => {
                        let mut num_buf = String::new();
                        num_buf.push(c);
                        loop {
                            if let Some(next) = next_char_ms(30) {
                                if next.is_ascii_digit() {
                                    num_buf.push(next);
                                    continue;
                                }
                                let count: usize = num_buf.parse().unwrap_or(1);
                                match next {
                                    'P' => {
                                        let mut s = state.lock().unwrap();
                                        s.euclidean_pulses = count.min(16);
                                        let (p, l, r) = (s.euclidean_pulses, s.euclidean_length, s.euclidean_rotation);
                                        euclidean_apply(&mut s.steps, p, l, r);
                                    }
                                    'L' => {
                                        let mut s = state.lock().unwrap();
                                        s.euclidean_length = count.min(16).max(1);
                                        let (p, l, r) = (s.euclidean_pulses, s.euclidean_length, s.euclidean_rotation);
                                        euclidean_apply(&mut s.steps, p, l, r);
                                    }
                                    'R' => {
                                        let mut s = state.lock().unwrap();
                                        s.euclidean_rotation = count.min(255);
                                        let (p, l, r) = (s.euclidean_pulses, s.euclidean_length, s.euclidean_rotation);
                                        euclidean_apply(&mut s.steps, p, l, r);
                                    }
                                    'h' | 'l' => {
                                        let mut s = state.lock().unwrap();
                                        if next == 'l' {
                                            s.selected = (s.selected + count) % NUM_STEPS;
                                        } else {
                                            s.selected = s.selected.saturating_sub(count).min(NUM_STEPS - 1);
                                        }
                                    }
                                    'w' => {
                                        let mut s = state.lock().unwrap();
                                        for _ in 0..count {
                                            for i in 1..=NUM_STEPS {
                                                let idx = (s.selected + i) % NUM_STEPS;
                                                if s.steps[idx].active {
                                                    s.selected = idx;
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    'b' => {
                                        let mut s = state.lock().unwrap();
                                        for _ in 0..count {
                                            for i in 1..=NUM_STEPS {
                                                let idx = (s.selected + NUM_STEPS - i) % NUM_STEPS;
                                                if s.steps[idx].active {
                                                    s.selected = idx;
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                                break;
                            }
                            break;
                        }
                    }
                    // Play/pause
                    KeyCode::Char(' ') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        s.playing = !s.playing;
                    }
                    // Toggle step on/off
                    KeyCode::Enter if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        s.steps[sel].active = !s.steps[sel].active;
                    }
                    // Delete step (save to clipboard)
                    KeyCode::Char('x') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        s.clipboard = Some(s.steps[sel].clone());
                        s.steps[sel].active = false;
                    }
                    // Paste step from clipboard
                    KeyCode::Char('p') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        if let Some(ref clip) = s.clipboard {
                            s.steps[sel] = clip.clone();
                            s.steps[sel].active = true;
                        }
                    }
                    // Raise note by semitone
                    KeyCode::Char('k') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        s.steps[sel].note = (s.steps[sel].note + 1).min(127);
                    }
                    // Lower note by semitone
                    KeyCode::Char('j') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        s.steps[sel].note = s.steps[sel].note.saturating_sub(1).max(0);
                    }
                    // Raise note by octave
                    KeyCode::Char('K') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        s.steps[sel].note = (s.steps[sel].note + 12).min(127);
                    }
                    // Lower note by octave
                    KeyCode::Char('J') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        s.steps[sel].note = s.steps[sel].note.saturating_sub(12).max(0);
                    }
                    // Stop
                    KeyCode::Char('s') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        s.playing = false;
                    }
                    // Euclidean pulses (recalculate with current)
                    KeyCode::Char('P') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        let (p, l, r) = (s.euclidean_pulses, s.euclidean_length, s.euclidean_rotation);
                        euclidean_apply(&mut s.steps, p, l, r);
                    }
                    // Euclidean length (recalculate with current)
                    KeyCode::Char('L') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        let (p, l, r) = (s.euclidean_pulses, s.euclidean_length, s.euclidean_rotation);
                        euclidean_apply(&mut s.steps, p, l, r);
                    }
                    // Euclidean rotation (increment by 1)
                    KeyCode::Char('R') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        s.euclidean_rotation = (s.euclidean_rotation + 1) % s.euclidean_length;
                        let (p, l, r) = (s.euclidean_pulses, s.euclidean_length, s.euclidean_rotation);
                        euclidean_apply(&mut s.steps, p, l, r);
                    }
                    KeyCode::Char('l') | KeyCode::Right if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        s.selected = (s.selected + 1) % NUM_STEPS;
                    }
                    KeyCode::Char('h') | KeyCode::Left if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        s.selected = if s.selected == 0 { NUM_STEPS - 1 } else { s.selected - 1 };
                    }
                    KeyCode::Char('0') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        s.selected = 0;
                    }
                    KeyCode::Char('$') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        s.selected = NUM_STEPS - 1;
                    }
                    KeyCode::Char('w') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        for i in 1..=NUM_STEPS {
                            let idx = (s.selected + i) % NUM_STEPS;
                            if s.steps[idx].active {
                                s.selected = idx;
                                break;
                            }
                        }
                    }
                    KeyCode::Char('b') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        for i in 1..=NUM_STEPS {
                            let idx = (s.selected + NUM_STEPS - i) % NUM_STEPS;
                            if s.steps[idx].active {
                                s.selected = idx;
                                break;
                            }
                        }
                    }
                    KeyCode::Char('g') if input_mode == "normal" => {
                        if pending_g {
                            let mut s = state.lock().unwrap();
                            s.selected = 0;
                            pending_g = false;
                        } else {
                            pending_g = true;
                        }
                    }
                    KeyCode::Char('n') if input_mode == "normal" => {
                        input_mode = String::from("note");
                        input_buffer.clear();
                    }
                    KeyCode::Char('t') if input_mode == "normal" => {
                        input_mode = String::from("bpm");
                        input_buffer.clear();
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
                                    s.steps[sel].note = note.clamp(0, 127);
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
                    KeyCode::Char('?') if input_mode == "normal" => {
                        show_help = !show_help;
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
