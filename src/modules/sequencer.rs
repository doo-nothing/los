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

/// Hard ceiling on pattern length. Tracks default to 16; crank `#L` or
/// `:set length 128` for the long game — the UI scrolls, nothing clips.
const NUM_STEPS: usize = 128;

/// The pane height at which this module renders with zero waste:
/// header + track rows + rule + detail strip (3) + rule + modeline.
/// `conductor::house_dims` snaps the SEQ pane to this — if the draw
/// gains or loses a line, update the arithmetic here and the layout
/// follows (the geometry tests pin the relationship).
pub const CONTENT_LINES: usize = crate::NUM_TRACKS + 7;

/// Per-step mod-in binding: `source`'s live modbus value offsets `target`
/// at trigger time, scaled by `amount` (docs/plans/sequencer-v2.md §5).
#[derive(Debug, Clone, PartialEq)]
struct StepBind {
    target: state::BindTarget,
    source: String,
    amount: f32,
}

#[derive(Debug, Clone, PartialEq)]
struct Step {
    active: bool,
    /// MIDI note on unscaled tracks; scale degree biased at 60 when the
    /// track has a scale (60 = root, 61 = one degree up, …).
    note: u8,
    velocity: u8,
    mod_value: f32,
    /// Trigger probability 0–100.
    prob: u8,
    bind: Option<StepBind>,
}

impl Default for Step {
    fn default() -> Self {
        Self { active: false, note: 60, velocity: 100, mod_value: 0.0, prob: 100, bind: None }
    }
}

/// Number of pattern slots per track (`a`–`h`).
const NUM_SLOTS: usize = 8;

/// One pattern: what `"a`–`"h` switch between. The active slot's data
/// lives inline in [`Track`]; inactive slots are parked here.
#[derive(Debug, Clone, PartialEq)]
struct PatternData {
    steps: Vec<Step>,
    length: usize,
    pulses: usize,
    rotation: usize,
}

impl PatternData {
    /// A silent 16-step pattern — what an untouched slot holds.
    fn empty() -> Self {
        Self { steps: vec![Step::default(); NUM_STEPS], length: 16, pulses: 0, rotation: 0 }
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
    cycle: state::CycleMode,
    /// Cents-based scale; `None` = chromatic 12-TET MIDI (the default).
    scale: Option<crate::theory::scales::Scale>,
    /// MIDI root note the scale hangs from.
    root: u8,
    /// Active pattern slot index 0–7 (`a`–`h`).
    active_slot: usize,
    /// Parked pattern slots; `slots[active_slot]` is `None` (its data is
    /// inline above). Untouched slots are `None` and read as empty.
    slots: [Option<PatternData>; NUM_SLOTS],
}

impl Track {
    fn new() -> Self {
        let mut t = Track::empty();
        for i in (0..16).step_by(4) {
            t.steps[i].active = true;
        }
        t.pulses = 5;
        t
    }

    /// A silent 16-step track (no pulses) — the blank-slate default.
    fn empty() -> Self {
        Self {
            steps: vec![Step::default(); NUM_STEPS],
            length: 16,
            pulses: 0,
            rotation: 0,
            muted: false,
            mode: state::TrackMode::Note,
            cycle: state::CycleMode::Forward,
            scale: None,
            root: 60,
            active_slot: 0,
            slots: Default::default(),
        }
    }

