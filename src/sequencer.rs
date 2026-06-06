use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};

use crate::shm::{
    AudioEvent, EventRingbuf, ShmTransport,
    PARAM_SHAPE,
};

const NUM_STEPS: usize = 16;

#[derive(Clone)]
struct Step {
    active: bool,
    note: u8,
}

fn enable_raw_mode() -> Result<()> {
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut termios) } != 0 {
        anyhow::bail!("tcgetattr failed");
    }
    unsafe { libc::cfmakeraw(&mut termios) };
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios) } != 0 {
        anyhow::bail!("tcsetattr failed");
    }
    Ok(())
}

pub fn run(_instance: usize) -> Result<()> {
    let sample_rate = 48000.0;

    let mut events = match EventRingbuf::open() {
        Ok(e) => e,
        Err(_) => EventRingbuf::create()?,
    };

    let transport = match ShmTransport::open() {
        Ok(t) => t,
        Err(_) => ShmTransport::create(sample_rate as u32)?,
    };

    enable_raw_mode().context("enabling raw mode for sequencer")?;

    let mut steps = vec![
        Step { active: false, note: 60 }; NUM_STEPS
    ];
    for i in (0..NUM_STEPS).step_by(4) {
        steps[i].active = true;
    }

    let mut bpm: f64 = 120.0;
    let mut playing = true;
    let mut last_step: i32 = -1;
    let mut last_note: Option<u8> = None;
    let mut selected: usize = 0;
    let mut pending_g = false;

    let mut poll_fds = [libc::pollfd {
        fd: io::stdin().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];

    eprintln!("sequencer: vi keys (j/k select, p play, s stop, 0/$, w/b, space toggle, n<note>, t<bpm>, q quit)");

    loop {
        // Sequencer step logic via transport clock
        let clock = transport.clock();
        let samples_per_step = (60.0 / bpm * sample_rate / 4.0) as u64;
        let current_step = if samples_per_step > 0 && playing {
            (clock / samples_per_step) as usize % NUM_STEPS
        } else {
            last_step.max(0) as usize
        };

        if current_step as i32 != last_step {
            if playing {
                if let Some(n) = last_note {
                    let _ = events.write_event(&AudioEvent::note_off(n, last_step as u32));
                }
                if steps[current_step].active {
                    let note = steps[current_step].note;
                    let _ = events.write_event(&AudioEvent::note_on(note, 100, current_step as u32));
                    last_note = Some(note);
                } else {
                    last_note = None;
                }
            }
            last_step = current_step as i32;
        }

        // Read stdin (raw mode, so individual keypresses)
        let has_input = unsafe { libc::poll(poll_fds.as_mut_ptr(), 1, 50) };
        if has_input > 0 && (poll_fds[0].revents & libc::POLLIN) != 0 {
            let mut buf = [0u8; 1];
            if io::stdin().read(&mut buf).unwrap_or(0) == 0 {
                continue;
            }
            let ch = buf[0];

            // Count prefix
            if ch.is_ascii_digit() && ch != b'0' {
                let mut num_buf = vec![ch];
                loop {
                    let more = check_input(20);
                    if let Some(c) = more {
                        if c.is_ascii_digit() {
                            num_buf.push(c);
                            continue;
                        }
                        let count_str = String::from_utf8_lossy(&num_buf);
                        let count = count_str.parse().unwrap_or(1);
                        handle_key(c, &mut steps, &mut playing, &mut selected, count, &mut events);
                        break;
                    }
                    break;
                }
                continue;
            }

            if pending_g {
                if ch == b'g' {
                    selected = 0;
                }
                pending_g = false;
                continue;
            }
            if ch == b'g' {
                pending_g = true;
                continue;
            }
            if ch == b'n' {
                // Read note number
                let mut note_buf = String::new();
                loop {
                    if let Some(c) = check_input(100) {
                        if c.is_ascii_digit() {
                            note_buf.push(c as char);
                            continue;
                        }
                        break;
                    }
                    break;
                }
                if let Ok(note) = note_buf.trim().parse::<u8>() {
                    steps[selected].note = note.clamp(0, 127);
                }
                continue;
            }
            if ch == b't' {
                let mut bpm_buf = String::new();
                loop {
                    if let Some(c) = check_input(100) {
                        if c.is_ascii_digit() || c == b'.' {
                            bpm_buf.push(c as char);
                            continue;
                        }
                        break;
                    }
                    break;
                }
                if let Ok(t) = bpm_buf.trim().parse::<f64>() {
                    bpm = t.clamp(20.0, 300.0);
                }
                continue;
            }

            handle_key(ch, &mut steps, &mut playing, &mut selected, 1, &mut events);
        }

        display_grid(&steps, current_step, last_note, bpm, playing as usize, selected);
    }
}

fn check_input(timeout_ms: i32) -> Option<u8> {
    let mut poll_fds = [libc::pollfd {
        fd: io::stdin().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    let has = unsafe { libc::poll(poll_fds.as_mut_ptr(), 1, timeout_ms) };
    if has > 0 && (poll_fds[0].revents & libc::POLLIN) != 0 {
        let mut buf = [0u8; 1];
        if io::stdin().read(&mut buf).unwrap_or(0) > 0 {
            return Some(buf[0]);
        }
    }
    None
}

fn handle_key(ch: u8, steps: &mut [Step], playing: &mut bool, selected: &mut usize, count: usize, events: &mut EventRingbuf) {
    match ch {
        b'p' => *playing = !*playing,
        b's' => *playing = false,
        b' ' => {
            steps[*selected].active = !steps[*selected].active;
        }
        b'j' | b'l' => {
            *selected = selected.saturating_add(count).min(NUM_STEPS - 1);
        }
        b'k' | b'h' => {
            *selected = selected.saturating_sub(count).min(NUM_STEPS - 1);
        }
        b'0' => *selected = 0,
        b'$' => *selected = NUM_STEPS - 1,
        b'w' => {
            for i in 1..=NUM_STEPS {
                let idx = (*selected + i) % NUM_STEPS;
                if steps[idx].active {
                    *selected = idx;
                    break;
                }
            }
        }
        b'b' => {
            for i in 1..=NUM_STEPS {
                let idx = (*selected + NUM_STEPS - i) % NUM_STEPS;
                if steps[idx].active {
                    *selected = idx;
                    break;
                }
            }
        }
        b'1'..=b'4' => {
            // Quick param sends (future: track selection)
            let val = (((ch - b'1') as f32 / 3.0) * 127.0) as u8;
            let _ = events.write_event(&AudioEvent::param(PARAM_SHAPE, val));
        }
        b'q' | 0x03 => std::process::exit(0), // q or Ctrl-C
        _ => {}
    }
}

fn display_grid(steps: &[Step], current: usize, note: Option<u8>, bpm: f64, playing: usize, selected: usize) {
    let _ = io::stdout().write(b"\x1b[2J\x1b[H");

    let mut line = String::new();
    for i in 0..NUM_STEPS {
        if i == selected && i == current {
            line.push_str(">[");
        } else if i == selected {
            line.push_str("*[");
        } else if i == current {
            line.push_str(">[");
        } else {
            line.push_str(" [");
        }
        if steps[i].active {
            line.push('X');
        } else {
            line.push(' ');
        }
        line.push(']');
    }

    let note_str = note.map_or(String::new(), |n| format!(" ♪ {:>3}", midi_note_name(n)));
    let play_char = if playing != 0 { "►" } else { "■" };
    line.push_str(&format!("{}  {}  {} BPM  step{}", note_str, play_char, bpm as u32, current));

    let _ = io::stdout().write(line.as_bytes());
    let _ = io::stdout().write(b"  [q=quit]");
    let _ = io::stdout().flush();
}

fn midi_note_name(note: u8) -> String {
    let names = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    let octave = (note / 12).saturating_sub(1);
    let idx = (note % 12) as usize;
    format!("{}{}", names[idx], octave)
}
