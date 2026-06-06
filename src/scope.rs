use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;

use anyhow::Result;

use crate::shm::AudioRingbuf;

const BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub fn run(_instance: usize) -> Result<()> {
    let ringbuf = match AudioRingbuf::open("/los_mix_in") {
        Ok(rb) => rb,
        Err(_) => AudioRingbuf::create("/los_mix_in")
            .map_err(|e| anyhow::anyhow!("creating audio ringbuffer: {}", e))?,
    };
    let channels = ringbuf.channels() as usize;
    let slot_len = ringbuf.slot_len();

    let mut slot = vec![0.0f32; slot_len];
    let stdin_fd = io::stdin().as_raw_fd();

    let mut poll_fds = [libc::pollfd {
        fd: stdin_fd,
        events: libc::POLLIN,
        revents: 0,
    }];

    eprintln!("los scope: running");

    loop {
        // Check for quit
        let has_input = unsafe { libc::poll(poll_fds.as_mut_ptr(), 1, 100) };
        if has_input > 0 && (poll_fds[0].revents & libc::POLLIN) != 0 {
            let mut buf = [0u8; 8];
            let _ = io::stdin().read(&mut buf);
            if buf[0] == b'q' {
                break;
            }
        }

        // Peek the latest audio data
        if ringbuf.peek_latest(&mut slot).unwrap_or(false) {
            let width = terminal_width();
            render_scope(&slot, channels, width);
        }
    }

    Ok(())
}

fn terminal_width() -> usize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            ws.ws_col as usize
        } else {
            80
        }
    }
}

fn render_scope(samples: &[f32], channels: usize, width: usize) {
    let n = samples.len() / channels;
    if n == 0 {
        return;
    }

    let mut line = String::from("\r");

    for col in 0..width {
        // Map column to sample index
        let idx = col * n / width;
        let sample = samples[idx * channels]; // left channel
        let amp = sample.abs().min(1.0);
        let level = (amp * 8.0).round() as usize;
        line.push(BARS[level.min(8)]);
    }

    line.push_str("  [q=quit]\x1b[J");

    let _ = io::stdout().write(line.as_bytes());
    let _ = io::stdout().flush();
}
