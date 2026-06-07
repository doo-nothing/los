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
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::shm::{AudioRingbuf, ShmTransport};
use crate::state;

const NUM_TRACKS: usize = 4;
const SHM_NAME: &str = "/los_mix_in";

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
    master: f32,
    master_meter: f32,
    selected: usize,
}

impl Default for MixerState {
    fn default() -> Self {
        Self {
            tracks: vec![TrackState::default(); NUM_TRACKS],
            master: 0.8,
            master_meter: 0.0,
            selected: 0,
        }
    }
}

fn mixer_thread(
    state: Arc<Mutex<MixerState>>,
    shutdown: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let mut ringbuf = AudioRingbuf::open(SHM_NAME)
        .map_err(|e| anyhow::anyhow!("Failed to open audio ringbuffer: {}", e))?;

    let mut transport = ShmTransport::open()
        .or_else(|_| ShmTransport::create(48000))?;

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No output device"))?;

    let config = device
        .default_output_config()
        .map_err(|e| anyhow::anyhow!("Failed to get output config: {}", e))?;

    let channels = config.channels() as usize;
    let slot_len = ringbuf.slot_len();

    let stream = device
        .build_output_stream(
            &config.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut s = state.lock().unwrap();
                let mut peak = 0.0f32;
                let mut written = 0;

                // Fill output with complete slots from the ringbuffer
                while written + slot_len <= data.len() {
                    if let Ok(true) = ringbuf.read(&mut data[written..written + slot_len]) {
                        for sample in data[written..written + slot_len].iter_mut() {
                            *sample *= s.master;
                            peak = peak.max(sample.abs());
                        }
                        written += slot_len;
                    } else {
                        break;
                    }
                }

                // Zero-fill any remaining output (silence)
                for sample in data[written..].iter_mut() {
                    *sample = 0.0;
                }

                s.master_meter = peak;
                
                // Advance transport clock by the number of frames processed
                transport.add_clock_frames((data.len() / channels) as u64);
            },
            move |err| {
                eprintln!("Audio error: {}", err);
            },
            None,
        )
        .map_err(|e| anyhow::anyhow!("Failed to build output stream: {}", e))?;

    stream.play().map_err(|e| anyhow::anyhow!("Failed to play stream: {}", e))?;

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(())
}

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &MixerState,
    show_help: bool,
) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        let track_width = area.width / (NUM_TRACKS as u16 + 1);

        for i in 0..NUM_TRACKS {
            let track = &state.tracks[i];
            let is_selected = state.selected == i;
            
            let x = i as u16 * track_width;
            let rect = Rect::new(x, 0, track_width, area.height);

            let style = if is_selected {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::White)
            };

            let mute_str = if track.mute { "M" } else { " " };
            let solo_str = if track.solo { "S" } else { " " };

            let track_text = format!(
                "T{} [{}{}] \nL:{:.0}%\nP:{:+.0}\n\n{:.0}%",
                i + 1,
                mute_str,
                solo_str,
                track.level * 100.0,
                track.pan * 100.0,
                track.meter * 100.0
            );

            let track_widget = Paragraph::new(track_text).style(style);
            f.render_widget(track_widget, rect);
        }

        let master_x = NUM_TRACKS as u16 * track_width;
        let master_rect = Rect::new(master_x, 0, track_width, area.height);
        let is_master_selected = state.selected == NUM_TRACKS;
        
        let master_style = if is_master_selected {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Cyan)
        };

        let master_text = format!(
            "MASTER\n\n{:.0}%\n\n{:.0}%",
            state.master * 100.0,
            state.master_meter * 100.0
        );

        let master_widget = Paragraph::new(master_text).style(master_style);
        f.render_widget(master_widget, master_rect);

        // Help overlay
        if show_help {
            let help_text = vec![
                Line::from("━━━ Mixer Help ━━━"),
                Line::from(""),
                Line::from("Navigation:"),
                Line::from("  h/l, ←/→  Select track/master"),
                Line::from(""),
                Line::from("Adjusting:"),
                Line::from("  j/k, ↓/↑   Decrease/increase level"),
                Line::from("  +/-        Decrease/increase pan"),
                Line::from("  m          Toggle mute"),
                Line::from("  s          Toggle solo"),
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

pub fn run() -> Result<()> {
    // Initialize terminal with retry logic (handles tmux PTY race)
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("mixer", 0);
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

    let state = Arc::new(Mutex::new(MixerState::default()));
    
    // Load saved state if available
    if let Ok(params) = state::load_module_state::<state::MixerParams>("mixer", 0) {
        let mut s = state.lock().unwrap();
        if let Some(v) = params.master { s.master = v; }
        for (i, tp) in params.tracks.iter().enumerate().take(s.tracks.len()) {
            s.tracks[i] = TrackState {
                level: tp.level,
                pan: tp.pan,
                mute: tp.mute,
                solo: tp.solo,
                meter: 0.0,
            };
        }
    }
    
    let state_clone = Arc::clone(&state);

    let (_tx, rx) = std::sync::mpsc::channel();

    let _mixer_handle = std::thread::spawn(move || {
        if let Err(e) = mixer_thread(state_clone, rx) {
            eprintln!("Mixer thread error: {}", e);
        }
    });

    let mut show_help = false;

    loop {
        // Check for save-on-signal
        if state::check_save_signal() {
            let s = state.lock().unwrap();
            let params = state::MixerParams {
                master: Some(s.master),
                tracks: s.tracks.iter().map(|t| state::MixerTrackParam {
                    level: t.level,
                    pan: t.pan,
                    mute: t.mute,
                    solo: t.solo,
                }).collect(),
            };
            drop(s);
            let _ = state::save_module_state("mixer", 0, &params);
        }
        
        // Check for reload-on-signal
        if state::check_reload_signal() {
            if let Ok(params) = state::load_module_state::<state::MixerParams>("mixer", 0) {
                let mut s = state.lock().unwrap();
                if let Some(v) = params.master { s.master = v; }
                for (i, tp) in params.tracks.iter().enumerate().take(s.tracks.len()) {
                    s.tracks[i] = TrackState {
                        level: tp.level,
                        pan: tp.pan,
                        mute: tp.mute,
                        solo: tp.solo,
                        meter: 0.0,
                    };
                }
            }
        }
        
        let current_state = state.lock().unwrap().clone();
        draw_ui(&mut terminal, &current_state, show_help)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Ctrl-s: save module state
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let s = state.lock().unwrap();
                    let params = state::MixerParams {
                        master: Some(s.master),
                        tracks: s.tracks.iter().map(|t| state::MixerTrackParam {
                            level: t.level,
                            pan: t.pan,
                            mute: t.mute,
                            solo: t.solo,
                        }).collect(),
                    };
                    drop(s);
                    let _ = state::save_module_state("mixer", 0, &params);
                    continue;
                }
                match key.code {
                    KeyCode::Char('h') | KeyCode::Left => {
                        let mut s = state.lock().unwrap();
                        s.selected = if s.selected == 0 { NUM_TRACKS } else { s.selected - 1 };
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        let mut s = state.lock().unwrap();
                        s.selected = (s.selected + 1) % (NUM_TRACKS + 1);
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        if sel < NUM_TRACKS {
                            s.tracks[sel].level = (s.tracks[sel].level - 0.05).max(0.0);
                        } else {
                            s.master = (s.master - 0.05).max(0.0);
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        if sel < NUM_TRACKS {
                            s.tracks[sel].level = (s.tracks[sel].level + 0.05).min(1.0);
                        } else {
                            s.master = (s.master + 0.05).min(1.0);
                        }
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        if sel < NUM_TRACKS {
                            s.tracks[sel].pan = (s.tracks[sel].pan + 0.1).min(1.0);
                        }
                    }
                    KeyCode::Char('-') => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        if sel < NUM_TRACKS {
                            s.tracks[sel].pan = (s.tracks[sel].pan - 0.1).max(-1.0);
                        }
                    }
                    KeyCode::Char('m') => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        if sel < NUM_TRACKS {
                            s.tracks[sel].mute = !s.tracks[sel].mute;
                        }
                    }
                    KeyCode::Char('s') => {
                        let mut s = state.lock().unwrap();
                        let sel = s.selected;
                        if sel < NUM_TRACKS {
                            s.tracks[sel].solo = !s.tracks[sel].solo;
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
