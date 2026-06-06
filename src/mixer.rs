use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::shm::{AudioRingbuf, ShmTransport};

pub fn run() -> Result<()> {
    let ringbuf_name = "/los_mix_in";

    // Try to open the existing ringbuffer
    let ringbuf = AudioRingbuf::open(ringbuf_name).with_context(|| {
        format!("could not open SHM ringbuffer '{ringbuf_name}' — is a voice/tone running?")
    })?;

    let channels = ringbuf.channels() as usize;
    let slot_len = ringbuf.slot_len();
    let sample_rate = 48000u32;

    // Open (or create) the transport SHM with the sample clock.
    // Uses open() first to avoid overwriting the clock if the voice
    // already created it.
    let mut transport = match ShmTransport::open() {
        Ok(t) => t,
        Err(_) => ShmTransport::create(sample_rate)
            .context("creating transport SHM")?,
    };

    // Find an audio device
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no audio output device found")?;

    eprintln!(
        "los mixer: using device '{}' ({} Hz, {} channels)",
        device.name()?,
        sample_rate,
        channels,
    );

    // Build the stream config
    let config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Fixed(ringbuf.slot_frames()),
    };

    let ringbuf = Arc::new(std::sync::Mutex::new(ringbuf));

    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut buf = ringbuf.lock().unwrap();

                // Read as many complete slots as we can
                let mut written = 0;
                while written + slot_len <= data.len() {
                    match buf.read(&mut data[written..written + slot_len]) {
                        Ok(true) => written += slot_len,
                        Ok(false) => break,
                        Err(_) => break,
                    }
                }

                // Zero-fill any remaining (silence)
                if written < data.len() {
                    for sample in data[written..].iter_mut() {
                        *sample = 0.0;
                    }
                }
            },
            |err| eprintln!("mixer audio error: {err}"),
            None,
        )
        .context("building audio output stream")?;

    stream.play().context("starting audio stream")?;

    eprintln!("los mixer: running (press Ctrl-C to stop)");

    // Update the transport clock based on wall-clock time
    let start = Instant::now();
    loop {
        let elapsed = start.elapsed().as_secs_f64();
        let expected = (elapsed * sample_rate as f64) as u64;
        transport.set_clock(expected);
        std::thread::sleep(Duration::from_millis(5));
    }
}