    /// A note track from (step, midi-note) pairs; step 0 gets an accent.
    fn with_melody(notes: &[(usize, u8)]) -> Self {
        let mut t = Track::empty();
        for &(i, note) in notes {
            t.steps[i].active = true;
            t.steps[i].note = note;
            t.steps[i].velocity = if i == 0 { 112 } else { 100 };
        }
        t.pulses = notes.len();
        t
    }
}

/// The fresh-session pattern: t1 carries a melody and t3 a bass line
/// (wired by default into maths ch1/ch3 -> voice 0/1 amps); t2 and t4 are
/// modulation tracks left empty for patching; the rest are blank slates.
fn default_tracks(count: usize) -> Vec<Track> {
    let mut tracks: Vec<Track> = (0..count).map(|_| Track::empty()).collect();
    if let Some(t) = tracks.get_mut(0) {
        // A minor lead: arpeggio up, answer down
        *t = Track::with_melody(&[
            (0, 69),  // A4
            (2, 72),  // C5
            (4, 76),  // E5
            (7, 74),  // D5
            (8, 72),  // C5
            (11, 67), // G4
            (12, 69), // A4
            (14, 71), // B4
        ]);
    }
    if let Some(t) = tracks.get_mut(2) {
        // bass roots: Am . . . | F . G .
        *t = Track::with_melody(&[(0, 45), (4, 45), (8, 41), (12, 43)]);
    }
    for i in [1usize, 3] {
        if let Some(t) = tracks.get_mut(i) {
            t.mode = state::TrackMode::Modulation;
        }
    }
    tracks
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
    /// Persistent per-track marks (`X` toggles, `gX` clears) — the
    /// non-consecutive multi-select. Kept in sync with `tracks`.
    marks: Vec<bool>,
    /// Track row the V-line selection anchors on (`V` + j/k extends).
    visual_track_anchor: Option<usize>,
    /// Active value layer: what the grid shows and what k/j/N edit.
    layer: state::BindTarget,
}

impl Default for SequencerState {
    fn default() -> Self {
        let track_count = crate::NUM_TRACKS;
        Self {
            tracks: default_tracks(track_count),
            current_track: 0,
            bpm: 120.0,
            // a fresh session waits for Space — never opens with sound
            playing: false,
            current_steps: vec![0; track_count],
            selected: 0,
            last_notes: vec![None; track_count],
            register: None,
            visual_anchor: None,
            mod_base: None,
            marks: vec![false; track_count],
            visual_track_anchor: None,
            layer: state::BindTarget::Note,
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
    SetCycle {
        track: usize,
        old: state::CycleMode,
        new: state::CycleMode,
    },
    SwitchSlot {
        track: usize,
        from: usize,
        to: usize,
    },
    SaveSlot {
        track: usize,
        slot: usize,
        old: Option<PatternData>,
        new: PatternData,
    },
    /// `:scale` — stores the whole track because retuning rewrites every
    /// step's pitch representation (inline pattern AND parked slots).
    SetScale {
        track: usize,
        old: Box<Track>,
        new: Box<Track>,
    },
    /// A multi-track edit (marks / V-line): one `u` reverts all of it.
    Group {
        cmds: Vec<Command>,
        desc: &'static str,
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
    Track(Box<Track>),
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
    /// paste register contents *into* the track at the cursor (a yanked
    /// track overwrites steps here; `gp` makes new tracks instead)
    Paste { before: bool, times: usize },
    /// k/j/K/J on the active value layer (note/velocity/prob/mod);
    /// the note layer keeps the legacy modulation-track behavior
    Adjust { layer: state::BindTarget, steps: i32, coarse: bool },
    /// the `N` prompt: set the active layer's value outright
    SetStepValue { layer: state::BindTarget, value: f32 },
    Op { op: Operator, motion: Motion, count: usize },
    OpTrack(Operator),
    ToggleMute,
    ToggleMode,
    NewTrack { before: bool },
    /// `gp`/`gP`: materialize the register as a new track
    PasteAsTrack { before: bool, times: usize },
    Rotate { steps: i32 },
    Euclid(EuclidOp),
    /// `gc`/`gC`, `:set cycle` — absolute so dot-repeat is predictable
    SetCycle(state::CycleMode),
    /// `"a`–`"h`: bring a pattern slot to the front
    SwitchSlot(usize),
    /// `"A`–`"H`: copy the active pattern into a slot
    SaveSlot(usize),
    /// `:fill`, `F` — auto-fill the pattern with a generator
    Fill { kind: state::FillKind, arg: f32, seed: u64 },
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
            s.tracks[tidx].steps[a..=b].clone_from_slice(&new);
            h.push(Command::EditSteps { track: tidx, start: a, old, new });
            s.selected = a;
            None
        }
        Action::CutStep => {
            let sel = s.selected;
            let old_step = s.tracks[tidx].steps[sel].clone();
            s.register = Some(Register::Steps(vec![old_step.clone()]));
            s.tracks[tidx].steps[sel].active = false;
            let new_step = s.tracks[tidx].steps[sel].clone();
            push_step_edit(h, tidx, sel, old_step, new_step);
            None
        }
        Action::YankStep => {
            let sel = s.selected;
            s.register = Some(Register::Steps(vec![s.tracks[tidx].steps[sel].clone()]));
            Some(String::from("Yanked 1 step"))
        }
        Action::Paste { before, times } => match s.register.clone() {
            None => Some(String::from("Nothing in register")),
            Some(reg) => {
                // both register kinds paste INTO the track at the cursor —
                // a yanked track contributes its pattern's steps (gp/gP
                // materialize registers as new tracks instead)
                let slice: Vec<Step> = match reg {
                    Register::Steps(v) => v,
                    Register::Track(t) => t.steps[..t.length].to_vec(),
                };
                if slice.is_empty() {
                    return Some(String::from("Nothing in register"));
                }
                let len = s.tracks[tidx].length;
                let total = (slice.len() * times.max(&1)).min(len);
                let start = if *before {
                    (s.selected + 1).saturating_sub(total)
                } else {
                    s.selected.min(len - 1)
                };
                let end = (start + total - 1).min(len - 1);
                let old: Vec<Step> = s.tracks[tidx].steps[start..=end].to_vec();
                let new: Vec<Step> = (0..=(end - start)).map(|i| slice[i % slice.len()].clone()).collect();
                if old == new {
                    return None;
                }
                s.tracks[tidx].steps[start..=end].clone_from_slice(&new);
                h.push(Command::EditSteps { track: tidx, start, old, new });
                s.selected = start;
                None
            }
        },
        Action::PasteAsTrack { before, times } => match s.register.clone() {
            None => Some(String::from("Nothing in register")),
            Some(reg) => {
                let trk = match reg {
                    Register::Track(t) => (*t).clone(),
                    // a steps register becomes a track of exactly that
                    // length — yank 3 steps, gp a 3-step polymeter
                    Register::Steps(v) => {
                        let mut t = Track::empty();
                        let n = v.len().clamp(1, NUM_STEPS);
                        for (i, st) in v.into_iter().take(NUM_STEPS).enumerate() {
                            t.steps[i] = st;
                        }
                        t.length = n;
                        t.pulses = t.steps[..n].iter().filter(|st| st.active).count();
                        t
                    }
                };
                let times = (*times).max(1);
                if times > 1 {
                    h.begin_group();
                }
                for i in 0..times {
                    let at = if *before { tidx } else { tidx + 1 + i };
                    insert_track(s, at, trk.clone());
                    h.push(Command::PasteTrack { at, track: trk.clone() });
                }
                if times > 1 {
                    h.end_group("Paste tracks");
                }
                None
            }
        },
        Action::Adjust { layer, steps, coarse } => {
            let sel = s.selected;
            let old_step = s.tracks[tidx].steps[sel].clone();
            let mode = s.tracks[tidx].mode;
            // coarse note jumps move a whole period: an octave unscaled,
            // the scale's degree count when tuned
            let period = s.tracks[tidx].scale.as_ref().map_or(12, |sc| sc.len() as i32);
            let st = &mut s.tracks[tidx].steps[sel];
            let n = *steps;
            match layer {
                state::BindTarget::Note if mode == TrackMode::Modulation => {
                    let d = n as f32 * if *coarse { 0.1 } else { 0.01 };
                    st.mod_value = (st.mod_value + d).clamp(-1.0, 1.0);
                }
                state::BindTarget::Note => {
                    let d = n * if *coarse { period } else { 1 };
                    st.note = (i32::from(st.note) + d).clamp(0, 127) as u8;
                }
                state::BindTarget::Velocity => {
                    let d = n * if *coarse { 16 } else { 4 };
                    st.velocity = (i32::from(st.velocity) + d).clamp(1, 127) as u8;
                }
                state::BindTarget::Prob => {
                    let d = n * if *coarse { 25 } else { 5 };
                    st.prob = (i32::from(st.prob) + d).clamp(0, 100) as u8;
                }
                state::BindTarget::Mod => {
                    let d = n as f32 * if *coarse { 0.1 } else { 0.01 };
                    st.mod_value = (st.mod_value + d).clamp(-1.0, 1.0);
                }
            }
            let new_step = s.tracks[tidx].steps[sel].clone();
            push_step_edit(h, tidx, sel, old_step, new_step);
            None
        }
        Action::SetStepValue { layer, value } => {
            let sel = s.selected;
            let old_step = s.tracks[tidx].steps[sel].clone();
            let st = &mut s.tracks[tidx].steps[sel];
            match layer {
                state::BindTarget::Note => st.note = (*value as i32).clamp(0, 127) as u8,
                state::BindTarget::Velocity => st.velocity = (*value as i32).clamp(1, 127) as u8,
                state::BindTarget::Prob => st.prob = (*value as i32).clamp(0, 100) as u8,
                state::BindTarget::Mod => st.mod_value = value.clamp(-1.0, 1.0),
            }
            let new_step = s.tracks[tidx].steps[sel].clone();
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
                        s.tracks[tidx].steps[a..=b].clone_from_slice(&new);
                        h.push(Command::EditSteps { track: tidx, start: a, old, new });
                    }
                    s.selected = a;
                    None
                }
            }
        }
        Action::OpTrack(op) => match op {
            Operator::Yank => {
                s.register = Some(Register::Track(Box::new(s.tracks[tidx].clone())));
                Some(String::from("Yanked track"))
            }
            Operator::Delete => {
                if s.tracks.len() <= 1 {
                    return Some(String::from("Can't delete the last track"));
                }
                let track = s.tracks.remove(tidx);
                s.current_steps.remove(tidx);
                s.last_notes.remove(tidx);
                s.register = Some(Register::Track(Box::new(track.clone())));
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
                s.tracks[tidx].steps[..len].clone_from_slice(&new);
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
            s.tracks[tidx].steps[..len].clone_from_slice(&new);
            h.push(Command::EditSteps { track: tidx, start: 0, old, new });
            None
        }
        Action::Euclid(op) => {
            let old = EuclidState::capture(&s.tracks[tidx]);
            match op {
                EuclidOp::Pulses(n) => s.tracks[tidx].pulses = (*n).min(NUM_STEPS),
                EuclidOp::Length(n) => {
                    s.tracks[tidx].length = (*n).clamp(1, NUM_STEPS);
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
        Action::SetCycle(mode) => {
            let old = s.tracks[tidx].cycle;
            if old == *mode {
                return None;
            }
            s.tracks[tidx].cycle = *mode;
            h.push(Command::SetCycle { track: tidx, old, new: *mode });
            Some(format!("cycle = {}", mode.name()))
        }
        Action::SwitchSlot(slot) => {
            let to = (*slot).min(NUM_SLOTS - 1);
            let from = s.tracks[tidx].active_slot;
            if from == to {
                return Some(format!("Already on pattern {}", slot_letter(to)));
            }
            switch_slot(&mut s.tracks[tidx], to);
            s.selected = s.selected.min(s.tracks[tidx].length - 1);
            h.push(Command::SwitchSlot { track: tidx, from, to });
            Some(format!("Pattern {}", slot_letter(to)))
        }
        Action::SaveSlot(slot) => {
            let slot = (*slot).min(NUM_SLOTS - 1);
            if slot == s.tracks[tidx].active_slot {
                return Some(format!("{} is the active pattern", slot_letter(slot)));
            }
            let trk = &s.tracks[tidx];
            let copy = PatternData {
                steps: trk.steps.clone(),
                length: trk.length,
                pulses: trk.pulses,
                rotation: trk.rotation,
            };
            let old = trk.slots[slot].clone();
            if old.as_ref() == Some(&copy) {
                return None;
            }
            s.tracks[tidx].slots[slot] = Some(copy.clone());
            h.push(Command::SaveSlot { track: tidx, slot, old, new: copy });
            Some(format!("Saved → {}", slot_letter(slot)))
        }
        Action::Fill { kind, arg, seed } => {
            use crate::theory::gen;
            let len = s.tracks[tidx].length;
            let old: Vec<Step> = s.tracks[tidx].steps[..len].to_vec();
            let mut g: Vec<gen::GenStep> = old
                .iter()
                .map(|st| gen::GenStep {
                    active: st.active,
                    note: st.note,
                    velocity: st.velocity,
                    prob: st.prob,
                })
                .collect();
            match kind {
                state::FillKind::Mutate => gen::mutate(&mut g, *arg, 12, *seed),
                state::FillKind::Density => gen::density_fill(&mut g, *arg, *seed),
                state::FillKind::Markov => {
                    // learn from every OTHER track's pattern
                    let sources: Vec<Vec<gen::GenStep>> = s
                        .tracks
                        .iter()
                        .enumerate()
                        .filter(|&(t, _)| t != tidx)
                        .map(|(_, trk)| {
                            trk.steps[..trk.length]
                                .iter()
                                .map(|st| gen::GenStep {
                                    active: st.active,
                                    note: st.note,
                                    velocity: st.velocity,
                                    prob: st.prob,
                                })
                                .collect()
                        })
                        .collect();
                    gen::markov(&mut g, &sources, *seed);
                }
                state::FillKind::Cantor => gen::lsystem(&mut g, gen::LRule::Cantor, *seed),
                state::FillKind::ThueMorse => gen::lsystem(&mut g, gen::LRule::ThueMorse, *seed),
                state::FillKind::Fibonacci => gen::lsystem(&mut g, gen::LRule::Fibonacci, *seed),
                state::FillKind::Sierpinski => gen::lsystem(&mut g, gen::LRule::Sierpinski, *seed),
            }
            let new: Vec<Step> = old
                .iter()
                .zip(g)
                .map(|(st, gs)| Step {
                    active: gs.active,
                    note: gs.note,
                    velocity: gs.velocity,
                    prob: gs.prob,
                    mod_value: st.mod_value,
                    bind: st.bind.clone(),
                })
                .collect();
            if old == new {
                return Some(format!("fill {}: no change", kind.name()));
            }
            s.tracks[tidx].steps[..len].clone_from_slice(&new);
            h.push(Command::EditSteps { track: tidx, start: 0, old, new });
            Some(format!("fill {}", kind.name()))
        }
    }
}

/// `a`–`h` for pattern slots 0–7.
fn slot_letter(slot: usize) -> char {
    (b'a' + (slot as u8).min(7)) as char
}

/// Swap pattern slot `to` to the front of the track (the active slot's
/// data lives inline; the outgoing pattern parks in its slot).
fn switch_slot(track: &mut Track, to: usize) {
    let from = track.active_slot;
    if from == to || to >= NUM_SLOTS {
        return;
    }
    let parked = PatternData {
        steps: std::mem::take(&mut track.steps),
        length: track.length,
        pulses: track.pulses,
        rotation: track.rotation,
    };
    track.slots[from] = Some(parked);
    let incoming = track.slots[to].take().unwrap_or_else(PatternData::empty);
    track.steps = incoming.steps;
    track.length = incoming.length;
    track.pulses = incoming.pulses;
    track.rotation = incoming.rotation;
    track.active_slot = to;
}

/// Insert a track at `at`, keeping the per-track bookkeeping vectors in sync.
fn insert_track(state: &mut SequencerState, at: usize, track: Track) {
    let at = at.min(state.tracks.len());
    state.tracks.insert(at, track);
    state.current_steps.insert(at, 0);
    state.last_notes.insert(at, None);
    state.marks.insert(at, false);
    state.focus(at, Some(0));
}

/// Remove the track at `at`, keeping the per-track bookkeeping vectors in
/// sync. Never removes the last remaining track.
fn remove_track(state: &mut SequencerState, at: usize) {
    if state.tracks.len() > 1 && at < state.tracks.len() {
        state.tracks.remove(at);
        state.current_steps.remove(at);
        state.last_notes.remove(at);
        state.marks.remove(at);
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
            Command::SetCycle { .. } => "Set cycle mode",
            Command::SwitchSlot { .. } => "Switch pattern",
            Command::SaveSlot { .. } => "Save pattern slot",
            Command::SetScale { .. } => "Set scale",
            Command::Group { desc, .. } => desc,
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
                    state.tracks[*track].steps[*step] = old_step.clone();
                    state.focus(*track, Some(*step));
                }
            }
            Command::EditSteps { track, start, old, .. } => {
                if *track < state.tracks.len() && start + old.len() <= state.tracks[*track].steps.len() {
                    state.tracks[*track].steps[*start..start + old.len()].clone_from_slice(old);
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
            Command::SetCycle { track, old, .. } => {
                if *track < state.tracks.len() {
                    state.tracks[*track].cycle = *old;
                    state.focus(*track, None);
                }
            }
            Command::SwitchSlot { track, from, to } => {
                if *track < state.tracks.len() && state.tracks[*track].active_slot == *to {
                    switch_slot(&mut state.tracks[*track], *from);
                    state.focus(*track, None);
                }
            }
            Command::SaveSlot { track, slot, old, .. } => {
                if *track < state.tracks.len() && *slot < NUM_SLOTS {
                    state.tracks[*track].slots[*slot] = old.clone();
                    state.focus(*track, None);
                }
            }
            Command::SetScale { track, old, .. } => {
                if *track < state.tracks.len() {
                    state.tracks[*track] = (**old).clone();
                    state.focus(*track, None);
                }
            }
            Command::Group { cmds, .. } => {
                for cmd in cmds.iter().rev() {
                    cmd.undo(state);
                }
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
                    state.tracks[*track].steps[*step] = new_step.clone();
                    state.focus(*track, Some(*step));
                }
            }
            Command::EditSteps { track, start, new, .. } => {
                if *track < state.tracks.len() && start + new.len() <= state.tracks[*track].steps.len() {
                    state.tracks[*track].steps[*start..start + new.len()].clone_from_slice(new);
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
            Command::SetCycle { track, new, .. } => {
                if *track < state.tracks.len() {
                    state.tracks[*track].cycle = *new;
                    state.focus(*track, None);
                }
            }
            Command::SwitchSlot { track, from, to } => {
                if *track < state.tracks.len() && state.tracks[*track].active_slot == *from {
                    switch_slot(&mut state.tracks[*track], *to);
                    state.focus(*track, None);
                }
            }
            Command::SaveSlot { track, slot, new, .. } => {
                if *track < state.tracks.len() && *slot < NUM_SLOTS {
                    state.tracks[*track].slots[*slot] = Some(new.clone());
                    state.focus(*track, None);
                }
            }
            Command::SetScale { track, new, .. } => {
                if *track < state.tracks.len() {
                    state.tracks[*track] = (**new).clone();
                    state.focus(*track, None);
                }
            }
            Command::Group { cmds, .. } => {
                for cmd in cmds {
                    cmd.redo(state);
                }
            }
        }
    }
}

const HISTORY_CAP: usize = 100;

struct History {
    commands: Vec<(Command, Instant)>,
    index: usize,
    /// While `Some`, pushes collect here instead of the history — closed
    /// by `end_group` into one undoable [`Command::Group`].
    group: Option<Vec<Command>>,
}

impl History {
    fn new() -> Self {
        Self { commands: vec![], index: 0, group: None }
    }

    /// Start collecting commands for a single undo entry (multi-track edits).
    fn begin_group(&mut self) {
        self.group = Some(vec![]);
    }

    /// Close the group: zero commands vanish, one is pushed plain, more
    /// become a [`Command::Group`] labeled `desc`.
    fn end_group(&mut self, desc: &'static str) {
        let Some(mut cmds) = self.group.take() else {
            return;
        };
        match cmds.len() {
            0 => {}
            1 => {
                if let Some(cmd) = cmds.pop() {
                    self.push(cmd);
                }
            }
            _ => self.push(Command::Group { cmds, desc }),
        }
    }

    fn push(&mut self, cmd: Command) {
        // Group collection bypasses coalescing — a multi-track sweep is
        // already one entry.
        if let Some(group) = self.group.as_mut() {
            group.push(cmd);
            return;
        }
        // Sweep rule (docs/keybindings.md): consecutive edits of the same
        // step within the coalescing window merge into one undo entry, so a
        // held transpose key reverts with a single u.
        if self.index == self.commands.len() {
            if let Some((Command::EditStep { track, step, old_step, new_step }, at)) =
                self.commands.last_mut()
            {
                if let Command::EditStep { track: t2, step: s2, new_step: n2, .. } = &cmd {
                    if t2 == track && s2 == step && at.elapsed() < crate::undo::COALESCE_WINDOW {
                        *new_step = n2.clone();
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

/// Which actions fan out across a multi-select (V-line span or marked
/// tracks). Yanks and pastes stay single-track (one register, vi
/// semantics); track lifecycle ops don't fan out either.
fn is_multi_action(action: &Action) -> bool {
    match action {
        Action::ToggleStep
        | Action::ToggleSpan(_)
        | Action::CutStep
        | Action::Adjust { .. }
        | Action::SetStepValue { .. }
        | Action::ToggleMute
        | Action::ToggleMode
        | Action::Rotate { .. }
        | Action::Euclid(_)
        | Action::SetCycle(_)
        | Action::SwitchSlot(_)
        | Action::SaveSlot(_)
        | Action::Fill { .. } => true,
        Action::Op { op, .. } => matches!(op, Operator::Delete | Operator::Change),
        Action::OpTrack(op) => matches!(op, Operator::Delete | Operator::Change),
        Action::YankStep
        | Action::Paste { .. }
        | Action::PasteAsTrack { .. }
        | Action::NewTrack { .. } => false,
    }
}

/// The marked tracks, or just the current one when nothing is marked.
fn marked_targets(s: &SequencerState) -> Vec<usize> {
    let marked: Vec<usize> = s
        .marks
        .iter()
        .take(s.tracks.len())
        .enumerate()
        .filter_map(|(i, &m)| m.then_some(i))
        .collect();
    if marked.is_empty() {
        vec![s.current_track]
    } else {
        marked
    }
}

/// Apply an action honoring the multi-select doctrine: explicit V-line
/// span beats marked tracks beats the current track. Multi-track edits
/// land as ONE undo entry. Register-filling deletes that fan out leave
/// the register holding the LAST target's steps (dired-style; documented
/// in docs/sequencer.md).
fn apply_selected(
    s: &mut SequencerState,
    h: &mut History,
    action: &Action,
    span: Option<(usize, usize)>,
) -> Option<String> {
    let targets: Vec<usize> = match span {
        Some((a, b)) => {
            let hi = b.min(s.tracks.len().saturating_sub(1));
            (a.min(hi)..=hi).collect()
        }
        None if is_multi_action(action) => marked_targets(s),
        None => vec![s.current_track],
    };
    if targets.len() <= 1 || !is_multi_action(action) {
        return apply_action(s, h, action);
    }
    let cur = s.current_track;
    let sel = s.selected;
    // deleting tracks shifts indices — walk top-down so they stay valid
    let mut targets = targets;
    if matches!(action, Action::OpTrack(Operator::Delete)) {
        targets.reverse();
    }
    h.begin_group();
    let mut msg = None;
    for &t in &targets {
        if t >= s.tracks.len() {
            continue;
        }
        s.current_track = t;
        s.selected = sel.min(s.tracks[t].length.saturating_sub(1));
        msg = apply_action(s, h, action);
    }
    h.end_group("Edit tracks");
    s.current_track = cur.min(s.tracks.len().saturating_sub(1));
    s.selected = sel.min(s.tracks[s.current_track].length.saturating_sub(1));
    msg
}

/// A step's pitch in cents above the track root, under the track's current
/// tuning (MIDI semitones when unscaled, scale degrees when tuned).
fn step_pitch_cents(track: &Track, note: u8) -> f64 {
    match &track.scale {
        None => f64::from(i32::from(note) - i32::from(track.root)) * 100.0,
        Some(sc) => sc.pitch_cents(i32::from(note) - 60),
    }
}

/// Rebuild a track under a new tuning, converting every step's pitch to
/// the nearest representable one — inline pattern AND parked slots (all
/// of a track's patterns share its scale).
fn retuned_track(
    track: &Track,
    new_scale: Option<crate::theory::scales::Scale>,
    new_root: u8,
) -> Track {
    let convert = |steps: &[Step]| -> Vec<Step> {
        steps
            .iter()
            .map(|st| {
                let cents = step_pitch_cents(track, st.note);
                let note = match &new_scale {
                    // back to MIDI: nearest semitone from the new root
                    None => (i32::from(new_root) + (cents / 100.0).round() as i32).clamp(0, 127),
                    Some(sc) => (60 + sc.quantize_cents(cents)).clamp(0, 127),
                };
                Step { note: note as u8, ..st.clone() }
            })
            .collect()
    };
    let mut new = track.clone();
    new.steps = convert(&track.steps);
    for p in new.slots.iter_mut().flatten() {
        p.steps = convert(&p.steps);
    }
    new.scale = new_scale;
    new.root = new_root;
    new
}

/// `:scale` executor: retune the target tracks (marks honored), one undo
/// entry for the lot. `scale_change`: `None` keeps each track's own scale
/// (root-only change), `Some(x)` sets it to `x` (`Some(None)` = chromatic).
#[allow(clippy::option_option)] // keep-vs-set-vs-clear is exactly two layers
fn set_scale(
    s: &mut SequencerState,
    h: &mut History,
    scale_change: Option<Option<crate::theory::scales::Scale>>,
    new_root: Option<u8>,
) -> String {
    let targets = marked_targets(s);
    let multi = targets.len() > 1;
    if multi {
        h.begin_group();
    }
    let mut label = String::from("chromatic");
    for &t in &targets {
        if t >= s.tracks.len() {
            continue;
        }
        let old = s.tracks[t].clone();
        let scale = scale_change.clone().unwrap_or_else(|| old.scale.clone());
        let root = new_root.unwrap_or(old.root);
        let new = retuned_track(&old, scale, root);
        label = new
            .scale
            .as_ref()
            .map_or_else(|| String::from("chromatic"), |sc| sc.name.clone());
        if new == old {
            continue;
        }
        s.tracks[t] = new.clone();
        h.push(Command::SetScale { track: t, old: Box::new(old), new: Box::new(new) });
    }
    if multi {
        h.end_group("Set scale");
    }
    match new_root {
        Some(r) => format!("scale = {} root {}", label, midi_note_name(r)),
        None => format!("scale = {}", label),
    }
}

/// Parse a root note: a MIDI number ("57") or a note name ("A3", "c#4",
/// "eb2"). Octave -1 to 9, MIDI C4 = 60 convention.
fn parse_root(input: &str) -> Option<u8> {
    let t = input.trim();
    if let Ok(n) = t.parse::<i32>() {
        return (0..=127).contains(&n).then_some(n as u8);
    }
    let mut chars = t.chars();
    let letter = chars.next()?.to_ascii_uppercase();
    let base: i32 = match letter {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let rest: String = chars.collect();
    let (accidental, oct_str) = match rest.chars().next() {
        Some('#') => (1, &rest[1..]),
        Some('b') => (-1, &rest[1..]),
        _ => (0, rest.as_str()),
    };
    let octave: i32 = oct_str.parse().ok()?;
    let midi = (octave + 1) * 12 + base + accidental;
    (0..=127).contains(&midi).then_some(midi as u8)
}

/// Lock + apply with multi-select honored — the key handlers' entry point.
fn exec_action(
    state: &Mutex<SequencerState>,
    history: &mut History,
    action: &Action,
) -> Option<String> {
    let mut s = state.lock().unwrap();
    apply_selected(&mut s, history, action, None)
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

    // Global step counter the playheads derive from; per-track drunk-walk
    // positions; manifest + resolution cache for per-step mod-in bindings.
    let mut last_gstep: Option<u64> = None;
    let mut drunk: Vec<usize> = Vec::new();
    let mut manifest: Option<Manifest> = Manifest::open().ok();
    let mut bind_cache: std::collections::HashMap<String, Option<usize>> =
        std::collections::HashMap::new();
    let mut bind_cache_at = Instant::now();

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        if modbus.is_none() {
            modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();
        }
        if manifest.is_none() {
            manifest = Manifest::open().ok();
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
            while s.current_steps.len() < s.tracks.len() {
                s.current_steps.push(0);
            }
            while s.last_notes.len() < s.tracks.len() {
                s.last_notes.push(None);
            }
            while s.marks.len() < s.tracks.len() {
                s.marks.push(false);
            }
            while drunk.len() < s.tracks.len() {
                drunk.push(0);
            }

            // Release any held note that a step advance can no longer end:
            // paused/stopped transport, a track muted mid-note, or a track
            // switched out of note mode. Without this the voice keeps its
            // gate and gate-mode envelope channels sustain into a drone.
            for (t, n) in stuck_notes(&s.tracks, &s.last_notes, playing) {
                let _ = events.write_event(&AudioEvent::note_off_source(n, t as u8, s.current_steps[t] as u32));
                s.last_notes[t] = None;
                if let (Some(ref mut bus), Some(base)) = (modbus.as_mut(), s.mod_base) {
                    bus.set(base + t, 0.0);
                }
            }

            // Mute means silence on every output: keep a muted track's mod
            // channel at 0.0 (modulation-mode tracks otherwise freeze the
            // bus at their last value — the mute-audit bug).
            if let (Some(ref mut bus), Some(base)) = (modbus.as_mut(), s.mod_base) {
                for (t, trk) in s.tracks.iter().enumerate() {
                    if trk.muted {
                        bus.set(base + t, 0.0);
                    }
                }
            }

            let gstep = clock.checked_div(samples_per_step).unwrap_or(0);
            if playing && last_gstep != Some(gstep) {
                // bind sources re-resolve about once a second, like voice
                // param bindings (handles module restarts)
                if bind_cache_at.elapsed() > Duration::from_secs(1) {
                    bind_cache.clear();
                    bind_cache_at = Instant::now();
                }

                #[allow(clippy::needless_range_loop)] // t indexes four parallel vecs
                for t in 0..s.tracks.len() {
                    let len = s.tracks[t].length;
                    let prev_pos = s.current_steps[t];
                    let pos = cycle_pos(s.tracks[t].cycle, gstep, len, &mut drunk[t], track_seed(t))
                        .min(len.saturating_sub(1));
                    s.current_steps[t] = pos;
                    if s.tracks[t].muted {
                        continue;
                    }

                    // resolve this step's mod-in binding to a live bus value
                    let bind_value = s.tracks[t].steps[pos].bind.as_ref().and_then(|b| {
                        let ch = *bind_cache.entry(b.source.clone()).or_insert_with(|| {
                            manifest.as_ref().and_then(|m| {
                                crate::routing::SourceAddr::parse(&b.source)
                                    .and_then(|a| crate::routing::resolve(&m.entries(), &a))
                            })
                        });
                        ch.and_then(|ch| modbus.as_ref().map(|bus| bus.get(ch)))
                    });

                    let track = &s.tracks[t];
                    let step = &track.steps[pos];
                    let off = bind_offsets(step.bind.as_ref(), bind_value);
                    let prob = (i32::from(step.prob) + off.prob).clamp(0, 100) as u8;

                    match track.mode {
                        TrackMode::Note => {
                            // probability-failed steps behave as inactive:
                            // gate closes, bus reads 0
                            let fires = step.active && step_fires(prob, t, gstep);
                            let velocity =
                                (i32::from(step.velocity) + off.velocity).clamp(1, 127) as u8;
                            let mod_val =
                                if fires { f32::from(velocity) / 127.0 } else { 0.0 };
                            if let (Some(ref mut bus), Some(base)) = (modbus.as_mut(), s.mod_base) {
                                bus.set(base + t, mod_val);
                            }
                            let hz = step_hz(track, step.note, off.degrees) as f32;
                            let note_id = step.note;
                            if let Some(n) = s.last_notes[t] {
                                let _ = events.write_event(&AudioEvent::note_off_source(
                                    n, t as u8, prev_pos as u32,
                                ));
                            }
                            if fires {
                                let _ = events.write_event(&AudioEvent::note_on_hz(
                                    hz, velocity, t as u8, pos as u32,
                                ));
                                s.last_notes[t] = Some(note_id);
                            } else {
                                s.last_notes[t] = None;
                            }
                        }
                        TrackMode::Modulation => {
                            // probability-failed steps HOLD the bus (sample
                            // and hold); the active flag stays cosmetic for
                            // modulation tracks, as it always was
                            if step_fires(prob, t, gstep) {
                                let v = (step.mod_value + off.mod_value).clamp(-1.0, 1.0);
                                if let (Some(ref mut bus), Some(base)) =
                                    (modbus.as_mut(), s.mod_base)
                                {
                                    bus.set(base + t, v);
                                }
                            }
                        }
                    }
                }
                last_gstep = Some(gstep);
            }
        } // lock released here, before sleep

        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(())
}

/// Where a track's playhead sits at global step `gstep`, given its cycle
/// mode. Every mode except Drunk is a pure function of `gstep` (resync-safe
/// across pause/resume and module restarts); Drunk walks `drunk` in place.
fn cycle_pos(
    mode: state::CycleMode,
    gstep: u64,
    len: usize,
    drunk: &mut usize,
    seed: u64,
) -> usize {
    use crate::theory::rng::Rng;
    if len <= 1 {
        return 0;
    }
    let l = len as u64;
    match mode {
        state::CycleMode::Forward => (gstep % l) as usize,
        state::CycleMode::Reverse => (l - 1 - gstep % l) as usize,
        state::CycleMode::PingPong => {
            // triangle over period 2·len−2: 0,1,…,len−1,len−2,…,1
            let period = 2 * l - 2;
            let k = gstep % period;
            if k < l {
                k as usize
            } else {
                (period - k) as usize
            }
        }
        // even lengths only ever visit half the pattern — that's the charm
        state::CycleMode::EveryOther => ((gstep * 2) % l) as usize,
        state::CycleMode::Spiral => {
            // outside-in interleave: 0, len−1, 1, len−2, …
            let k = gstep % l;
            if k.is_multiple_of(2) {
                (k / 2) as usize
            } else {
                (l - 1 - k / 2) as usize
            }
        }
        state::CycleMode::PrimeJump => ((gstep * prime_jump_mult(len)) % l) as usize,
        state::CycleMode::Random => {
            Rng::new(seed ^ gstep.wrapping_mul(0x9E37_79B9_7F4A_7C15)).below(l) as usize
        }
        state::CycleMode::Drunk => {
            let mut r = Rng::new(seed ^ gstep.wrapping_mul(0xA24B_AED4_963E_E407));
            let delta = r.below(3) as i64 - 1; // −1, 0, +1
            *drunk = (*drunk as i64 + delta).rem_euclid(len as i64) as usize;
            *drunk
        }
    }
}

/// Multiplier for the prime-jump cycle: the smallest prime ≥ len/3 that is
/// coprime with `len`, so the orbit is a fixed strange permutation that
/// still visits every step.
fn prime_jump_mult(len: usize) -> u64 {
    fn gcd(mut a: usize, mut b: usize) -> usize {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    }
    fn is_prime(n: usize) -> bool {
        n >= 2 && (2..).take_while(|d| d * d <= n).all(|d| !n.is_multiple_of(d))
    }
    let mut m = (len / 3).max(2);
    // Bertrand's postulate guarantees a coprime prime well below this cap.
    while m <= 2 * len + 3 {
        if is_prime(m) && gcd(m, len) == 1 {
            return m as u64;
        }
        m += 1;
    }
    1
}

/// Per-track seed for the stochastic cycle modes and the probability gate.
fn track_seed(t: usize) -> u64 {
    (t as u64 + 1).wrapping_mul(0xD6E8_FEB8_6659_FD93)
}

/// Probability gate: does this step fire at global step `gstep`?
/// Deterministic per (track, gstep) so pause/resume can't replay
/// differently than a straight run.
fn step_fires(prob: u8, track: usize, gstep: u64) -> bool {
    if prob >= 100 {
        return true;
    }
    if prob == 0 {
        return false;
    }
    let mut r = crate::theory::rng::Rng::new(
        track_seed(track) ^ gstep.wrapping_mul(0xBF58_476D_1CE4_E5B9),
    );
    r.below(100) < u64::from(prob)
}

/// A step's frequency. No scale: `note` is MIDI, 12-TET. With a scale:
/// `note` is a degree biased at 60 (60 = root), tuned by the cents engine
/// from the track root — this is the microtonal path.
fn step_hz(track: &Track, note: u8, degree_offset: i32) -> f64 {
    use crate::theory::scales;
    match &track.scale {
        None => scales::midi_to_hz((i32::from(note) + degree_offset).clamp(0, 127) as u8),
        Some(sc) => sc.degree_to_hz(
            i32::from(note) - 60 + degree_offset,
            scales::midi_to_hz(track.root),
        ),
    }
}

/// What a step's mod-in binding contributes at trigger time, given the
/// source's live modbus value. One binding per step, one target each.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct BindOffsets {
    degrees: i32,
    velocity: i32,
    prob: i32,
    mod_value: f32,
}

fn bind_offsets(bind: Option<&StepBind>, value: Option<f32>) -> BindOffsets {
    let mut o = BindOffsets::default();
    let (Some(b), Some(v)) = (bind, value) else {
        return o;
    };
    let v = v * b.amount;
    match b.target {
        state::BindTarget::Note => o.degrees = (v * 12.0).round() as i32,
        state::BindTarget::Velocity => o.velocity = (v * 127.0).round() as i32,
        state::BindTarget::Prob => o.prob = (v * 100.0).round() as i32,
        state::BindTarget::Mod => o.mod_value = v,
    }
    o
}

/// Held notes that the step-advance loop can no longer end: the transport
/// is paused/stopped, the track was muted mid-note, or the track switched
/// out of note mode. These must be released explicitly or they hang.
fn stuck_notes(tracks: &[Track], last_notes: &[Option<u8>], playing: bool) -> Vec<(usize, u8)> {
    last_notes
        .iter()
        .take(tracks.len())
        .enumerate()
        .filter_map(|(t, n)| n.map(|n| (t, n)))
        .filter(|(t, _)| {
            !playing || tracks[*t].muted || tracks[*t].mode == TrackMode::Modulation
        })
        .collect()
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

            let info_text = row_info(trk);
            // window the steps so the info column never clips (long
            // patterns scroll; ‹ › mark hidden steps)
            let visible = row_visible(w, info_text.chars().count(), trk.length);
            let anchor = if is_cur { state.selected } else { tstep };
            let start = row_window_start(trk.length, anchor, visible);
            let mut spans: Vec<Span> = Vec::with_capacity(visible + 10);
            // the track label wears its channel-slot identity color — the
            // exact hue any param bound to this track shows on its bar
            let cable = match state.mod_base {
                Some(base) => theme::channel_color(base + ti),
                None => theme::source_color(&format!("sequencer/0/t{}", ti + 1)),
            };
            spans.push(Span::styled(
                format!("{}", if is_cur { theme::PLAYHEAD } else { ' ' }),
                if is_cur { theme::chrome_hi() } else { theme::chrome() },
            ));
            spans.push(Span::styled(
                format!("t{} ", ti + 1),
                if trk.muted { theme::dim() } else { theme::signal(cable) },
            ));
            spans.push(Span::styled(
                if start > 0 { "‹" } else { " " }.to_string(),
                theme::dim(),
            ));
            for i in start..start + visible {
                if i > start && (i - start).is_multiple_of(4) {
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
                let style = if is_cur && i == state.selected {
                    // your edit cursor, visible in the overview too
                    theme::selected()
                } else if state.playing && i == tstep && !trk.muted {
                    if on {
                        theme::flash(hue)
                    } else {
                        theme::signal(theme::clock())
                    }
                } else if !state.playing && i == tstep {
                    // paused: a still CLOCK-hue marker says "you are here"
                    theme::signal(theme::clock())
                } else if state.playing && !trk.muted && wake_offset(i, tstep, trk.length).is_some() {
                    theme::signal(theme::clock())
                } else if trk.muted {
                    theme::dim()
                } else if on {
                    match trk.mode {
                        // pitch-class wheel: see the melody from across the room
                        TrackMode::Note => theme::signal(theme::pitch_color(step.note)),
                        TrackMode::Modulation => theme::signal(theme::cv_ramp(step.mod_value)),
                    }
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
                } else if !state.playing && i == tstep && !on {
                    theme::PLAYHEAD
                } else {
                    glyph
                };
                spans.push(Span::styled(shown.to_string(), style));
            }
            spans.push(Span::styled(
                if start + visible < trk.length { "›" } else { " " }.to_string(),
                theme::dim(),
            ));
            spans.push(Span::styled(info_text, if is_cur { row_style } else { theme::dim() }));
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
            } else if i == cstep {
                // playing or paused: the position always shows
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
                    TrackMode::Note => theme::pitch_color(step.note),
                    TrackMode::Modulation => theme::cv_ramp(step.mod_value),
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
                if step.active {
                    theme::signal(theme::pitch_color(step.note))
                } else {
                    theme::dim()
                },
            ));
        }
        lines.push(Line::from(nums));
        lines.push(Line::from(vals));
        lines.push(Line::from(vels));

        // the detail strip hugs the track overview (zoomed + zoomed-out
        // views read as one unit); only the modeline pins to the pane
        // bottom, vim-style — spare height is the quiet middle
        theme::anchor_bottom(&mut lines, area.height as usize, 2);
        lines.push(theme::rule(w));

        // ── status ──────────────────────────────────────────────────────
        let mut mode_label = match mode {
            "insert" if !submode.is_empty() => format!("INSERT[{}:{}]", submode, input_buffer),
            "insert" => String::from("INSERT"),
            "visual" => String::from("VISUAL"),
            "visual_line" => String::from("V-LINE"),
            _ => String::from("NORMAL"),
        };
        // the active value layer rides the mode label (note layer is silent)
        if let Some(tag) = layer_tag(state.layer) {
            mode_label.push(' ');
            mode_label.push_str(tag);
        }
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

/// Modeline tag for the active value layer; the note layer (the default
/// view) stays untagged.
fn layer_tag(layer: state::BindTarget) -> Option<&'static str> {
    match layer {
        state::BindTarget::Note => None,
        state::BindTarget::Velocity => Some("'v"),
        state::BindTarget::Prob => Some("'p"),
        state::BindTarget::Mod => Some("'m"),
    }
}

/// Track-row geometry shared by the renderer and the mouse hit-test:
/// window start for a track row given its length, the anchor to keep in
/// view, and how many steps fit.
fn row_window_start(len: usize, anchor: usize, visible: usize) -> usize {
    if len <= visible {
        0
    } else {
        anchor.saturating_sub(visible / 2).min(len - visible)
    }
}

/// How many steps fit in a track row of width `w` with this info column.
fn row_visible(w: usize, info_len: usize, len: usize) -> usize {
    let budget = w.saturating_sub(5 + info_len + 2);
    ((budget * 4) / 5).clamp(4, len.max(4)).min(len)
}

/// The info column text for a track row (drawn and measured identically).
fn row_info(trk: &Track) -> String {
    format!(
        "  {:>3} P{} R{}{}{}",
        trk.length,
        trk.pulses,
        trk.rotation,
        if trk.mode == TrackMode::Modulation { " ⌁" } else { "" },
        if trk.muted { " M" } else { "" },
    )
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

fn step_to_param(step: &Step) -> state::StepParam {
    state::StepParam {
        active: step.active,
        note: step.note,
        velocity: step.velocity,
        mod_value: step.mod_value,
        prob: step.prob,
        bind: step.bind.as_ref().map(|b| state::StepBindParam {
            target: b.target,
            source: b.source.clone(),
            amount: b.amount,
        }),
    }
}

fn step_from_param(p: &state::StepParam) -> Step {
    Step {
        active: p.active,
        note: p.note,
        velocity: p.velocity,
        mod_value: p.mod_value,
        prob: p.prob.min(100),
        bind: p.bind.as_ref().map(|b| StepBind {
            target: b.target,
            source: b.source.clone(),
            amount: b.amount,
        }),
    }
}

/// Steps worth saving: everything up to the last one that differs from the
/// default (so untouched 128-step tails don't bloat the file).
fn trimmed_steps(steps: &[Step]) -> Vec<state::StepParam> {
    let keep = steps
        .iter()
        .rposition(|st| *st != Step::default())
        .map_or(0, |i| i + 1);
    steps[..keep].iter().map(step_to_param).collect()
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
            steps: trk.steps.iter().map(step_to_param).collect(),
            length: Some(trk.length),
            pulses: Some(trk.pulses),
            rotation: Some(trk.rotation),
            muted: trk.muted,
            mode: trk.mode,
            cycle: trk.cycle,
            scale: trk.scale.as_ref().map(|sc| sc.name.clone()),
            scale_cents: trk.scale.as_ref().map(|sc| sc.degrees.clone()).unwrap_or_default(),
            scale_period: trk.scale.as_ref().map(|sc| sc.period),
            root: Some(trk.root),
            active_slot: trk.active_slot,
            slots: trk
                .slots
                .iter()
                .enumerate()
                .filter_map(|(i, slot)| slot.as_ref().map(|p| (i, p)))
                .filter(|(_, p)| **p != PatternData::empty())
                .map(|(i, p)| state::SlotParam {
                    slot: i,
                    steps: trimmed_steps(&p.steps),
                    length: Some(p.length),
                    pulses: Some(p.pulses),
                    rotation: Some(p.rotation),
                })
                .collect(),
        }).collect(),
        macros: vec![],
        lane: vec![],
        lane_len: None,
    }
}

/// Patch view of the params: musical content only — the transport play flag
/// is session state, not patch state (and would dirty the patch constantly).
fn patch_params(s: &SequencerState) -> state::SequencerParams {
    let mut p = snapshot_params(s);
    p.playing = None;
    p
}

/// Resolve a saved scale: stored cents are authoritative (covers `.scl`
/// imports), a bare name re-resolves against the library.
fn scale_from_params(tp: &state::TrackParam) -> Option<crate::theory::scales::Scale> {
    if !tp.scale_cents.is_empty() {
        return Some(crate::theory::scales::Scale {
            name: tp.scale.clone().unwrap_or_else(|| String::from("imported")),
            degrees: tp.scale_cents.clone(),
            period: tp.scale_period.unwrap_or(1200.0),
        });
    }
    tp.scale.as_deref().and_then(crate::theory::scales::lookup)
}

fn steps_from_params(saved: &[state::StepParam]) -> Vec<Step> {
    let mut steps = vec![Step::default(); NUM_STEPS];
    for (i, step) in saved.iter().enumerate().take(steps.len()) {
        steps[i] = step_from_param(step);
    }
    steps
}

/// Rebuild tracks (and bookkeeping vectors) from saved params.
fn apply_tracks(s: &mut SequencerState, params: &state::SequencerParams) {
    if params.tracks.is_empty() {
        return;
    }
    s.tracks.clear();
    s.current_steps.clear();
    s.last_notes.clear();
    s.marks.clear();
    for tp in &params.tracks {
        let mut slots: [Option<PatternData>; NUM_SLOTS] = Default::default();
        for sp in &tp.slots {
            if sp.slot < NUM_SLOTS {
                slots[sp.slot] = Some(PatternData {
                    steps: steps_from_params(&sp.steps),
                    length: sp.length.unwrap_or(16).clamp(1, NUM_STEPS),
                    pulses: sp.pulses.unwrap_or(0),
                    rotation: sp.rotation.unwrap_or(0),
                });
            }
        }
        let active_slot = tp.active_slot.min(NUM_SLOTS - 1);
        slots[active_slot] = None; // the active slot's data lives inline
        s.tracks.push(Track {
            steps: steps_from_params(&tp.steps),
            length: tp.length.unwrap_or(16).clamp(1, NUM_STEPS),
            pulses: tp.pulses.unwrap_or(5),
            rotation: tp.rotation.unwrap_or(0),
            muted: tp.muted,
            mode: tp.mode,
            cycle: tp.cycle,
            scale: scale_from_params(tp),
            root: tp.root.unwrap_or(60).min(127),
            active_slot,
            slots,
        });
        s.current_steps.push(0);
        s.last_notes.push(None);
        s.marks.push(false);
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
                trk.steps[i] = step_from_param(step);
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
    // `'` pressed: next key (n/v/p/m) picks the value layer
    let mut pending_layer = false;
    // `"` pressed: next key (a-h / A-H) switches / saves a pattern slot
    let mut pending_quote = false;
    // last fill, for `F` (re-run with a fresh seed) — the seed counter
    // keeps repeats deterministic within a session
    let mut last_fill: Option<(state::FillKind, f32)> = None;
    let mut fill_seed: u64 = 0xF111_5EED;
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
                            undo_msg = exec_action(&state, &mut history, &action);
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
            let ev = event::read()?;
            if let Event::Mouse(m) = ev {
                use crossterm::event::{MouseButton, MouseEventKind};
                match m.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let mut s = state.lock().unwrap();
                        let len = s.track().length;
                        s.selected = if m.kind == MouseEventKind::ScrollUp {
                            (s.selected + 1) % len
                        } else {
                            (s.selected + len - 1) % len
                        };
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        let mut s = state.lock().unwrap();
                        let n = s.tracks.len();
                        let y = m.row as usize;
                        // track rows live at y = 1..=n
                        if (1..=n).contains(&y) {
                            s.current_track = y - 1;
                            // map column back to a step: label(4) + ‹(1),
                            // then 4 cells + 1 space repeating, window
                            // starts where the draw started it
                            let trk_len = s.track().length;
                            let rel = (m.column as usize).saturating_sub(5);
                            let idx_in_window = rel - rel / 5;
                            // identical geometry to the renderer
                            let w = terminal.size().map(|r| r.width as usize).unwrap_or(60);
                            let info_len = row_info(s.track()).chars().count();
                            let visible = row_visible(w, info_len, trk_len);
                            let start = row_window_start(trk_len, s.selected, visible);
                            s.selected = (start + idx_in_window).min(trk_len - 1);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if let Event::Key(key) = ev {
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
                                                "pulses" => s.tracks[tidx].pulses = n.min(NUM_STEPS),
                                                "length" => {
                                                    s.tracks[tidx].length = n.clamp(1, NUM_STEPS);
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
                                    "cycle" => match state::CycleMode::parse(&v) {
                                        Some(mode) => {
                                            drop(s);
                                            let action = Action::SetCycle(mode);
                                            undo_msg =
                                                exec_action(&state, &mut history, &action);
                                            last_change = Some(action);
                                        }
                                        None => {
                                            undo_msg = Some(format!(
                                                "Unknown cycle: {} (forward reverse pingpong random drunk everyother spiral primejump)",
                                                v
                                            ));
                                        }
                                    },
                                    "root" => match parse_root(&v) {
                                        Some(r) => {
                                            undo_msg = Some(set_scale(
                                                &mut s,
                                                &mut history,
                                                None,
                                                Some(r),
                                            ));
                                        }
                                        None => undo_msg = Some(format!("Invalid root: {}", v)),
                                    },
                                    _ => undo_msg = Some(format!("Unknown setting: {}", k)),
                                }
                            }
                            ExCommand::Unknown(c) => {
                                let (head, rest) = match c.split_once(char::is_whitespace) {
                                    Some((h2, r)) => (h2, r.trim().to_string()),
                                    None => (c.as_str(), String::new()),
                                };
                                match head {
                                    "scale" => {
                                        let mut s = state.lock().unwrap();
                                        if rest.is_empty() {
                                            let trk = s.track();
                                            undo_msg = Some(match &trk.scale {
                                                Some(sc) => format!(
                                                    "scale = {} root {} ({} degrees)",
                                                    sc.name,
                                                    midi_note_name(trk.root),
                                                    sc.len()
                                                ),
                                                None => String::from("scale = chromatic (:scale <name>|off|root <note>|<file.scl>)"),
                                            });
                                        } else if rest == "off" {
                                            undo_msg = Some(set_scale(&mut s, &mut history, Some(None), None));
                                        } else if let Some(root) = rest.strip_prefix("root ") {
                                            undo_msg = Some(match parse_root(root) {
                                                Some(r) => set_scale(&mut s, &mut history, None, Some(r)),
                                                None => format!("Invalid root: {}", root),
                                            });
                                        } else if rest.ends_with(".scl") {
                                            undo_msg = Some(
                                                match crate::theory::scl::load_scl(std::path::Path::new(&rest)) {
                                                    Ok(sc) => set_scale(&mut s, &mut history, Some(Some(sc)), None),
                                                    Err(e) => e.to_string(),
                                                },
                                            );
                                        } else {
                                            undo_msg = Some(match crate::theory::scales::lookup(&rest) {
                                                Some(sc) => set_scale(&mut s, &mut history, Some(Some(sc)), None),
                                                None => format!(
                                                    "Unknown scale: {} ({} built-ins — docs/sequencer.md)",
                                                    rest,
                                                    crate::theory::scales::names().len()
                                                ),
                                            });
                                        }
                                    }
                                    "fill" => {
                                        let (kind_str, arg_str) =
                                            match rest.split_once(char::is_whitespace) {
                                                Some((a, b)) => (a, b.trim()),
                                                None => (rest.as_str(), ""),
                                            };
                                        let parsed = if kind_str.is_empty() {
                                            last_fill
                                        } else {
                                            state::FillKind::parse(kind_str).map(|kind| {
                                                let default = match kind {
                                                    state::FillKind::Mutate => 0.3,
                                                    state::FillKind::Density => 0.5,
                                                    _ => 0.0,
                                                };
                                                (kind, arg_str.parse::<f32>().unwrap_or(default))
                                            })
                                        };
                                        match parsed {
                                            Some((kind, arg)) => {
                                                last_fill = Some((kind, arg));
                                                fill_seed = fill_seed.wrapping_add(1);
                                                let action =
                                                    Action::Fill { kind, arg, seed: fill_seed };
                                                undo_msg =
                                                    exec_action(&state, &mut history, &action);
                                                last_change = Some(action);
                                            }
                                            None => {
                                                undo_msg = Some(format!(
                                                    "Unknown fill: {} (mutate density markov cantor thuemorse fibonacci sierpinski)",
                                                    kind_str
                                                ));
                                            }
                                        }
                                    }
                                    _ => undo_msg = Some(format!("Not a command: {}", c)),
                                }
                            }
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

                // Input submode handling: the N value prompt for the active
                // layer (note 0-127, vel 1-127, prob 0-100, mod -1..1)
                if !submode.is_empty() {
                    match key.code {
                        KeyCode::Enter => {
                            let layer = match submode.as_str() {
                                "note" => Some(state::BindTarget::Note),
                                "vel" => Some(state::BindTarget::Velocity),
                                "prob" => Some(state::BindTarget::Prob),
                                "mod" => Some(state::BindTarget::Mod),
                                _ => None,
                            };
                            if let (Some(layer), Ok(value)) = (layer, input_buffer.parse::<f32>())
                            {
                                let action = Action::SetStepValue { layer, value };
                                let mut s = state.lock().unwrap();
                                apply_selected(&mut s, &mut history, &action, None);
                                last_change = Some(action);
                            }
                            submode.clear();
                            input_buffer.clear();
                        }
                        KeyCode::Char(c) if c.is_ascii_digit() || c == '.' || c == '-' => {
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
                    {
                        let mut s = state.lock().unwrap();
                        s.visual_anchor = None;
                        s.visual_track_anchor = None;
                    }
                    submode.clear();
                    input_buffer.clear();
                    pending_count = None;
                    pending_op = None;
                    pending_find = None;
                    pending_angle = None;
                    pending_g = false;
                    pending_layer = false;
                    pending_quote = false;
                    continue;
                }

                // `'` layer prefix: 'n/'v/'p/'m pick the value layer
                // (normal + insert; what the grid shows and k/j/N edit)
                if pending_layer {
                    pending_layer = false;
                    let layer = match key.code {
                        KeyCode::Char('n') => Some(state::BindTarget::Note),
                        KeyCode::Char('v') => Some(state::BindTarget::Velocity),
                        KeyCode::Char('p') => Some(state::BindTarget::Prob),
                        KeyCode::Char('m') => Some(state::BindTarget::Mod),
                        _ => None,
                    };
                    if let Some(layer) = layer {
                        state.lock().unwrap().layer = layer;
                        undo_msg = Some(format!(
                            "layer: {}",
                            match layer {
                                state::BindTarget::Note => "note",
                                state::BindTarget::Velocity => "velocity",
                                state::BindTarget::Prob => "probability",
                                state::BindTarget::Mod => "mod",
                            }
                        ));
                        undo_time = Some(Instant::now());
                    }
                    continue;
                }
                if key.code == KeyCode::Char('\'')
                    && (mode == "normal" || mode == "insert")
                    && submode.is_empty()
                {
                    pending_count = None;
                    pending_layer = true;
                    continue;
                }

                // `"` slot prefix: "a-"h switch pattern, "A-"H save into slot
                if pending_quote {
                    pending_quote = false;
                    let action = match key.code {
                        KeyCode::Char(c @ 'a'..='h') => {
                            Some(Action::SwitchSlot((c as u8 - b'a') as usize))
                        }
                        KeyCode::Char(c @ 'A'..='H') => {
                            Some(Action::SaveSlot((c as u8 - b'A') as usize))
                        }
                        _ => None,
                    };
                    if let Some(action) = action {
                        undo_msg = exec_action(&state, &mut history, &action);
                        undo_time = Some(Instant::now());
                        last_change = Some(action);
                    }
                    continue;
                }
                if key.code == KeyCode::Char('"') && mode == "normal" {
                    pending_count = None;
                    pending_quote = true;
                    continue;
                }

                // g-prefix chords (normal mode): gg first track, gt# go to
                // track, gc/gC cycle mode, gp/gP paste-as-track, gX clear marks
                if mode == "normal" && pending_g {
                    pending_g = false;
                    match key.code {
                        KeyCode::Char('g') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            s.current_track = 0;
                            s.selected = 0;
                            continue;
                        }
                        KeyCode::Char('t') => {
                            pending_count = None;
                            gt_target = Some(String::from("track"));
                            gt_input.clear();
                            gt_last_key = Some(Instant::now());
                            continue;
                        }
                        KeyCode::Char(c @ ('c' | 'C')) => {
                            pending_count = None;
                            let cur = state.lock().unwrap().track().cycle;
                            let all = state::CycleMode::ALL;
                            let i = all.iter().position(|m| *m == cur).unwrap_or(0);
                            let next = if c == 'c' {
                                all[(i + 1) % all.len()]
                            } else {
                                all[(i + all.len() - 1) % all.len()]
                            };
                            let action = Action::SetCycle(next);
                            undo_msg = exec_action(&state, &mut history, &action);
                            undo_time = Some(Instant::now());
                            last_change = Some(action);
                            continue;
                        }
                        KeyCode::Char(c @ ('p' | 'P')) => {
                            let times: usize =
                                pending_count.take().and_then(|n| n.parse().ok()).unwrap_or(1);
                            let action = Action::PasteAsTrack {
                                before: c == 'P',
                                times: times.max(1),
                            };
                            undo_msg = exec_action(&state, &mut history, &action);
                            undo_time = Some(Instant::now());
                            last_change = Some(action);
                            continue;
                        }
                        KeyCode::Char('X') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            for m in s.marks.iter_mut() {
                                *m = false;
                            }
                            undo_msg = Some(String::from("Marks cleared"));
                            undo_time = Some(Instant::now());
                            continue;
                        }
                        _ => {} // unrecognized chord: fall through plain
                    }
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
                                        undo_msg = apply_selected(&mut s, &mut history, &action, None);
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
                            undo_msg = exec_action(&state, &mut history, &action);
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
                                undo_msg = exec_action(&state, &mut history, &action);
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

                // Digits accumulate into pending_count (both modes) — except
                // on the prob layer in insert mode, where they set the value
                // directly, Orca-style: 1-9 → 10-90%, 0 → 100%
                if let KeyCode::Char(c) = key.code {
                    if c.is_ascii_digit() {
                        let prob_entry =
                            mode == "insert" && state.lock().unwrap().layer == state::BindTarget::Prob;
                        if prob_entry {
                            let value = if c == '0' {
                                100.0
                            } else {
                                f32::from(c as u8 - b'0') * 10.0
                            };
                            let action =
                                Action::SetStepValue { layer: state::BindTarget::Prob, value };
                            exec_action(&state, &mut history, &action);
                            last_change = Some(action);
                        } else if c == '0' && pending_count.is_none() {
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
                        exec_action(&state, &mut history, &action);
                        last_change = Some(action);
                        continue;
                    }
                }

                // Visual mode: motions extend the selection; operators act on it
                // V-LINE: a selection of whole tracks; operators fan out
                // over the span as one undo entry
                if mode == "visual_line" {
                    match key.code {
                        KeyCode::Char('v') | KeyCode::Char('V') => {
                            mode = String::from("normal");
                            let mut s = state.lock().unwrap();
                            s.visual_track_anchor = None;
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            let n: usize =
                                pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            s.current_track = (s.current_track + n).min(s.tracks.len() - 1);
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            let n: usize =
                                pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            s.current_track = s.current_track.saturating_sub(n);
                        }
                        KeyCode::Char('o') => {
                            let mut s = state.lock().unwrap();
                            if let Some(a) = s.visual_track_anchor {
                                let cur = s.current_track;
                                s.visual_track_anchor = Some(cur);
                                s.current_track = a.min(s.tracks.len().saturating_sub(1));
                            }
                        }
                        KeyCode::Char(c @ ('y' | 'd' | 'c' | 'x' | '~' | 'm' | 'M')) => {
                            let (span, action) = {
                                let mut s = state.lock().unwrap();
                                let anchor =
                                    s.visual_track_anchor.take().unwrap_or(s.current_track);
                                let span =
                                    (anchor.min(s.current_track), anchor.max(s.current_track));
                                let action = match c {
                                    'y' => Action::OpTrack(Operator::Yank),
                                    'c' => Action::OpTrack(Operator::Change),
                                    'm' => Action::ToggleMute,
                                    'M' => Action::ToggleMode,
                                    '~' => {
                                        s.selected = 0;
                                        Action::ToggleSpan(NUM_STEPS)
                                    }
                                    _ => Action::OpTrack(Operator::Delete),
                                };
                                (span, action)
                            };
                            // yank takes one track (one register, vi rules)
                            let span = if matches!(action, Action::OpTrack(Operator::Yank)) {
                                None
                            } else {
                                Some(span)
                            };
                            {
                                let mut s = state.lock().unwrap();
                                undo_msg = apply_selected(&mut s, &mut history, &action, span);
                            }
                            undo_time = Some(Instant::now());
                            let to_insert = matches!(action, Action::OpTrack(Operator::Change));
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

                // VISUAL: motions extend the step selection (handled by the
                // mode-independent navigation above); operators act on it
                if mode == "visual" {
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
                                match c {
                                    '~' => Action::ToggleSpan(span),
                                    'y' => Action::Op { op: Operator::Yank, motion: Motion::Span(span), count: 1 },
                                    'c' => Action::Op { op: Operator::Change, motion: Motion::Span(span), count: 1 },
                                    _ => Action::Op { op: Operator::Delete, motion: Motion::Span(span), count: 1 },
                                }
                            };
                            undo_msg = exec_action(&state, &mut history, &action);
                            undo_time = Some(Instant::now());
                            let to_insert =
                                matches!(action, Action::Op { op: Operator::Change, .. });
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
                            let mut s = state.lock().unwrap();
                            let cur = s.current_track;
                            s.visual_track_anchor = Some(cur);
                            mode = String::from("visual_line");
                        }

                        // Operators (await motion; doubled = whole track)
                        KeyCode::Char(c @ ('y' | 'd' | 'c')) => {
                            let opcount: usize =
                                pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            pending_op = Operator::from_char(c).map(|op| (op, opcount.max(1)));
                            pending_g = false;
                        }
                        // Shorthands to end-of-pattern: Y=y$, D=d$, C=c$
                        KeyCode::Char(c @ ('Y' | 'D')) => {
                            pending_count = None;
                            let op = if c == 'Y' { Operator::Yank } else { Operator::Delete };
                            let action = Action::Op { op, motion: Motion::End, count: 1 };
                            undo_msg = exec_action(&state, &mut history, &action);
                            undo_time = Some(Instant::now());
                            if action.is_change() {
                                last_change = Some(action);
                            }
                        }
                        KeyCode::Char('C') => {
                            pending_count = None;
                            let action =
                                Action::Op { op: Operator::Change, motion: Motion::End, count: 1 };
                            undo_msg = exec_action(&state, &mut history, &action);
                            undo_time = Some(Instant::now());
                            last_change = Some(action);
                            mode = String::from("insert");
                        }
                        // Track marks: the non-consecutive multi-select.
                        // Marked tracks receive every track-level edit.
                        KeyCode::Char('X') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let cur = s.current_track;
                            if cur < s.marks.len() {
                                s.marks[cur] = !s.marks[cur];
                                let n = s.marks.iter().filter(|&&m| m).count();
                                undo_msg = Some(format!(
                                    "{} t{} ({} marked)",
                                    if s.marks[cur] { "Marked" } else { "Unmarked" },
                                    cur + 1,
                                    n
                                ));
                                undo_time = Some(Instant::now());
                            }
                        }
                        // F re-runs the last :fill with a fresh seed — the
                        // spam-until-it-grooves gesture (. repeats exactly)
                        KeyCode::Char('F') => {
                            pending_count = None;
                            match last_fill {
                                Some((kind, arg)) => {
                                    fill_seed = fill_seed.wrapping_add(1);
                                    let action = Action::Fill { kind, arg, seed: fill_seed };
                                    undo_msg = exec_action(&state, &mut history, &action);
                                    undo_time = Some(Instant::now());
                                    last_change = Some(action);
                                }
                                None => {
                                    undo_msg = Some(String::from(
                                        "No fill yet (:fill mutate|density|markov|…)",
                                    ));
                                    undo_time = Some(Instant::now());
                                }
                            }
                        }

                        // Step edits
                        KeyCode::Char('x') => {
                            pending_count = None;
                            let action = Action::CutStep;
                            exec_action(&state, &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('~') => {
                            pending_count = None;
                            let action = Action::ToggleStep;
                            exec_action(&state, &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('p') => {
                            let times: usize =
                                pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let action = Action::Paste { before: false, times: times.max(1) };
                            undo_msg = exec_action(&state, &mut history, &action);
                            undo_time = Some(Instant::now());
                            last_change = Some(action);
                        }
                        KeyCode::Char('P') => {
                            // bare P only — counted #P is the Euclidean pulses setter
                            pending_count = None;
                            let action = Action::Paste { before: true, times: 1 };
                            undo_msg = exec_action(&state, &mut history, &action);
                            undo_time = Some(Instant::now());
                            last_change = Some(action);
                        }
                        KeyCode::Char('.') => {
                            pending_count = None;
                            if let Some(action) = last_change.clone() {
                                undo_msg = exec_action(&state, &mut history, &action);
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
                            exec_action(&state, &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('O') => {
                            pending_count = None;
                            let action = Action::NewTrack { before: true };
                            exec_action(&state, &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('m') => {
                            pending_count = None;
                            pending_g = false;
                            let action = Action::ToggleMute;
                            exec_action(&state, &mut history, &action);
                            last_change = Some(action);
                        }
                        // M toggles note/modulation mode (@ is reserved for
                        // macros; kept as a legacy alias until they land)
                        KeyCode::Char('M') | KeyCode::Char('@') => {
                            pending_count = None;
                            pending_g = false;
                            let action = Action::ToggleMode;
                            exec_action(&state, &mut history, &action);
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
                                exec_action(&state, &mut history, &action);
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
                            // chords (gg/gt/gc/gp/gX) resolve in the
                            // g-prefix block above on the NEXT key
                            pending_g = true;
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
                            exec_action(&state, &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('x') => {
                            pending_count = None;
                            let action = Action::CutStep;
                            exec_action(&state, &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('y') => {
                            pending_count = None;
                            undo_msg = exec_action(&state, &mut history, &Action::YankStep);
                            undo_time = Some(Instant::now());
                        }
                        KeyCode::Char('p') => {
                            let times: usize =
                                pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let action = Action::Paste { before: false, times: times.max(1) };
                            undo_msg = exec_action(&state, &mut history, &action);
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
                                undo_msg = exec_action(&state, &mut history, &action);
                                undo_time = Some(Instant::now());
                            }
                        }
                        // k/j fine, K/J coarse — edits whatever the active
                        // layer shows (note keeps the legacy mod-track feel)
                        KeyCode::Char(c @ ('k' | 'j' | 'K' | 'J')) => {
                            let n: i32 = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let (steps, coarse) = match c {
                                'k' => (n, false),
                                'j' => (-n, false),
                                'K' => (n, true),
                                _ => (-n, true),
                            };
                            let layer = state.lock().unwrap().layer;
                            let action = Action::Adjust { layer, steps, coarse };
                            exec_action(&state, &mut history, &action);
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
                        // N prompts a literal value for the active layer
                        KeyCode::Char('N') => {
                            pending_count = None;
                            submode = String::from(match state.lock().unwrap().layer {
                                state::BindTarget::Note => "note",
                                state::BindTarget::Velocity => "vel",
                                state::BindTarget::Prob => "prob",
                                state::BindTarget::Mod => "mod",
                            });
                            input_buffer.clear();
                        }
                        // Euclidean re-apply (no count)
                        KeyCode::Char('P') | KeyCode::Char('L') => {
                            pending_count = None;
                            let action = Action::Euclid(EuclidOp::Reapply);
                            exec_action(&state, &mut history, &action);
                            last_change = Some(action);
                        }
                        KeyCode::Char('R') => {
                            pending_count = None;
                            let action = Action::Euclid(EuclidOp::RotatePlus(1));
                            exec_action(&state, &mut history, &action);
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
        // plain euclid-starter tracks: tests want predictable steps, not
        // the curated fresh-session melody defaults
        let mut s = SequencerState {
            tracks: (0..n).map(|_| Track::new()).collect(),
            ..Default::default()
        };
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
        apply_action(s, h, &Action::Adjust { layer: state::BindTarget::Note, steps: 1, coarse: false });
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
        s.tracks[tidx].length = length.clamp(1, NUM_STEPS);
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
        let before = s.tracks[0].steps[4].clone();
        cut_step(&mut s, &mut h);
        assert!(!s.tracks[0].steps[4].active);
        assert!(h.undo(&mut s).is_some());
        assert_eq!(s.tracks[0].steps[4], before);
    }

    #[test]
    fn paste_step_undo() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.register = Some(Register::Steps(vec![Step { active: true, note: 72, velocity: 90, ..Step::default() }]));
        s.selected = 1;
        let before = s.tracks[0].steps[1].clone();
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
            Step { active: true, note: 60, ..Step::default() },
            Step { active: true, note: 62, ..Step::default() },
        ]));
        s.selected = 5;
        apply_action(&mut s, &mut h, &Action::Paste { before: true, times: 1 });
        assert_eq!(s.tracks[0].steps[4].note, 60);
        assert_eq!(s.tracks[0].steps[5].note, 62);
        assert_eq!(s.selected, 4, "cursor moves to paste start");
    }

    #[test]
    fn track_register_pastes_into_the_row() {
        // p with a yanked track overwrites steps in place — no new track
        let mut s = state_with_tracks(2);
        let mut h = History::new();
        s.tracks[0].steps[7].note = 99;
        apply_action(&mut s, &mut h, &Action::OpTrack(Operator::Yank));
        s.current_track = 1;
        s.selected = 0;
        apply_action(&mut s, &mut h, &Action::Paste { before: false, times: 1 });
        assert_eq!(s.tracks.len(), 2, "p never creates tracks anymore");
        assert_eq!(s.tracks[1].steps[7].note, 99, "steps landed in the row");
        assert!(h.undo(&mut s).is_some());
        assert_ne!(s.tracks[1].steps[7].note, 99, "undo reverts the overwrite");
    }

    #[test]
    fn gp_materializes_register_as_track() {
        let mut s = state_with_tracks(2);
        let mut h = History::new();
        s.tracks[0].steps[7].note = 99;
        // a yanked TRACK inserts wholesale
        apply_action(&mut s, &mut h, &Action::OpTrack(Operator::Yank));
        s.current_track = 1;
        apply_action(&mut s, &mut h, &Action::PasteAsTrack { before: false, times: 1 });
        assert_eq!(s.tracks.len(), 3);
        assert_eq!(s.tracks[2].steps[7].note, 99, "track pasted after current");
        assert!(h.undo(&mut s).is_some());
        assert_eq!(s.tracks.len(), 2);
        // a yanked steps register becomes a track of exactly that length
        s.register = Some(Register::Steps(vec![
            Step { active: true, note: 71, ..Step::default() },
            Step::default(),
            Step { active: true, note: 67, ..Step::default() },
        ]));
        s.current_track = 0;
        apply_action(&mut s, &mut h, &Action::PasteAsTrack { before: false, times: 1 });
        assert_eq!(s.tracks.len(), 3);
        assert_eq!(s.tracks[1].length, 3, "polymeter: track is yank-sized");
        assert_eq!(s.tracks[1].steps[0].note, 71);
        assert_eq!(s.tracks[1].pulses, 2);
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
        s.register = Some(Register::Steps(vec![Step { active: true, note: 65, ..Step::default() }]));
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
            apply_action(&mut s, &mut h, &Action::Adjust { layer: state::BindTarget::Note, steps: 1, coarse: false });
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
        apply_action(&mut s, &mut h, &Action::Adjust { layer: state::BindTarget::Note, steps: 3, coarse: false });
        apply_action(&mut s, &mut h, &Action::Adjust { layer: state::BindTarget::Note, steps: -3, coarse: false });
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

    #[test]
    fn row_window_geometry() {
        // short pattern: no scroll
        assert_eq!(row_window_start(16, 8, 24), 0);
        // long pattern: anchor centered, clamped at both ends
        assert_eq!(row_window_start(128, 0, 40), 0);
        assert_eq!(row_window_start(128, 64, 40), 44);
        assert_eq!(row_window_start(128, 127, 40), 88, "clamps at the tail");
        // visible never exceeds the pattern or underflows
        assert_eq!(row_visible(60, 12, 16), 16);
        assert!(row_visible(60, 12, 128) >= 4);
        assert!(row_visible(10, 12, 128) >= 4, "tiny panes stay sane");
    }

    #[test]
    fn stuck_notes_release_on_pause_mute_and_mode_switch() {
        let mut tracks = vec![Track::new(), Track::new(), Track::new()];
        let held = vec![Some(60u8), Some(62), None];
        // playing, nothing muted: no stuck notes
        assert!(stuck_notes(&tracks, &held, true).is_empty());
        // paused: every held note is stuck
        assert_eq!(stuck_notes(&tracks, &held, false), vec![(0, 60), (1, 62)]);
        // muted mid-note: that track's note is stuck even while playing
        tracks[0].muted = true;
        assert_eq!(stuck_notes(&tracks, &held, true), vec![(0, 60)]);
        // switched to modulation mode mid-note: stuck
        tracks[0].muted = false;
        tracks[1].mode = TrackMode::Modulation;
        assert_eq!(stuck_notes(&tracks, &held, true), vec![(1, 62)]);
        // last_notes longer than tracks never panics
        let extra = vec![Some(60u8); 5];
        assert_eq!(stuck_notes(&tracks, &extra, false).len(), 3);
    }

    #[test]
    fn fresh_session_defaults_are_playable_but_paused() {
        let s = SequencerState::default();
        assert!(!s.playing, "fresh sessions start paused");
        // t1 melody + t3 bass, both note mode with active steps
        for ti in [0usize, 2] {
            assert_eq!(s.tracks[ti].mode, state::TrackMode::Note);
            let actives = s.tracks[ti].steps[..16].iter().filter(|st| st.active).count();
            assert!(actives >= 4, "t{} carries a pattern", ti + 1);
            assert_eq!(s.tracks[ti].pulses, actives, "pulse count stays honest");
        }
        // t2/t4 are modulation tracks, empty
        for ti in [1usize, 3] {
            assert_eq!(s.tracks[ti].mode, state::TrackMode::Modulation);
            assert!(s.tracks[ti].steps.iter().all(|st| !st.active));
        }
        // the rest are silent blank slates
        for t in &s.tracks[4..] {
            assert!(t.steps.iter().all(|st| !st.active));
        }
        // bass sits well below the lead
        let lead_min = s.tracks[0].steps[..16].iter().filter(|st| st.active).map(|st| st.note).min();
        let bass_max = s.tracks[2].steps[..16].iter().filter(|st| st.active).map(|st| st.note).max();
        assert!(bass_max < lead_min, "t3 is the bass");
    }

    // ── playback engine: cycle modes, probability, pitch, bindings ──────

    /// Collect the positions a cycle mode visits over `n` global steps.
    fn walk(mode: state::CycleMode, len: usize, n: u64) -> Vec<usize> {
        let mut drunk = 0usize;
        (0..n).map(|g| cycle_pos(mode, g, len, &mut drunk, track_seed(0))).collect()
    }

    #[test]
    fn cycle_forward_reverse_pingpong() {
        assert_eq!(walk(state::CycleMode::Forward, 4, 6), vec![0, 1, 2, 3, 0, 1]);
        assert_eq!(walk(state::CycleMode::Reverse, 4, 6), vec![3, 2, 1, 0, 3, 2]);
        // pingpong over 4 steps: period 6, no double-hit at the ends
        assert_eq!(
            walk(state::CycleMode::PingPong, 4, 8),
            vec![0, 1, 2, 3, 2, 1, 0, 1]
        );
    }

    #[test]
    fn cycle_exotics_stay_in_range_and_permute() {
        for mode in state::CycleMode::ALL {
            for len in [1usize, 2, 3, 4, 7, 16, 128] {
                for pos in walk(mode, len, 256) {
                    assert!(pos < len, "{mode:?} len {len} escaped: {pos}");
                }
            }
        }
        // spiral interleaves outside-in
        assert_eq!(walk(state::CycleMode::Spiral, 6, 6), vec![0, 5, 1, 4, 2, 3]);
        // every-other skips by two (odd lengths cover everything)
        assert_eq!(walk(state::CycleMode::EveryOther, 5, 5), vec![0, 2, 4, 1, 3]);
        // prime-jump visits every step exactly once per cycle
        for len in [4usize, 6, 12, 16] {
            let mut seen = walk(state::CycleMode::PrimeJump, len, len as u64);
            seen.sort_unstable();
            assert_eq!(seen, (0..len).collect::<Vec<_>>(), "prime-jump len {len}");
        }
    }

    #[test]
    fn cycle_random_is_deterministic_and_varied() {
        let a = walk(state::CycleMode::Random, 16, 64);
        let b = walk(state::CycleMode::Random, 16, 64);
        assert_eq!(a, b, "same seed, same path");
        let distinct: std::collections::HashSet<_> = a.iter().collect();
        assert!(distinct.len() > 4, "random should wander");
    }

    #[test]
    fn cycle_drunk_steps_at_most_one() {
        let path = walk(state::CycleMode::Drunk, 16, 256);
        for w in path.windows(2) {
            let d = (w[1] as i64 - w[0] as i64).rem_euclid(16);
            assert!(d == 0 || d == 1 || d == 15, "drunk lurched {} -> {}", w[0], w[1]);
        }
    }

    #[test]
    fn prime_jump_mult_is_coprime() {
        fn gcd(mut a: u64, mut b: u64) -> u64 {
            while b != 0 {
                (a, b) = (b, a % b);
            }
            a
        }
        for len in 2usize..=128 {
            let m = prime_jump_mult(len);
            assert_eq!(gcd(m, len as u64), 1, "len {len} got multiplier {m}");
        }
    }

    #[test]
    fn probability_gate_extremes_and_determinism() {
        for g in 0..64 {
            assert!(step_fires(100, 0, g), "p=100 always fires");
            assert!(!step_fires(0, 0, g), "p=0 never fires");
        }
        // deterministic per (track, gstep)
        assert_eq!(step_fires(50, 3, 17), step_fires(50, 3, 17));
        // roughly half over many steps
        let hits = (0..2000).filter(|&g| step_fires(50, 1, g)).count();
        assert!((800..1200).contains(&hits), "p=50 fired {hits}/2000");
        // different tracks decorrelate
        let same = (0..500)
            .filter(|&g| step_fires(50, 0, g) == step_fires(50, 1, g))
            .count();
        assert!(same < 400, "tracks 0/1 agreed {same}/500 times");
    }

    #[test]
    fn step_hz_unscaled_is_midi_and_scaled_is_degrees() {
        let trk = Track::empty();
        assert!((step_hz(&trk, 69, 0) - 440.0).abs() < 1e-9);
        assert!((step_hz(&trk, 69, 12) - 880.0).abs() < 1e-6);
        assert!((step_hz(&trk, 127, 12) - crate::theory::scales::midi_to_hz(127)).abs() < 1e-6, "offset clamps at the top");

        let mut scaled = Track::empty();
        scaled.scale = crate::theory::scales::lookup("major pentatonic");
        scaled.root = 69; // A4
        // note 60 = the root; +5 degrees = one octave in a pentatonic
        assert!((step_hz(&scaled, 60, 0) - 440.0).abs() < 1e-9);
        assert!((step_hz(&scaled, 65, 0) - 880.0).abs() < 1e-6);
        assert!((step_hz(&scaled, 60, 5) - 880.0).abs() < 1e-6, "bind offset moves in degrees");
        assert!((step_hz(&scaled, 55, 0) - 220.0).abs() < 1e-6, "below the root works");
    }

    #[test]
    fn bind_offsets_target_one_param() {
        let bind = StepBind {
            target: state::BindTarget::Velocity,
            source: String::from("envelope/0/ch1"),
            amount: 0.5,
        };
        let o = bind_offsets(Some(&bind), Some(1.0));
        assert_eq!(o.velocity, 64);
        assert_eq!((o.degrees, o.prob), (0, 0));
        assert_eq!(o.mod_value, 0.0);
        // no live value (orphaned source) → no offset
        assert_eq!(bind_offsets(Some(&bind), None), BindOffsets::default());
        assert_eq!(bind_offsets(None, Some(1.0)), BindOffsets::default());
        // note target moves in degrees, scaled ±12 per unit
        let nb = StepBind { target: state::BindTarget::Note, source: String::new(), amount: -1.0 };
        assert_eq!(bind_offsets(Some(&nb), Some(0.5)).degrees, -6);
        let pb = StepBind { target: state::BindTarget::Prob, source: String::new(), amount: 1.0 };
        assert_eq!(bind_offsets(Some(&pb), Some(-0.25)).prob, -25);
    }

    #[test]
    fn persistence_roundtrips_new_fields() {
        let mut s = state_with_tracks(2);
        s.tracks[0].cycle = state::CycleMode::PingPong;
        s.tracks[0].scale = crate::theory::scales::lookup("rast");
        s.tracks[0].root = 62;
        s.tracks[0].steps[3].prob = 40;
        s.tracks[0].steps[3].bind = Some(StepBind {
            target: state::BindTarget::Prob,
            source: String::from("envelope/0/ch2"),
            amount: 0.75,
        });
        s.tracks[1].active_slot = 2;
        let mut alt = PatternData::empty();
        alt.steps[0].active = true;
        alt.steps[0].note = 71;
        alt.length = 12;
        s.tracks[1].slots[5] = Some(alt.clone());

        let params = snapshot_params(&s);
        let toml = state::to_toml_string(&params).expect("serializes");
        let back: state::SequencerParams = toml::from_str(&toml).expect("parses");
        let mut s2 = SequencerState::default();
        apply_tracks(&mut s2, &back);

        assert_eq!(s2.tracks[0].cycle, state::CycleMode::PingPong);
        let sc = s2.tracks[0].scale.as_ref().expect("scale survives");
        assert_eq!(sc.name, "rast");
        assert!((sc.degrees[2] - 350.0).abs() < 1e-9, "quarter tones survive");
        assert_eq!(s2.tracks[0].root, 62);
        assert_eq!(s2.tracks[0].steps[3].prob, 40);
        let b = s2.tracks[0].steps[3].bind.as_ref().expect("bind survives");
        assert_eq!(b.target, state::BindTarget::Prob);
        assert_eq!(b.source, "envelope/0/ch2");
        assert!((b.amount - 0.75).abs() < 1e-6);
        assert_eq!(s2.tracks[1].active_slot, 2);
        assert_eq!(s2.tracks[1].slots[5], Some(alt));
        assert!(s2.tracks[1].slots[2].is_none(), "active slot stays inline");
    }

    // ── vi grammar v2: multi-select, slots, layers, scales, fills ───────

    #[test]
    fn marked_tracks_fan_out_as_one_undo() {
        let mut s = state_with_tracks(3);
        let mut h = History::new();
        s.marks = vec![true, false, true];
        s.current_track = 1;
        let msg = apply_selected(&mut s, &mut h, &Action::ToggleMute, None);
        let _ = msg;
        assert!(s.tracks[0].muted && s.tracks[2].muted, "marked tracks muted");
        assert!(!s.tracks[1].muted, "unmarked current track untouched");
        assert!(h.undo(&mut s).is_some());
        assert!(!s.tracks[0].muted && !s.tracks[2].muted, "one u reverts both");
        assert_eq!(h.undo(&mut s), None, "it was a single entry");
    }

    #[test]
    fn vline_span_deletes_tracks_safely() {
        let mut s = state_with_tracks(4);
        let mut h = History::new();
        for (i, t) in s.tracks.iter_mut().enumerate() {
            t.steps[0].note = 40 + i as u8;
        }
        s.current_track = 1;
        apply_selected(&mut s, &mut h, &Action::OpTrack(Operator::Delete), Some((1, 2)));
        assert_eq!(s.tracks.len(), 2);
        assert_eq!(s.tracks[0].steps[0].note, 40);
        assert_eq!(s.tracks[1].steps[0].note, 43, "outer tracks survive");
        assert!(h.undo(&mut s).is_some());
        assert_eq!(s.tracks.len(), 4, "one u restores the span");
        assert_eq!(s.tracks[1].steps[0].note, 41);
        assert_eq!(s.tracks[2].steps[0].note, 42);
    }

    #[test]
    fn yank_never_fans_out() {
        let mut s = state_with_tracks(3);
        let mut h = History::new();
        s.marks = vec![true, true, true];
        s.current_track = 0;
        apply_selected(&mut s, &mut h, &Action::YankStep, None);
        assert!(matches!(s.register, Some(Register::Steps(ref v)) if v.len() == 1));
        assert_eq!(h.undo(&mut s), None, "yank is not a change");
    }

    #[test]
    fn switch_slot_swaps_patterns_and_undoes() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.tracks[0].steps[0].note = 99;
        let original = s.tracks[0].steps[0].clone();
        apply_action(&mut s, &mut h, &Action::SwitchSlot(1));
        assert_eq!(s.tracks[0].active_slot, 1);
        assert!(s.tracks[0].slots[1].is_none(), "active slot data is inline");
        assert!(s.tracks[0].slots[0].is_some(), "old pattern parked in a");
        assert_ne!(s.tracks[0].steps[0], original, "slot b starts empty");
        // edit b, switch back, both survive
        s.tracks[0].steps[2].active = true;
        apply_action(&mut s, &mut h, &Action::SwitchSlot(0));
        assert_eq!(s.tracks[0].steps[0], original, "pattern a intact");
        assert!(s.tracks[0].slots[1].as_ref().is_some_and(|p| p.steps[2].active));
        // undo walks back through both switches
        assert_eq!(h.undo(&mut s), Some("Switch pattern"));
        assert_eq!(s.tracks[0].active_slot, 1);
        assert_eq!(h.undo(&mut s), Some("Switch pattern"));
        assert_eq!(s.tracks[0].active_slot, 0);
        assert_eq!(s.tracks[0].steps[0], original);
    }

    #[test]
    fn save_slot_copies_and_undoes() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        s.tracks[0].steps[0].note = 77;
        apply_action(&mut s, &mut h, &Action::SaveSlot(3));
        let saved = s.tracks[0].slots[3].as_ref().expect("slot d filled");
        assert_eq!(saved.steps[0].note, 77);
        assert_eq!(s.tracks[0].active_slot, 0, "saving doesn't switch");
        assert!(h.undo(&mut s).is_some());
        assert!(s.tracks[0].slots[3].is_none());
        // saving onto the active slot refuses
        let msg = apply_action(&mut s, &mut h, &Action::SaveSlot(0));
        assert_eq!(msg.as_deref(), Some("a is the active pattern"));
    }

    #[test]
    fn scale_assignment_converts_and_undoes() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        // C major triad in MIDI on a chromatic track, root C4
        s.tracks[0].steps[0].note = 60;
        s.tracks[0].steps[1].note = 64;
        s.tracks[0].steps[2].note = 67;
        let major = crate::theory::scales::lookup("major").expect("major exists");
        set_scale(&mut s, &mut h, Some(Some(major)), None);
        // degrees biased at 60: root, 3rd (degree 2), 5th (degree 4)
        assert_eq!(s.tracks[0].steps[0].note, 60);
        assert_eq!(s.tracks[0].steps[1].note, 62);
        assert_eq!(s.tracks[0].steps[2].note, 64);
        // back to chromatic: notes return
        set_scale(&mut s, &mut h, Some(None), None);
        assert_eq!(s.tracks[0].steps[1].note, 64);
        assert_eq!(s.tracks[0].steps[2].note, 67);
        assert!(h.undo(&mut s).is_some() && h.undo(&mut s).is_some());
        assert!(s.tracks[0].scale.is_none());
        assert_eq!(s.tracks[0].steps[1].note, 64, "full round trip");
    }

    #[test]
    fn adjust_edits_the_active_layer_with_clamps() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        apply_action(&mut s, &mut h, &Action::Adjust { layer: state::BindTarget::Velocity, steps: 2, coarse: false });
        assert_eq!(s.tracks[0].steps[0].velocity, 108);
        apply_action(&mut s, &mut h, &Action::Adjust { layer: state::BindTarget::Velocity, steps: 10, coarse: true });
        assert_eq!(s.tracks[0].steps[0].velocity, 127, "velocity clamps high");
        apply_action(&mut s, &mut h, &Action::Adjust { layer: state::BindTarget::Prob, steps: -3, coarse: true });
        assert_eq!(s.tracks[0].steps[0].prob, 25);
        apply_action(&mut s, &mut h, &Action::Adjust { layer: state::BindTarget::Prob, steps: -3, coarse: true });
        assert_eq!(s.tracks[0].steps[0].prob, 0, "prob clamps at zero");
        apply_action(&mut s, &mut h, &Action::Adjust { layer: state::BindTarget::Mod, steps: 5, coarse: true });
        assert!((s.tracks[0].steps[0].mod_value - 0.5).abs() < 1e-6);
        // coarse note jump on a scaled track moves one period
        s.tracks[0].scale = crate::theory::scales::lookup("major pentatonic");
        apply_action(&mut s, &mut h, &Action::Adjust { layer: state::BindTarget::Note, steps: 1, coarse: true });
        assert_eq!(s.tracks[0].steps[0].note, 65, "+1 period = +5 degrees");
    }

    #[test]
    fn set_step_value_clamps_per_layer() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        apply_action(&mut s, &mut h, &Action::SetStepValue { layer: state::BindTarget::Prob, value: 250.0 });
        assert_eq!(s.tracks[0].steps[0].prob, 100);
        apply_action(&mut s, &mut h, &Action::SetStepValue { layer: state::BindTarget::Note, value: -5.0 });
        assert_eq!(s.tracks[0].steps[0].note, 0);
        apply_action(&mut s, &mut h, &Action::SetStepValue { layer: state::BindTarget::Mod, value: 0.25 });
        assert!((s.tracks[0].steps[0].mod_value - 0.25).abs() < 1e-6);
        assert!(h.undo(&mut s).is_some());
        assert_eq!(s.tracks[0].steps[0].mod_value, 0.0);
    }

    #[test]
    fn fill_density_hits_target_and_undoes() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        let before: Vec<Step> = s.tracks[0].steps[..16].to_vec();
        apply_action(&mut s, &mut h, &Action::Fill { kind: state::FillKind::Density, arg: 0.5, seed: 7 });
        let active = s.tracks[0].steps[..16].iter().filter(|st| st.active).count();
        assert_eq!(active, 8, "density 0.5 over 16 = 8 triggers");
        assert!(h.undo(&mut s).is_some());
        assert_eq!(s.tracks[0].steps[..16].to_vec(), before);
    }

    #[test]
    fn fill_markov_survives_a_lonely_track() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        // no other tracks to learn from — must not panic
        apply_action(&mut s, &mut h, &Action::Fill { kind: state::FillKind::Markov, arg: 0.0, seed: 3 });
        assert!(s.tracks[0].steps[..16].iter().all(|st| st.note > 0));
    }

    #[test]
    fn fill_same_seed_repeats_exactly() {
        let mut a = state_with_tracks(1);
        let mut b = state_with_tracks(1);
        let mut h = History::new();
        let act = Action::Fill { kind: state::FillKind::Mutate, arg: 0.8, seed: 42 };
        apply_action(&mut a, &mut h, &act);
        apply_action(&mut b, &mut h, &act);
        assert_eq!(a.tracks[0].steps, b.tracks[0].steps, "dot-repeat is exact");
    }

    #[test]
    fn parse_root_accepts_numbers_and_names() {
        assert_eq!(parse_root("60"), Some(60));
        assert_eq!(parse_root("0"), Some(0));
        assert_eq!(parse_root("127"), Some(127));
        assert_eq!(parse_root("128"), None);
        assert_eq!(parse_root("C4"), Some(60));
        assert_eq!(parse_root("a3"), Some(57));
        assert_eq!(parse_root("C#4"), Some(61));
        assert_eq!(parse_root("eb2"), Some(39));
        assert_eq!(parse_root("C-1"), Some(0));
        assert_eq!(parse_root("H4"), None);
        assert_eq!(parse_root(""), None);
    }

    #[test]
    fn old_save_files_load_with_defaults() {
        // a pre-v2 save knows nothing of prob/bind/cycle/scale/slots
        let toml = r#"
bpm = 100.0

[[tracks]]
length = 8
pulses = 2
rotation = 0
muted = false
mode = "note"

[[tracks.steps]]
active = true
note = 64
velocity = 90
"#;
        let params: state::SequencerParams = toml::from_str(toml).expect("legacy parses");
        let mut s = SequencerState::default();
        apply_tracks(&mut s, &params);
        assert_eq!(s.tracks.len(), 1);
        let st = &s.tracks[0].steps[0];
        assert!(st.active);
        assert_eq!(st.prob, 100, "legacy steps always fire");
        assert!(st.bind.is_none());
        assert_eq!(s.tracks[0].cycle, state::CycleMode::Forward);
        assert!(s.tracks[0].scale.is_none());
        assert_eq!(s.tracks[0].root, 60);
        assert_eq!(s.tracks[0].active_slot, 0);
        assert!(s.tracks[0].slots.iter().all(Option::is_none));
    }
}
