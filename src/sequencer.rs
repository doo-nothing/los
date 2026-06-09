use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{self, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{enable_raw_mode, EnterAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use crate::shm::{AudioEvent, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state::{self, TrackMode, TrackParam};

const NUM_STEPS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Step {
    active: bool,
    note: u8,
    velocity: u8,
    mod_value: f32,
}

impl Default for Step {
    fn default() -> Self {
        Self { active: false, note: 60, velocity: 100, mod_value: 0.0 }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Track {
    steps: Vec<Step>,
    length: usize,
    pulses: usize,
    rotation: usize,
    muted: bool,
    mode: state::TrackMode,
}

impl Track {
    fn new() -> Self {
        let mut steps = vec![Step::default(); NUM_STEPS];
        for i in (0..NUM_STEPS).step_by(4) {
            steps[i].active = true;
        }
        Self { steps, length: 16, pulses: 5, rotation: 0, muted: false, mode: state::TrackMode::Note }
    }
}

#[derive(Clone)]
struct SequencerState {
    tracks: Vec<Track>,
    current_track: usize,
    bpm: f64,
    playing: bool,
    current_steps: Vec<usize>,
    selected: usize,
    last_notes: Vec<Option<u8>>,
    clipboard: Option<Step>,
    track_clipboard: Option<Track>,
}

impl Default for SequencerState {
    fn default() -> Self {
        let track_count = crate::NUM_TRACKS;
        Self {
            tracks: vec![Track::new(); track_count],
            current_track: 0,
            bpm: 120.0,
            playing: true,
            current_steps: vec![0; track_count],
            selected: 0,
            last_notes: vec![None; track_count],
            clipboard: None,
            track_clipboard: None,
        }
    }
}

impl SequencerState {
    fn track(&self) -> &Track {
        &self.tracks[self.current_track]
    }
    fn track_mut(&mut self) -> &mut Track {
        &mut self.tracks[self.current_track]
    }
    /// Move the cursor to a track (and optionally a step), clamping both so
    /// they stay valid after undo/redo changes track count or length.
    fn focus(&mut self, track: usize, step: Option<usize>) {
        self.current_track = track.min(self.tracks.len().saturating_sub(1));
        let len = self.tracks[self.current_track].length;
        self.selected = step.unwrap_or(self.selected).min(len.saturating_sub(1));
    }
}

/// Every undoable user action in the sequencer has a corresponding variant here.
///
/// When adding a new state-modifying action:
/// 1. Add a variant storing old + new state (enough to undo & redo)
/// 2. Implement `undo()` and `redo()` below
/// 3. Add the description to `description()`
/// 4. Call `history.push(Command::YourVariant { ... })` at the action site
///
/// Non-undoable: navigation, clipboard yank, help toggle, save operations.
#[derive(Clone)]
enum Command {
    ToggleStep {
        track: usize,
        step: usize,
        was_active: bool,
    },
    EditStep {
        track: usize,
        step: usize,
        old_step: Step,
        new_step: Step,
    },
    SetTrackParams {
        track: usize,
        old: EuclidState,
        new: EuclidState,
    },
    ToggleMute {
        track: usize,
        was_muted: bool,
    },
    ToggleMode {
        track: usize,
        was_mode: state::TrackMode,
    },
    NewTrack {
        at: usize,
    },
    DeleteTrack {
        at: usize,
        track: Track,
    },
    PasteTrack {
        at: usize,
        track: Track,
    },
    SetBpm {
        old_bpm: f64,
        new_bpm: f64,
    },
}

/// Snapshot of a track's Euclidean params + step pattern, captured before and
/// after an action that rewrites the pattern (P/L/R keys).
#[derive(Debug, Clone, PartialEq)]
struct EuclidState {
    pulses: usize,
    length: usize,
    rotation: usize,
    steps: Vec<Step>,
}

impl EuclidState {
    fn capture(track: &Track) -> Self {
        Self {
            pulses: track.pulses,
            length: track.length,
            rotation: track.rotation,
            steps: track.steps.clone(),
        }
    }

    fn restore(&self, track: &mut Track) {
        track.pulses = self.pulses;
        track.length = self.length;
        track.rotation = self.rotation;
        track.steps = self.steps.clone();
    }
}

/// Insert a track at `at`, keeping the per-track bookkeeping vectors in sync.
fn insert_track(state: &mut SequencerState, at: usize, track: Track) {
    let at = at.min(state.tracks.len());
    state.tracks.insert(at, track);
    state.current_steps.insert(at, 0);
    state.last_notes.insert(at, None);
    state.focus(at, Some(0));
}

/// Remove the track at `at`, keeping the per-track bookkeeping vectors in
/// sync. Never removes the last remaining track.
fn remove_track(state: &mut SequencerState, at: usize) {
    if state.tracks.len() > 1 && at < state.tracks.len() {
        state.tracks.remove(at);
        state.current_steps.remove(at);
        state.last_notes.remove(at);
        state.focus(state.current_track, Some(0));
    }
}

impl Command {
    fn description(&self) -> &'static str {
        match self {
            Command::ToggleStep { .. } => "Toggle step",
            Command::EditStep { .. } => "Edit step",
            Command::SetTrackParams { .. } => "Set track params",
            Command::ToggleMute { .. } => "Toggle mute",
            Command::ToggleMode { .. } => "Toggle mode",
            Command::NewTrack { .. } => "New track",
            Command::DeleteTrack { .. } => "Delete track",
            Command::PasteTrack { .. } => "Paste track",
            Command::SetBpm { .. } => "Set BPM",
        }
    }

    fn undo(&self, state: &mut SequencerState) {
        match self {
            Command::ToggleStep { track, step, was_active } => {
                if *track < state.tracks.len() && *step < state.tracks[*track].steps.len() {
                    state.tracks[*track].steps[*step].active = *was_active;
                    state.focus(*track, Some(*step));
                }
            }
            Command::EditStep { track, step, old_step, .. } => {
                if *track < state.tracks.len() && *step < state.tracks[*track].steps.len() {
                    state.tracks[*track].steps[*step] = *old_step;
                    state.focus(*track, Some(*step));
                }
            }
            Command::SetTrackParams { track, old, .. } => {
                if *track < state.tracks.len() {
                    old.restore(&mut state.tracks[*track]);
                    state.focus(*track, None);
                }
            }
            Command::ToggleMute { track, was_muted } => {
                if *track < state.tracks.len() {
                    state.tracks[*track].muted = *was_muted;
                    state.focus(*track, None);
                }
            }
            Command::ToggleMode { track, was_mode } => {
                if *track < state.tracks.len() {
                    state.tracks[*track].mode = *was_mode;
                    state.focus(*track, None);
                }
            }
            Command::NewTrack { at } => remove_track(state, *at),
            Command::DeleteTrack { at, track: saved_track } => {
                insert_track(state, *at, saved_track.clone());
            }
            Command::PasteTrack { at, .. } => remove_track(state, *at),
            Command::SetBpm { old_bpm, .. } => {
                state.bpm = *old_bpm;
            }
        }
    }

    fn redo(&self, state: &mut SequencerState) {
        match self {
            Command::ToggleStep { track, step, was_active } => {
                if *track < state.tracks.len() && *step < state.tracks[*track].steps.len() {
                    state.tracks[*track].steps[*step].active = !*was_active;
                    state.focus(*track, Some(*step));
                }
            }
            Command::EditStep { track, step, new_step, .. } => {
                if *track < state.tracks.len() && *step < state.tracks[*track].steps.len() {
                    state.tracks[*track].steps[*step] = *new_step;
                    state.focus(*track, Some(*step));
                }
            }
            Command::SetTrackParams { track, new, .. } => {
                if *track < state.tracks.len() {
                    new.restore(&mut state.tracks[*track]);
                    state.focus(*track, None);
                }
            }
            Command::ToggleMute { track, was_muted } => {
                if *track < state.tracks.len() {
                    state.tracks[*track].muted = !*was_muted;
                    state.focus(*track, None);
                }
            }
            Command::ToggleMode { track, was_mode } => {
                if *track < state.tracks.len() {
                    state.tracks[*track].mode = match was_mode {
                        state::TrackMode::Note => state::TrackMode::Modulation,
                        state::TrackMode::Modulation => state::TrackMode::Note,
                    };
                    state.focus(*track, None);
                }
            }
            Command::NewTrack { at } => insert_track(state, *at, Track::new()),
            Command::DeleteTrack { at, .. } => remove_track(state, *at),
            Command::PasteTrack { at, track } => insert_track(state, *at, track.clone()),
            Command::SetBpm { new_bpm, .. } => {
                state.bpm = *new_bpm;
            }
        }
    }
}

const HISTORY_CAP: usize = 100;

struct History {
    commands: Vec<Command>,
    index: usize,
}

impl History {
    fn new() -> Self {
        Self { commands: vec![], index: 0 }
    }

    fn push(&mut self, cmd: Command) {
        self.commands.truncate(self.index);
        self.commands.push(cmd);
        if self.commands.len() > HISTORY_CAP {
            self.commands.remove(0);
        }
        self.index = self.commands.len();
    }

    fn undo(&mut self, state: &mut SequencerState) -> Option<&'static str> {
        if self.index == 0 { return None; }
        self.index -= 1;
        self.commands[self.index].undo(state);
        Some(self.commands[self.index].description())
    }

    fn redo(&mut self, state: &mut SequencerState) -> Option<&'static str> {
        if self.index >= self.commands.len() { return None; }
        self.commands[self.index].redo(state);
        self.index += 1;
        Some(self.commands[self.index - 1].description())
    }
}

/// Run an undo or redo op up to `count` times, stopping early when history
/// runs out. Returns the status-bar message ("Undo: ...", "Undo ×3: ...", or
/// "Nothing to undo").
fn history_status(label: &str, count: usize, mut op: impl FnMut() -> Option<&'static str>) -> String {
    let mut done = 0;
    let mut last_desc = "";
    while done < count {
        match op() {
            Some(desc) => {
                last_desc = desc;
                done += 1;
            }
            None => break,
        }
    }
    match done {
        0 => format!("Nothing to {}", label.to_lowercase()),
        1 => format!("{}: {}", label, last_desc),
        n => format!("{} ×{}: {}", label, n, last_desc),
    }
}

/// Record a step edit, skipping no-ops so `u` always reverts a visible change.
fn push_step_edit(history: &mut History, track: usize, step: usize, old_step: Step, new_step: Step) {
    if old_step != new_step {
        history.push(Command::EditStep { track, step, old_step, new_step });
    }
}

/// Record a track-params change, skipping no-ops.
fn push_track_params(history: &mut History, track: usize, old: EuclidState, new: EuclidState) {
    if old != new {
        history.push(Command::SetTrackParams { track, old, new });
    }
}

fn sequencer_thread(
    state: Arc<Mutex<SequencerState>>,
    shutdown: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let sample_rate = 48000.0;

    let mut events = match EventRingbuf::open_producer() {
        Ok(e) => e,
        Err(_) => EventRingbuf::create()?,
    };

    let mut modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();

    let mut transport = match ShmTransport::open() {
        Ok(t) => t,
        Err(_) => ShmTransport::create(sample_rate as u32)?,
    };

    // Seed the global play flag from loaded/default state once; from here on
    // the SHM flag is the source of truth (any module or `los ctl` may flip it).
    {
        let s = state.lock().unwrap();
        transport.set_playing(s.playing);
    }

    let mut last_steps: Vec<i32> = vec![-1];

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        if modbus.is_none() {
            modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();
        }

        // The SHM transport play flag is the source of truth; s.playing is a
        // mirror kept for the UI and save files.
        let playing = transport.playing();
        let bpm = {
            let mut s = state.lock().unwrap();
            s.playing = playing;
            s.bpm
        };

        let clock = transport.clock();
        let samples_per_step = (60.0 / bpm * sample_rate / 4.0) as u64;

        // Scope the lock so it's not held during sleep
        {
            let mut s = state.lock().unwrap();
            
            // Grow tracking vectors as tracks are added
            while last_steps.len() < s.tracks.len() {
                last_steps.push(-1);
            }
            while s.current_steps.len() < s.tracks.len() {
                s.current_steps.push(0);
            }
            while s.last_notes.len() < s.tracks.len() {
                s.last_notes.push(None);
            }

            for (t, last_step) in last_steps.iter_mut().enumerate().take(s.tracks.len()) {
                let len = s.tracks[t].length;
                let current_step = if samples_per_step > 0 && playing {
                    (clock / samples_per_step) as usize % len
                } else {
                    (*last_step).max(0) as usize
                };

                if current_step as i32 != *last_step {
                    s.current_steps[t] = current_step;

                    if playing && !s.tracks[t].muted {
                        let track = &s.tracks[t];
                        let step = &track.steps[current_step];

                        // Always write step value to modbus per-track channel
                        let mod_val = match track.mode {
                            TrackMode::Note => {
                                if step.active {
                                    step.velocity as f32 / 127.0
                                } else {
                                    0.0
                                }
                            }
                            TrackMode::Modulation => step.mod_value,
                        };
                        if let Some(ref mut bus) = modbus {
                            bus.set(8 + t, mod_val);
                        }

                        // Note mode: send note-on/note-off events
                        if track.mode == TrackMode::Note {
                            if let Some(n) = s.last_notes[t] {
                                let _ = events.write_event(&AudioEvent::note_off_source(n, t as u8, *last_step as u32));
                            }
                            if step.active {
                                let _ = events.write_event(&AudioEvent::note_on_source(step.note, step.velocity, t as u8, current_step as u32));
                                s.last_notes[t] = Some(step.note);
                            } else {
                                s.last_notes[t] = None;
                            }
                        }
                    }
                    *last_step = current_step as i32;
                }
            }
        } // lock released here, before sleep

        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(())
}

fn midi_note_name(note: u8) -> String {
    let names = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    let octave = (note / 12).saturating_sub(1);
    let idx = (note % 12) as usize;
    format!("{}{}", names[idx], octave)
}

fn euclidean_apply(steps: &mut [Step], pulses: usize, length: usize, rotation: usize) {
    let len = length.min(steps.len());
    let mut pattern = vec![false; len];
    if pulses > 0 && pulses <= len {
        let mut bucket = 0usize;
        for pat in pattern.iter_mut().take(len) {
            bucket += pulses;
            if bucket >= len {
                bucket -= len;
                *pat = true;
            }
        }
    }
    let rot = rotation % len;
    for (i, step) in steps.iter_mut().enumerate().take(len) {
        let src = (i + len - rot) % len;
        step.active = pattern[src];
    }
    // Deactivate any steps beyond the set length
    for step in steps.iter_mut().skip(len) {
        step.active = false;
    }
}

fn compact_track_row(track: &Track, track_idx: usize, current_track: usize, selected: usize) -> String {
    let is_current = track_idx == current_track;
    let mut row = format!("{}T{} ", if is_current { "▶" } else { " " }, track_idx + 1);
    if track.muted {
        row.push_str("[mute]   ");
    } else {
        let mode_str = match track.mode {
            state::TrackMode::Note => "[NOTE]",
            state::TrackMode::Modulation => "[MOD] ",
        };
        row.push_str(&format!("{:<8} ", mode_str));
    }
    for (i, step) in track.steps.iter().enumerate().take(track.length) {
        let ch = if i == selected && is_current {
            '▽'
        } else if step.active {
            '●'
        } else {
            '○'
        };
        row.push(ch);
    }
    // Pad to 16 chars if shorter
    for _ in track.length..16 {
        row.push(' ');
    }
    row.push_str(&format!("  P:{} L:{} R:{}", track.pulses, track.length, track.rotation));
    row
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &SequencerState,
    mode: &str,
    submode: &str,
    input_buffer: &str,
    pending_count: &Option<String>,
    gt_target: &Option<String>,
    gt_input: &str,
    show_help: bool,
    undo_msg: &Option<String>,
) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        let track_rows = state.tracks.len().clamp(1, 6) as u16;
        
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(track_rows), // Stacked track list
                Constraint::Length(3),            // Step grid
                Constraint::Length(1),            // Note display
                Constraint::Min(0),               // Help or empty
                Constraint::Length(1),            // Status line
            ])
            .split(area);

        // Stacked track rows
        for (ti, trk) in state.tracks.iter().enumerate() {
            let sel = if ti == state.current_track { state.selected } else { 0 };
            let row_text = compact_track_row(trk, ti, state.current_track, sel);
            let style = if ti == state.current_track {
                Style::default().fg(Color::Yellow)
            } else if trk.muted {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Cyan)
            };
            let row_widget = Paragraph::new(row_text).style(style);
            if ti < chunks[0].height as usize {
                let row_rect = Rect::new(chunks[0].x, chunks[0].y + ti as u16, chunks[0].width, 1);
                f.render_widget(row_widget, row_rect);
            }
        }

        let len = state.track().length;
        let play_str = if state.playing { "▶" } else { "■" };
        let cstep = state.current_steps[state.current_track];
        
        let step_width = area.width as usize / len;
        let step_width = step_width.clamp(3, 10);
        
        for i in 0..len {
            let step = &state.track().steps[i];
            let is_current = i == cstep;
            let is_selected = i == state.selected;

            let x = (i * step_width) as u16;
            let rect = Rect::new(x, chunks[1].y, step_width as u16, 3);

            let (bg, fg, marker) = if is_current && is_selected {
                (Color::Yellow, Color::Black, "▶")
            } else if is_current {
                (Color::Green, Color::Black, "▶")
            } else if is_selected {
                (Color::Blue, Color::White, "*")
            } else if step.active {
                (Color::DarkGray, Color::White, " ")
            } else {
                (Color::Reset, Color::DarkGray, " ")
            };

            let third_line = match state.track().mode {
                state::TrackMode::Note => midi_note_name(step.note),
                state::TrackMode::Modulation => format!("{:+.2}", step.mod_value),
            };
            let step_text = format!(
                "{}{}\n{}\n{}",
                marker,
                i,
                if step.active { "●" } else { "○" },
                third_line,
            );

            let step_widget = Paragraph::new(step_text)
                .style(Style::default().fg(fg).bg(bg))
                .block(Block::default().borders(Borders::NONE));
            f.render_widget(step_widget, rect);
        }

        let sel_step = &state.track().steps[state.selected];
        let note_info = match state.track().mode {
            state::TrackMode::Note => format!(
                "Step {}: {} | Note: {} ({}) | Vel: {} | {}",
                state.selected,
                if sel_step.active { "ON" } else { "OFF" },
                sel_step.note,
                midi_note_name(sel_step.note),
                sel_step.velocity,
                if !input_buffer.is_empty() { format!("Input: {}", input_buffer) } else { String::new() }
            ),
            state::TrackMode::Modulation => format!(
                "Step {}: {} | Value: {:+.2} | {}",
                state.selected,
                if sel_step.active { "ON" } else { "OFF" },
                sel_step.mod_value,
                if !input_buffer.is_empty() { format!("Input: {}", input_buffer) } else { String::new() }
            ),
        };
        let note_widget = Paragraph::new(note_info)
            .style(Style::default().fg(Color::Yellow));
        f.render_widget(note_widget, chunks[2]);

        // Bottom status bar
        let mode_label = if mode == "insert" {
            if !submode.is_empty() {
                format!(" INSERT[{}] ", submode)
            } else {
                " INSERT ".to_string()
            }
        } else {
            " NORMAL ".to_string()
        };
        let mode_str = match state.track().mode {
            state::TrackMode::Note => "NOTE",
            state::TrackMode::Modulation => "MOD",
        };
        let mut status_parts = vec![
            mode_label,
            format!("T{}/{}", state.current_track + 1, state.tracks.len()),
            mode_str.to_string(),
            format!("{} {} BPM", play_str, state.bpm as u32),
            format!("Step {}/{}", cstep, len),
            format!("Sel {}", state.selected),
            format!("P:{} L:{} R:{}", state.track().pulses, len, state.track().rotation),
        ];
        if let Some(ref count) = pending_count {
            status_parts.push(format!("Count:{}", count));
        }
        if !submode.is_empty() {
            status_parts.push(format!("{}:{}", submode.to_uppercase(), input_buffer));
        }
        if let Some(ref target) = gt_target {
            status_parts.push(format!("gt{}:{}", target, gt_input));
        }
        if let Some(ref msg) = undo_msg {
            status_parts.push(msg.clone());
        }
        let status = status_parts.join(" | ");
        let status_style = if mode == "insert" {
            Style::default().fg(Color::Green).bg(Color::Black)
        } else {
            Style::default().fg(Color::Cyan).bg(Color::Black)
        };
        let status_widget = Paragraph::new(status).style(status_style);
        f.render_widget(status_widget, chunks[4]);

        if show_help {
            let help_text = vec![
                Line::from("━━━ Sequencer Help ━━━"),
                Line::from(""),
                Line::from("Navigation (both modes):"),
                Line::from("  h/l, ←/→  Move left/right"),
                Line::from("  gg         First step"),
                Line::from("  G          Last step"),
                Line::from("  w          Next active step"),
                Line::from("  b          Previous active step"),
                Line::from("  [ / ]      Previous/next track"),
                Line::from("  #j/#k      Jump # tracks down/up"),
                Line::from("  1..9       Jump to track"),
                Line::from(""),
                Line::from("Normal mode:"),
                Line::from("  Enter/i    Enter insert mode"),
                Line::from("  n          New track"),
                Line::from("  dd         Delete track"),
                Line::from("  yy         Yank track"),
                Line::from("  P          Paste track after current"),
                Line::from("  m          Toggle mute"),
                Line::from("  gt#        Go to track #"),
                Line::from("  space      Play/pause"),
                Line::from("  s          Stop"),
                Line::from("  u / #u     Undo (# times)"),
                Line::from("  ^r / #^r   Redo (# times)"),
                Line::from("  t<NUM>     Set BPM"),
                Line::from(""),
                Line::from("Insert mode:"),
                Line::from("  Esc        Return to normal"),
                Line::from("  Enter/space Toggle step"),
                Line::from("  x          Cut step"),
                Line::from("  y          Yank step"),
                Line::from("  p          Paste step"),
                Line::from("  k/K        Raise note (st/oct)"),
                Line::from("  j/J        Lower note (st/oct)"),
                Line::from("  N<NUM>     Set note (e.g. N60)"),
                Line::from("  gt#        Go to step #"),
                Line::from("  <N>P/L/R   Euclidean pulses/length/rotation"),
                Line::from("  R          Rotate by 1"),
                Line::from(""),
                Line::from("  ?          Toggle this help"),
                Line::from("  Close pane: tmux prefix + x"),
            ];
            let help = Paragraph::new(help_text)
                .style(Style::default().fg(Color::White).bg(Color::Black))
                .block(Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title("Help"));
            f.render_widget(help, area);
        }
    })?;

    Ok(())
}

