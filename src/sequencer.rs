use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;

use anyhow::Result;

use crate::shm::{AudioEvent, EventRingbuf, ShmTransport};

const NUM_STEPS: usize = 16;

#[derive(Clone)]
struct Step {
    active: bool,
    note: u8,
}

pub fn run(instance: usize) -> Result<()> {
    let sample_rate = 48000.0;

    // Open or create the event ringbuffer
    let mut events = match EventRingbuf::open() {
        Ok(e) => e,
        Err(_) => EventRingbuf::create()?,
    };

    // Open or create the transport
    let transport = match ShmTransport::open() {
        Ok(t) => t,
        Err(_) => ShmTransport::create(sample_rate as u32)?,
    };

    let mut steps = vec![
        Step {
            active: false,
            note: 60,
        };
        NUM_STEPS
    ];

    // Default pattern: every 4th step
    for i in (0..NUM_STEPS).step_by(4) {
        steps[i].active = true;
    }

    let mut bpm: f64 = 120.0;
    let mut playing = true;
    let mut last_step: i32 = -1;
    let mut last_note: Option<u8> = None;
    let mut selected_step: usize = 0;

    let stdin_fd = io::stdin().as_raw_fd();

    // Set stdin to non-blocking
    let mut poll_fds = [libc::pollfd {
        fd: stdin_fd,
        events: libc::POLLIN,
        revents: 0,
    }];

    eprintln!("los sequencer {}: running (type step#, n<note>, t<bpm>, p=toggle play, q=quit)", instance);

    loop {
        let clock = transport.clock();
        let samples_per_step = (60.0 / bpm * sample_rate / 4.0) as u64;
        let current_step = if samples_per_step > 0 && playing {
            (clock / samples_per_step) as usize % NUM_STEPS
        } else {
            last_step.max(0) as usize
        };

        // Step boundary crossed: send events
        if current_step as i32 != last_step {
            if playing {
                // Note-off for previous active note
                if let Some(n) = last_note {
                    let _ = events.write_event(&AudioEvent::note_off(n, last_step as u32));
                }
                // Note-on for current step if active
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

        // Read and process stdin commands
        let has_input = unsafe { libc::poll(poll_fds.as_mut_ptr(), 1, 50) };
        if has_input > 0 && (poll_fds[0].revents & libc::POLLIN) != 0 {
            let mut buf = [0u8; 128];
            let n = io::stdin().read(&mut buf).unwrap_or(0);
            let line = String::from_utf8_lossy(&buf[..n]).trim().to_string();
            if !line.is_empty() {
                handle_command(&line, &mut steps, &mut bpm, &mut playing, &mut selected_step);
            }
        }

        // Display the grid
        display_grid(&steps, current_step, last_note, bpm, playing, selected_step);
    }
}

fn handle_command(line: &str, steps: &mut [Step], bpm: &mut f64, playing: &mut bool, selected: &mut usize) {
    let line = line.trim();

    if line == "p" {
        *playing = !*playing;
        return;
    }
    if line == "q" {
        std::process::exit(0);
    }

    if let Ok(n) = line.parse::<usize>() {
        if n < NUM_STEPS {
            steps[n].active = !steps[n].active;
            *selected = n;
            return;
        }
    }

    if let Some(rest) = line.strip_prefix('n') {
        if let Ok(note) = rest.trim().parse::<u8>() {
            steps[*selected].note = note.clamp(0, 127);
        }
        return;
    }

    if let Some(rest) = line.strip_prefix('t') {
        if let Ok(t) = rest.trim().parse::<f64>() {
            *bpm = t.clamp(20.0, 300.0);
        }
        return;
    }
}

fn display_grid(steps: &[Step], current: usize, note: Option<u8>, bpm: f64, playing: bool, selected: usize) {
    // Clear the pane and move cursor home to avoid scrolling
    let _ = io::stdout().write(b"\x1b[2J\x1b[H");

    // Build the step display line
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

    // Note label
    let note_str = note.map_or(String::new(), |n| format!(" ♪ {:>3}", midi_note_name(n)));
    let play_char = if playing { "►" } else { "■" };

    line.push_str(&format!("{}  {}  {} BPM  step {}", note_str, play_char, bpm as u32, current));

    // Write the line and flush stdout
    let _ = io::stdout().write(line.as_bytes());
    let _ = io::stdout().flush();
}

fn midi_note_name(note: u8) -> String {
    let names = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    let octave = (note / 12).saturating_sub(1);
    let idx = (note % 12) as usize;
    format!("{}{}", names[idx], octave)
}
