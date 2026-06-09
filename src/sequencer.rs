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
    style::Style,
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use crate::shm::{AudioEvent, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state::{self, TrackMode};

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
    register: Option<Register>,
    visual_anchor: Option<usize>,
    /// Modbus base channel claimed at registration (track outputs write here).
    mod_base: Option<usize>,
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
            register: None,
            visual_anchor: None,
            mod_base: None,
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
    EditSteps {
        track: usize,
        start: usize,
        old: Vec<Step>,
        new: Vec<Step>,
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

/// The unified vi register: holds either a step range ("charwise") or a
/// whole track ("linewise"); paste does whatever fits the contents.
#[derive(Debug, Clone, PartialEq)]
enum Register {
    Steps(Vec<Step>),
    Track(Track),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Operator {
    Yank,
    Delete,
    Change,
}

impl Operator {
    fn from_char(c: char) -> Option<Self> {
        match c {
            'y' => Some(Operator::Yank),
            'd' => Some(Operator::Delete),
            'c' => Some(Operator::Change),
            _ => None,
        }
    }
}

/// Step motions. A *word* is a maximal run of consecutive active steps.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Motion {
    Left,
    Right,
    WordFwd,
    WordBack,
    WordEnd,
    Home,
    End,
    /// up to but not including step n (`t#`)
    Till(usize),
    /// to step n inclusive (`f#`)
    Find(usize),
    /// a span of n steps starting at the cursor (visual selections, dot-repeat)
    Span(usize),
}

fn is_word_start(steps: &[Step], len: usize, i: usize) -> bool {
    steps[i].active && !steps[(i + len - 1) % len].active
}

fn is_word_end(steps: &[Step], len: usize, i: usize) -> bool {
    steps[i].active && !steps[(i + 1) % len].active
}

/// Cursor target for bare-motion navigation (wraps around the pattern).
/// Returns `from` unchanged when the pattern has no word boundary.
fn nav_word(steps: &[Step], len: usize, from: usize, motion: Motion) -> usize {
    for i in 1..=len {
        let idx = match motion {
            Motion::WordBack => (from + len - i) % len,
            _ => (from + i) % len,
        };
        let hit = match motion {
            Motion::WordFwd | Motion::WordBack => is_word_start(steps, len, idx),
            Motion::WordEnd => is_word_end(steps, len, idx),
            _ => return from,
        };
        if hit {
            return idx;
        }
    }
    from
}

/// Inclusive step range covered by `operator{motion}` from the cursor.
/// Operator ranges never wrap (vi semantics); None = invalid/no-op motion.
fn motion_range(track: &Track, cursor: usize, motion: Motion, count: usize) -> Option<(usize, usize)> {
    let len = track.length;
    let steps = &track.steps;
    let cursor = cursor.min(len.saturating_sub(1));
    match motion {
        Motion::Right => Some((cursor, (cursor + count - 1).min(len - 1))),
        Motion::Left => {
            if cursor == 0 {
                None
            } else {
                Some((cursor.saturating_sub(count), cursor - 1))
            }
        }
        Motion::End => Some((cursor, len - 1)),
        Motion::Home => {
            if cursor == 0 {
                None
            } else {
                Some((0, cursor - 1))
            }
        }
        Motion::WordFwd => {
            let mut found = 0;
            for i in cursor + 1..len {
                if is_word_start(steps, len, i) {
                    found += 1;
                    if found == count {
                        return Some((cursor, i - 1));
                    }
                }
            }
            Some((cursor, len - 1)) // dw at the last word eats to the end
        }
        Motion::WordEnd => {
            let mut found = 0;
            for i in cursor + 1..len {
                if is_word_end(steps, len, i) {
                    found += 1;
                    if found == count {
                        return Some((cursor, i));
                    }
                }
            }
            None
        }
        Motion::WordBack => {
            let mut found = 0;
            for i in (0..cursor).rev() {
                if is_word_start(steps, len, i) {
                    found += 1;
                    if found == count {
                        return Some((i, cursor - 1));
                    }
                }
            }
            None
        }
        Motion::Till(n) => {
            let n = n.min(len - 1);
            match n.cmp(&cursor) {
                std::cmp::Ordering::Greater => Some((cursor, n - 1)),
                std::cmp::Ordering::Less => Some((n + 1, cursor)),
                std::cmp::Ordering::Equal => None,
            }
        }
        Motion::Find(n) => {
            let n = n.min(len - 1);
            Some((cursor.min(n), cursor.max(n)))
        }
        Motion::Span(n) => Some((cursor, (cursor + n.max(1) - 1).min(len - 1))),
    }
}

/// Euclidean parameter edits (`#P`/`#L`/`#R`, bare `P`/`L`/`R`, `:set`).
#[derive(Debug, Clone, Copy, PartialEq)]
enum EuclidOp {
    Pulses(usize),
    Length(usize),
    Rotation(usize),
    RotatePlus(usize),
    Reapply,
}

/// Every user edit, cursor-relative — the single entry point used by the key
/// handlers, visual mode, and dot-repeat (`.` re-applies the last change).
#[derive(Debug, Clone, PartialEq)]
enum Action {
    ToggleStep,
    /// toggle every step in a span (visual `~`)
    ToggleSpan(usize),
    CutStep,
    YankStep,
    Paste { before: bool, times: usize },
    Transpose { note: i32, mod_value: f32 },
    SetNote(u8),
    Op { op: Operator, motion: Motion, count: usize },
    OpTrack(Operator),
    ToggleMute,
    ToggleMode,
    NewTrack { before: bool },
    Rotate { steps: i32 },
    Euclid(EuclidOp),
}

impl Action {
    /// Changes are dot-repeatable; pure yanks are not (vi semantics).
    fn is_change(&self) -> bool {
        !matches!(
            self,
            Action::YankStep
                | Action::Op { op: Operator::Yank, .. }
                | Action::OpTrack(Operator::Yank)
        )
    }
}

/// Apply an action at the current cursor, recording undo history.
/// Returns a status message when there is something worth saying.
fn apply_action(s: &mut SequencerState, h: &mut History, action: &Action) -> Option<String> {
    let tidx = s.current_track;
    match action {
        Action::ToggleStep => {
            let sel = s.selected;
            let was_active = s.tracks[tidx].steps[sel].active;
            s.tracks[tidx].steps[sel].active = !was_active;
            h.push(Command::ToggleStep { track: tidx, step: sel, was_active });
            None
        }
        Action::ToggleSpan(n) => {
            let (a, b) = motion_range(&s.tracks[tidx], s.selected, Motion::Span(*n), 1)?;
            let old: Vec<Step> = s.tracks[tidx].steps[a..=b].to_vec();
            let mut new = old.clone();
            for st in new.iter_mut() {
                st.active = !st.active;
            }
            s.tracks[tidx].steps[a..=b].copy_from_slice(&new);
            h.push(Command::EditSteps { track: tidx, start: a, old, new });
            s.selected = a;
            None
        }
        Action::CutStep => {
            let sel = s.selected;
            let old_step = s.tracks[tidx].steps[sel];
            s.register = Some(Register::Steps(vec![old_step]));
            s.tracks[tidx].steps[sel].active = false;
            let new_step = s.tracks[tidx].steps[sel];
            push_step_edit(h, tidx, sel, old_step, new_step);
            None
        }
        Action::YankStep => {
            let sel = s.selected;
            s.register = Some(Register::Steps(vec![s.tracks[tidx].steps[sel]]));
            Some(String::from("Yanked 1 step"))
        }
        Action::Paste { before, times } => match s.register.clone() {
            None => Some(String::from("Nothing in register")),
            Some(Register::Steps(slice)) => {
                let len = s.tracks[tidx].length;
                let total = (slice.len() * times.max(&1)).min(len);
                let start = if *before {
                    (s.selected + 1).saturating_sub(total)
                } else {
                    s.selected.min(len - 1)
                };
                let end = (start + total - 1).min(len - 1);
                let old: Vec<Step> = s.tracks[tidx].steps[start..=end].to_vec();
                let new: Vec<Step> = (0..=(end - start)).map(|i| slice[i % slice.len()]).collect();
                if old == new {
                    return None;
                }
                s.tracks[tidx].steps[start..=end].copy_from_slice(&new);
                h.push(Command::EditSteps { track: tidx, start, old, new });
                s.selected = start;
                None
            }
            Some(Register::Track(trk)) => {
                for i in 0..*times.max(&1) {
                    let at = if *before { tidx } else { tidx + 1 + i };
                    insert_track(s, at, trk.clone());
                    h.push(Command::PasteTrack { at, track: trk.clone() });
                }
                None
            }
        },
        Action::Transpose { note, mod_value } => {
            let sel = s.selected;
            let old_step = s.tracks[tidx].steps[sel];
            if s.tracks[tidx].mode == TrackMode::Modulation {
                let v = old_step.mod_value + mod_value;
                s.tracks[tidx].steps[sel].mod_value = v.clamp(-1.0, 1.0);
            } else {
                let n = (old_step.note as i32 + note).clamp(0, 127);
                s.tracks[tidx].steps[sel].note = n as u8;
            }
            let new_step = s.tracks[tidx].steps[sel];
            push_step_edit(h, tidx, sel, old_step, new_step);
            None
        }
        Action::SetNote(n) => {
            let sel = s.selected;
            let old_step = s.tracks[tidx].steps[sel];
            s.tracks[tidx].steps[sel].note = (*n).min(127);
            let new_step = s.tracks[tidx].steps[sel];
            push_step_edit(h, tidx, sel, old_step, new_step);
            None
        }
        Action::Op { op, motion, count } => {
            let Some((a, b)) = motion_range(&s.tracks[tidx], s.selected, *motion, (*count).max(1)) else {
                return Some(String::from("No motion"));
            };
            let slice: Vec<Step> = s.tracks[tidx].steps[a..=b].to_vec();
            match op {
                Operator::Yank => {
                    let n = slice.len();
                    s.register = Some(Register::Steps(slice));
                    s.selected = a;
                    Some(format!("Yanked {} step{}", n, if n == 1 { "" } else { "s" }))
                }
                Operator::Delete | Operator::Change => {
                    let old = slice.clone();
                    s.register = Some(Register::Steps(slice));
                    let mut new = old.clone();
                    for st in new.iter_mut() {
                        st.active = false;
                    }
                    if old != new {
                        s.tracks[tidx].steps[a..=b].copy_from_slice(&new);
                        h.push(Command::EditSteps { track: tidx, start: a, old, new });
                    }
                    s.selected = a;
                    None
                }
            }
        }
        Action::OpTrack(op) => match op {
            Operator::Yank => {
                s.register = Some(Register::Track(s.tracks[tidx].clone()));
                Some(String::from("Yanked track"))
            }
            Operator::Delete => {
                if s.tracks.len() <= 1 {
                    return Some(String::from("Can't delete the last track"));
                }
                let track = s.tracks.remove(tidx);
                s.current_steps.remove(tidx);
                s.last_notes.remove(tidx);
                s.register = Some(Register::Track(track.clone()));
                h.push(Command::DeleteTrack { at: tidx, track });
                s.focus(tidx, Some(0));
                None
            }
            Operator::Change => {
                let len = s.tracks[tidx].length;
                let old: Vec<Step> = s.tracks[tidx].steps[..len].to_vec();
                s.register = Some(Register::Steps(old.clone()));
                let mut new = old.clone();
                for st in new.iter_mut() {
                    st.active = false;
                }
                s.tracks[tidx].steps[..len].copy_from_slice(&new);
                h.push(Command::EditSteps { track: tidx, start: 0, old, new });
                s.selected = 0;
                None
            }
        },
        Action::ToggleMute => {
            let was_muted = s.tracks[tidx].muted;
            s.tracks[tidx].muted = !was_muted;
            h.push(Command::ToggleMute { track: tidx, was_muted });
            None
        }
        Action::ToggleMode => {
            let was_mode = s.tracks[tidx].mode;
            s.tracks[tidx].mode = match was_mode {
                TrackMode::Note => TrackMode::Modulation,
                TrackMode::Modulation => TrackMode::Note,
            };
            h.push(Command::ToggleMode { track: tidx, was_mode });
            None
        }
        Action::NewTrack { before } => {
            let at = if *before { tidx } else { tidx + 1 };
            insert_track(s, at, Track::new());
            h.push(Command::NewTrack { at });
            None
        }
        Action::Rotate { steps: n } => {
            let len = s.tracks[tidx].length;
            let old: Vec<Step> = s.tracks[tidx].steps[..len].to_vec();
            let mut new = old.clone();
            let shift = (n.rem_euclid(len as i32)) as usize;
            new.rotate_right(shift);
            if old == new {
                return None;
            }
            s.tracks[tidx].steps[..len].copy_from_slice(&new);
            h.push(Command::EditSteps { track: tidx, start: 0, old, new });
            None
        }
        Action::Euclid(op) => {
            let old = EuclidState::capture(&s.tracks[tidx]);
            match op {
                EuclidOp::Pulses(n) => s.tracks[tidx].pulses = (*n).min(16),
                EuclidOp::Length(n) => {
                    s.tracks[tidx].length = (*n).clamp(1, 16);
                    let len = s.tracks[tidx].length;
                    s.selected = s.selected.min(len - 1);
                }
                EuclidOp::Rotation(n) => s.tracks[tidx].rotation = (*n).min(255),
                EuclidOp::RotatePlus(n) => {
                    s.tracks[tidx].rotation =
                        (s.tracks[tidx].rotation + n) % s.tracks[tidx].length;
                }
                EuclidOp::Reapply => {
                    s.selected = s.selected.min(s.tracks[tidx].length - 1);
                }
            }
            let (p, l, r) = (s.tracks[tidx].pulses, s.tracks[tidx].length, s.tracks[tidx].rotation);
            euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
            let new = EuclidState::capture(&s.tracks[tidx]);
            push_track_params(h, tidx, old, new);
            None
        }
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
            Command::EditSteps { .. } => "Edit steps",
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
            Command::EditSteps { track, start, old, .. } => {
                if *track < state.tracks.len() && start + old.len() <= state.tracks[*track].steps.len() {
                    state.tracks[*track].steps[*start..start + old.len()].copy_from_slice(old);
                    state.focus(*track, Some(*start));
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
            Command::EditSteps { track, start, new, .. } => {
                if *track < state.tracks.len() && start + new.len() <= state.tracks[*track].steps.len() {
                    state.tracks[*track].steps[*start..start + new.len()].copy_from_slice(new);
                    state.focus(*track, Some(*start));
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
    commands: Vec<(Command, Instant)>,
    index: usize,
}

impl History {
    fn new() -> Self {
        Self { commands: vec![], index: 0 }
    }

    fn push(&mut self, cmd: Command) {
        // Sweep rule (docs/keybindings.md): consecutive edits of the same
        // step within the coalescing window merge into one undo entry, so a
        // held transpose key reverts with a single u.
        if self.index == self.commands.len() {
            if let Some((Command::EditStep { track, step, old_step, new_step }, at)) =
                self.commands.last_mut()
            {
                if let Command::EditStep { track: t2, step: s2, new_step: n2, .. } = &cmd {
                    if t2 == track && s2 == step && at.elapsed() < crate::undo::COALESCE_WINDOW {
                        *new_step = *n2;
                        *at = Instant::now();
                        if old_step == new_step {
                            // sweep returned to where it started — drop it
                            self.commands.pop();
                            self.index = self.commands.len();
                        }
                        return;
                    }
                }
            }
        }
        self.commands.truncate(self.index);
        self.commands.push((cmd, Instant::now()));
        if self.commands.len() > HISTORY_CAP {
            self.commands.remove(0);
        }
        self.index = self.commands.len();
    }

    fn undo(&mut self, state: &mut SequencerState) -> Option<&'static str> {
        if self.index == 0 { return None; }
        self.index -= 1;
        self.commands[self.index].0.undo(state);
        Some(self.commands[self.index].0.description())
    }

    fn redo(&mut self, state: &mut SequencerState) -> Option<&'static str> {
        if self.index >= self.commands.len() { return None; }
        self.commands[self.index].0.redo(state);
        self.index += 1;
        Some(self.commands[self.index - 1].0.description())
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
        transport.set_bpm(bpm as f32);

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
                        if let (Some(ref mut bus), Some(base)) = (modbus.as_mut(), s.mod_base) {
                            bus.set(base + t, mod_val);
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
    pending: Option<&str>,
) -> Result<()> {
    use crate::theme;
    use ratatui::text::Span;

    terminal.draw(|f| {
        let area = f.area();
        let w = area.width as usize;

        // ── header ──────────────────────────────────────────────────────
        let cstep = state.current_steps[state.current_track];
        let cur_len = state.track().length;
        let echo = theme::transport_echo(
            state.bpm as f32,
            state.playing,
            Some(&format!("{:02}/{:02}", cstep + 1, cur_len)),
        );
        let ctx = format!("t{}/{}", state.current_track + 1, state.tracks.len());
        let mut lines: Vec<Line> = vec![theme::header("SEQ", &ctx, &echo, w)];

        // ── track rows ──────────────────────────────────────────────────
        for (ti, trk) in state.tracks.iter().enumerate() {
            let is_cur = ti == state.current_track;
            let tstep = state.current_steps[ti];
            let hue = match trk.mode {
                TrackMode::Note => theme::note(),
                TrackMode::Modulation => theme::cv(),
            };
            let row_style = if trk.muted { theme::dim() } else { theme::value() };

            let mut spans: Vec<Span> = Vec::with_capacity(trk.length + 8);
            spans.push(Span::styled(
                format!("{}t{} ", if is_cur { theme::PLAYHEAD } else { ' ' }, ti + 1),
                if is_cur { theme::chrome_hi() } else { theme::chrome() },
            ));
            for i in 0..trk.length {
                if i > 0 && i % 4 == 0 {
                    spans.push(Span::raw(" "));
                }
                let step = &trk.steps[i];
                let on = step.active;
                let glyph = match (trk.mode, on) {
                    (TrackMode::Note, true) => theme::STEP_ON,
                    (TrackMode::Note, false) => theme::STEP_OFF,
                    (TrackMode::Modulation, true) => theme::MOD_ON,
                    (TrackMode::Modulation, false) => theme::MOD_OFF,
                };
                // playhead + wake (CLOCK hue), trigger flash on the live cell
                let style = if state.playing && i == tstep && !trk.muted {
                    if on {
                        theme::flash(hue)
                    } else {
                        theme::signal(theme::clock())
                    }
                } else if state.playing && !trk.muted && wake_offset(i, tstep, trk.length).is_some() {
                    theme::signal(theme::clock())
                } else if trk.muted {
                    theme::dim()
                } else if on {
                    theme::signal(hue)
                } else {
                    theme::dim()
                };
                let shown = if state.playing && !trk.muted {
                    match wake_offset(i, tstep, trk.length) {
                        Some(o) if i != tstep => theme::WAKE[2 - o.min(2)],
                        _ if i == tstep => {
                            if on { glyph } else { theme::PLAYHEAD }
                        }
                        _ => glyph,
                    }
                } else {
                    glyph
                };
                spans.push(Span::styled(shown.to_string(), style));
            }
            let info = format!(
                "  {:2} P{} R{}{}{}",
                trk.length,
                trk.pulses,
                trk.rotation,
                if trk.mode == TrackMode::Modulation { " ⌁" } else { "" },
                if trk.muted { " M" } else { "" },
            );
            spans.push(Span::styled(info, if is_cur { row_style } else { theme::dim() }));
            lines.push(Line::from(spans));
        }

        lines.push(theme::rule(w));

        // ── detail strip: current track, three rows ─────────────────────
        let trk = state.track();
        let cell = 5usize; // chars per step column
        let visible = (w / cell).max(1).min(trk.length);
        // window scrolls to keep the selected step in view
        let first = state.selected.saturating_sub(visible.saturating_sub(1));
        let in_visual = |i: usize| {
            mode == "visual"
                && state.visual_anchor.is_some_and(|a| {
                    let (lo, hi) = (a.min(state.selected), a.max(state.selected));
                    i >= lo && i <= hi
                })
        };

        let mut nums: Vec<Span> = Vec::new();
        let mut vals: Vec<Span> = Vec::new();
        let mut vels: Vec<Span> = Vec::new();
        for i in first..(first + visible).min(trk.length) {
            let step = &trk.steps[i];
            let sel = i == state.selected;
            let num_style = if sel {
                theme::selected()
            } else if i == cstep && state.playing {
                theme::signal(theme::clock())
            } else {
                theme::dim()
            };
            nums.push(Span::styled(format!("{:^cell$}", format!("{:02}", i + 1)), num_style));

            let val = match trk.mode {
                TrackMode::Note => {
                    if step.active {
                        midi_note_name(step.note)
                    } else {
                        String::from("·")
                    }
                }
                TrackMode::Modulation => format!("{:+.2}", step.mod_value),
            };
            let val_style = if sel {
                theme::selected()
            } else if in_visual(i) {
                theme::flash(theme::cv())
            } else if step.active {
                theme::signal(match trk.mode {
                    TrackMode::Note => theme::note(),
                    TrackMode::Modulation => theme::cv(),
                })
            } else {
                theme::dim()
            };
            vals.push(Span::styled(format!("{:^cell$}", val), val_style));

            let vel = if step.active && trk.mode == TrackMode::Note {
                theme::meter_char(step.velocity as f32 / 127.0).to_string()
            } else {
                String::from("·")
            };
            vels.push(Span::styled(
                format!("{:^cell$}", vel),
                if step.active { theme::signal(theme::note()) } else { theme::dim() },
            ));
        }
        lines.push(Line::from(nums));
        lines.push(Line::from(vals));
        lines.push(Line::from(vels));

        lines.push(theme::rule(w));

        // ── status ──────────────────────────────────────────────────────
        let mode_label = match mode {
            "insert" if !submode.is_empty() => format!("INSERT[{}:{}]", submode, input_buffer),
            "insert" => String::from("INSERT"),
            "visual" => String::from("VISUAL"),
            "visual_line" => String::from("V-LINE"),
            _ => String::from("NORMAL"),
        };
        let mut msg_parts: Vec<String> = Vec::new();
        if let Some(c) = pending_count {
            msg_parts.push(format!("{}…", c));
        }
        if let Some(p) = pending {
            msg_parts.push(p.to_string());
        }
        if let Some(t) = gt_target {
            msg_parts.push(format!("gt{}:{}", t, gt_input));
        }
        if let Some(m) = undo_msg {
            msg_parts.push(m.clone());
        }
        let msg = msg_parts.join(theme::SEP);
        lines.push(theme::status(&mode_label, &msg, "", w));

        f.render_widget(Paragraph::new(lines), area);

        // ── help overlay ────────────────────────────────────────────────
        if show_help {
            let help_text = sequencer_help();
            let help = Paragraph::new(help_text)
                .style(Style::default().fg(theme::ink()).bg(theme::bg()))
                .block(Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" SEQ ", theme::chrome_hi())));
            f.render_widget(help, area);
        }
    })?;

    Ok(())
}

fn sequencer_help() -> Vec<Line<'static>> {
    vec![
        Line::from("━━━ SEQ ━━━"),
        Line::from(""),
        Line::from("Motions (word = run of active steps):"),
        Line::from("  h/l        Step left/right (counts: 5l)"),
        Line::from("  w / b / e  Word fwd / back / end"),
        Line::from("  0 / $      First / last step"),
        Line::from("  f# / t#    To / till step #"),
        Line::from("  j/k, [/]   Track down/up (counts)"),
        Line::from("  gg/G, gt#  First / last / go to track"),
        Line::from(""),
        Line::from("Operators (normal/visual): y d c"),
        Line::from("  y/d/c{motion}; yy/dd/cc whole track"),
        Line::from(""),
        Line::from("Normal: Enter/i insert · v/V visual"),
        Line::from("  x cut  ~ toggle  p/P paste (#p ×N)"),
        Line::from("  . repeat · o/O new track · >>/<< rotate"),
        Line::from("  m mute · @ track mode · space/s transport"),
        Line::from("  u/^r undo/redo (counts)"),
        Line::from("  :w/:e/:q patches · :set bpm/pulses/…"),
        Line::from(""),
        Line::from("Insert: Enter/space toggle · x/y/p step"),
        Line::from("  k/K j/J note ±st/oct · N<n> set note"),
        Line::from("  #P/#L/#R euclid · P/L/R re-apply/rotate"),
        Line::from(""),
        Line::from("  ? closes help"),
    ]
}

/// Wake position: Some(0..=2) when `i` trails the playhead by 1–3 steps.
fn wake_offset(i: usize, head: usize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let behind = (head + len - i) % len;
    if (1..=3).contains(&behind) {
        Some(behind - 1)
    } else {
        None
    }
}

fn snapshot_params(s: &SequencerState) -> state::SequencerParams {
    state::SequencerParams {
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
    }
}

/// Patch view of the params: musical content only — the transport play flag
/// is session state, not patch state (and would dirty the patch constantly).
fn patch_params(s: &SequencerState) -> state::SequencerParams {
    let mut p = snapshot_params(s);
    p.playing = None;
    p
}

/// Rebuild tracks (and bookkeeping vectors) from saved params.
fn apply_tracks(s: &mut SequencerState, params: &state::SequencerParams) {
    if params.tracks.is_empty() {
        return;
    }
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
    let len = s.tracks[s.current_track].length;
    s.selected = s.selected.min(len.saturating_sub(1));
}

pub fn run(instance: usize) -> Result<()> {
    // Setup SIGUSR1 handler for save-on-signal
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("sequencer", instance);
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let _ = manifest.register("sequencer", instance, None, 8);
    let claimed_base = manifest.claimed_base();
    
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
    state.lock().unwrap().mod_base = claimed_base;
    
    // Load saved state if available
    if let Ok(params) = state::load_module_state::<state::SequencerParams>("sequencer", instance) {
        let mut s = state.lock().unwrap();
        if let Some(bpm) = params.bpm { s.bpm = bpm; }
        if let Some(playing) = params.playing { s.playing = playing; }
        if !params.tracks.is_empty() {
            apply_tracks(&mut s, &params);
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
    let mut pending_g = false;
    // Operator awaiting its motion: (operator, count typed before it)
    let mut pending_op: Option<(Operator, usize)> = None;
    // f/t target being typed: (kind, operator context, count, digits, last key time)
    let mut pending_find: Option<(char, Option<Operator>, usize, String, Instant)> = None;
    // first half of a >> / << chord
    let mut pending_angle: Option<char> = None;
    // last change, for dot-repeat
    let mut last_change: Option<Action> = None;
    let mut gt_target: Option<String> = None;
    let mut gt_input = String::new();
    let mut gt_last_key: Option<Instant> = None;
    let mut history = History::new();
    let mut undo_msg: Option<String> = None;
    let mut undo_time: Option<Instant> = None;
    let mut ex = crate::excmd::ExLine::default();
    let mut patch_name: Option<String> = None;
    let mut baseline = state::to_toml_string(&patch_params(&state.lock().unwrap())).unwrap_or_default();
    let mut should_quit = false;

    loop {
        
        // Check for save-on-signal
        if state::check_save_signal() {
            let params = snapshot_params(&state.lock().unwrap());
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
                apply_tracks(&mut s, &params);
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

        // Auto-execute f#/t# on timeout (only when digits were collected)
        if let Some((kind, op, fcount, ref digits, t0)) = pending_find.clone() {
            if t0.elapsed() > Duration::from_millis(300) && !digits.is_empty() {
                pending_find = None;
                if let Ok(n) = digits.parse::<usize>() {
                    let motion = if kind == 'f' { Motion::Find(n) } else { Motion::Till(n) };
                    match op {
                        None => {
                            let mut s = state.lock().unwrap();
                            let len = s.track().length;
                            let n = n.min(len - 1);
                            s.selected = match (kind, n.cmp(&s.selected)) {
                                ('t', std::cmp::Ordering::Greater) => n - 1,
                                ('t', std::cmp::Ordering::Less) => n + 1,
                                ('t', std::cmp::Ordering::Equal) => s.selected,
                                _ => n,
                            };
                        }
                        Some(op) => {
                            let action = Action::Op { op, motion, count: fcount.max(1) };
                            undo_msg = apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            undo_time = Some(Instant::now());
                            if action.is_change() {
                                last_change = Some(action);
                            }
                            if op == Operator::Change {
                                mode = String::from("insert");
                            }
                        }
                    }
                }
            }
        }

        // Clear undo message after 2 seconds
        if let Some(t) = undo_time { if t.elapsed() > Duration::from_secs(2) { undo_msg = None; undo_time = None; } }
        let current_state = state.lock().unwrap().clone();
        let status_msg = if ex.is_active() { Some(ex.display()) } else { undo_msg.clone() };
        let pending_hint = if let Some((op, _)) = pending_op {
            Some(match op {
                Operator::Yank => "y…",
                Operator::Delete => "d…",
                Operator::Change => "c…",
            })
        } else if let Some((kind, _, _, ref d, _)) = pending_find {
            let _ = (kind, d);
            Some("f…")
        } else if pending_angle.is_some() {
            Some(">…")
        } else {
            None
        };
        draw_ui(&mut terminal, &current_state, &mode, &submode, &input_buffer, &pending_count, &gt_target, &gt_input, show_help, &status_msg, pending_hint)?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Undo/redo status message clears on the next keypress
                // (the undo/redo handlers below set a fresh one)
                undo_msg = None;
                undo_time = None;

                // Ex command line captures every key while open
                if ex.is_active() {
                    let candidates = crate::excmd::patch_names(&state::patches_dir());
                    if let crate::excmd::ExEvent::Submit(cmd) = ex.handle_key(key.code, &candidates) {
                        use crate::excmd::ExCommand;
                        match cmd {
                            ExCommand::Write(name) => {
                                let params = patch_params(&state.lock().unwrap());
                                undo_msg = Some(match crate::excmd::ex_write(name, &mut patch_name, &mut baseline, &params) {
                                    Ok(m) | Err(m) => m,
                                });
                            }
                            ExCommand::Edit(name) => match state::load_patch::<state::SequencerParams>(&name) {
                                Ok(p) => {
                                    let mut s = state.lock().unwrap();
                                    if let Some(bpm) = p.bpm { s.bpm = bpm; }
                                    apply_tracks(&mut s, &p);
                                    baseline = state::to_toml_string(&patch_params(&s)).unwrap_or_default();
                                    drop(s);
                                    history = History::new();
                                    patch_name = Some(name.clone());
                                    undo_msg = Some(format!("Loaded {}", name));
                                }
                                Err(e) => undo_msg = Some(e.to_string()),
                            },
                            ExCommand::Quit { force } => {
                                let params = patch_params(&state.lock().unwrap());
                                if !force && crate::excmd::is_dirty(&params, &baseline) {
                                    undo_msg = Some(String::from("Unsaved changes (:q! to discard, :w <name> to save)"));
                                } else {
                                    should_quit = true;
                                }
                            }
                            ExCommand::WriteQuit(name) => {
                                let params = patch_params(&state.lock().unwrap());
                                match crate::excmd::ex_write(name, &mut patch_name, &mut baseline, &params) {
                                    Ok(_) => should_quit = true,
                                    Err(m) => undo_msg = Some(m),
                                }
                            }
                            ExCommand::Set(k, v) => {
                                let mut s = state.lock().unwrap();
                                match k.as_str() {
                                    "bpm" => match v.parse::<f64>() {
                                        Ok(b) => {
                                            let old_bpm = s.bpm;
                                            s.bpm = b.clamp(20.0, 300.0);
                                            if old_bpm != s.bpm {
                                                history.push(Command::SetBpm { old_bpm, new_bpm: s.bpm });
                                            }
                                            undo_msg = Some(format!("bpm = {}", s.bpm));
                                        }
                                        Err(_) => undo_msg = Some(format!("Invalid bpm: {}", v)),
                                    },
                                    "pulses" | "length" | "rotation" => match v.parse::<usize>() {
                                        Ok(n) => {
                                            let tidx = s.current_track;
                                            let old = EuclidState::capture(&s.tracks[tidx]);
                                            match k.as_str() {
                                                "pulses" => s.tracks[tidx].pulses = n.min(16),
                                                "length" => {
                                                    s.tracks[tidx].length = n.clamp(1, 16);
                                                    let len = s.tracks[tidx].length;
                                                    s.selected = s.selected.min(len - 1);
                                                }
                                                _ => s.tracks[tidx].rotation = n.min(255),
                                            }
                                            let (p, l, r) = (s.tracks[tidx].pulses, s.tracks[tidx].length, s.tracks[tidx].rotation);
                                            euclidean_apply(&mut s.tracks[tidx].steps, p, l, r);
                                            let new = EuclidState::capture(&s.tracks[tidx]);
                                            push_track_params(&mut history, tidx, old, new);
                                            undo_msg = Some(format!("{} = {}", k, n));
                                        }
                                        Err(_) => undo_msg = Some(format!("Invalid {}: {}", k, v)),
                                    },
                                    _ => undo_msg = Some(format!("Unknown setting: {}", k)),
                                }
                            }
                            ExCommand::Unknown(c) => undo_msg = Some(format!("Not a command: {}", c)),
                        }
                        undo_time = Some(Instant::now());
                    }
                    if should_quit {
                        break;
                    }
                    continue;
                }

                // ':' opens the ex command line (any mode, when no prompt is active)
                if key.code == KeyCode::Char(':') && submode.is_empty() && gt_target.is_none() {
                    pending_count = None;
                    pending_g = false;
                    ex.open();
                    continue;
                }

                // Ctrl-s: save module state
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let params = snapshot_params(&state.lock().unwrap());
                    let _ = state::save_module_state("sequencer", instance, &params);
                    continue;
                }

                // Ctrl-r: redo (count-prefixed: 3<C-r> redoes 3 times)
                if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
                    let count = pending_count.take().and_then(|c| c.parse().ok()).unwrap_or(1).max(1);
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
                                        let action = Action::SetNote(note);
                                        apply_action(&mut s, &mut history, &action);
                                        last_change = Some(action);
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
                    show_help = !show_help;
                    continue;
                }

                // Esc: leave insert/visual mode, cancel all pending state
                if key.code == KeyCode::Esc {
                    if mode == "insert" || mode == "visual" || mode == "visual_line" {
                        mode = String::from("normal");
                    }
                    state.lock().unwrap().visual_anchor = None;
                    submode.clear();
                    input_buffer.clear();
                    pending_count = None;
                    pending_op = None;
                    pending_find = None;
                    pending_angle = None;
                    pending_g = false;
                    continue;
                }

                // f#/t# target collection (digits, executed on Enter,
                // non-digit key, or the 300ms timeout in the outer loop)
                if let Some((kind, op, fcount, ref mut digits, _)) = pending_find {
                    match key.code {
                        KeyCode::Char(c) if c.is_ascii_digit() => {
                            digits.push(c);
                            if let Some((_, _, _, _, ref mut t)) = pending_find {
                                *t = Instant::now();
                            }
                            continue;
                        }
                        _ => {
                            let digits = digits.clone();
                            pending_find = None;
                            if let Ok(n) = digits.parse::<usize>() {
                                let motion = if kind == 'f' { Motion::Find(n) } else { Motion::Till(n) };
                                match op {
                                    None => {
                                        let mut s = state.lock().unwrap();
                                        let len = s.track().length;
                                        let n = n.min(len - 1);
                                        s.selected = match (kind, n.cmp(&s.selected)) {
                                            ('t', std::cmp::Ordering::Greater) => n - 1,
                                            ('t', std::cmp::Ordering::Less) => n + 1,
                                            ('t', std::cmp::Ordering::Equal) => s.selected,
                                            _ => n,
                                        };
                                    }
                                    Some(op) => {
                                        let action = Action::Op { op, motion, count: fcount.max(1) };
                                        let mut s = state.lock().unwrap();
                                        undo_msg = apply_action(&mut s, &mut history, &action);
                                        undo_time = Some(Instant::now());
                                        drop(s);
                                        if action.is_change() {
                                            last_change = Some(action);
                                        }
                                        if op == Operator::Change {
                                            mode = String::from("insert");
                                        }
                                    }
                                }
                            }
                            if key.code == KeyCode::Enter {
                                continue;
                            }
                            // other keys execute the find, then fall through
                        }
                    }
                }

                // Operator-pending: the next key supplies the motion
                if let Some((op, opcount)) = pending_op {
                    if let KeyCode::Char(c) = key.code {
                        if c.is_ascii_digit() && !(c == '0' && pending_count.is_none()) {
                            pending_count.get_or_insert_with(String::new).push(c);
                            continue;
                        }
                    }
                    pending_op = None;
                    let mcount: usize = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                    let count = (opcount * mcount).max(1);
                    let motion = match key.code {
                        KeyCode::Char('h') | KeyCode::Left => Some(Motion::Left),
                        KeyCode::Char('l') | KeyCode::Right => Some(Motion::Right),
                        KeyCode::Char('w') => Some(Motion::WordFwd),
                        KeyCode::Char('b') => Some(Motion::WordBack),
                        KeyCode::Char('e') => Some(Motion::WordEnd),
                        KeyCode::Char('0') => Some(Motion::Home),
                        KeyCode::Char('$') => Some(Motion::End),
                        _ => None,
                    };
                    match key.code {
                        // doubled operator = whole track (yy / dd / cc)
                        KeyCode::Char(c2) if Operator::from_char(c2) == Some(op) => {
                            let action = Action::OpTrack(op);
                            undo_msg = apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            undo_time = Some(Instant::now());
                            if action.is_change() {
                                last_change = Some(action);
                            }
                            if op == Operator::Change {
                                mode = String::from("insert");
                            }
                        }
                        KeyCode::Char(c2 @ ('f' | 't')) => {
                            pending_find = Some((c2, Some(op), count, String::new(), Instant::now()));
                        }
                        _ => {
                            if let Some(m) = motion {
                                let action = Action::Op { op, motion: m, count };
                                undo_msg = apply_action(&mut state.lock().unwrap(), &mut history, &action);
                                undo_time = Some(Instant::now());
                                if action.is_change() {
                                    last_change = Some(action);
                                }
                                if op == Operator::Change {
                                    mode = String::from("insert");
                                }
                            }
                            // anything else cancels the operator
                        }
                    }
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
                        continue;
                    }
                }

                // Mode-independent navigation (cursor wraps around the pattern)
                match key.code {
                    KeyCode::Char('l') | KeyCode::Right => {
                        let n = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                        let mut s = state.lock().unwrap();
                        let len = s.track().length;
                        s.selected = (s.selected + n) % len;
                        continue;
                    }
                    KeyCode::Char('h') | KeyCode::Left => {
                        let n: usize = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                        let mut s = state.lock().unwrap();
                        let len = s.track().length;
                        s.selected = (s.selected + len - (n % len)) % len;
                        continue;
                    }
                    KeyCode::Char('w') | KeyCode::Char('b') | KeyCode::Char('e') => {
                        let m = match key.code {
                            KeyCode::Char('w') => Motion::WordFwd,
                            KeyCode::Char('b') => Motion::WordBack,
                            _ => Motion::WordEnd,
                        };
                        let n = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                        let mut s = state.lock().unwrap();
                        let len = s.track().length;
                        for _ in 0..n {
                            let cur = s.selected;
                            let next = nav_word(&s.track().steps, len, cur, m);
                            s.selected = next;
                        }
                        continue;
                    }
                    KeyCode::Char('$') => {
                        pending_count = None;
                        let mut s = state.lock().unwrap();
                        s.selected = s.track().length - 1;
                        continue;
                    }
                    KeyCode::Char(c @ ('f' | 't')) if submode.is_empty() => {
                        pending_count = None;
                        pending_find = Some((c, None, 1, String::new(), Instant::now()));
                        continue;
                    }
                    _ => {}
                }

                // Count-prefixed Euclidean setters (both modes): #P #L #R
                if pending_count.is_some() {
                    let eop = match key.code {
                        KeyCode::Char('P') => Some(EuclidOp::Pulses(0)),
                        KeyCode::Char('L') => Some(EuclidOp::Length(0)),
                        KeyCode::Char('R') => Some(EuclidOp::Rotation(0)),
                        _ => None,
                    };
                    if let Some(eop) = eop {
                        let n: usize = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(0);
                        let eop = match eop {
                            EuclidOp::Pulses(_) => EuclidOp::Pulses(n),
                            EuclidOp::Length(_) => EuclidOp::Length(n),
                            _ => EuclidOp::Rotation(n),
                        };
                        let action = Action::Euclid(eop);
                        apply_action(&mut state.lock().unwrap(), &mut history, &action);
                        last_change = Some(action);
                        continue;
                    }
                }

                // Visual mode: motions extend the selection; operators act on it
                if mode == "visual" || mode == "visual_line" {
                    let linewise = mode == "visual_line";
                    match key.code {
                        KeyCode::Char('v') | KeyCode::Char('V') => {
                            mode = String::from("normal");
                            state.lock().unwrap().visual_anchor = None;
                        }
                        KeyCode::Char('o') => {
                            let mut s = state.lock().unwrap();
                            if let Some(a) = s.visual_anchor {
                                s.visual_anchor = Some(s.selected);
                                s.selected = a;
                            }
                        }
                        KeyCode::Char(c @ ('y' | 'd' | 'c' | 'x' | '~')) => {
                            let action = {
                                let mut s = state.lock().unwrap();
                                let anchor = s.visual_anchor.take().unwrap_or(s.selected);
                                let (a, b) = (anchor.min(s.selected), anchor.max(s.selected));
                                s.selected = a;
                                let span = b - a + 1;
                                if linewise {
                                    match c {
                                        'y' => Action::OpTrack(Operator::Yank),
                                        'c' => Action::OpTrack(Operator::Change),
                                        _ => Action::OpTrack(Operator::Delete),
                                    }
                                } else {
                                    match c {
                                        '~' => Action::ToggleSpan(span),
                                        'y' => Action::Op { op: Operator::Yank, motion: Motion::Span(span), count: 1 },
                                        'c' => Action::Op { op: Operator::Change, motion: Motion::Span(span), count: 1 },
                                        _ => Action::Op { op: Operator::Delete, motion: Motion::Span(span), count: 1 },
                                    }
                                }
                            };
                            undo_msg = apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            undo_time = Some(Instant::now());
                            let to_insert = matches!(
                                action,
                                Action::Op { op: Operator::Change, .. } | Action::OpTrack(Operator::Change)
                            );
                            if action.is_change() {
                                last_change = Some(action);
                            }
                            mode = if to_insert {
                                String::from("insert")
                            } else {
                                String::from("normal")
                            };
                        }
                        _ => {
                            pending_count = None;
                        }
                    }
                    continue;
                }

                if mode == "normal" {
                    match key.code {
                        // Enter insert mode
                        KeyCode::Enter | KeyCode::Char('i') => {
                            pending_count = None;
                            pending_g = false;
                            mode = String::from("insert");
                        }
                        KeyCode::Char('v') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            s.visual_anchor = Some(sel);
                            mode = String::from("visual");
                        }
                        KeyCode::Char('V') => {
                            pending_count = None;
                            mode = String::from("visual_line");
                        }

                        // Operators (await motion; doubled = whole track)
                        KeyCode::Char(c @ ('y' | 'd' | 'c')) => {
                            let opcount: usize =
                                pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            pending_op = Operator::from_char(c).map(|op| (op, opcount.max(1)));
                            pending_g = false;
                        }

                        // Step edits
                        KeyCode::Char('x') => {
                            pending_count = None;
                            let action = Action::CutStep;
                            apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('~') => {
                            pending_count = None;
                            let action = Action::ToggleStep;
                            apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('p') => {
                            let times: usize =
                                pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let action = Action::Paste { before: false, times: times.max(1) };
                            undo_msg = apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            undo_time = Some(Instant::now());
                            last_change = Some(action);
                        }
                        KeyCode::Char('P') => {
                            // bare P only — counted #P is the Euclidean pulses setter
                            pending_count = None;
                            let action = Action::Paste { before: true, times: 1 };
                            undo_msg = apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            undo_time = Some(Instant::now());
                            last_change = Some(action);
                        }
                        KeyCode::Char('.') => {
                            pending_count = None;
                            if let Some(action) = last_change.clone() {
                                undo_msg = apply_action(&mut state.lock().unwrap(), &mut history, &action);
                                undo_time = Some(Instant::now());
                                if matches!(
                                    action,
                                    Action::Op { op: Operator::Change, .. } | Action::OpTrack(Operator::Change)
                                ) {
                                    mode = String::from("insert");
                                }
                            } else {
                                undo_msg = Some(String::from("Nothing to repeat"));
                                undo_time = Some(Instant::now());
                            }
                        }

                        // Track ops
                        KeyCode::Char('o') | KeyCode::Char('n') => {
                            pending_count = None;
                            pending_g = false;
                            let action = Action::NewTrack { before: false };
                            apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('O') => {
                            pending_count = None;
                            let action = Action::NewTrack { before: true };
                            apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('m') => {
                            pending_count = None;
                            pending_g = false;
                            let action = Action::ToggleMute;
                            apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('@') => {
                            pending_count = None;
                            pending_g = false;
                            let action = Action::ToggleMode;
                            apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char(c @ ('>' | '<')) => {
                            let n: i32 = pending_count
                                .take()
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(1);
                            if pending_angle == Some(c) {
                                pending_angle = None;
                                let action = Action::Rotate { steps: if c == '>' { n } else { -n } };
                                apply_action(&mut state.lock().unwrap(), &mut history, &action);
                                last_change = Some(action);
                            } else {
                                pending_angle = Some(c);
                                // keep the count for the second key
                                if n > 1 {
                                    pending_count = Some(n.to_string());
                                }
                            }
                        }

                        // Track navigation
                        KeyCode::Char('k') | KeyCode::Up => {
                            let n: usize = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            s.current_track = s.current_track.saturating_sub(n);
                            s.selected = 0;
                            pending_g = false;
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            let n: usize = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            s.current_track = (s.current_track + n).min(s.tracks.len() - 1);
                            s.selected = 0;
                            pending_g = false;
                        }
                        KeyCode::Char('[') => {
                            pending_count = None;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            if s.current_track > 0 {
                                s.current_track -= 1;
                            }
                            s.selected = 0;
                        }
                        KeyCode::Char(']') => {
                            pending_count = None;
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
                                pending_g = false;
                                let mut s = state.lock().unwrap();
                                s.current_track = 0;
                                s.selected = 0;
                            } else {
                                pending_g = true;
                            }
                        }
                        KeyCode::Char('G') => {
                            pending_count = None;
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            s.current_track = s.tracks.len() - 1;
                            s.selected = 0;
                        }

                        // Transport (writes the global SHM play flag; the
                        // sequencer thread mirrors it back into s.playing)
                        KeyCode::Char(' ') => {
                            pending_count = None;
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
                            pending_g = false;
                            let mut s = state.lock().unwrap();
                            undo_msg = Some(history_status("Undo", count, || history.undo(&mut s)));
                            undo_time = Some(Instant::now());
                        }

                        _ => {
                            pending_count = None;
                            pending_g = false;
                            pending_angle = None;
                        }
                    }
                } else {
                    // Insert mode: direct step entry & tuning
                    match key.code {
                        KeyCode::Enter | KeyCode::Char(' ') => {
                            pending_count = None;
                            let action = Action::ToggleStep;
                            apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('x') => {
                            pending_count = None;
                            let action = Action::CutStep;
                            apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('y') => {
                            pending_count = None;
                            undo_msg = apply_action(&mut state.lock().unwrap(), &mut history, &Action::YankStep);
                            undo_time = Some(Instant::now());
                        }
                        KeyCode::Char('p') => {
                            let times: usize =
                                pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let action = Action::Paste { before: false, times: times.max(1) };
                            undo_msg = apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            undo_time = Some(Instant::now());
                            last_change = Some(action);
                        }
                        KeyCode::Char('v') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.selected;
                            s.visual_anchor = Some(sel);
                            mode = String::from("visual");
                        }
                        KeyCode::Char('.') => {
                            pending_count = None;
                            if let Some(action) = last_change.clone() {
                                undo_msg = apply_action(&mut state.lock().unwrap(), &mut history, &action);
                                undo_time = Some(Instant::now());
                            }
                        }
                        KeyCode::Char(c @ ('k' | 'j' | 'K' | 'J')) => {
                            let n: i32 = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let (note, mod_value) = match c {
                                'k' => (1, 0.01),
                                'j' => (-1, -0.01),
                                'K' => (12, 0.1),
                                _ => (-12, -0.1),
                            };
                            let action = Action::Transpose { note: note * n, mod_value: mod_value * n as f32 };
                            apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            last_change = Some(action);
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
                        KeyCode::Char('N') => {
                            pending_count = None;
                            submode = String::from("note");
                            input_buffer.clear();
                        }
                        // Euclidean re-apply (no count)
                        KeyCode::Char('P') | KeyCode::Char('L') => {
                            pending_count = None;
                            let action = Action::Euclid(EuclidOp::Reapply);
                            apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('R') => {
                            pending_count = None;
                            let action = Action::Euclid(EuclidOp::RotatePlus(1));
                            apply_action(&mut state.lock().unwrap(), &mut history, &action);
                            last_change = Some(action);
                        }
                        _ => {
                            pending_count = None;
                            pending_g = false;
                        }
                    }
                }
            }
        }
        if should_quit {
            break;
        }
    }

    crossterm::terminal::disable_raw_mode()?;
    execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    Ok(())
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

    /// All edit helpers go through apply_action — the same code path the
    /// key handlers use.
    fn toggle_step(s: &mut SequencerState, h: &mut History) {
        apply_action(s, h, &Action::ToggleStep);
    }

    fn cut_step(s: &mut SequencerState, h: &mut History) {
        apply_action(s, h, &Action::CutStep);
    }

    fn paste_step(s: &mut SequencerState, h: &mut History) {
        apply_action(s, h, &Action::Paste { before: false, times: 1 });
    }

    fn transpose_up(s: &mut SequencerState, h: &mut History) {
        apply_action(s, h, &Action::Transpose { note: 1, mod_value: 0.01 });
    }

    fn new_track(s: &mut SequencerState, h: &mut History) {
        apply_action(s, h, &Action::NewTrack { before: false });
    }

    fn delete_track(s: &mut SequencerState, h: &mut History) {
        apply_action(s, h, &Action::OpTrack(Operator::Delete));
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
        s.register = Some(Register::Steps(vec![Step { active: true, note: 72, velocity: 90, mod_value: 0.0 }]));
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

    // ── grammar tests ───────────────────────────────────────────────────

    /// Track with words at steps 2-3 and 8-10 (length 16).
    fn track_with_words(s: &mut SequencerState) {
        for st in s.tracks[0].steps.iter_mut() {
            st.active = false;
        }
        for i in [2, 3, 8, 9, 10] {
            s.tracks[0].steps[i].active = true;
        }
    }

    #[test]
    fn word_motions_navigate_runs() {
        let mut s = state_with_tracks(1);
        track_with_words(&mut s);
        let steps = &s.tracks[0].steps;
        assert_eq!(nav_word(steps, 16, 0, Motion::WordFwd), 2, "w to first word start");
        assert_eq!(nav_word(steps, 16, 2, Motion::WordFwd), 8, "w skips within-word steps");
        assert_eq!(nav_word(steps, 16, 8, Motion::WordFwd), 2, "w wraps");
        assert_eq!(nav_word(steps, 16, 8, Motion::WordBack), 2, "b to previous word start");
        assert_eq!(nav_word(steps, 16, 0, Motion::WordEnd), 3, "e to word end");
        assert_eq!(nav_word(steps, 16, 3, Motion::WordEnd), 10, "e to next word end");
    }

    #[test]
    fn motion_ranges_follow_vi_semantics() {
        let mut s = state_with_tracks(1);
        track_with_words(&mut s);
        let t = &s.tracks[0];
        assert_eq!(motion_range(t, 2, Motion::WordFwd, 1), Some((2, 7)), "dw eats up to next word");
        assert_eq!(motion_range(t, 8, Motion::WordFwd, 1), Some((8, 15)), "dw at last word eats to end");
        assert_eq!(motion_range(t, 2, Motion::WordEnd, 1), Some((2, 3)), "de is inclusive");
        assert_eq!(motion_range(t, 8, Motion::WordBack, 1), Some((2, 7)), "db back to word start");
        assert_eq!(motion_range(t, 5, Motion::End, 1), Some((5, 15)));
        assert_eq!(motion_range(t, 5, Motion::Home, 1), Some((0, 4)));
        assert_eq!(motion_range(t, 0, Motion::Home, 1), None, "d0 at 0 is a no-op");
        assert_eq!(motion_range(t, 2, Motion::Till(8), 1), Some((2, 7)), "t is exclusive");
        assert_eq!(motion_range(t, 2, Motion::Find(8), 1), Some((2, 8)), "f is inclusive");
        assert_eq!(motion_range(t, 8, Motion::Till(2), 1), Some((3, 8)), "t works backward");
        assert_eq!(motion_range(t, 4, Motion::Right, 3), Some((4, 6)), "3l range");
        assert_eq!(motion_range(t, 4, Motion::Span(4), 1), Some((4, 7)));
    }

    #[test]
    fn yank_word_and_paste() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        track_with_words(&mut s);
        s.tracks[0].steps[8].note = 72;
        s.selected = 8;
        // yw at step 8: yanks 8..=15 (last word to end)
        let msg = apply_action(&mut s, &mut h, &Action::Op { op: Operator::Yank, motion: Motion::WordEnd, count: 1 });
        assert_eq!(msg.as_deref(), Some("Yanked 3 steps")); // ye: 8..=10
        // paste at step 12 overwrites 12..=14
        s.selected = 12;
        apply_action(&mut s, &mut h, &Action::Paste { before: false, times: 1 });
        assert!(s.tracks[0].steps[12].active);
        assert_eq!(s.tracks[0].steps[12].note, 72);
        assert!(s.tracks[0].steps[13].active && s.tracks[0].steps[14].active);
        // undo restores
        assert!(h.undo(&mut s).is_some());
        assert!(!s.tracks[0].steps[12].active);
    }

    #[test]
    fn delete_word_clears_and_yanks() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        track_with_words(&mut s);
        s.selected = 8;
        apply_action(&mut s, &mut h, &Action::Op { op: Operator::Delete, motion: Motion::WordEnd, count: 1 });
        assert!(!s.tracks[0].steps[8].active && !s.tracks[0].steps[9].active && !s.tracks[0].steps[10].active);
        match s.register {
            Some(Register::Steps(ref v)) => {
                assert_eq!(v.len(), 3);
                assert!(v[0].active, "register holds the pre-delete steps");
            }
            _ => panic!("register should hold steps"),
        }
        assert!(h.undo(&mut s).is_some());
        assert!(s.tracks[0].steps[8].active, "undo restores the word");
    }

    #[test]
    fn paste_before_ends_at_cursor() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.register = Some(Register::Steps(vec![
            Step { active: true, note: 60, velocity: 100, mod_value: 0.0 },
            Step { active: true, note: 62, velocity: 100, mod_value: 0.0 },
        ]));
        s.selected = 5;
        apply_action(&mut s, &mut h, &Action::Paste { before: true, times: 1 });
        assert_eq!(s.tracks[0].steps[4].note, 60);
        assert_eq!(s.tracks[0].steps[5].note, 62);
        assert_eq!(s.selected, 4, "cursor moves to paste start");
    }

    #[test]
    fn track_register_roundtrip() {
        let mut s = state_with_tracks(2);
        let mut h = History::new();
        s.tracks[0].steps[7].note = 99;
        apply_action(&mut s, &mut h, &Action::OpTrack(Operator::Yank));
        s.current_track = 1;
        apply_action(&mut s, &mut h, &Action::Paste { before: false, times: 1 });
        assert_eq!(s.tracks.len(), 3);
        assert_eq!(s.tracks[2].steps[7].note, 99, "track pasted after current");
        assert!(h.undo(&mut s).is_some());
        assert_eq!(s.tracks.len(), 2);
    }

    #[test]
    fn delete_track_fills_register() {
        let mut s = state_with_tracks(2);
        let mut h = History::new();
        apply_action(&mut s, &mut h, &Action::OpTrack(Operator::Delete));
        assert_eq!(s.tracks.len(), 1);
        assert!(matches!(s.register, Some(Register::Track(_))));
        // last track refuses
        let msg = apply_action(&mut s, &mut h, &Action::OpTrack(Operator::Delete));
        assert_eq!(msg.as_deref(), Some("Can't delete the last track"));
        assert_eq!(s.tracks.len(), 1);
    }

    #[test]
    fn change_track_clears_steps_into_register() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        apply_action(&mut s, &mut h, &Action::OpTrack(Operator::Change));
        assert!(s.tracks[0].steps.iter().all(|st| !st.active));
        assert!(matches!(s.register, Some(Register::Steps(ref v)) if v.len() == 16));
        assert!(h.undo(&mut s).is_some());
        assert!(s.tracks[0].steps[0].active, "cc undo restores pattern");
    }

    #[test]
    fn rotate_shifts_pattern() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        // default pattern: active at 0,4,8,12
        apply_action(&mut s, &mut h, &Action::Rotate { steps: 1 });
        assert!(!s.tracks[0].steps[0].active);
        assert!(s.tracks[0].steps[1].active && s.tracks[0].steps[5].active);
        apply_action(&mut s, &mut h, &Action::Rotate { steps: -1 });
        assert!(s.tracks[0].steps[0].active, "<< rotates back");
        assert!(h.undo(&mut s).is_some() && h.undo(&mut s).is_some());
        assert!(s.tracks[0].steps[0].active);
    }

    #[test]
    fn toggle_span_flips_range() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.selected = 0;
        apply_action(&mut s, &mut h, &Action::ToggleSpan(5));
        // steps 0..=4: 0 and 4 were active -> now inactive; 1,2,3 now active
        assert!(!s.tracks[0].steps[0].active && !s.tracks[0].steps[4].active);
        assert!(s.tracks[0].steps[1].active && s.tracks[0].steps[3].active);
        assert!(h.undo(&mut s).is_some());
        assert!(s.tracks[0].steps[0].active);
    }

    #[test]
    fn dot_repeat_actions_are_changes_only() {
        assert!(Action::ToggleStep.is_change());
        assert!(Action::CutStep.is_change());
        assert!(Action::Op { op: Operator::Delete, motion: Motion::WordFwd, count: 1 }.is_change());
        assert!(!Action::YankStep.is_change());
        assert!(!Action::Op { op: Operator::Yank, motion: Motion::WordFwd, count: 1 }.is_change());
        assert!(!Action::OpTrack(Operator::Yank).is_change());
        assert!(Action::OpTrack(Operator::Delete).is_change());
    }

    #[test]
    fn dot_repeat_replays_at_new_cursor() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.selected = 1;
        let action = Action::ToggleStep;
        apply_action(&mut s, &mut h, &action);
        assert!(s.tracks[0].steps[1].active);
        // "." at a new cursor
        s.selected = 2;
        apply_action(&mut s, &mut h, &action);
        assert!(s.tracks[0].steps[2].active);
        assert_eq!(h.commands.len(), 2, "each repeat is its own undo entry");
    }

    #[test]
    fn new_track_before_and_after() {
        let mut s = state_with_tracks(2);
        let mut h = History::new();
        s.current_track = 0;
        s.tracks[0].steps[0].note = 11;
        apply_action(&mut s, &mut h, &Action::NewTrack { before: false });
        assert_eq!(s.tracks.len(), 3);
        assert_eq!(s.current_track, 1, "o lands on the new track");
        apply_action(&mut s, &mut h, &Action::NewTrack { before: true });
        assert_eq!(s.tracks.len(), 4);
        assert_eq!(s.current_track, 1, "O inserts before, cursor on it");
        assert!(h.undo(&mut s).is_some() && h.undo(&mut s).is_some());
        assert_eq!(s.tracks.len(), 2);
        assert_eq!(s.tracks[0].steps[0].note, 11);
    }

    #[test]
    fn counted_paste_repeats_register() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        for st in s.tracks[0].steps.iter_mut() {
            st.active = false;
        }
        s.register = Some(Register::Steps(vec![Step { active: true, note: 65, velocity: 100, mod_value: 0.0 }]));
        s.selected = 3;
        apply_action(&mut s, &mut h, &Action::Paste { before: false, times: 3 });
        assert!(s.tracks[0].steps[3].active && s.tracks[0].steps[4].active && s.tracks[0].steps[5].active);
        assert!(!s.tracks[0].steps[6].active);
    }

    #[test]
    fn transpose_sweep_coalesces() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        for _ in 0..5 {
            apply_action(&mut s, &mut h, &Action::Transpose { note: 1, mod_value: 0.01 });
        }
        assert_eq!(s.tracks[0].steps[0].note, 65);
        assert_eq!(h.commands.len(), 1, "sweep is one undo entry");
        assert!(h.undo(&mut s).is_some());
        assert_eq!(s.tracks[0].steps[0].note, 60, "one u reverts the whole sweep");
    }

    #[test]
    fn round_trip_sweep_leaves_no_history() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        apply_action(&mut s, &mut h, &Action::Transpose { note: 3, mod_value: 0.0 });
        apply_action(&mut s, &mut h, &Action::Transpose { note: -3, mod_value: 0.0 });
        assert_eq!(s.tracks[0].steps[0].note, 60);
        assert_eq!(h.undo(&mut s), None, "no-op sweep records nothing");
    }

    // ── backfill: helpers that predate the test suite ───────────────────

    #[test]
    fn euclidean_distributes_pulses() {
        let mut steps = vec![Step::default(); 16];
        euclidean_apply(&mut steps, 4, 16, 0);
        let active: Vec<usize> = (0..16).filter(|&i| steps[i].active).collect();
        assert_eq!(active.len(), 4);
        // 4 pulses over 16 steps must be evenly spaced
        assert_eq!(active[1] - active[0], 4);
        assert_eq!(active[2] - active[1], 4);
        assert_eq!(active[3] - active[2], 4);
    }

    #[test]
    fn euclidean_rotation_shifts_pattern() {
        let mut a = vec![Step::default(); 16];
        let mut b = vec![Step::default(); 16];
        euclidean_apply(&mut a, 4, 16, 0);
        euclidean_apply(&mut b, 4, 16, 1);
        for i in 0..16 {
            assert_eq!(a[i].active, b[(i + 1) % 16].active, "rotation shifts by one at {}", i);
        }
    }

    #[test]
    fn euclidean_short_length_deactivates_tail() {
        let mut steps = vec![Step::default(); 16];
        for st in steps.iter_mut() {
            st.active = true;
        }
        euclidean_apply(&mut steps, 2, 8, 0);
        assert!(steps[8..].iter().all(|s| !s.active), "steps beyond length must be off");
        assert_eq!(steps[..8].iter().filter(|s| s.active).count(), 2);
    }

    #[test]
    fn euclidean_zero_and_full_pulses() {
        let mut steps = vec![Step::default(); 16];
        euclidean_apply(&mut steps, 0, 16, 0);
        assert!(steps.iter().all(|s| !s.active));
        euclidean_apply(&mut steps, 16, 16, 0);
        assert!(steps.iter().all(|s| s.active));
    }

    #[test]
    fn midi_note_names() {
        assert_eq!(midi_note_name(60), "C4");
        assert_eq!(midi_note_name(69), "A4");
        assert_eq!(midi_note_name(61), "C#4");
        assert_eq!(midi_note_name(0), "C0"); // octave saturates at 0
        assert_eq!(midi_note_name(127), "G9");
    }

    #[test]
    fn focus_clamps_track_and_step() {
        let mut s = state_with_tracks(2);
        s.tracks[1].length = 8;
        s.focus(9, Some(20));
        assert_eq!(s.current_track, 1, "track clamps to last");
        assert_eq!(s.selected, 7, "step clamps to track length");
    }

    #[test]
    fn wake_trails_the_playhead_and_wraps() {
        assert_eq!(wake_offset(8, 9, 16), Some(0), "one behind");
        assert_eq!(wake_offset(7, 9, 16), Some(1));
        assert_eq!(wake_offset(6, 9, 16), Some(2));
        assert_eq!(wake_offset(5, 9, 16), None, "wake is 3 cells");
        assert_eq!(wake_offset(9, 9, 16), None, "the head is not its own wake");
        assert_eq!(wake_offset(15, 1, 16), Some(1), "wraps across the bar line");
    }
}