pub fn run(instance: usize) -> Result<()> {
    // Setup SIGUSR1 handler for save-on-signal
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("sequencer", instance);
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let _ = manifest.register("sequencer", instance, None);
    
    for attempt in 0..20 {
        match enable_raw_mode() {
            Ok(()) => break,
            Err(e) => {
                if attempt < 19 {
                    std::thread::sleep(Duration::from_millis(200));
                } else {
                    return Err(anyhow::anyhow!("Failed to enable raw mode after 20 attempts: {}", e));
                }
            }
        }
    }
    
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let state = Arc::new(Mutex::new(SequencerState::default()));
    
    // Load saved state if available
    if let Ok(params) = state::load_module_state::<state::SequencerParams>("sequencer", instance) {
        let mut s = state.lock().unwrap();
        if let Some(bpm) = params.bpm { s.bpm = bpm; }
        if let Some(playing) = params.playing { s.playing = playing; }
        if !params.tracks.is_empty() {
            // Rebuild tracks from saved state exactly — clear defaults first
            s.tracks.clear();
            s.current_steps.clear();
            s.last_notes.clear();
            for tp in &params.tracks {
                let mut steps = vec![Step::default(); NUM_STEPS];
                for (i, step) in tp.steps.iter().enumerate().take(steps.len()) {
                    steps[i] = Step { active: step.active, note: step.note, velocity: step.velocity, mod_value: step.mod_value };
                }
                s.tracks.push(Track {
                    steps,
                    length: tp.length.unwrap_or(16),
                    pulses: tp.pulses.unwrap_or(5),
                    rotation: tp.rotation.unwrap_or(0),
                    muted: tp.muted,
                    mode: tp.mode,
                });
                s.current_steps.push(0);
                s.last_notes.push(None);
            }
        } else {
            // Fallback: load flat fields into first track
            let trk = &mut s.tracks[0];
            if let Some(p) = params.euclidean_pulses { trk.pulses = p; }
            if let Some(l) = params.euclidean_length { trk.length = l; }
            if let Some(r) = params.euclidean_rotation { trk.rotation = r; }
            for (i, step) in params.steps.iter().enumerate().take(trk.steps.len()) {
                trk.steps[i] = Step { active: step.active, note: step.note, velocity: step.velocity, mod_value: step.mod_value };
            }
        }
    }
    
    let state_clone = Arc::clone(&state);

    let (_tx, rx) = std::sync::mpsc::channel();

    let _seq_handle = std::thread::spawn(move || {
        if let Err(e) = sequencer_thread(state_clone, rx) {
            eprintln!("Sequencer thread error: {}", e);
        }
    });

    // UI-side transport handle for play/stop keys (lazily reopened if the
    // transport doesn't exist yet when a key is first pressed)
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();

    let mut mode = String::from("normal");
    let mut submode = String::new();
    let mut input_buffer = String::new();
    let mut show_help = false;
    let mut pending_count: Option<String> = None;
    let mut pending_d = false;
    let mut pending_y = false;
    let mut pending_g = false;
    let mut gt_target: Option<String> = None;
    let mut gt_input = String::new();
    let mut gt_last_key: Option<Instant> = None;
    let mut history = History::new();
    let mut undo_msg: Option<String> = None;
    let mut undo_time: Option<Instant> = None;

    loop {
        
        // Check for save-on-signal
        if state::check_save_signal() {
            let s = state.lock().unwrap();
            let params = state::SequencerParams {
                bpm: Some(s.bpm),
                playing: Some(s.playing),
                euclidean_pulses: None,
                euclidean_length: None,
                euclidean_rotation: None,
                steps: vec![],
                tracks: s.tracks.iter().map(|trk| state::TrackParam {
                    steps: trk.steps.iter().map(|step| state::StepParam {
                        active: step.active,
                        note: step.note,
                        velocity: step.velocity,
                        mod_value: step.mod_value,
                    }).collect(),
                    length: Some(trk.length),
                    pulses: Some(trk.pulses),
                    rotation: Some(trk.rotation),
                    muted: trk.muted,
                    mode: trk.mode,
                }).collect(),
            };
            drop(s);
            let _ = state::save_module_state("sequencer", instance, &params);
        }
        
        // Check for reload-on-signal
        if state::check_reload_signal() {
            if let Ok(params) = state::load_module_state::<state::SequencerParams>("sequencer", instance) {
                let mut s = state.lock().unwrap();
                if let Some(bpm) = params.bpm { s.bpm = bpm; }
                if let Some(playing) = params.playing {
                    s.playing = playing;
                    if transport_ui.is_none() {
                        transport_ui = ShmTransport::open().ok();
                    }
                    if let Some(ref mut t) = transport_ui {
                        t.set_playing(playing);
                    }
                }
                if !params.tracks.is_empty() {
                    s.tracks.clear();
                    s.current_steps.clear();
                    s.last_notes.clear();
                    for tp in &params.tracks {
                        let mut steps = vec![Step::default(); NUM_STEPS];
                        for (i, step) in tp.steps.iter().enumerate().take(steps.len()) {
                            steps[i] = Step { active: step.active, note: step.note, velocity: step.velocity, mod_value: step.mod_value };
                        }
                        s.tracks.push(Track {
                            steps,
                            length: tp.length.unwrap_or(16),
                            pulses: tp.pulses.unwrap_or(5),
                            rotation: tp.rotation.unwrap_or(0),
                            muted: tp.muted,
                            mode: tp.mode,
                        });
                        s.current_steps.push(0);
                        s.last_notes.push(None);
                    }
                    s.current_track = s.current_track.min(s.tracks.len().saturating_sub(1));
                }
            }
        }
        
        // Auto-execute gt on timeout (only when digits were collected)
        if let Some(ref target) = gt_target {
            if gt_last_key.as_ref().is_some_and(|t| t.elapsed() > Duration::from_millis(300)) && !gt_input.is_empty() {
                let mut s = state.lock().unwrap();
                match target.as_str() {
                    "track" => {
                        if let Ok(tnum) = gt_input.parse::<usize>() {
                            let tidx = tnum.saturating_sub(1).min(s.tracks.len().saturating_sub(1));
                            s.current_track = tidx;
                            s.selected = 0;
                        }
                    }
                    "step" => {
                        if let Ok(step) = gt_input.parse::<usize>() {
                            let len = s.track_mut().length;
                            s.selected = step.min(len - 1);
                        }
                    }
                    _ => {}
                }
                gt_target = None;
                gt_input.clear();
                gt_last_key = None;
            }
        }

        // Clear undo message after 2 seconds
        if let Some(t) = undo_time { if t.elapsed() > Duration::from_secs(2) { undo_msg = None; undo_time = None; } }
        let current_state = state.lock().unwrap().clone();
        draw_ui(&mut terminal, &current_state, &mode, &submode, &input_buffer, &pending_count, &gt_target, &gt_input, show_help, &undo_msg)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Undo/redo status message clears on the next keypress
                // (the undo/redo handlers below set a fresh one)
                undo_msg = None;
                undo_time = None;

                // Ctrl-s: save module state
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let s = state.lock().unwrap();
                    let params = state::SequencerParams {
                        bpm: Some(s.bpm),
                        playing: Some(s.playing),
                        euclidean_pulses: None,
                        euclidean_length: None,
                        euclidean_rotation: None,
                        steps: vec![],
                        tracks: s.tracks.iter().map(|trk| TrackParam {
                            steps: trk.steps.iter().map(|step| state::StepParam {
                                active: step.active,
                                note: step.note,
                                velocity: step.velocity,
                                mod_value: step.mod_value,
                            }).collect(),
                            length: Some(trk.length),
                            pulses: Some(trk.pulses),
                            rotation: Some(trk.rotation),
                            muted: trk.muted,
                            mode: trk.mode,
                        }).collect(),
                    };
                    drop(s);
                    let _ = state::save_module_state("sequencer", 0, &params);
                    continue;
                }

                // Ctrl-r: redo (count-prefixed: 3<C-r> redoes 3 times)
                if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
                    let count = pending_count.take().and_then(|c| c.parse().ok()).unwrap_or(1).max(1);
                    pending_d = false;
                    pending_y = false;
                    pending_g = false;
                    gt_target = None;
                    gt_input.clear();
                    gt_last_key = None;
                    submode.clear();
                    input_buffer.clear();
                    let mut s = state.lock().unwrap();
                    undo_msg = Some(history_status("Redo", count, || history.redo(&mut s)));
                    undo_time = Some(Instant::now());
                    continue;
                }

                // gt<#> inline collection (mode-independent)
                if let Some(ref target) = gt_target {
                    match key.code {
                        KeyCode::Char(c) if c.is_ascii_digit() => {
                            gt_input.push(c);
                            gt_last_key = Some(Instant::now());
                            continue;
                        }
                        KeyCode::Enter => {
                            if !gt_input.is_empty() {
                                let mut s = state.lock().unwrap();
                                match target.as_str() {
                                    "track" => {
                                        if let Ok(tnum) = gt_input.parse::<usize>() {
                                            let tidx = tnum.saturating_sub(1).min(s.tracks.len().saturating_sub(1));
                                            s.current_track = tidx;
                                            s.selected = 0;
                                        }
                                    }
                                    "step" => {
                                        if let Ok(step) = gt_input.parse::<usize>() {
                                            let len = s.track_mut().length;
                                            s.selected = step.min(len - 1);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            gt_target = None;
                            gt_input.clear();
                            gt_last_key = None;
                            continue;
                        }
                        _ => {
                            // Execute pending gt before handling this key
                            if !gt_input.is_empty() {
                                let mut s = state.lock().unwrap();
                                match target.as_str() {
                                    "track" => {
                                        if let Ok(tnum) = gt_input.parse::<usize>() {
                                            let tidx = tnum.saturating_sub(1).min(s.tracks.len().saturating_sub(1));
                                            s.current_track = tidx;
                                            s.selected = 0;
                                        }
                                    }
                                    "step" => {
                                        if let Ok(step) = gt_input.parse::<usize>() {
                                            let len = s.track_mut().length;
                                            s.selected = step.min(len - 1);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            gt_target = None;
                            gt_input.clear();
                            gt_last_key = None;
                            // Fall through to normal key processing
                        }
                    }
                }

                // Input submode handling (mode-independent)
                if !submode.is_empty() {
                    match key.code {
                        KeyCode::Enter => {
                            let mut s = state.lock().unwrap();
                            match submode.as_str() {
                                "note" => {
                                    if let Ok(note) = input_buffer.parse::<u8>() {
                                        let sel = s.selected;
                                        let tidx = s.current_track;
                                        let old_step = s.tracks[tidx].steps[sel];
                                        s.tracks[tidx].steps[sel].note = note.clamp(0, 127);
                                        let new_step = s.tracks[tidx].steps[sel];
                                        push_step_edit(&mut history, tidx, sel, old_step, new_step);
                                    }
                                }
                                "bpm" => {
                                    if let Ok(bpm) = input_buffer.parse::<f64>() {
                                        let old_bpm = s.bpm;
                                        s.bpm = bpm.clamp(20.0, 300.0);
                                        if old_bpm != s.bpm {
                                            history.push(Command::SetBpm { old_bpm, new_bpm: s.bpm });
                                        }
                                    }
                                }
                                "track" => {
                                    if let Ok(tnum) = input_buffer.parse::<usize>() {
                                        let tidx = tnum.saturating_sub(1).min(s.tracks.len().saturating_sub(1));
                                        s.current_track = tidx;
                                        s.selected = 0;
                                    }
                                }
                                "step" => {
                                    if let Ok(step) = input_buffer.parse::<usize>() {
                                        let len = s.track_mut().length;
                                        s.selected = step.min(len - 1);
                                    }
                                }
                                _ => {}
                            }
                            submode.clear();
                            input_buffer.clear();
                        }
                        KeyCode::Char(c) if c.is_ascii_digit() || c == '.' => {
                            input_buffer.push(c);
                        }
                        _ => {
                            submode.clear();
                            input_buffer.clear();
                        }
                    }
                    continue;
                }

                // Help toggle (mode-independent)
                if key.code == KeyCode::Char('?') {
                    pending_count = None;
                    pending_d = false;
                    pending_y = false;
                    show_help = !show_help;
                    continue;
                }

                // Mode switch: Esc in insert → normal
                if key.code == KeyCode::Esc && mode == "insert" {
                    mode = String::from("normal");
                    submode.clear();
                    input_buffer.clear();
                    pending_count = None;
                    continue;
                }

                // Digits accumulate into pending_count (both modes)
                if let KeyCode::Char(c) = key.code {
                    if c.is_ascii_digit() {
                        if c == '0' && pending_count.is_none() {
                            let mut s = state.lock().unwrap();
                            s.selected = 0;
                        } else {
                            pending_count.get_or_insert(String::new()).push(c);
                        }
                        pending_d = false;
                        pending_y = false;
                        continue;
                    }
                }

                // Mode-independent navigation
                match key.code {
                    KeyCode::Char('l') | KeyCode::Right if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        s.selected = (s.selected + count) % len;
                        continue;
                    }
                    KeyCode::Char('h') | KeyCode::Left if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        s.selected = s.selected.saturating_sub(count).min(len - 1);
                        continue;
                    }
                    KeyCode::Char('w') if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        for _ in 0..count {
                            for i in 1..=len {
                                let idx = (s.selected + i) % len;
                                if s.track_mut().steps[idx].active {
                                    s.selected = idx;
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                    KeyCode::Char('b') if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        for _ in 0..count {
                            for i in 1..=len {
                                let idx = (s.selected + len - i) % len;
                                if s.track_mut().steps[idx].active {
                                    s.selected = idx;
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        pending_count = None;
                        pending_d = false;
                        pending_y = false;
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        s.selected = (s.selected + 1) % len;
                        continue;
                    }
                    KeyCode::Char('h') | KeyCode::Left => {
                        pending_count = None;
                        pending_d = false;
                        pending_y = false;
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        s.selected = s.selected.saturating_sub(1).min(len - 1);
                        continue;
                    }
                    KeyCode::Char('w') => {
                        pending_count = None;
                        pending_d = false;
                        pending_y = false;
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        for i in 1..=len {
                            let idx = (s.selected + i) % len;
                            if s.track_mut().steps[idx].active {
                                s.selected = idx;
                                break;
                            }
                        }
                        continue;
                    }
                    KeyCode::Char('b') => {
                        pending_count = None;
                        pending_d = false;
                        pending_y = false;
                        let mut s = state.lock().unwrap();
                        let len = s.track_mut().length;
                        for i in 1..=len {
                            let idx = (s.selected + len - i) % len;
                            if s.track_mut().steps[idx].active {
                                s.selected = idx;
                                break;
                            }
                        }
                        continue;
                    }
                    KeyCode::Char('0') => {
                        pending_count = None;
                        pending_d = false;
                        pending_y = false;
                        let mut s = state.lock().unwrap();
                        s.selected = 0;
                        continue;
                    }
                    KeyCode::Char('$') => {
                        pending_count = None;
                        pending_d = false;
                        pending_y = false;
                        let mut s = state.lock().unwrap();
                        s.selected = s.track_mut().length - 1;
                        continue;
                    }
                    _ => {}
                }

                // Count-prefixed P, L, R (both modes)
                match key.code {
                    KeyCode::Char('P') if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(0);
                        let mut s = state.lock().unwrap();
                        let tidx = s.current_track;
                        let old = EuclidState::capture(&s.tracks[tidx]);
                        if count > 0 { s.tracks[tidx].pulses = count.min(16); }
                        let (p, l, r) = (s.tracks[tidx].pulses, s.tracks[tidx].length, s.tracks[tidx].rotation);
                        euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                        let new = EuclidState::capture(&s.tracks[tidx]);
                        push_track_params(&mut history, tidx, old, new);
                        pending_d = false; pending_y = false;
                        continue;
                    }
                    KeyCode::Char('L') if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(0);
                        let mut s = state.lock().unwrap();
                        let tidx = s.current_track;
                        let old = EuclidState::capture(&s.tracks[tidx]);
                        if count > 0 { s.tracks[tidx].length = count.clamp(1, 16); }
                        s.selected = s.selected.min(s.tracks[tidx].length - 1);
                        let (p, l, r) = (s.tracks[tidx].pulses, s.tracks[tidx].length, s.tracks[tidx].rotation);
                        euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                        let new = EuclidState::capture(&s.tracks[tidx]);
                        push_track_params(&mut history, tidx, old, new);
                        pending_d = false; pending_y = false;
                        continue;
                    }
                    KeyCode::Char('R') if pending_count.is_some() => {
                        let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(0);
                        let mut s = state.lock().unwrap();
                        let tidx = s.current_track;
                        let old = EuclidState::capture(&s.tracks[tidx]);
                        if count > 0 {
                            s.tracks[tidx].rotation = count.min(255);
                        } else {
                            s.tracks[tidx].rotation = (s.tracks[tidx].rotation + 1) % s.tracks[tidx].length;
                        }
                        let (p, l, r) = (s.tracks[tidx].pulses, s.tracks[tidx].length, s.tracks[tidx].rotation);
                        euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                        let new = EuclidState::capture(&s.tracks[tidx]);
                        push_track_params(&mut history, tidx, old, new);
                        pending_d = false; pending_y = false;
                        continue;
                    }
                    _ => {}
                }

                // Count-prefixed track jump (both modes)
                if let KeyCode::Char(c) = key.code {
                    if c.is_ascii_digit() && pending_count.is_some() {
                        // Already handled above
                    }
                }

                if mode == "normal" {
                    match key.code {
                        // Enter insert mode
                        KeyCode::Enter | KeyCode::Char('i') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            mode = String::from("insert");
                        }

                        // Count-prefixed track jump
                        KeyCode::Char(c) if c.is_ascii_digit() && pending_count.is_some() => {
                            let count: usize = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            let tidx = count.saturating_sub(1).min(s.tracks.len().saturating_sub(1));
                            s.current_track = tidx;
                            s.selected = 0;
                            pending_g = false;
                        }

                        // Track ops
                        KeyCode::Char('n') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            s.tracks.push(Track::new());
                            s.current_steps.push(0);
                            s.last_notes.push(None);
                            s.current_track = s.tracks.len() - 1;
                            s.selected = 0;
                            history.push(Command::NewTrack { at: s.current_track });
                        }
                        KeyCode::Char('d') => {
                            pending_count = None;
                            pending_g = false;
                            if pending_d {
                                pending_d = false;
                                let mut s = state.lock().unwrap();
                                if s.tracks.len() > 1 {
                                    let was = s.current_track;
                                    let track = s.tracks.remove(was);
                                    s.track_clipboard = Some(track.clone());
                                    s.current_steps.remove(was);
                                    s.last_notes.remove(was);
                                    if s.current_track >= s.tracks.len() {
                                        s.current_track = s.tracks.len() - 1;
                                    }
                                    s.selected = 0;
                                    history.push(Command::DeleteTrack { at: was, track });
                                }
                            } else {
                                pending_d = true;
                                pending_y = false;
                            }
                        }
                        KeyCode::Char('y') => {
                            pending_count = None;
                            pending_g = false;
                            if pending_y {
                                pending_y = false;
                                let mut s = state.lock().unwrap();
                                s.track_clipboard = Some(s.tracks[s.current_track].clone());
                            } else {
                                pending_y = true;
                                pending_d = false;
                            }
                        }
                        KeyCode::Char('P') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            let clip = s.track_clipboard.clone();
                            if let Some(track) = clip {
                                let at = s.current_track + 1;
                                s.tracks.insert(at, track.clone());
                                s.current_steps.insert(at, 0);
                                s.last_notes.insert(at, None);
                                s.current_track = at;
                                s.selected = 0;
                                history.push(Command::PasteTrack { at, track });
                            }
                        }
                        KeyCode::Char('m') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            let tidx = s.current_track;
                            let was_muted = s.tracks[tidx].muted;
                            s.tracks[tidx].muted = !was_muted;
                            history.push(Command::ToggleMute { track: tidx, was_muted });
                        }
                        KeyCode::Char('@') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            let tidx = s.current_track;
                            let was_mode = s.tracks[tidx].mode;
                            s.tracks[tidx].mode = match was_mode {
                                state::TrackMode::Note => state::TrackMode::Modulation,
                                state::TrackMode::Modulation => state::TrackMode::Note,
                            };
                            history.push(Command::ToggleMode { track: tidx, was_mode });
                        }
                        KeyCode::Char('k') | KeyCode::Up if pending_count.is_some() => {
                            let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            for _ in 0..count {
                                if s.current_track > 0 {
                                    s.current_track -= 1;
                                }
                            }
                            s.selected = 0;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                        }
                        KeyCode::Char('j') | KeyCode::Down if pending_count.is_some() => {
                            let count = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            for _ in 0..count {
                                if s.current_track + 1 < s.tracks.len() {
                                    s.current_track += 1;
                                }
                            }
                            s.selected = 0;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                        }
                        KeyCode::Char('[') | KeyCode::Char('k') | KeyCode::Up => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            if s.current_track > 0 {
                                s.current_track -= 1;
                            }
                            s.selected = 0;
                        }
                        KeyCode::Char(']') | KeyCode::Char('j') | KeyCode::Down => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            if s.current_track + 1 < s.tracks.len() {
                                s.current_track += 1;
                            }
                            s.selected = 0;
                        }
                        KeyCode::Char('g') => {
                            pending_count = None;
                            if pending_g {
                                // gg = go to first track
                                pending_g = false;
                                let mut s = state.lock().unwrap();
                                s.current_track = 0;
                                s.selected = 0;
                            } else {
                                pending_g = true;
                                pending_d = false;
                                pending_y = false;
                            }
                        }
                        KeyCode::Char('t') if pending_g => {
                            pending_g = false;
                            gt_target = Some(String::from("track"));
                            gt_input.clear();
                            gt_last_key = Some(Instant::now());
                        }
                        KeyCode::Char('G') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            s.current_track = s.tracks.len() - 1;
                            s.selected = 0;
                        }

                        // Transport (writes the global SHM play flag; the
                        // sequencer thread mirrors it back into s.playing)
                        KeyCode::Char(' ') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            if transport_ui.is_none() {
                                transport_ui = ShmTransport::open().ok();
                            }
                            if let Some(ref mut t) = transport_ui {
                                let playing = t.toggle_playing();
                                state.lock().unwrap().playing = playing;
                            }
                        }
                        KeyCode::Char('s') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            if transport_ui.is_none() {
                                transport_ui = ShmTransport::open().ok();
                            }
                            if let Some(ref mut t) = transport_ui {
                                t.set_playing(false);
                            }
                            state.lock().unwrap().playing = false;
                        }
                        KeyCode::Char('u') => {
                            // Count-prefixed: 3u undoes 3 times
                            let count = pending_count.take().and_then(|c| c.parse().ok()).unwrap_or(1).max(1);
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            undo_msg = Some(history_status("Undo", count, || history.undo(&mut s)));
                            undo_time = Some(Instant::now());
                        }
                        KeyCode::Char('t') => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                            submode = String::from("bpm");
                            input_buffer.clear();
                        }

                        _ => {
                            pending_count = None;
                            pending_d = false;
                            pending_y = false;
                            pending_g = false;
                        }
                    }
                } else {
                    // Insert mode
                    match key.code {
                        KeyCode::Enter | KeyCode::Char(' ') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            let tidx = s.current_track;
                            let was_active = s.tracks[tidx].steps[sel].active;
                            s.tracks[tidx].steps[sel].active = !was_active;
                            history.push(Command::ToggleStep { track: tidx, step: sel, was_active });
                        }
                        KeyCode::Char('x') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            let tidx = s.current_track;
                            let old_step = s.tracks[tidx].steps[sel];
                            s.clipboard = Some(old_step);
                            s.tracks[tidx].steps[sel].active = false;
                            let new_step = s.tracks[tidx].steps[sel];
                            push_step_edit(&mut history, tidx, sel, old_step, new_step);
                        }
                        KeyCode::Char('y') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            s.clipboard = Some(s.track_mut().steps[sel]);
                        }
                        KeyCode::Char('p') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            let tidx = s.current_track;
                            let old_step = s.tracks[tidx].steps[sel];
                            if let Some(ref clip) = s.clipboard {
                                s.tracks[tidx].steps[sel] = *clip;
                                s.tracks[tidx].steps[sel].active = true;
                            }
                            let new_step = s.tracks[tidx].steps[sel];
                            push_step_edit(&mut history, tidx, sel, old_step, new_step);
                        }
                        KeyCode::Char('k') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            let tidx = s.current_track;
                            let old_step = s.tracks[tidx].steps[sel];
                            if s.tracks[tidx].mode == state::TrackMode::Modulation {
                                let v = s.tracks[tidx].steps[sel].mod_value + 0.01;
                                s.tracks[tidx].steps[sel].mod_value = v.min(1.0);
                            } else {
                                s.tracks[tidx].steps[sel].note = (s.tracks[tidx].steps[sel].note + 1).min(127);
                            }
                            let new_step = s.tracks[tidx].steps[sel];
                            push_step_edit(&mut history, tidx, sel, old_step, new_step);
                        }
                        KeyCode::Char('j') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            let tidx = s.current_track;
                            let old_step = s.tracks[tidx].steps[sel];
                            if s.tracks[tidx].mode == state::TrackMode::Modulation {
                                let v = s.tracks[tidx].steps[sel].mod_value - 0.01;
                                s.tracks[tidx].steps[sel].mod_value = v.max(-1.0);
                            } else {
                                s.tracks[tidx].steps[sel].note = s.tracks[tidx].steps[sel].note.saturating_sub(1);
                            }
                            let new_step = s.tracks[tidx].steps[sel];
                            push_step_edit(&mut history, tidx, sel, old_step, new_step);
                        }
                        KeyCode::Char('K') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            let tidx = s.current_track;
                            let old_step = s.tracks[tidx].steps[sel];
                            if s.tracks[tidx].mode == state::TrackMode::Modulation {
                                let v = s.tracks[tidx].steps[sel].mod_value + 0.1;
                                s.tracks[tidx].steps[sel].mod_value = v.min(1.0);
                            } else {
                                s.tracks[tidx].steps[sel].note = (s.tracks[tidx].steps[sel].note + 12).min(127);
                            }
                            let new_step = s.tracks[tidx].steps[sel];
                            push_step_edit(&mut history, tidx, sel, old_step, new_step);
                        }
                        KeyCode::Char('J') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            let tidx = s.current_track;
                            let old_step = s.tracks[tidx].steps[sel];
                            if s.tracks[tidx].mode == state::TrackMode::Modulation {
                                let v = s.tracks[tidx].steps[sel].mod_value - 0.1;
                                s.tracks[tidx].steps[sel].mod_value = v.max(-1.0);
                            } else {
                                s.tracks[tidx].steps[sel].note = s.tracks[tidx].steps[sel].note.saturating_sub(12);
                            }
                            let new_step = s.tracks[tidx].steps[sel];
                            push_step_edit(&mut history, tidx, sel, old_step, new_step);
                        }
                        KeyCode::Char('g') => {
                            pending_count = None;
                            if pending_g {
                                // gg = go to step 0
                                pending_g = false;
                                let mut s = state.lock().unwrap();
                                s.selected = 0;
                            } else {
                                pending_g = true;
                            }
                        }
                        KeyCode::Char('t') if pending_g => {
                            pending_g = false;
                            gt_target = Some(String::from("step"));
                            gt_input.clear();
                            gt_last_key = Some(Instant::now());
                        }
                        KeyCode::Char('t') => {
                            pending_count = None;
                            submode = String::from("bpm");
                            input_buffer.clear();
                        }
                        KeyCode::Char('N') => {
                            pending_count = None;
                            submode = String::from("note");
                            input_buffer.clear();
                        }
                        // Euclidean (no count)
                        KeyCode::Char('P') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let tidx = s.current_track;
                            let old = EuclidState::capture(&s.tracks[tidx]);
                            let (p, l, r) = (s.tracks[tidx].pulses, s.tracks[tidx].length, s.tracks[tidx].rotation);
                            euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                            let new = EuclidState::capture(&s.tracks[tidx]);
                            push_track_params(&mut history, tidx, old, new);
                        }
                        KeyCode::Char('L') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let tidx = s.current_track;
                            s.selected = s.selected.min(s.tracks[tidx].length - 1);
                            let old = EuclidState::capture(&s.tracks[tidx]);
                            let (p, l, r) = (s.tracks[tidx].pulses, s.tracks[tidx].length, s.tracks[tidx].rotation);
                            euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                            let new = EuclidState::capture(&s.tracks[tidx]);
                            push_track_params(&mut history, tidx, old, new);
                        }
                        KeyCode::Char('R') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let tidx = s.current_track;
                            let old = EuclidState::capture(&s.tracks[tidx]);
                            s.tracks[tidx].rotation = (s.tracks[tidx].rotation + 1) % s.tracks[tidx].length;
                            let (p, l, r) = (s.tracks[tidx].pulses, s.tracks[tidx].length, s.tracks[tidx].rotation);
                            euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                            let new = EuclidState::capture(&s.tracks[tidx]);
                            push_track_params(&mut history, tidx, old, new);
                        }
                        _ => {
                            pending_count = None;
                            pending_g = false;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_tracks(n: usize) -> SequencerState {
        let mut s = SequencerState::default();
        s.tracks.truncate(n);
        s.current_steps.truncate(n);
        s.last_notes.truncate(n);
        s
    }

    /// Simulate the insert-mode Enter/Space handler.
    fn toggle_step(s: &mut SequencerState, h: &mut History) {
        let (tidx, sel) = (s.current_track, s.selected);
        let was_active = s.tracks[tidx].steps[sel].active;
        s.tracks[tidx].steps[sel].active = !was_active;
        h.push(Command::ToggleStep { track: tidx, step: sel, was_active });
    }

    /// Simulate the insert-mode `x` handler.
    fn cut_step(s: &mut SequencerState, h: &mut History) {
        let (tidx, sel) = (s.current_track, s.selected);
        let old_step = s.tracks[tidx].steps[sel];
        s.clipboard = Some(old_step);
        s.tracks[tidx].steps[sel].active = false;
        let new_step = s.tracks[tidx].steps[sel];
        push_step_edit(h, tidx, sel, old_step, new_step);
    }

    /// Simulate the insert-mode `p` handler.
    fn paste_step(s: &mut SequencerState, h: &mut History) {
        let (tidx, sel) = (s.current_track, s.selected);
        let old_step = s.tracks[tidx].steps[sel];
        if let Some(clip) = s.clipboard {
            s.tracks[tidx].steps[sel] = clip;
            s.tracks[tidx].steps[sel].active = true;
        }
        let new_step = s.tracks[tidx].steps[sel];
        push_step_edit(h, tidx, sel, old_step, new_step);
    }

    /// Simulate the insert-mode `k` (transpose up a semitone) handler.
    fn transpose_up(s: &mut SequencerState, h: &mut History) {
        let (tidx, sel) = (s.current_track, s.selected);
        let old_step = s.tracks[tidx].steps[sel];
        s.tracks[tidx].steps[sel].note = (old_step.note + 1).min(127);
        let new_step = s.tracks[tidx].steps[sel];
        push_step_edit(h, tidx, sel, old_step, new_step);
    }

    /// Simulate the normal-mode `n` (new track) handler.
    fn new_track(s: &mut SequencerState, h: &mut History) {
        s.tracks.push(Track::new());
        s.current_steps.push(0);
        s.last_notes.push(None);
        s.current_track = s.tracks.len() - 1;
        s.selected = 0;
        h.push(Command::NewTrack { at: s.current_track });
    }

    /// Simulate the normal-mode `dd` (delete track) handler.
    fn delete_track(s: &mut SequencerState, h: &mut History) {
        if s.tracks.len() > 1 {
            let was = s.current_track;
            let track = s.tracks.remove(was);
            s.track_clipboard = Some(track.clone());
            s.current_steps.remove(was);
            s.last_notes.remove(was);
            if s.current_track >= s.tracks.len() {
                s.current_track = s.tracks.len() - 1;
            }
            s.selected = 0;
            h.push(Command::DeleteTrack { at: was, track });
        }
    }

    /// Simulate a count-prefixed `L` (set length) handler.
    fn set_length(s: &mut SequencerState, h: &mut History, length: usize) {
        let tidx = s.current_track;
        let old = EuclidState::capture(&s.tracks[tidx]);
        s.tracks[tidx].length = length.clamp(1, 16);
        s.selected = s.selected.min(s.tracks[tidx].length - 1);
        let (p, l, r) = (s.tracks[tidx].pulses, s.tracks[tidx].length, s.tracks[tidx].rotation);
        euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
        let new = EuclidState::capture(&s.tracks[tidx]);
        push_track_params(h, tidx, old, new);
    }

    #[test]
    fn toggle_step_undo() {
        let mut s = state_with_tracks(2);
        let mut h = History::new();
        s.selected = 1;
        assert!(!s.tracks[0].steps[1].active);
        toggle_step(&mut s, &mut h);
        assert!(s.tracks[0].steps[1].active);
        assert_eq!(h.undo(&mut s), Some("Toggle step"));
        assert!(!s.tracks[0].steps[1].active);
    }

    #[test]
    fn cut_step_undo() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.selected = 4; // active by default
        let before = s.tracks[0].steps[4];
        cut_step(&mut s, &mut h);
        assert!(!s.tracks[0].steps[4].active);
        assert!(h.undo(&mut s).is_some());
        assert_eq!(s.tracks[0].steps[4], before);
    }

    #[test]
    fn paste_step_undo() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.clipboard = Some(Step { active: true, note: 72, velocity: 90, mod_value: 0.0 });
        s.selected = 1;
        let before = s.tracks[0].steps[1];
        paste_step(&mut s, &mut h);
        assert_eq!(s.tracks[0].steps[1].note, 72);
        assert!(h.undo(&mut s).is_some());
        assert_eq!(s.tracks[0].steps[1], before);
    }

    #[test]
    fn paste_with_empty_clipboard_is_not_undoable() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        paste_step(&mut s, &mut h);
        assert_eq!(h.undo(&mut s), None);
    }

    #[test]
    fn transpose_undo() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        transpose_up(&mut s, &mut h);
        assert_eq!(s.tracks[0].steps[0].note, 61);
        assert!(h.undo(&mut s).is_some());
        assert_eq!(s.tracks[0].steps[0].note, 60);
    }

    #[test]
    fn transpose_at_max_note_is_not_undoable() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.tracks[0].steps[0].note = 127;
        transpose_up(&mut s, &mut h);
        assert_eq!(h.undo(&mut s), None);
    }

    #[test]
    fn delete_track_undo_restores_contents() {
        let mut s = state_with_tracks(3);
        let mut h = History::new();
        s.current_track = 1;
        s.tracks[1].steps[3].note = 99;
        let deleted = s.tracks[1].clone();
        delete_track(&mut s, &mut h);
        assert_eq!(s.tracks.len(), 2);
        assert_eq!(h.undo(&mut s), Some("Delete track"));
        assert_eq!(s.tracks.len(), 3);
        assert_eq!(s.tracks[1], deleted);
        assert_eq!(s.current_steps.len(), 3);
        assert_eq!(s.last_notes.len(), 3);
        assert_eq!(s.current_track, 1);
    }

    #[test]
    fn new_track_undo() {
        let mut s = state_with_tracks(2);
        let mut h = History::new();
        new_track(&mut s, &mut h);
        assert_eq!(s.tracks.len(), 3);
        assert_eq!(h.undo(&mut s), Some("New track"));
        assert_eq!(s.tracks.len(), 2);
        assert_eq!(s.current_steps.len(), 2);
        assert_eq!(s.last_notes.len(), 2);
        assert!(s.current_track < s.tracks.len());
    }

    #[test]
    fn multi_step_undo() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.selected = 1;
        toggle_step(&mut s, &mut h);
        s.selected = 2;
        toggle_step(&mut s, &mut h);
        assert!(s.tracks[0].steps[1].active && s.tracks[0].steps[2].active);
        assert!(h.undo(&mut s).is_some());
        assert!(h.undo(&mut s).is_some());
        assert!(!s.tracks[0].steps[1].active && !s.tracks[0].steps[2].active);
        assert_eq!(h.undo(&mut s), None);
    }

    #[test]
    fn redo_reapplies() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.selected = 1;
        toggle_step(&mut s, &mut h);
        assert!(h.undo(&mut s).is_some());
        assert!(!s.tracks[0].steps[1].active);
        assert_eq!(h.redo(&mut s), Some("Toggle step"));
        assert!(s.tracks[0].steps[1].active);
        assert_eq!(h.redo(&mut s), None);
    }

    #[test]
    fn new_action_after_undo_clears_redo() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.selected = 1;
        toggle_step(&mut s, &mut h);
        assert!(h.undo(&mut s).is_some());
        s.selected = 2;
        toggle_step(&mut s, &mut h);
        assert_eq!(h.redo(&mut s), None);
        assert_eq!(h.commands.len(), 1);
    }

    #[test]
    fn mute_toggle_undo() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        let was_muted = s.tracks[0].muted;
        s.tracks[0].muted = !was_muted;
        h.push(Command::ToggleMute { track: 0, was_muted });
        assert_eq!(h.undo(&mut s), Some("Toggle mute"));
        assert_eq!(s.tracks[0].muted, was_muted);
        assert!(h.redo(&mut s).is_some());
        assert_eq!(s.tracks[0].muted, !was_muted);
    }

    #[test]
    fn mode_toggle_undo() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.tracks[0].mode = TrackMode::Modulation;
        h.push(Command::ToggleMode { track: 0, was_mode: TrackMode::Note });
        assert_eq!(h.undo(&mut s), Some("Toggle mode"));
        assert_eq!(s.tracks[0].mode, TrackMode::Note);
        assert!(h.redo(&mut s).is_some());
        assert_eq!(s.tracks[0].mode, TrackMode::Modulation);
    }

    #[test]
    fn bpm_undo_redo() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        h.push(Command::SetBpm { old_bpm: 120.0, new_bpm: 90.0 });
        s.bpm = 90.0;
        assert_eq!(h.undo(&mut s), Some("Set BPM"));
        assert_eq!(s.bpm, 120.0);
        assert!(h.redo(&mut s).is_some());
        assert_eq!(s.bpm, 90.0);
    }

    #[test]
    fn track_params_undo_restores_pattern() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        // Hand-edit a step so the euclidean rewrite is destructive
        s.tracks[0].steps[1].active = true;
        let before = EuclidState::capture(&s.tracks[0]);
        set_length(&mut s, &mut h, 8);
        assert_eq!(s.tracks[0].length, 8);
        assert!(h.undo(&mut s).is_some());
        assert_eq!(EuclidState::capture(&s.tracks[0]), before);
    }

    #[test]
    fn track_params_redo_clamps_selected() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        set_length(&mut s, &mut h, 8);
        assert!(h.undo(&mut s).is_some()); // back to length 16
        s.selected = 15;
        assert!(h.redo(&mut s).is_some()); // length 8 again
        assert_eq!(s.tracks[0].length, 8);
        assert!(s.selected < 8);
    }

    #[test]
    fn noop_track_params_not_pushed() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        // Applying euclidean to an untouched euclidean pattern changes nothing
        set_length(&mut s, &mut h, 16);
        set_length(&mut s, &mut h, 16);
        assert!(h.undo(&mut s).is_some()); // the first one DID rewrite steps
        assert_eq!(h.undo(&mut s), None);
    }

    #[test]
    fn undo_focuses_changed_location() {
        let mut s = state_with_tracks(4);
        let mut h = History::new();
        s.current_track = 2;
        s.selected = 5;
        toggle_step(&mut s, &mut h);
        s.current_track = 0;
        s.selected = 0;
        assert!(h.undo(&mut s).is_some());
        assert_eq!(s.current_track, 2);
        assert_eq!(s.selected, 5);
    }

    #[test]
    fn history_capped() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        for _ in 0..(HISTORY_CAP + 50) {
            toggle_step(&mut s, &mut h);
        }
        assert_eq!(h.commands.len(), HISTORY_CAP);
        for _ in 0..HISTORY_CAP {
            assert!(h.undo(&mut s).is_some());
        }
        assert_eq!(h.undo(&mut s), None);
    }

    #[test]
    fn undo_redo_empty_history() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        assert_eq!(h.undo(&mut s), None);
        assert_eq!(h.redo(&mut s), None);
    }

    #[test]
    fn count_undo_runs_n_times() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        for sel in 1..=3 {
            s.selected = sel;
            toggle_step(&mut s, &mut h);
        }
        let msg = history_status("Undo", 3, || h.undo(&mut s));
        assert_eq!(msg, "Undo ×3: Toggle step");
        assert!(!s.tracks[0].steps[1].active);
        assert!(!s.tracks[0].steps[2].active);
        assert!(!s.tracks[0].steps[3].active);
    }

    #[test]
    fn count_undo_stops_at_history_end() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.selected = 1;
        toggle_step(&mut s, &mut h);
        s.selected = 2;
        toggle_step(&mut s, &mut h);
        let msg = history_status("Undo", 10, || h.undo(&mut s));
        assert_eq!(msg, "Undo ×2: Toggle step");
        assert_eq!(h.undo(&mut s), None);
    }

    #[test]
    fn count_redo_runs_n_times() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        for sel in 1..=3 {
            s.selected = sel;
            toggle_step(&mut s, &mut h);
        }
        assert_eq!(history_status("Undo", 3, || h.undo(&mut s)), "Undo ×3: Toggle step");
        let msg = history_status("Redo", 2, || h.redo(&mut s));
        assert_eq!(msg, "Redo ×2: Toggle step");
        assert!(s.tracks[0].steps[1].active);
        assert!(s.tracks[0].steps[2].active);
        assert!(!s.tracks[0].steps[3].active);
    }

    #[test]
    fn single_undo_message_has_no_multiplier() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.selected = 1;
        toggle_step(&mut s, &mut h);
        assert_eq!(history_status("Undo", 1, || h.undo(&mut s)), "Undo: Toggle step");
        assert_eq!(history_status("Undo", 1, || h.undo(&mut s)), "Nothing to undo");
        assert_eq!(history_status("Redo", 5, || h.redo(&mut s)), "Redo: Toggle step");
        assert_eq!(history_status("Redo", 1, || h.redo(&mut s)), "Nothing to redo");
    }
}
