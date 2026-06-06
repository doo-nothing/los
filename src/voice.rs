use std::io;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph},
    Terminal,
};

use crate::shm::{
    AudioRingbuf, EventRingbuf, ShmTransport,
    EVENT_NOTE_ON, EVENT_NOTE_OFF, EVENT_PARAM,
    PARAM_SHAPE, PARAM_SUB, PARAM_FM, PARAM_OUTPUT,
};

const SAMPLE_RATE: f64 = 48000.0;
const SLOT_FRAMES: usize = 64;

// ── ADSR envelope ──────────────────────────────────────────────────────────

struct Adsr {
    state: u8,
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
            attack_rate: 1.0 / (0.005 * sample_rate),
            decay_rate: 1.0 / (0.1 * sample_rate),
            sustain: 0.7,
            release_rate: 1.0 / (0.3 * sample_rate),
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

fn midi_to_freq(note: u8) -> f64 {
    440.0 * 2.0_f64.powf((note as f64 - 69.0) / 12.0)
}

#[derive(Clone, Copy)]
struct VoiceState {
    shape: f32,
    sub_lvl: f32,
    fm_amt: f32,
    output: u8,
    adsr_level: f32,
    freq: f64,
    velocity: f32,
}

impl Default for VoiceState {
    fn default() -> Self {
        Self {
            shape: 0.0,
            sub_lvl: 0.0,
            fm_amt: 0.0,
            output: 2,
            adsr_level: 0.0,
            freq: midi_to_freq(60),
            velocity: 1.0,
        }
    }
}

fn audio_thread(
    state: Arc<Mutex<VoiceState>>,
    event_rx: mpsc::Receiver<()>,
) -> Result<()> {
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
    adsr.trigger();
    let mut phase: f32 = 0.0;

    let mut block = vec![0.0f32; slot_len];
    let capacity = ringbuf.num_slots() as u64;

    loop {
        if event_rx.try_recv().is_ok() {
            break;
        }

        if events.is_none() {
            events = EventRingbuf::open().ok();
        }

        if let Some(ref mut evbuf) = events {
            while let Some(event) = evbuf.read_event() {
                match event.event_type {
                    EVENT_NOTE_ON => {
                        adsr.trigger();
                        let mut s = state.lock().unwrap();
                        s.freq = midi_to_freq(event.note);
                        s.velocity = event.velocity as f32 / 127.0;
                    }
                    EVENT_NOTE_OFF => {
                        adsr.release();
                    }
                    EVENT_PARAM => {
                        let val = event.velocity as f32 / 127.0;
                        let mut s = state.lock().unwrap();
                        match event.note {
                            PARAM_SHAPE => s.shape = val,
                            PARAM_SUB => s.sub_lvl = val,
                            PARAM_FM => s.fm_amt = val,
                            PARAM_OUTPUT => s.output = if val < 0.333 { 0 } else if val < 0.666 { 1 } else { 2 },
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }

        let s = state.lock().unwrap();
        let freq = s.freq;
        let velocity = s.velocity;
        let shape = s.shape;
        let sub_lvl = s.sub_lvl;
        let fm_amt = s.fm_amt;
        let output = s.output;
        drop(s);

        for frame in 0..slot_frames {
            let env = adsr.tick();

            let mut s = state.lock().unwrap();
            s.adsr_level = env;
            drop(s);

            let fm_lfo = (phase as f64 * 0.1).sin() as f32 * fm_amt * 0.5;
            let mod_freq = freq * (1.0 + fm_lfo as f64);
            phase = (phase + mod_freq as f32 / SAMPLE_RATE as f32).fract();

            let sine = (phase * 2.0 * std::f32::consts::PI).sin();
            let saw = 2.0 * phase - 1.0;
            let square = if phase < 0.5 { 1.0 } else { -1.0 };

            let shaped = if shape < 0.5 {
                let t = shape * 2.0;
                sine * (1.0 - t) + saw * t
            } else {
                let t = (shape - 0.5) * 2.0;
                saw * (1.0 - t) + square * t
            };

            let sub_phase = (phase * 0.5).fract();
            let sub = if sub_phase < 0.5 { 1.0 } else { -1.0 };

            let out = match output {
                0 => sine,
                1 => sine * (1.0 - sub_lvl) + sub * sub_lvl,
                _ => shaped * (1.0 - sub_lvl) + sub * sub_lvl,
            };

            let amp = out * 0.3 * env * velocity;
            block[frame * channels] = amp;
            block[frame * channels + 1] = amp;
        }

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

        if ringbuf.available() >= capacity / 2 {
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    Ok(())
}

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &VoiceState,
    selected_param: usize,
) -> Result<()> {
    terminal.draw(|f| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(0),
            ])
            .split(f.area());

        let title = Paragraph::new("LOS Voice - STO Wave Shaping")
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(title, chunks[0]);

        let shape_style = if selected_param == 0 {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let shape_gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL).title("Shape (sine→saw→square)"))
            .gauge_style(shape_style)
            .percent((state.shape * 100.0) as u16)
            .label(format!("{:.2}", state.shape));
        f.render_widget(shape_gauge, chunks[1]);

        let sub_style = if selected_param == 1 {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let sub_gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL).title("Sub Oscillator"))
            .gauge_style(sub_style)
            .percent((state.sub_lvl * 100.0) as u16)
            .label(format!("{:.2}", state.sub_lvl));
        f.render_widget(sub_gauge, chunks[2]);

        let fm_style = if selected_param == 2 {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let fm_gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL).title("FM Amount"))
            .gauge_style(fm_style)
            .percent((state.fm_amt * 100.0) as u16)
            .label(format!("{:.2}", state.fm_amt));
        f.render_widget(fm_gauge, chunks[3]);

        let output_style = if selected_param == 3 {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let output_label = match state.output {
            0 => "Sine",
            1 => "Sine + Sub",
            _ => "Shaped + Sub",
        };
        let output_para = Paragraph::new(Line::from(vec![
            Span::raw("Output: "),
            Span::styled(output_label, output_style),
        ]))
        .block(Block::default().borders(Borders::ALL).title("Output Routing"));
        f.render_widget(output_para, chunks[4]);

        let adsr_gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL).title("ADSR Envelope"))
            .gauge_style(Style::default().fg(Color::Green))
            .percent((state.adsr_level * 100.0) as u16)
            .label(format!("{:.2}", state.adsr_level));
        f.render_widget(adsr_gauge, chunks[5]);

        let help_text = vec![
            Line::from("Controls:"),
            Line::from("  j/k or ↑/↓ : Select parameter"),
            Line::from("  h/l or ←/→ : Adjust value"),
            Line::from("  1/2/3      : Set output mode"),
            Line::from("  q          : Quit"),
            Line::from(""),
            Line::from(format!("Freq: {:.1} Hz  Vel: {:.2}", state.freq, state.velocity)),
        ];
        let help = Paragraph::new(help_text)
            .style(Style::default().fg(Color::Gray))
            .block(Block::default().borders(Borders::ALL).title("Help"));
        f.render_widget(help, chunks[6]);
    })?;

    Ok(())
}

