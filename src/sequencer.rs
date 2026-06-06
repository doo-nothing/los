use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Terminal,
};

use crate::shm::{AudioEvent, EventRingbuf, ShmTransport};

const NUM_STEPS: usize = 16;

#[derive(Clone)]
struct Step {
    active: bool,
    note: u8,
}

#[derive(Clone)]
struct SequencerState {
    steps: Vec<Step>,
    bpm: f64,
    playing: bool,
    current_step: usize,
    selected: usize,
    last_note: Option<u8>,
}

impl Default for SequencerState {
    fn default() -> Self {
        let mut steps = vec![Step { active: false, note: 60 }; NUM_STEPS];
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
                    let _ = events.write_event(&AudioEvent::note_on(note, 100, current_step as u32));
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

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &SequencerState,
    input_mode: &str,
) -> Result<()> {
    terminal.draw(|f| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(5),
                Constraint::Length(3),
                Constraint::Min(0),
            ])
            .split(f.area());

        // Title
        let title = Paragraph::new("LOS Sequencer")
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(title, chunks[0]);

        // Step grid
        let rows: Vec<Row> = (0..NUM_STEPS)
            .map(|i| {
                let step = &state.steps[i];
                let is_current = i == state.current_step;
                let is_selected = i == state.selected;

                let marker = if is_current && is_selected {
                    ">*"
                } else if is_current {
                    ">"
                } else if is_selected {
                    "*"
                } else {
                    " "
                };

                let active_str = if step.active { "X" } else { " " };
                let note_str = midi_note_name(step.note);

                let style = if is_current && is_selected {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else if is_current {
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else if is_selected {
                    Style::default().fg(Color::Yellow)
                } else if step.active {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::DarkGray)
                };

                Row::new(vec![
                    Cell::from(marker).style(style),
                    Cell::from(format!("{:2}", i)).style(style),
                    Cell::from(format!("[{}]", active_str)).style(style),
                    Cell::from(note_str).style(style),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            &[
                Constraint::Length(2),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(4),
            ],
        )
        .header(
            Row::new(vec!["", "#", "On", "Note"])
                .style(Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title("Steps"));
        f.render_widget(table, chunks[1]);

        // Transport info
        let play_str = if state.playing { "► Playing" } else { "■ Stopped" };
        let transport_text = vec![Line::from(vec![
            Span::raw("BPM: "),
            Span::styled(format!("{:.0}", state.bpm), Style::default().fg(Color::Yellow)),
            Span::raw("  "),
            Span::styled(play_str, Style::default().fg(Color::Green)),
            Span::raw("  Step: "),
            Span::styled(format!("{}", state.current_step), Style::default().fg(Color::Cyan)),
        ])];
        let transport = Paragraph::new(transport_text)
            .block(Block::default().borders(Borders::ALL).title("Transport"));
        f.render_widget(transport, chunks[2]);

        // Help
        let help_text = vec![
            Line::from("Navigation:"),
            Line::from("  h/l or ←/→ : Move left/right"),
            Line::from("  0/$        : First/last step"),
            Line::from("  w/b        : Next/prev active step"),
            Line::from("  gg         : Go to first step"),
            Line::from(""),
            Line::from("Editing:"),
            Line::from("  space      : Toggle step"),
            Line::from("  n<note>    : Set note (e.g., n60)"),
            Line::from("  t<bpm>     : Set BPM (e.g., t120)"),
            Line::from("  e<pulses>  : Euclidean fill"),
            Line::from(""),
            Line::from("Transport:"),
            Line::from("  p          : Play/pause"),
            Line::from("  s          : Stop"),
            Line::from("  q          : Quit"),
            Line::from(""),
            Line::from(format!("Mode: {}", input_mode)),
        ];
        let help = Paragraph::new(help_text)
            .style(Style::default().fg(Color::Gray))
            .block(Block::default().borders(Borders::ALL).title("Help"));
        f.render_widget(help, chunks[3]);
    })?;

    Ok(())
}

pub fn run(_instance: usize) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let state = Arc::new(Mutex::new(SequencerState::default()));
    let state_clone = Arc::clone(&state);

    let (tx, rx) = std::sync::mpsc::channel();

    let seq_handle = std::thread::spawn(move || {
        if let Err(e) = sequencer_thread(state_clone, rx) {
            eprintln!("Sequencer thread error: {}", e);
        }
    });

    let mut input_mode = String::from("normal");
    let mut pending_g = false;
    let mut input_buffer = String::new();

    loop {
        let current_state = state.lock().unwrap().clone();
        draw_ui(&mut terminal, &current_state, &input_mode)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc if input_mode == "normal" => break,
                    KeyCode::Char('p') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        s.playing = !s.playing;
                    }
                    KeyCode::Char('s') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        s.playing = false;
                    }
                    KeyCode::Char(' ') if input_mode == "normal" => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        s.steps[sel].active = !s.steps[sel].active;
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
                    KeyCode::Char('e') if input_mode == "normal" => {
                        input_mode = String::from("euclidean");
                        input_buffer.clear();
                    }
                    KeyCode::Char(c) if input_mode != "normal" => {
                        if c.is_ascii_digit() || c == '.' {
                            input_buffer.push(c);
                        } else if c == '\n' || c == '\r' {
                            // Apply input
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
                                "euclidean" => {
                                    if let Ok(pulses) = input_buffer.parse::<usize>() {
                                        let pulses = pulses.min(NUM_STEPS);
                                        // Euclidean rhythm algorithm
                                        let mut pattern = vec![false; NUM_STEPS];
                                        if pulses > 0 {
                                            let mut bucket = 0usize;
                                            for i in 0..NUM_STEPS {
                                                bucket += pulses;
                                                if bucket >= NUM_STEPS {
                                                    bucket -= NUM_STEPS;
                                                    pattern[i] = true;
                                                }
                                            }
                                        }
                                        for (i, step) in s.steps.iter_mut().enumerate() {
                                            step.active = pattern[i];
                                        }
                                    }
                                }
                                _ => {}
                            }
                            input_mode = String::from("normal");
                            input_buffer.clear();
                        } else if c == '\x1b' {
                            // Escape cancels input
                            input_mode = String::from("normal");
                            input_buffer.clear();
                        }
                    }
                    KeyCode::Enter if input_mode != "normal" => {
                        // Apply input
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
                            "euclidean" => {
                                if let Ok(pulses) = input_buffer.parse::<usize>() {
                                    let pulses = pulses.min(NUM_STEPS);
                                    let mut pattern = vec![false; NUM_STEPS];
                                    if pulses > 0 {
                                        let mut bucket = 0usize;
                                        for i in 0..NUM_STEPS {
                                            bucket += pulses;
                                            if bucket >= NUM_STEPS {
                                                bucket -= NUM_STEPS;
                                                pattern[i] = true;
                                            }
                                        }
                                    }
                                    for (i, step) in s.steps.iter_mut().enumerate() {
                                        step.active = pattern[i];
                                    }
                                }
                            }
                            _ => {}
                        }
                        input_mode = String::from("normal");
                        input_buffer.clear();
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
