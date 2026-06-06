use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::shm::{AudioRingbuf, ShmTransport};

// ── oscillator ──────────────────────────────────────────────────────────────

enum Waveform {
    Sine,
    Triangle,
    Saw,
    Square,
}

struct Oscillator {
    phase: f64,
    waveform: Waveform,
}

impl Oscillator {
    fn new(waveform: Waveform) -> Self {
        Self { phase: 0.0, waveform }
    }

    fn tick(&mut self, freq: f64, sample_rate: f64) -> f32 {
        let inc = freq / sample_rate;
        self.phase = (self.phase + inc).fract();
        match self.waveform {
            Waveform::Sine => (self.phase * 2.0 * std::f64::consts::PI).sin() as f32,
            Waveform::Triangle => (4.0 * (0.5 - (self.phase - 0.5).abs()) - 1.0) as f32,
            Waveform::Saw => (2.0 * self.phase - 1.0) as f32,
            Waveform::Square => {
                if self.phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
        }
    }
}

// ── ADSR envelope ──────────────────────────────────────────────────────────

struct Adsr {
    state: u8, // 0=idle, 1=attack, 2=decay, 3=sustain, 4=release
    level: f32,
    attack_rate: f32,
    decay_rate: f32,
    sustain: f32,
    release_rate: f32,
}

impl Adsr {
    fn new(sample_rate: f32) -> Self {
        Self {
            state: 0,
            level: 0.0,
            attack_rate: 1.0 / (0.005 * sample_rate), // 5ms attack
            decay_rate: 1.0 / (0.1 * sample_rate),   // 100ms decay
            sustain: 0.7,
            release_rate: 1.0 / (0.3 * sample_rate), // 300ms release
        }
    }

    fn trigger(&mut self) {
        self.state = 1;
        self.level = 0.0;
    }

    fn release(&mut self) {
        if self.state >= 1 && self.state <= 3 {
            self.state = 4;
        }
    }

    fn tick(&mut self) -> f32 {
        match self.state {
            1 => {
                self.level += self.attack_rate;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.state = 2;
                }
            }
            2 => {
                self.level -= self.decay_rate;
                if self.level <= self.sustain {
                    self.level = self.sustain;
                    self.state = 3;
                }
            }
            3 => {} // sustain
            4 => {
                self.level -= self.release_rate;
                if self.level <= 0.0 {
                    self.level = 0.0;
                    self.state = 0;
                }
            }
            _ => {}
        }
        self.level
    }
}

// ── state-variable filter ───────────────────────────────────────────────────

struct Svf {
    cutoff: f64,
    resonance: f64,
    state1: f64,
    state2: f64,
}

impl Svf {
    fn new(cutoff: f64, resonance: f64) -> Self {
        Self {
            cutoff,
            resonance,
            state1: 0.0,
            state2: 0.0,
        }
    }

    fn process(&mut self, input: f64, sample_rate: f64) -> f64 {
        let f = 2.0 * (self.cutoff * std::f64::consts::PI / sample_rate).sin();
        let q = 1.0 - self.resonance * 0.95;
        let hp = input - self.state1 * q - self.state2;
        let bp = self.state1 + f * hp;
        let lp = self.state2 + f * bp;
        self.state1 = bp;
        self.state2 = lp;
        lp
    }
}

// ── voice ──────────────────────────────────────────────────────────────────

pub fn run(frequency: f32, instance: usize) -> Result<()> {
    let shm_name = "/los_mix_in";
    let sample_rate = 48000.0;
    let channels = 2usize;
    let slot_frames = 64usize;
    let slot_len = slot_frames * channels;

    // Open the audio ringbuffer (created by tone, or create if first)
    let mut ringbuf = match AudioRingbuf::open(shm_name) {
        Ok(rb) => rb,
        Err(_) => AudioRingbuf::create(shm_name)
            .context("creating SHM audio ringbuffer")?,
    };

    // Open (or create) the transport for clock-based pacing
    let transport = match ShmTransport::open() {
        Ok(t) => t,
        Err(_) => ShmTransport::create(sample_rate as u32)?,
    };

    let mut osc = Oscillator::new(Waveform::Sine);
    let mut adsr = Adsr::new(sample_rate as f32);
    let mut filter = Svf::new(1000.0, 0.3);

    let freq = frequency as f64;

    // Trigger the envelope once for continuous tone
    adsr.trigger();

    let mut block = vec![0.0f32; slot_len];
    let slot_dur = Duration::from_nanos(slot_frames as u64 * 1_000_000_000 / sample_rate as u64);

    eprintln!(
        "los voice {}: {} Hz, {} ch, {} frames/slot",
        instance, frequency, channels, slot_frames,
    );

    loop {
        let tick = Instant::now();

        // Read transport clock to pace ourselves
        let mixer_clock = transport.clock();
        let voice_clock = ringbuf.write_index() * slot_frames as u64;

        // If we're more than 4 blocks ahead, sleep
        let ahead_frames = voice_clock.saturating_sub(mixer_clock);
        if ahead_frames > (4 * slot_frames) as u64 {
            let slack = ahead_frames - (2 * slot_frames) as u64;
            let slack_us = slack * 1_000_000 / sample_rate as u64;
            std::thread::sleep(Duration::from_micros(slack_us.min(10_000)));
            continue;
        }

        // Generate one block of audio
        for frame in 0..slot_frames {
            let env = adsr.tick();
            let raw = osc.tick(freq, sample_rate);
            let flt = filter.process(raw as f64, sample_rate) as f32;
            let amp = flt * env;
            block[frame * channels] = amp;
            block[frame * channels + 1] = amp;
        }

        // Write to ringbuffer
        loop {
            match ringbuf.write(&block) {
                Ok(()) => break,
                Err(_) => {
                    // Full — yield and retry
                    std::thread::yield_now();
                }
            }
        }

        // Pace to real-time
        let elapsed = tick.elapsed();
        if elapsed < slot_dur {
            std::thread::sleep(slot_dur - elapsed);
        }
    }
}