pub fn run(_frequency: f32, _instance: usize) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let state = Arc::new(Mutex::new(VoiceState::default()));
    let state_clone = Arc::clone(&state);

    let (tx, rx) = mpsc::channel();

    let audio_handle = std::thread::spawn(move || {
        if let Err(e) = audio_thread(state_clone, rx) {
            eprintln!("Audio thread error: {}", e);
        }
    });

    let mut selected_param = 0usize;

    loop {
        let current_state = *state.lock().unwrap();
        draw_ui(&mut terminal, &current_state, selected_param)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('j') | KeyCode::Down => {
                        selected_param = (selected_param + 1) % 4;
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        selected_param = if selected_param == 0 { 3 } else { selected_param - 1 };
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        let mut s = state.lock().unwrap();
                        match selected_param {
                            0 => s.shape = (s.shape + 0.05).min(1.0),
                            1 => s.sub_lvl = (s.sub_lvl + 0.05).min(1.0),
                            2 => s.fm_amt = (s.fm_amt + 0.05).min(1.0),
                            _ => {}
                        }
                    }
                    KeyCode::Char('h') | KeyCode::Left => {
                        let mut s = state.lock().unwrap();
                        match selected_param {
                            0 => s.shape = (s.shape - 0.05).max(0.0),
                            1 => s.sub_lvl = (s.sub_lvl - 0.05).max(0.0),
                            2 => s.fm_amt = (s.fm_amt - 0.05).max(0.0),
                            _ => {}
                        }
                    }
                    KeyCode::Char('1') => {
                        let mut s = state.lock().unwrap();
                        s.output = 0;
                    }
                    KeyCode::Char('2') => {
                        let mut s = state.lock().unwrap();
                        s.output = 1;
                    }
                    KeyCode::Char('3') => {
                        let mut s = state.lock().unwrap();
                        s.output = 2;
                    }
                    _ => {}
                }
            }
        }
    }

    let _ = tx.send(());
    audio_handle.join().unwrap();

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
