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
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::shm::AudioRingbuf;

const NUM_TRACKS: usize = 4;

#[derive(Clone)]
struct TrackState {
    level: f32,
    pan: f32,
    mute: bool,
    solo: bool,
    meter: f32,
}

impl Default for TrackState {
    fn default() -> Self {
        Self {
            level: 0.8,
            pan: 0.0,
            mute: false,
            solo: false,
            meter: 0.0,
        }
    }
}

#[derive(Clone)]
struct MixerState {
    tracks: Vec<TrackState>,
    master_level: f32,
    master_meter: f32,
    selected_track: usize,
    selected_param: usize, // 0=level, 1=pan, 2=mute, 3=solo
}

impl Default for MixerState {
    fn default() -> Self {
        Self {
            tracks: vec![TrackState::default(); NUM_TRACKS],
            master_level: 0.8,
            master_meter: 0.0,
            selected_track: 0,
            selected_param: 0,
        }
    }
}

fn mixer_thread(
    state: Arc<Mutex<MixerState>>,
    shutdown: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let ringbuf_name = "/los_mix_in";

    let ringbuf = AudioRingbuf::open(ringbuf_name)
        .map_err(|e| anyhow::anyhow!("could not open SHM ringbuffer '{}': {}", ringbuf_name, e))?;

    let channels = ringbuf.channels() as usize;
    let slot_len = ringbuf.slot_len();
    let sample_rate = 48000u32;

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no audio output device found"))?;

    let config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Fixed(ringbuf.slot_frames()),
    };

    let ringbuf = Arc::new(std::sync::Mutex::new(ringbuf));
    let state_clone = Arc::clone(&state);

    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut buf = ringbuf.lock().unwrap();
                let mut s = state_clone.lock().unwrap();

                let mut written = 0;
                let mut peak = 0.0f32;

                while written + slot_len <= data.len() {
                    match buf.read(&mut data[written..written + slot_len]) {
                        Ok(true) => {
                            // Apply track levels and panning
                            for i in (written..written + slot_len).step_by(channels) {
                                let left = data[i];
                                let right = data[i + 1];

                                // Simple stereo mix (for now, just apply master level)
                                let mixed_left = left * s.master_level;
                                let mixed_right = right * s.master_level;

                                data[i] = mixed_left;
                                data[i + 1] = mixed_right;

                                peak = peak.max(mixed_left.abs()).max(mixed_right.abs());
                            }
                            written += slot_len;
                        }
                        Ok(false) => break,
                        Err(_) => break,
                    }
                }

                if written < data.len() {
                    for sample in data[written..].iter_mut() {
                        *sample = 0.0;
                    }
                }

                s.master_meter = peak;
            },
            |err| eprintln!("mixer audio error: {}", err),
            None,
        )
        .map_err(|e| anyhow::anyhow!("building audio output stream: {}", e))?;

    stream.play().map_err(|e| anyhow::anyhow!("starting audio stream: {}", e))?;

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    Ok(())
}

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &MixerState,
) -> Result<()> {
    terminal.draw(|f| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3 + NUM_TRACKS as u16 * 4),
                Constraint::Length(3),
                Constraint::Min(0),
            ])
            .split(f.area());

        // Title
        let title = Paragraph::new("LOS Mixer")
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(title, chunks[0]);

        // Tracks
        let track_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(vec![Constraint::Length(4); NUM_TRACKS])
            .split(chunks[1]);

        for (i, track) in state.tracks.iter().enumerate() {
            let is_selected = i == state.selected_track;
            let track_title = format!("Track {}", i + 1);

            let level_style = if is_selected && state.selected_param == 0 {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let pan_style = if is_selected && state.selected_param == 1 {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let mute_style = if track.mute {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Green)
            };

            let solo_style = if track.solo {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let track_content = vec![
                Line::from(vec![
                    Span::raw("Level: "),
                    Span::styled(format!("{:.0}%", track.level * 100.0), level_style),
                ]),
                Line::from(vec![
                    Span::raw("Pan: "),
                    Span::styled(
                        if track.pan == 0.0 {
                            "C".to_string()
                        } else if track.pan < 0.0 {
                            format!("L{:.0}", -track.pan * 100.0)
                        } else {
                            format!("R{:.0}", track.pan * 100.0)
                        },
                        pan_style,
                    ),
                ]),
                Line::from(vec![
                    Span::raw("Mute: "),
                    Span::styled(if track.mute { "M" } else { "-" }, mute_style),
                    Span::raw("  Solo: "),
                    Span::styled(if track.solo { "S" } else { "-" }, solo_style),
                ]),
                Line::from(vec![
                    Span::raw("Meter: "),
                    Span::styled(
                        "█".repeat((track.meter * 20.0) as usize),
                        Style::default().fg(Color::Green),
                    ),
                ]),
            ];

            let track_widget = Paragraph::new(track_content)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(track_title)
                        .border_style(if is_selected {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::White)
                        }),
                );
            f.render_widget(track_widget, track_chunks[i]);
        }

        // Master
        let master_content = vec![Line::from(vec![
            Span::raw("Master Level: "),
            Span::styled(
                format!("{:.0}%", state.master_level * 100.0),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  Meter: "),
            Span::styled(
                "█".repeat((state.master_meter * 20.0) as usize),
                Style::default().fg(Color::Green),
            ),
        ])];
        let master_widget = Paragraph::new(master_content)
            .block(Block::default().borders(Borders::ALL).title("Master"));
        f.render_widget(master_widget, chunks[2]);

        // Help
        let help_text = vec![
            Line::from("Navigation:"),
            Line::from("  j/k or ↑/↓ : Select track"),
            Line::from("  h/l or ←/→ : Select parameter"),
            Line::from(""),
            Line::from("Editing:"),
            Line::from("  ↑/↓        : Adjust level/master"),
            Line::from("  ←/→        : Adjust pan"),
            Line::from("  m          : Toggle mute"),
            Line::from("  s          : Toggle solo"),
            Line::from("  q          : Quit"),
        ];
        let help = Paragraph::new(help_text)
            .style(Style::default().fg(Color::Gray))
            .block(Block::default().borders(Borders::ALL).title("Help"));
        f.render_widget(help, chunks[3]);
    })?;

    Ok(())
}

