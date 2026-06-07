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

use crate::shm::{AudioRingbuf, Manifest, ShmTransport};
use crate::state;

const SHM_NAME: &str = "/los_mix_in";

#[derive(Clone)]
struct TrackState {
    name: String,
    level: f32,
    pan: f32,
    mute: bool,
    solo: bool,
    meter: f32,
}

struct AudioSource {
    shm_name: String,
    ringbuf: AudioRingbuf,
}

struct MixerInner {
    tracks: Vec<TrackState>,
    audio_sources: Vec<AudioSource>,
    master: f32,
    master_meter: f32,
    selected: usize,
    scope_rb: Option<AudioRingbuf>,
}

fn mixer_thread(
    state: Arc<Mutex<MixerInner>>,
    shutdown: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let manifest = Manifest::open().or_else(|_| Manifest::create())?;

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
    let slot_len = 128; // max 64 frames * 2 channels

    let state_cb = Arc::clone(&state);

    let stream = device
        .build_output_stream(
            &config.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut inner = state_cb.lock().unwrap();
                let mut peak = 0.0f32;
                let mut written = 0;

                while written + slot_len <= data.len() {
                    for sample in data[written..written + slot_len].iter_mut() {
                        *sample = 0.0;
                    }

                    let mut voice_buf = [0.0f32; 128];
                    let track_info: Vec<(f32, bool)> = inner.tracks.iter()
                        .map(|t| (t.level, t.mute))
                        .collect();

                    for (i, source) in inner.audio_sources.iter_mut().enumerate() {
                        if i >= track_info.len() { break; }
                        if track_info[i].1 { continue; }
                        let track_level = track_info[i].0;
                        if let Ok(true) = source.ringbuf.read(&mut voice_buf[..slot_len]) {
                            for j in 0..slot_len {
                                data[written + j] += voice_buf[j] * track_level;
                            }
                        }
                    }

                    for sample in data[written..written + slot_len].iter_mut() {
                        *sample *= inner.master;
                        peak = peak.max(sample.abs());
                    }

                    if let Some(ref mut scope_rb) = inner.scope_rb {
                        let _ = scope_rb.write(&data[written..written + slot_len]);
                    }

                    written += slot_len;
                }

                for sample in data[written..].iter_mut() {
                    *sample = 0.0;
                }

                inner.master_meter = peak;
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

        let entries = manifest.entries();

        let mut inner = state.lock().unwrap();

        let mut to_remove = Vec::new();
        for (i, source) in inner.audio_sources.iter().enumerate() {
            let still_alive = entries.iter().any(|e| {
                e.audio_shm.as_deref() == Some(&source.shm_name)
            });
            if !still_alive {
                to_remove.push(i);
            }
        }
        for i in to_remove.into_iter().rev() {
            inner.audio_sources.remove(i);
            inner.tracks.remove(i);
            if inner.selected > 0 && inner.selected >= inner.tracks.len() {
                inner.selected = inner.tracks.len().saturating_sub(1);
            }
        }

        for entry in &entries {
            let has_shm = entry.audio_shm.is_some();
            if !has_shm { continue; }
            let shm_name = entry.audio_shm.as_ref().unwrap();

            let already = inner.audio_sources.iter().any(|s| s.shm_name == *shm_name);
            if already { continue; }

            if let Ok(ringbuf) = AudioRingbuf::open(shm_name) {
                let label = format!("{} {}", capitalize(&entry.module_name), entry.instance);
                inner.audio_sources.push(AudioSource {
                    shm_name: shm_name.clone(),
                    ringbuf,
                });
                inner.tracks.push(TrackState {
                    name: label,
                    level: 0.8,
                    pan: 0.0,
                    mute: false,
                    solo: false,
                    meter: 0.0,
                });
            }
        }

        if inner.scope_rb.is_none() {
            inner.scope_rb = AudioRingbuf::open(SHM_NAME)
                .or_else(|_| AudioRingbuf::create(SHM_NAME)).ok();
        }

        drop(inner);

        std::thread::sleep(Duration::from_millis(500));
    }

    Ok(())
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    tracks: &[TrackState],
    master: f32,
    master_meter: f32,
    selected: usize,
    show_help: bool,
) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        let num_tracks = tracks.len();
        let track_width = if num_tracks > 0 {
            area.width / (num_tracks as u16 + 1)
        } else {
            area.width
        };

        for (i, track) in tracks.iter().enumerate() {
            let is_selected = selected == i;

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
                "{} [{}{}]\nL:{:.0}%\nP:{:+.0}\n\n{:.0}%",
                track.name,
                mute_str,
                solo_str,
                track.level * 100.0,
                track.pan * 100.0,
                track.meter * 100.0
            );

            let track_widget = Paragraph::new(track_text).style(style);
            f.render_widget(track_widget, rect);
        }

        let master_x = if num_tracks > 0 {
            num_tracks as u16 * track_width
        } else {
            0
        };
        let master_rect = Rect::new(master_x, 0, track_width, area.height);
        let is_master_selected = selected == num_tracks;

        let master_style = if is_master_selected {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Cyan)
        };

        let master_text = format!(
            "MASTER\n\n{:.0}%\n\n{:.0}%",
            master * 100.0,
            master_meter * 100.0
        );

        let master_widget = Paragraph::new(master_text).style(master_style);
        f.render_widget(master_widget, master_rect);

        if show_help {
            let help_text = vec![
                Line::from("Mixer Help"),
                Line::from(""),
                Line::from("  h/l, /  Select track/master"),
                Line::from("  j/k, /   Level up/down"),
                Line::from("  +/-       Pan left/right"),
                Line::from("  m         Toggle mute"),
                Line::from("  s         Toggle solo"),
                Line::from("  ?         Toggle help"),
                Line::from("  ^s        Save state"),
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
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("mixer", 0);
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("mixer", 0, None)?;

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

    let inner = Arc::new(Mutex::new(MixerInner {
        tracks: Vec::new(),
        audio_sources: Vec::new(),
        master: 0.8,
        master_meter: 0.0,
        selected: 0,
        scope_rb: None,
    }));

    if let Ok(params) = state::load_module_state::<state::MixerParams>("mixer", 0) {
        let mut s = inner.lock().unwrap();
        if let Some(v) = params.master { s.master = v; }
    }

    let inner_clone = Arc::clone(&inner);
    let (_tx, rx) = std::sync::mpsc::channel();

    let _mixer_handle = std::thread::spawn(move || {
        if let Err(e) = mixer_thread(inner_clone, rx) {
            eprintln!("Mixer thread error: {}", e);
        }
    });

    let mut show_help = false;

    loop {
        if state::check_save_signal() {
            let s = inner.lock().unwrap();
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

        if state::check_reload_signal() {
            if let Ok(params) = state::load_module_state::<state::MixerParams>("mixer", 0) {
                let mut s = inner.lock().unwrap();
                if let Some(v) = params.master { s.master = v; }
                for (i, tp) in params.tracks.iter().enumerate().take(s.tracks.len()) {
                    s.tracks[i].level = tp.level;
                    s.tracks[i].pan = tp.pan;
                    s.tracks[i].mute = tp.mute;
                    s.tracks[i].solo = tp.solo;
                }
            }
        }

        let snapshot = {
            let s = inner.lock().unwrap();
            (s.tracks.clone(), s.master, s.master_meter, s.selected)
        };

        draw_ui(
            &mut terminal,
            &snapshot.0,
            snapshot.1,
            snapshot.2,
            snapshot.3,
            show_help,
        )?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let s = inner.lock().unwrap();
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
                        let mut s = inner.lock().unwrap();
                        let max = s.tracks.len();
                        s.selected = if s.selected == 0 { max } else { s.selected.saturating_sub(1) };
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        let mut s = inner.lock().unwrap();
                        let max = s.tracks.len();
                        s.selected = if s.selected >= max { 0 } else { s.selected + 1 };
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        let mut s = inner.lock().unwrap();
                        let sel = s.selected;
                        if sel < s.tracks.len() {
                            s.tracks[sel].level = (s.tracks[sel].level - 0.05).max(0.0);
                        } else {
                            s.master = (s.master - 0.05).max(0.0);
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        let mut s = inner.lock().unwrap();
                        let sel = s.selected;
                        if sel < s.tracks.len() {
                            s.tracks[sel].level = (s.tracks[sel].level + 0.05).min(1.0);
                        } else {
                            s.master = (s.master + 0.05).min(1.0);
                        }
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        let mut s = inner.lock().unwrap();
                        let sel = s.selected;
                        if sel < s.tracks.len() {
                            s.tracks[sel].pan = (s.tracks[sel].pan + 0.1).min(1.0);
                        }
                    }
                    KeyCode::Char('-') => {
                        let mut s = inner.lock().unwrap();
                        let sel = s.selected;
                        if sel < s.tracks.len() {
                            s.tracks[sel].pan = (s.tracks[sel].pan - 0.1).max(-1.0);
                        }
                    }
                    KeyCode::Char('m') => {
                        let mut s = inner.lock().unwrap();
                        let sel = s.selected;
                        if sel < s.tracks.len() {
                            s.tracks[sel].mute = !s.tracks[sel].mute;
                        }
                    }
                    KeyCode::Char('s') => {
                        let mut s = inner.lock().unwrap();
                        let sel = s.selected;
                        if sel < s.tracks.len() {
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
