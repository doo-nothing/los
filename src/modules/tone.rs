use std::time::{Duration, Instant};
use std::thread;

use anyhow::{Context, Result};

use crate::shm::{AudioRingbuf, Manifest};

pub fn run(frequency: f32, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_tone_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name)
        .context("creating SHM audio ringbuffer")?;

    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("tone", instance, Some(&shm_name), 0)?;

    let sample_rate = 48000.0;
    let channels = ringbuf.channels() as usize;
    let slot_frames = ringbuf.slot_frames() as usize;
    let slot_len = ringbuf.slot_len();

    let mut phase = 0.0_f64;
    let freq = frequency;
    let amplitude = 0.3 / (instance + 1) as f32;

    eprintln!(
        "los tone {}: {} Hz -> {} ({} ch, {} frames/slot)",
        instance, freq, shm_name, channels, slot_frames,
    );

    let mut block = vec![0.0_f32; slot_len];
    let slot_duration = Duration::from_nanos(
        slot_frames as u64 * 1_000_000_000 / sample_rate as u64,
    );

    loop {
        let tick = Instant::now();

        for frame in 0..slot_frames {
            let t = phase + frame as f64;
            let sample = (t * freq as f64 * 2.0 * std::f64::consts::PI / sample_rate).sin() as f32;
            for ch in 0..channels {
                block[frame * channels + ch] = sample * amplitude;
            }
        }

        loop {
            match ringbuf.write(&block) {
                Ok(()) => break,
                Err(_) => {
                    thread::yield_now();
                }
            }
        }

        phase = (phase + slot_frames as f64) % sample_rate;

        let elapsed = tick.elapsed();
        if elapsed < slot_duration {
            thread::sleep(slot_duration - elapsed);
        }
    }
}
