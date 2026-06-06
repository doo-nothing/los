use std::time::Duration;

use anyhow::{Context, Result};

use crate::shm::{
    AudioRingbuf, EventRingbuf, ShmTransport,
    EVENT_NOTE_ON, EVENT_NOTE_OFF, EVENT_PARAM,
    PARAM_SHAPE, PARAM_SUB, PARAM_FM, PARAM_OUTPUT,
};

const SAMPLE_RATE: f64 = 48000.0;
const SLOT_FRAMES: usize = 64;

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
            3 => {}
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

// ── helpers ─────────────────────────────────────────────────────────────────

fn midi_to_freq(note: u8) -> f64 {
    440.0 * 2.0_f64.powf((note as f64 - 69.0) / 12.0)
}

// ── voice ──────────────────────────────────────────────────────────────────

pub fn run(_frequency: f32, instance: usize) -> Result<()> {
    let shm_name = "/los_mix_in";
    let channels = 2usize;
    let slot_frames = SLOT_FRAMES;
    let slot_len = slot_frames * channels;

    let mut ringbuf = match AudioRingbuf::open(shm_name) {
        Ok(rb) => rb,
        Err(_) => AudioRingbuf::create(shm_name)
            .context("creating SHM audio ringbuffer")?,
    };

    let _transport = match ShmTransport::open() {
        Ok(t) => t,
        Err(_) => ShmTransport::create(SAMPLE_RATE as u32)?,
    };

    let mut events = match EventRingbuf::open() {
        Ok(e) => Some(e),
        Err(_) => None,
    };

    let mut adsr = Adsr::new(SAMPLE_RATE as f32);
    adsr.trigger(); // start active so scope/mixer get audio immediately
    let mut phase: f32 = 0.0;
    let mut freq: f64 = midi_to_freq(60);
    let mut velocity: f32 = 1.0;

    // STO parameters
    let mut shape: f32 = 0.0;    // 0..1 morph sine→saw→square
    let mut sub_lvl: f32 = 0.0;  // 0..1 sub oscillator mix
    let mut fm_amt: f32 = 0.0;   // 0..1 FM amount
    let mut output: u8 = 2;      // 0=sine, 1=sine+sub, 2=shaped+sub

    let mut block = vec![0.0f32; slot_len];
    let capacity = ringbuf.num_slots() as u64;

    eprintln!(
        "los voice {}: {} ch, {} frames/slot",
        instance, channels, slot_frames,
    );

    loop {
        // Retry opening event ringbuffer if not yet available
        if events.is_none() {
            events = EventRingbuf::open().ok();
        }

        // Drain pending events
        if let Some(ref mut evbuf) = events {
            while let Some(event) = evbuf.read_event() {
                match event.event_type {
                    EVENT_NOTE_ON => {
                        adsr.trigger();
                        freq = midi_to_freq(event.note);
                        velocity = event.velocity as f32 / 127.0;
                    }
                    EVENT_NOTE_OFF => adsr.release(),
                    EVENT_PARAM => {
                        let val = event.velocity as f32 / 127.0;
                        match event.note {
                            PARAM_SHAPE => shape = val,
                            PARAM_SUB => sub_lvl = val,
                            PARAM_FM => fm_amt = val,
                            PARAM_OUTPUT => output = if val < 0.333 { 0 } else if val < 0.666 { 1 } else { 2 },
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }

        // Generate one block of audio (STO wave shaping)
        for frame in 0..slot_frames {
            let env = adsr.tick();

            // Advance phase with optional FM
            let fm_lfo = (phase as f64 * 0.1).sin() as f32 * fm_amt * 0.5;
            let mod_freq = freq * (1.0 + fm_lfo as f64);
            phase = (phase + mod_freq as f32 / SAMPLE_RATE as f32).fract();

            // STO wave shaping: morph sine → saw → square
            let sine = (phase * 2.0 * std::f32::consts::PI).sin();
            let saw = 2.0 * phase - 1.0;
            let square = if phase < 0.5 { 1.0 } else { -1.0 };

            let shaped = if shape < 0.5 {
                let t = shape * 2.0; // 0..1 within sine→saw range
                sine * (1.0 - t) + saw * t
            } else {
                let t = (shape - 0.5) * 2.0; // 0..1 within saw→square range
                saw * (1.0 - t) + square * t
            };

            // Sub oscillator (square at half frequency)
            let sub_phase = (phase * 0.5).fract();
            let sub = if sub_phase < 0.5 { 1.0 } else { -1.0 };

            // Output routing
            let out = match output {
                0 => sine,
                1 => sine * (1.0 - sub_lvl) + sub * sub_lvl,
                _ => shaped * (1.0 - sub_lvl) + sub * sub_lvl,
            };

            let amp = out * 0.3 * env * velocity;
            block[frame * channels] = amp;
            block[frame * channels + 1] = amp;
        }

        // Write to ringbuffer — backpressure via yield+occasional sleep
        let mut retries = 0u32;
        loop {
            match ringbuf.write(&block) {
                Ok(()) => break,
                Err(_) => {
                    retries += 1;
                    if retries > 100 {
                        std::thread::sleep(Duration::from_micros(200));
                        retries = 0;
                    } else {
                        std::thread::yield_now();
                    }
                }
            }
        }

        // Optional sleep if the ringbuffer is getting ahead of the mixer
        if ringbuf.available() >= capacity / 2 {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
}
