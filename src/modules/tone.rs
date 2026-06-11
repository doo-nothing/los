use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::shm::{AudioRingbuf, Manifest};

pub fn run(frequency: f32, instance: usize) -> Result<()> {
    let shm_name = format!("/los_audio_tone_{}", instance);
    let mut ringbuf = AudioRingbuf::create(&shm_name).context("creating SHM audio ringbuffer")?;

    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("tone", instance, Some(&shm_name), 0)?;

    // read the device's real rate when a session is up (test tones should
    // be in tune too); 48k only as the no-mixer fallback
    let sample_rate = crate::shm::ShmTransport::open()
        .map(|t| f64::from(t.sample_rate()).max(1.0))
        .unwrap_or(48000.0);
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
    let slot_duration =
        Duration::from_nanos(slot_frames as u64 * 1_000_000_000 / sample_rate as u64);

    loop {
        let tick = Instant::now();

        for frame in 0..slot_frames {
            let t = phase + frame as f64;
            let sample = (t * freq as f64 * 2.0 * std::f64::consts::PI / sample_rate).sin() as f32;
            for ch in 0..channels {
                block[frame * channels + ch] = sample * amplitude;
            }
        }

        // full ring: sleep, don't yield-spin — the consumer frees a
        // slot every ~1.3 ms
        loop {
            match ringbuf.write(&block) {
                Ok(()) => break,
                Err(_) => {
                    thread::sleep(Duration::from_micros(500));
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