pub fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let state = Arc::new(Mutex::new(MixerState::default()));
    let state_clone = Arc::clone(&state);

    let (tx, rx) = std::sync::mpsc::channel();

    let mixer_handle = std::thread::spawn(move || {
        if let Err(e) = mixer_thread(state_clone, rx) {
            eprintln!("Mixer thread error: {}", e);
        }
    });

    loop {
        let current_state = state.lock().unwrap().clone();
        draw_ui(&mut terminal, &current_state)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('j') | KeyCode::Down => {
                        let mut s = state.lock().unwrap();
                        if s.selected_param == 0 {
                            // Adjusting master level
                            s.master_level = (s.master_level - 0.05).max(0.0);
                        } else {
                            s.selected_track = (s.selected_track + 1) % NUM_TRACKS;
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        let mut s = state.lock().unwrap();
                        if s.selected_param == 0 {
                            // Adjusting master level
                            s.master_level = (s.master_level + 0.05).min(1.0);
                        } else {
                            s.selected_track = if s.selected_track == 0 {
                                NUM_TRACKS - 1
                            } else {
                                s.selected_track - 1
                            };
                        }
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        let mut s = state.lock().unwrap();
                        let track = s.selected_track;
                        if s.selected_param == 0 {
                            // Adjusting level
                            s.tracks[track].level =
                                (s.tracks[track].level + 0.05).min(1.0);
                        } else if s.selected_param == 1 {
                            // Adjusting pan
                            s.tracks[track].pan =
                                (s.tracks[track].pan + 0.1).min(1.0);
                        } else {
                            s.selected_param = (s.selected_param + 1) % 4;
                        }
                    }
                    KeyCode::Char('h') | KeyCode::Left => {
                        let mut s = state.lock().unwrap();
                        let track = s.selected_track;
                        if s.selected_param == 0 {
                            // Adjusting level
                            s.tracks[track].level =
                                (s.tracks[track].level - 0.05).max(0.0);
                        } else if s.selected_param == 1 {
                            // Adjusting pan
                            s.tracks[track].pan =
                                (s.tracks[track].pan - 0.1).max(-1.0);
                        } else {
                            s.selected_param = if s.selected_param == 0 {
                                3
                            } else {
                                s.selected_param - 1
                            };
                        }
                    }
                    KeyCode::Char('m') => {
                        let mut s = state.lock().unwrap();
                        let track = s.selected_track;
                        s.tracks[track].mute = !s.tracks[track].mute;
                    }
                    KeyCode::Char('s') => {
                        let mut s = state.lock().unwrap();
                        let track = s.selected_track;
                        s.tracks[track].solo = !s.tracks[track].solo;
                    }
                    _ => {}
                }
            }
        }
    }

    let _ = tx.send(());
    mixer_handle.join().unwrap();

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
