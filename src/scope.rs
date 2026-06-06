use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;

use anyhow::Result;

use crate::shm::AudioRingbuf;

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
        let has_input = unsafe { libc::poll(poll_fds.as_mut_ptr(), 1, 100) };
        if has_input > 0 && (poll_fds[0].revents & libc::POLLIN) != 0 {
            let mut buf = [0u8; 8];
            let _ = io::stdin().read(&mut buf);
            if buf[0] == b'q' {
                break;
            }
        }

        if ringbuf.peek_latest(&mut slot).unwrap_or(false) {
            let (w, h) = terminal_size();
            render_scope(&slot, channels, w, h);
        }
    }

    Ok(())
}

fn terminal_size() -> (usize, usize) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 {
            let w = if ws.ws_col > 0 { ws.ws_col as usize } else { 80 };
            let h = if ws.ws_row > 0 { ws.ws_row as usize } else { 6 };
            (w, h)
        } else {
            (80, 6)
        }
    }
}

fn render_scope(samples: &[f32], channels: usize, width: usize, height: usize) {
    let n = samples.len() / channels;
    if n == 0 || height < 3 || width < 2 {
        return;
    }

    // Auto-gain: peak amplitude across all channels
    let mut peak = 0.0f32;
    for i in (0..n).map(|i| i * channels) {
        let a = samples[i].abs();
        if a > peak { peak = a; }
    }
    let gain = if peak > 0.001 { 0.85 / peak } else { 1.0 };

    let _ = io::stdout().write(b"\x1b[2J\x1b[H");
    let wave_rows = height - 2;
    let mid = wave_rows as f32 / 2.0;

    // Info line at the very top
    let _ = io::stdout().write(
        format!("  scale: ─1.0────────────────0────────────────+1.0\n").as_bytes(),
    );

    for row in 0..wave_rows {
        let thresh = (mid - row as f32) / mid; // +1 (top) to -1 (bottom)
        let is_center = thresh.abs() < 0.04;
        let is_grid = (thresh.abs() - 0.5).abs() < 0.04;

        let mut line = String::with_capacity(width + 1);
        for col in 0..width {
            let frame = (col * n).saturating_div(width);
            let s_idx = (frame * channels).min(samples.len() - channels);
            let sample = samples[s_idx] * gain;

            let ch = if is_center && (col % 4 == 0) {
                '┼'
            } else if is_center {
                '─'
            } else if is_grid && (col % 4 == 0) {
                '╌'
            } else if thresh > 0.0 && sample > thresh * 0.85 {
                '█'
            } else if thresh < 0.0 && sample < thresh * 0.85 {
                '█'
            } else {
                ' '
            };
            line.push(ch);
        }
        line.push('\n');
        let _ = io::stdout().write(line.as_bytes());
    }

    let _ = io::stdout().write(b"  [q=quit]");
    let _ = io::stdout().flush();
}
