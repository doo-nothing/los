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
/// header + macro lane + track rows + rule + detail strip (3) + rule +
/// modeline. `conductor::house_dims` snaps the SEQ pane to this — if the
/// draw gains or loses a line, update the arithmetic here and the layout
/// follows (the geometry tests pin the relationship).
pub const CONTENT_LINES: usize = crate::NUM_TRACKS + 8;

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

/// Global steps per beat and per bar (16th-note steps, 4/4) — the macro
/// quantize grid and the macro lane's slot duration.
const STEPS_PER_BEAT: u64 = 4;
const STEPS_PER_BAR: u64 = 16;

/// Default macro lane length in slots (one bar each).
const DEFAULT_LANE_LEN: usize = 8;

/// A recorded macro: semantic commands (`q{a-z}` records, `:macro` edits)
/// plus when a live `@` firing takes effect.
#[derive(Debug, Clone, PartialEq, Default)]
struct Macro {
    cmds: Vec<state::MacroCmd>,
    quant: state::Quant,
}

/// A live `@{a-z}` firing waiting for its quantize boundary. `at` is
/// computed by the playback thread on first sight (it owns the clock).
#[derive(Debug, Clone, Copy, PartialEq)]
struct PendingMacro {
    idx: usize,
    at: Option<u64>,
}

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
    /// The 26 macro registers a–z.
    macros: Vec<Option<Macro>>,
    /// Last fired macro (`@@` refires it).
    last_macro: Option<usize>,
    /// One-shot status from thread-side macro firings ("@a" / "@a: no
    /// change"), drained into the modeline by the UI loop.
    macro_flash: Option<String>,
    /// The macro lane: one slot per bar, each optionally firing a macro.
    lane: Vec<Option<usize>>,
    /// Lane playhead (bar index, wrapped), kept by the playback thread.
    lane_pos: usize,
    /// Lane cursor (`k` from track 1 reaches the lane).
    lane_selected: usize,
    /// Whether the cursor sits on the lane instead of a track.
    on_lane: bool,
    /// The lane's own yank register.
    lane_register: Option<Vec<Option<usize>>>,
    /// Live firings waiting for their quantize boundary.
    pending_macros: Vec<PendingMacro>,
    /// Undo entries produced by thread-side macro firings, drained into
    /// the UI loop's history each frame.
    macro_outbox: Vec<Command>,
    /// Recently-visited positions per track, most recent first (max 3) —
    /// the playhead trail. Visit HISTORY, not position math, so reverse,
    /// spiral, random and drunk all read correctly.
    trails: Vec<Vec<usize>>,
    /// Note-offs owed to the voices: (note, source channel at note-on
    /// time). Filled whenever track indices shift or patterns are
    /// replaced wholesale — note-offs match by SOURCE, so a held note
    /// must be released under its original channel before bookkeeping
    /// moves. The playback thread drains this every tick.
    pending_offs: Vec<(u8, u8)>,
}

impl Default for SequencerState {
    fn default() -> Self {
        // a fresh session uses HALF the track budget: the default patch
        // only wires four rows, and a board born full made o/gp dead keys
        // (the modbus claim caps tracks at NUM_TRACKS)
        let track_count = crate::NUM_TRACKS.min(4);
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
            macros: vec![None; 26],
            last_macro: None,
            macro_flash: None,
            lane: vec![None; DEFAULT_LANE_LEN],
            lane_pos: 0,
            lane_selected: 0,
            on_lane: false,
            lane_register: None,
            pending_macros: Vec::new(),
            macro_outbox: Vec::new(),
            pending_offs: Vec::new(),
            trails: vec![Vec::new(); track_count],
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
    /// Macro lane edits (assign/clear/paste/length) — whole-lane snapshot.
    SetLane {
        old: Vec<Option<usize>>,
        new: Vec<Option<usize>>,
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
    /// One or more whole tracks, in row order (multi-select yanks fill
    /// several; `p` block-pastes them, `gp` inserts them all).
    Tracks(Vec<Track>),
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
    /// `B` (picker) / `gB`: patch a mod source into the step's current
    /// layer — `None` source clears the cable
    BindStep { target: state::BindTarget, source: Option<String> },
    /// `(` / `)`: scale the bound source's influence
    BindAmount { delta: f32 },
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
                // every register kind pastes INTO rows at the cursor — a
                // multi-track yank BLOCK-pastes down successive rows
                // (gp/gP materialize registers as new tracks instead)
                let slices: Vec<Vec<Step>> = match reg {
                    Register::Steps(v) => vec![v],
                    Register::Tracks(ts) => {
                        ts.iter().map(|t| t.steps[..t.length].to_vec()).collect()
                    }
                };
                if slices.iter().all(Vec::is_empty) {
                    return Some(String::from("Nothing in register"));
                }
                let block = slices.len() > 1;
                if block {
                    h.begin_group();
                }
                let mut wrote = 0usize;
                let mut first_start = None;
                for (k, slice) in slices.iter().enumerate() {
                    let row = tidx + k;
                    if row >= s.tracks.len() || slice.is_empty() {
                        break;
                    }
                    let len = s.tracks[row].length;
                    let total = (slice.len() * times.max(&1)).min(len);
                    let start = if *before {
                        (s.selected + 1).saturating_sub(total)
                    } else {
                        s.selected.min(len - 1)
                    };
                    let end = (start + total - 1).min(len - 1);
                    let old: Vec<Step> = s.tracks[row].steps[start..=end].to_vec();
                    let new: Vec<Step> =
                        (0..=(end - start)).map(|i| slice[i % slice.len()].clone()).collect();
                    if old == new {
                        continue;
                    }
                    s.tracks[row].steps[start..=end].clone_from_slice(&new);
                    h.push(Command::EditSteps { track: row, start, old, new });
                    first_start.get_or_insert(start);
                    wrote += 1;
                }
                if block {
                    h.end_group("Block paste");
                }
                if let Some(start) = first_start {
                    s.selected = start;
                }
                if wrote == 0 {
                    None
                } else if block {
                    Some(format!("{} rows", wrote))
                } else {
                    None
                }
            }
        },
        Action::PasteAsTrack { before, times } => match s.register.clone() {
            None => Some(String::from("Nothing in register")),
            Some(reg) => {
                let tracks: Vec<Track> = match reg {
                    Register::Tracks(ts) => ts,
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
                        vec![t]
                    }
                };
                let times = (*times).max(1);
                let many = times > 1 || tracks.len() > 1;
                if many {
                    h.begin_group();
                }
                let mut full = false;
                let mut inserted = 0usize;
                'outer: for _ in 0..times {
                    for trk in &tracks {
                        if s.tracks.len() >= crate::NUM_TRACKS {
                            full = true;
                            break 'outer;
                        }
                        let at = if *before { tidx + inserted } else { tidx + 1 + inserted };
                        insert_track(s, at, trk.clone());
                        h.push(Command::PasteTrack { at, track: trk.clone() });
                        inserted += 1;
                    }
                }
                if many {
                    h.end_group("Paste tracks");
                }
                full.then(|| format!("Track limit ({})", crate::NUM_TRACKS))
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
                s.register = Some(Register::Tracks(vec![s.tracks[tidx].clone()]));
                Some(String::from("Yanked track"))
            }
            Operator::Delete => {
                if s.tracks.len() <= 1 {
                    return Some(String::from("Can't delete the last track"));
                }
                flush_held_notes(s);
                let track = s.tracks.remove(tidx);
                s.current_steps.remove(tidx);
                s.last_notes.remove(tidx);
                s.marks.remove(tidx);
                if tidx < s.trails.len() {
                    s.trails.remove(tidx);
                }
                s.register = Some(Register::Tracks(vec![track.clone()]));
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
            if s.tracks.len() >= crate::NUM_TRACKS {
                return Some(format!("Track limit ({})", crate::NUM_TRACKS));
            }
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
        Action::BindStep { target, source } => {
            let sel = s.selected;
            let old_step = s.tracks[tidx].steps[sel].clone();
            s.tracks[tidx].steps[sel].bind = source.as_ref().map(|src| StepBind {
                target: *target,
                // rebinding the same source keeps its dialed-in amount
                amount: match &old_step.bind {
                    Some(b) if b.source == *src => b.amount,
                    _ => 1.0,
                },
                source: src.clone(),
            });
            let new_step = s.tracks[tidx].steps[sel].clone();
            let msg = match source {
                Some(src) => format!("step {} ← {}", sel + 1, src),
                None => String::from("bind cleared"),
            };
            push_step_edit(h, tidx, sel, old_step, new_step);
            Some(msg)
        }
        Action::BindAmount { delta } => {
            let sel = s.selected;
            let old_step = s.tracks[tidx].steps[sel].clone();
            let Some(b) = s.tracks[tidx].steps[sel].bind.as_mut() else {
                return Some(String::from("No binding on this step (B binds)"));
            };
            b.amount = (b.amount + delta).clamp(-2.0, 2.0);
            let msg = format!("amount {:+.2}", b.amount);
            let new_step = s.tracks[tidx].steps[sel].clone();
            push_step_edit(h, tidx, sel, old_step, new_step);
            Some(msg)
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

/// One macro command in the `:macro` text syntax.
fn fmt_macro_cmd(cmd: &state::MacroCmd) -> String {
    match cmd {
        state::MacroCmd::SwitchPattern { track, slot } => {
            format!("pat {} {}", track + 1, slot_letter(*slot))
        }
        state::MacroCmd::SetMute { track, muted: true } => format!("mute {}", track + 1),
        state::MacroCmd::SetMute { track, muted: false } => format!("unmute {}", track + 1),
        state::MacroCmd::SetCycle { track, mode } => {
            format!("cycle {} {}", track + 1, mode.name())
        }
        state::MacroCmd::TransposeTrack { track, by } => {
            format!("xpose {} {:+}", track + 1, by)
        }
        state::MacroCmd::RotateTrack { track, by } => format!("rot {} {:+}", track + 1, by),
        state::MacroCmd::SetScale { track, scale } if scale.is_empty() => {
            format!("scale {} off", track + 1)
        }
        state::MacroCmd::SetScale { track, scale } => format!("scale {} {}", track + 1, scale),
        state::MacroCmd::Fill { track, kind, arg } => {
            format!("fill {} {} {}", track + 1, kind.name(), arg)
        }
        state::MacroCmd::SetBpm { bpm } => format!("bpm {}", bpm),
        state::MacroCmd::SetSteps { track, start, steps } => {
            format!("edit {} @{} ×{}", track + 1, start + 1, steps.len())
        }
        state::MacroCmd::SetActive { track, step, active } => {
            format!("step {} {} {}", track + 1, step + 1, if *active { "on" } else { "off" })
        }
        state::MacroCmd::SetEuclid { track, pulses, length, rotation } => {
            format!("euclid {} {} {} {}", track + 1, pulses, length, rotation)
        }
        state::MacroCmd::SetMode { track, mode } => format!(
            "mode {} {}",
            track + 1,
            match mode {
                TrackMode::Note => "note",
                TrackMode::Modulation => "mod",
            }
        ),
    }
}

/// Parse the `:macro x = …` text syntax: commands separated by `|`,
/// 1-based track numbers, plus an optional `quant <q>` segment.
/// `pat 2 b | mute 3 | xpose 1 +7 | quant beat`
fn parse_macro_dsl(input: &str) -> Result<(Vec<state::MacroCmd>, Option<state::Quant>), String> {
    let mut cmds = Vec::new();
    let mut quant = None;
    for seg in input.split('|') {
        let words: Vec<&str> = seg.split_whitespace().collect();
        let parse_track = |w: &str| -> Result<usize, String> {
            w.parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .ok_or_else(|| format!("bad track: {w}"))
        };
        match words.as_slice() {
            [] => {}
            ["pat", t, slot] => {
                let slot = slot
                    .chars()
                    .next()
                    .filter(|c| ('a'..='h').contains(c))
                    .map(|c| (c as u8 - b'a') as usize)
                    .ok_or_else(|| format!("bad slot: {slot} (a-h)"))?;
                cmds.push(state::MacroCmd::SwitchPattern { track: parse_track(t)?, slot });
            }
            ["mute", t] => cmds.push(state::MacroCmd::SetMute { track: parse_track(t)?, muted: true }),
            ["unmute", t] => {
                cmds.push(state::MacroCmd::SetMute { track: parse_track(t)?, muted: false })
            }
            ["cycle", t, m] => {
                let mode = state::CycleMode::parse(m).ok_or_else(|| format!("bad cycle: {m}"))?;
                cmds.push(state::MacroCmd::SetCycle { track: parse_track(t)?, mode });
            }
            ["xpose", t, n] => {
                let by: i32 = n.parse().map_err(|_| format!("bad amount: {n}"))?;
                cmds.push(state::MacroCmd::TransposeTrack { track: parse_track(t)?, by });
            }
            ["rot", t, n] => {
                let by: i32 = n.parse().map_err(|_| format!("bad amount: {n}"))?;
                cmds.push(state::MacroCmd::RotateTrack { track: parse_track(t)?, by });
            }
            ["scale", t, rest @ ..] if !rest.is_empty() => {
                let name = rest.join(" ");
                let scale = if name == "off" { String::new() } else { name };
                if !scale.is_empty() && crate::theory::scales::lookup(&scale).is_none() {
                    return Err(format!("unknown scale: {scale}"));
                }
                cmds.push(state::MacroCmd::SetScale { track: parse_track(t)?, scale });
            }
            ["fill", t, kind] | ["fill", t, kind, _] => {
                let k = state::FillKind::parse(kind).ok_or_else(|| format!("bad fill: {kind}"))?;
                let arg = words
                    .get(3)
                    .and_then(|w| w.parse().ok())
                    .unwrap_or(match k {
                        state::FillKind::Mutate => 0.3,
                        state::FillKind::Density => 0.5,
                        _ => 0.0,
                    });
                cmds.push(state::MacroCmd::Fill { track: parse_track(t)?, kind: k, arg });
            }
            ["bpm", n] => {
                let bpm: f64 = n.parse().map_err(|_| format!("bad bpm: {n}"))?;
                cmds.push(state::MacroCmd::SetBpm { bpm });
            }
            ["step", t, n, onoff @ ("on" | "off")] => {
                let step: usize = n
                    .parse::<usize>()
                    .ok()
                    .and_then(|v| v.checked_sub(1))
                    .ok_or_else(|| format!("bad step: {n}"))?;
                cmds.push(state::MacroCmd::SetActive {
                    track: parse_track(t)?,
                    step,
                    active: *onoff == "on",
                });
            }
            ["euclid", t, p, l, r] => {
                let parse_n = |w: &str| -> Result<usize, String> {
                    w.parse().map_err(|_| format!("bad number: {w}"))
                };
                cmds.push(state::MacroCmd::SetEuclid {
                    track: parse_track(t)?,
                    pulses: parse_n(p)?,
                    length: parse_n(l)?,
                    rotation: parse_n(r)?,
                });
            }
            ["mode", t, m @ ("note" | "mod")] => {
                cmds.push(state::MacroCmd::SetMode {
                    track: parse_track(t)?,
                    mode: if *m == "note" { TrackMode::Note } else { TrackMode::Modulation },
                });
            }
            ["edit", ..] => {
                return Err(String::from(
                    "step edits are recorded with q, not written by hand",
                ))
            }
            ["quant", q] => {
                quant =
                    Some(state::Quant::parse(q).ok_or_else(|| format!("bad quant: {q}"))?);
            }
            other => return Err(format!("bad command: {}", other.join(" "))),
        }
    }
    Ok((cmds, quant))
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

/// Queue note-offs for every held note under its CURRENT source channel,
/// then forget them. Must run before track indices shift (insert/remove)
/// or patterns are replaced wholesale (reload, patch load) — otherwise
/// the eventual note-off goes to the wrong channel and a voice drones.
fn flush_held_notes(state: &mut SequencerState) {
    for t in 0..state.last_notes.len() {
        if let Some(n) = state.last_notes[t].take() {
            state.pending_offs.push((n, t as u8));
        }
    }
}

/// Insert a track at `at`, keeping the per-track bookkeeping vectors in
/// sync. Refuses beyond the modbus channel claim (the rig registers
/// exactly [`crate::NUM_TRACKS`] outputs; a 9th track would stomp the
/// next module's channels). ROUTING IS POSITIONAL: t-numbers are the
/// rig's jacks, so shifting rows re-wires by design — held notes flush
/// first because note-offs match by source byte (= row).
fn insert_track(state: &mut SequencerState, at: usize, track: Track) {
    if state.tracks.len() >= crate::NUM_TRACKS {
        return;
    }
    flush_held_notes(state);
    let at = at.min(state.tracks.len());
    state.tracks.insert(at, track);
    state.current_steps.insert(at, 0);
    state.last_notes.insert(at, None);
    state.marks.insert(at, false);
    state.trails.insert(at, Vec::new());
    state.focus(at, Some(0));
}

/// Remove the track at `at`, keeping the per-track bookkeeping vectors in
/// sync. Never removes the last remaining track.
fn remove_track(state: &mut SequencerState, at: usize) {
    if state.tracks.len() > 1 && at < state.tracks.len() {
        flush_held_notes(state);
        state.tracks.remove(at);
        state.current_steps.remove(at);
        state.last_notes.remove(at);
        state.marks.remove(at);
        if at < state.trails.len() {
            state.trails.remove(at);
        }
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
            Command::SetLane { .. } => "Edit macro lane",
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
            Command::SetLane { old, .. } => {
                state.lane = old.clone();
                state.lane_selected = state.lane_selected.min(state.lane.len().saturating_sub(1));
                state.lane_pos = state.lane_pos.min(state.lane.len().saturating_sub(1));
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
            Command::SetLane { new, .. } => {
                state.lane = new.clone();
                state.lane_selected = state.lane_selected.min(state.lane.len().saturating_sub(1));
                state.lane_pos = state.lane_pos.min(state.lane.len().saturating_sub(1));
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
    /// `q{a-z}` recording: every pushed command also lands here in its
    /// macro form. The History is the single choke point all edits pass
    /// through, so recording can't miss anything undoable.
    rec: Option<(usize, Vec<state::MacroCmd>)>,
}

impl History {
    fn new() -> Self {
        Self { commands: vec![], index: 0, group: None, rec: None }
    }

    /// Start recording macro `idx` (a–z).
    fn rec_start(&mut self, idx: usize) {
        self.rec = Some((idx, Vec::new()));
    }

    /// Stop recording, returning (macro index, captured commands).
    fn rec_stop(&mut self) -> Option<(usize, Vec<state::MacroCmd>)> {
        self.rec.take()
    }

    /// (macro index, commands captured so far) — the modeline ticker.
    fn rec_status(&self) -> Option<(usize, usize)> {
        self.rec.as_ref().map(|(i, c)| (*i, c.len()))
    }

    /// Record into the live macro, merging consecutive rewrites of the
    /// same range (a held `k` sweep records once, not per repeat).
    fn rec_tap(&mut self, cmd: &Command) {
        let Some((_, cmds)) = self.rec.as_mut() else {
            return;
        };
        for mc in macro_cmds_of(cmd) {
            match (cmds.last_mut(), &mc) {
                (
                    Some(state::MacroCmd::SetSteps { track: t0, start: s0, steps: old }),
                    state::MacroCmd::SetSteps { track, start, steps },
                ) if t0 == track && s0 == start && old.len() == steps.len() => {
                    *old = steps.clone();
                }
                _ => cmds.push(mc),
            }
        }
    }

    /// Push without recording — thread-fired macros drain through here so
    /// firing @b while recording @a doesn't capture @b's effects.
    fn push_raw(&mut self, cmd: Command) {
        let rec = self.rec.take();
        self.push(cmd);
        self.rec = rec;
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
        // already one entry. Members record when the closed Group lands
        // (flattened), so nothing taps twice.
        if let Some(group) = self.group.as_mut() {
            group.push(cmd);
            return;
        }
        self.rec_tap(&cmd);
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
        | Action::Fill { .. }
        | Action::BindStep { .. }
        | Action::BindAmount { .. } => true,
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
    let track_yank = matches!(action, Action::OpTrack(Operator::Yank));
    let targets: Vec<usize> = match span {
        Some((a, b)) => {
            let hi = b.min(s.tracks.len().saturating_sub(1));
            (a.min(hi)..=hi).collect()
        }
        None if is_multi_action(action) || track_yank => marked_targets(s),
        None => vec![s.current_track],
    };
    // a multi-select track yank fills the register with EVERY selected
    // track, row order — p block-pastes them, gp re-inserts them all
    if track_yank && targets.len() > 1 {
        let tracks: Vec<Track> = targets
            .iter()
            .filter(|&&t| t < s.tracks.len())
            .map(|&t| s.tracks[t].clone())
            .collect();
        let n = tracks.len();
        s.register = Some(Register::Tracks(tracks));
        return Some(format!("{} tracks yanked", n));
    }
    if targets.len() <= 1 || !is_multi_action(action) {
        return apply_action(s, h, action);
    }
    let cur = s.current_track;
    let sel = s.selected;
    // a multi-select track DELETE also yanks everything it removes
    let track_delete = matches!(action, Action::OpTrack(Operator::Delete));
    let deleted: Vec<Track> = if track_delete {
        targets
            .iter()
            .filter(|&&t| t < s.tracks.len())
            .map(|&t| s.tracks[t].clone())
            .collect()
    } else {
        Vec::new()
    };
    // deleting tracks shifts indices — walk top-down so they stay valid
    let mut targets = targets;
    if track_delete {
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
    if track_delete && !deleted.is_empty() {
        s.register = Some(Register::Tracks(deleted));
    }
    s.current_track = cur.min(s.tracks.len().saturating_sub(1));
    s.selected = sel.min(s.tracks[s.current_track].length.saturating_sub(1));
    msg
}

/// Map an undoable command to its macro form — what `q` recording
/// captures. Everything you can undo records, as ABSOLUTE state so
/// replays are exact; track lifecycle (new/delete/paste), lane edits,
/// and slot save-as stay out of macros on purpose.
fn macro_cmds_of(cmd: &Command) -> Vec<state::MacroCmd> {
    match cmd {
        Command::ToggleStep { track, step, was_active } => vec![state::MacroCmd::SetActive {
            track: *track,
            step: *step,
            active: !*was_active,
        }],
        Command::EditStep { track, step, new_step, .. } => vec![state::MacroCmd::SetSteps {
            track: *track,
            start: *step,
            steps: vec![step_to_param(new_step)],
        }],
        Command::EditSteps { track, start, new, .. } => vec![state::MacroCmd::SetSteps {
            track: *track,
            start: *start,
            steps: new.iter().map(step_to_param).collect(),
        }],
        Command::SetTrackParams { track, new, .. } => vec![state::MacroCmd::SetEuclid {
            track: *track,
            pulses: new.pulses,
            length: new.length,
            rotation: new.rotation,
        }],
        Command::ToggleMute { track, was_muted } => {
            vec![state::MacroCmd::SetMute { track: *track, muted: !*was_muted }]
        }
        Command::ToggleMode { track, was_mode } => vec![state::MacroCmd::SetMode {
            track: *track,
            mode: match was_mode {
                TrackMode::Note => TrackMode::Modulation,
                TrackMode::Modulation => TrackMode::Note,
            },
        }],
        Command::SetBpm { new_bpm, .. } => vec![state::MacroCmd::SetBpm { bpm: *new_bpm }],
        Command::SetCycle { track, new, .. } => {
            vec![state::MacroCmd::SetCycle { track: *track, mode: *new }]
        }
        Command::SwitchSlot { track, to, .. } => {
            vec![state::MacroCmd::SwitchPattern { track: *track, slot: *to }]
        }
        Command::SetScale { track, new, .. } => vec![state::MacroCmd::SetScale {
            track: *track,
            scale: new.scale.as_ref().map(|sc| sc.name.clone()).unwrap_or_default(),
        }],
        Command::Group { cmds, .. } => cmds.iter().flat_map(macro_cmds_of).collect(),
        Command::NewTrack { .. }
        | Command::DeleteTrack { .. }
        | Command::PasteTrack { .. }
        | Command::SaveSlot { .. }
        | Command::SetLane { .. } => Vec::new(),
    }
}

/// Execute one macro command directly (macros address tracks absolutely).
/// Returns the undo command when something changed.
fn apply_macro_cmd(s: &mut SequencerState, cmd: &state::MacroCmd, seed: u64) -> Option<Command> {
    match cmd {
        state::MacroCmd::SwitchPattern { track, slot } => {
            let (t, slot) = (*track, (*slot).min(NUM_SLOTS - 1));
            if t >= s.tracks.len() || s.tracks[t].active_slot == slot {
                return None;
            }
            let from = s.tracks[t].active_slot;
            switch_slot(&mut s.tracks[t], slot);
            if s.current_track == t {
                s.selected = s.selected.min(s.tracks[t].length.saturating_sub(1));
            }
            Some(Command::SwitchSlot { track: t, from, to: slot })
        }
        state::MacroCmd::SetMute { track, muted } => {
            let t = *track;
            if t >= s.tracks.len() || s.tracks[t].muted == *muted {
                return None;
            }
            s.tracks[t].muted = *muted;
            Some(Command::ToggleMute { track: t, was_muted: !*muted })
        }
        state::MacroCmd::SetCycle { track, mode } => {
            let t = *track;
            if t >= s.tracks.len() || s.tracks[t].cycle == *mode {
                return None;
            }
            let old = s.tracks[t].cycle;
            s.tracks[t].cycle = *mode;
            Some(Command::SetCycle { track: t, old, new: *mode })
        }
        state::MacroCmd::TransposeTrack { track, by } => {
            let t = *track;
            if t >= s.tracks.len() || *by == 0 {
                return None;
            }
            let len = s.tracks[t].length;
            let old: Vec<Step> = s.tracks[t].steps[..len].to_vec();
            let new: Vec<Step> = old
                .iter()
                .map(|st| Step {
                    note: (i32::from(st.note) + by).clamp(0, 127) as u8,
                    ..st.clone()
                })
                .collect();
            if new == old {
                return None;
            }
            s.tracks[t].steps[..len].clone_from_slice(&new);
            Some(Command::EditSteps { track: t, start: 0, old, new })
        }
        state::MacroCmd::RotateTrack { track, by } => {
            let t = *track;
            if t >= s.tracks.len() {
                return None;
            }
            let len = s.tracks[t].length;
            let old: Vec<Step> = s.tracks[t].steps[..len].to_vec();
            let mut new = old.clone();
            new.rotate_right(by.rem_euclid(len as i32) as usize);
            if new == old {
                return None;
            }
            s.tracks[t].steps[..len].clone_from_slice(&new);
            Some(Command::EditSteps { track: t, start: 0, old, new })
        }
        state::MacroCmd::SetScale { track, scale } => {
            let t = *track;
            if t >= s.tracks.len() {
                return None;
            }
            // macros resolve scales by library name only ("" = chromatic)
            let new_scale = if scale.is_empty() {
                None
            } else {
                match crate::theory::scales::lookup(scale) {
                    Some(sc) => Some(sc),
                    None => return None,
                }
            };
            let old = s.tracks[t].clone();
            let root = old.root;
            let new = retuned_track(&old, new_scale, root);
            if new == old {
                return None;
            }
            s.tracks[t] = new.clone();
            Some(Command::SetScale { track: t, old: Box::new(old), new: Box::new(new) })
        }
        state::MacroCmd::Fill { track, kind, arg } => {
            let t = *track;
            if t >= s.tracks.len() {
                return None;
            }
            // route through apply_action's Fill arm on a scratch history
            // (scratch = never recorded into a user's q-recording)
            let cur = s.current_track;
            s.current_track = t;
            let mut scratch = History::new();
            apply_action(s, &mut scratch, &Action::Fill { kind: *kind, arg: *arg, seed });
            s.current_track = cur.min(s.tracks.len().saturating_sub(1));
            scratch.commands.pop().map(|(cmd, _)| cmd)
        }
        state::MacroCmd::SetBpm { bpm } => {
            let old_bpm = s.bpm;
            let new_bpm = bpm.clamp(20.0, 300.0);
            if old_bpm == new_bpm {
                return None;
            }
            s.bpm = new_bpm;
            Some(Command::SetBpm { old_bpm, new_bpm })
        }
        state::MacroCmd::SetSteps { track, start, steps } => {
            let t = *track;
            if t >= s.tracks.len() || steps.is_empty() {
                return None;
            }
            let start = (*start).min(NUM_STEPS - 1);
            let end = (start + steps.len() - 1).min(NUM_STEPS - 1);
            let old: Vec<Step> = s.tracks[t].steps[start..=end].to_vec();
            let new: Vec<Step> =
                steps[..=(end - start)].iter().map(step_from_param).collect();
            if old == new {
                return None;
            }
            s.tracks[t].steps[start..=end].clone_from_slice(&new);
            Some(Command::EditSteps { track: t, start, old, new })
        }
        state::MacroCmd::SetActive { track, step, active } => {
            let (t, i) = (*track, (*step).min(NUM_STEPS - 1));
            if t >= s.tracks.len() || s.tracks[t].steps[i].active == *active {
                return None;
            }
            s.tracks[t].steps[i].active = *active;
            Some(Command::ToggleStep { track: t, step: i, was_active: !*active })
        }
        state::MacroCmd::SetEuclid { track, pulses, length, rotation } => {
            let t = *track;
            if t >= s.tracks.len() {
                return None;
            }
            let old = EuclidState::capture(&s.tracks[t]);
            s.tracks[t].pulses = (*pulses).min(NUM_STEPS);
            s.tracks[t].length = (*length).clamp(1, NUM_STEPS);
            s.tracks[t].rotation = (*rotation).min(255);
            let (p, l, r) = (s.tracks[t].pulses, s.tracks[t].length, s.tracks[t].rotation);
            euclidean_apply(&mut s.tracks[t].steps, p, l, r);
            if s.current_track == t {
                s.selected = s.selected.min(s.tracks[t].length - 1);
            }
            let new = EuclidState::capture(&s.tracks[t]);
            (old != new).then_some(Command::SetTrackParams { track: t, old, new })
        }
        state::MacroCmd::SetMode { track, mode } => {
            let t = *track;
            if t >= s.tracks.len() || s.tracks[t].mode == *mode {
                return None;
            }
            let was_mode = s.tracks[t].mode;
            s.tracks[t].mode = *mode;
            Some(Command::ToggleMode { track: t, was_mode })
        }
    }
}

/// Fire macro `idx` right now: run its commands, emit ONE undo group into
/// the outbox (drained by the UI loop), remember it for `@@`.
fn fire_macro_now(s: &mut SequencerState, idx: usize, seed: u64) {
    let Some(Some(m)) = s.macros.get(idx).cloned() else {
        return;
    };
    let mut undo: Vec<Command> = Vec::new();
    for cmd in &m.cmds {
        if let Some(c) = apply_macro_cmd(s, cmd, seed) {
            undo.push(c);
        }
    }
    s.last_macro = Some(idx);
    let letter = (b'a' + (idx as u8).min(25)) as char;
    s.macro_flash = Some(if undo.is_empty() {
        // the recorded state already holds (or the commands cancel out)
        format!("@{}: no change", letter)
    } else {
        format!("@{}", letter)
    });
    match undo.len() {
        0 => {}
        1 => {
            if let Some(c) = undo.pop() {
                s.macro_outbox.push(c);
            }
        }
        _ => s.macro_outbox.push(Command::Group { cmds: undo, desc: "Macro" }),
    }
}

/// The global step a quantized firing lands on (strictly after `gstep`).
fn quant_boundary(
    quant: state::Quant,
    gstep: u64,
    first_track_len: Option<usize>,
) -> u64 {
    let unit = match quant {
        state::Quant::Now => return gstep,
        state::Quant::Beat => STEPS_PER_BEAT,
        state::Quant::Bar => STEPS_PER_BAR,
        state::Quant::PatternEnd => first_track_len.map_or(STEPS_PER_BAR, |l| l as u64).max(1),
    };
    (gstep / unit + 1) * unit
}

/// The track a macro's first command touches (pattern-end quantize syncs
/// to that track's loop).
fn macro_first_track(m: &Macro) -> Option<usize> {
    m.cmds.first().map(|cmd| match cmd {
        state::MacroCmd::SwitchPattern { track, .. }
        | state::MacroCmd::SetMute { track, .. }
        | state::MacroCmd::SetCycle { track, .. }
        | state::MacroCmd::TransposeTrack { track, .. }
        | state::MacroCmd::RotateTrack { track, .. }
        | state::MacroCmd::SetScale { track, .. }
        | state::MacroCmd::Fill { track, .. }
        | state::MacroCmd::SetSteps { track, .. }
        | state::MacroCmd::SetActive { track, .. }
        | state::MacroCmd::SetEuclid { track, .. }
        | state::MacroCmd::SetMode { track, .. } => *track,
        state::MacroCmd::SetBpm { .. } => 0,
    })
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

/// The sequencer's command-bar completer: command names, scale names,
/// fill kinds, set keys/values, patch names, defined macros. `macro_ids`
/// is precomputed (the completer must not take the state lock).
fn seq_completer(macro_ids: Vec<String>) -> impl Fn(&str, &str) -> Vec<String> {
    move |head: &str, word: &str| {
        let items: Vec<String> = match head {
            "" => ["w", "e", "q", "q!", "wq", "x", "set", "scale", "fill", "macro"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            "e" | "edit" | "w" | "write" | "wq" | "x" => {
                crate::excmd::patch_names(&state::patches_dir())
            }
            "scale" => {
                let mut v = vec![String::from("off"), String::from("root")];
                v.extend(crate::theory::scales::names().iter().map(|s| s.to_string()));
                v
            }
            "fill" => {
                ["mutate", "density", "markov", "cantor", "thuemorse", "fibonacci", "sierpinski"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            }
            "set" => ["bpm", "pulses", "length", "rotation", "cycle", "root"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            "set cycle" => state::CycleMode::ALL.iter().map(|m| m.name().to_string()).collect(),
            "macro" => macro_ids.clone(),
            _ => Vec::new(),
        };
        crate::excmd::filter_prefix(items, word)
    }
}

/// Apply an auditionable command line into a fresh scratch history (the
/// command-bar arrow-key preview). Returns None when the line isn't
/// previewable — only :scale, :fill, and :set cycle audition.
fn start_preview(
    state: &Mutex<SequencerState>,
    line: &str,
    fill_seed: u64,
) -> Option<History> {
    use crate::excmd::ExCommand;
    // a scratch history never records into macros — auditions are silent
    let mut scratch = History::new();
    let mut s = state.lock().unwrap();
    let applied = match crate::excmd::parse(line) {
        ExCommand::Set(k, v) if k == "cycle" => state::CycleMode::parse(&v).map(|mode| {
            apply_selected(&mut s, &mut scratch, &Action::SetCycle(mode), None);
        }),
        ExCommand::Unknown(c) => {
            let (head, rest) = match c.split_once(char::is_whitespace) {
                Some((h, r)) => (h, r.trim()),
                None => (c.as_str(), ""),
            };
            match head {
                "scale" if rest == "off" => {
                    set_scale(&mut s, &mut scratch, Some(None), None);
                    Some(())
                }
                "scale" if !rest.is_empty() && !rest.starts_with("root") => {
                    let sc = if rest.ends_with(".scl") {
                        crate::theory::scl::load_scl(std::path::Path::new(rest)).ok()
                    } else {
                        crate::theory::scales::lookup(rest)
                    };
                    sc.map(|sc| {
                        set_scale(&mut s, &mut scratch, Some(Some(sc)), None);
                    })
                }
                "fill" if !rest.is_empty() => {
                    let (kind_str, arg_str) = match rest.split_once(char::is_whitespace) {
                        Some((a, b)) => (a, b.trim()),
                        None => (rest, ""),
                    };
                    state::FillKind::parse(kind_str).map(|kind| {
                        let default = match kind {
                            state::FillKind::Mutate => 0.3,
                            state::FillKind::Density => 0.5,
                            _ => 0.0,
                        };
                        let arg = arg_str.parse().unwrap_or(default);
                        // a stable per-session seed so arrowing back and
                        // forth re-plays the same take
                        let action =
                            Action::Fill { kind, arg, seed: fill_seed ^ 0xA0D1_710E };
                        apply_selected(&mut s, &mut scratch, &action, None);
                    })
                }
                _ => None,
            }
        }
        _ => None,
    };
    drop(s);
    applied.map(|()| scratch)
}

/// Revert (`keep = false`) or commit (`keep = true`) a live audition.
fn end_preview(
    state: &Mutex<SequencerState>,
    history: &mut History,
    preview: &mut Option<(History, String)>,
    keep: bool,
) {
    let Some((mut scratch, _)) = preview.take() else {
        return;
    };
    if keep {
        let cmds: Vec<Command> = scratch.commands.drain(..).map(|(c, _)| c).collect();
        match cmds.len() {
            0 => {}
            1 => {
                if let Some(c) = cmds.into_iter().next() {
                    history.push(c);
                }
            }
            _ => history.push(Command::Group { cmds, desc: "Audition" }),
        }
    } else {
        let mut s = state.lock().unwrap();
        while scratch.undo(&mut s).is_some() {}
    }
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

    // Global step counter the playheads derive from. Phase-accumulated so
    // a bpm change rescales the FUTURE only — dividing the absolute clock
    // would teleport gstep and re-fire the whole macro lane.
    let mut phase: Option<f64> = None;
    let mut last_clock: u64 = 0;
    let mut last_gstep: Option<u64> = None;
    // Bar counter for the macro lane (None until the first tick so the
    // very first bar's slot fires too).
    let mut last_bar: Option<u64> = None;
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
            while s.trails.len() < s.tracks.len() {
                s.trails.push(Vec::new());
            }
            while drunk.len() < s.tracks.len() {
                drunk.push(0);
            }

            // Note-offs owed from track inserts/removes/reloads: the held
            // note's source channel was captured before indices shifted.
            if !s.pending_offs.is_empty() {
                let offs: Vec<(u8, u8)> = s.pending_offs.drain(..).collect();
                for (n, src) in offs {
                    let _ = events.write_event(&AudioEvent::note_off_source(n, src, 0));
                    if let (Some(ref mut bus), Some(base)) = (modbus.as_mut(), s.mod_base) {
                        if (src as usize) < crate::NUM_TRACKS {
                            bus.set(base + src as usize, 0.0);
                        }
                    }
                }
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

            let gstep = if samples_per_step > 0 {
                let p = advance_phase(phase, clock, last_clock, samples_per_step);
                phase = Some(p);
                last_clock = clock;
                p as u64
            } else {
                last_gstep.unwrap_or(0)
            };

            // Live macro firings: quantized while playing, immediate when
            // the transport is stopped.
            if !s.pending_macros.is_empty() {
                let mut pending = std::mem::take(&mut s.pending_macros);
                pending.retain_mut(|p| {
                    if !playing {
                        fire_macro_now(&mut s, p.idx, gstep ^ 0x51AB);
                        return false;
                    }
                    let at = match p.at {
                        Some(at) => at,
                        None => {
                            let m = s.macros.get(p.idx).and_then(|m| m.clone());
                            let (quant, ftl) = m.map_or((state::Quant::Now, None), |m| {
                                let ftl = macro_first_track(&m)
                                    .and_then(|t| s.tracks.get(t))
                                    .map(|t| t.length);
                                (m.quant, ftl)
                            });
                            let at = quant_boundary(quant, gstep, ftl);
                            p.at = Some(at);
                            at
                        }
                    };
                    if gstep >= at {
                        fire_macro_now(&mut s, p.idx, gstep);
                        false
                    } else {
                        true
                    }
                });
                s.pending_macros.extend(pending);
            }

            // The macro lane: one slot per bar, fired exactly on the bar
            // line (the lane is its own quantizer).
            if playing && !s.lane.is_empty() {
                let bar = gstep / STEPS_PER_BAR;
                if last_bar != Some(bar) {
                    last_bar = Some(bar);
                    let pos = (bar as usize) % s.lane.len();
                    s.lane_pos = pos;
                    if let Some(idx) = s.lane[pos] {
                        fire_macro_now(&mut s, idx, gstep);
                    }
                }
            }

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
                    if pos != prev_pos {
                        s.trails[t].insert(0, prev_pos);
                        s.trails[t].truncate(3);
                    }
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

/// Advance the global step phase by the clock delta. Phase-accumulated so
/// a bpm change rescales only the future; a BACKWARD clock (mixer respawn,
/// session reload re-creating the transport) rebases instead of freezing —
/// `saturating_sub` here once silenced the whole sequencer until its
/// process restarted.
fn advance_phase(phase: Option<f64>, clock: u64, last_clock: u64, samples_per_step: u64) -> f64 {
    match phase {
        Some(p) if clock >= last_clock => {
            p + (clock - last_clock) as f64 / samples_per_step as f64
        }
        _ => clock as f64 / samples_per_step as f64,
    }
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
    picker: Option<(Vec<String>, usize)>,
    ex_menu: Option<(Vec<String>, Option<usize>)>,
    entries: &[crate::shm::ManifestEntry],
    live_binds: &std::collections::HashMap<usize, f32>,
    rec: Option<(usize, usize)>,
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

        // ── macro lane: one slot per bar, the sequencer of sequencers ───
        {
            let lane_len = state.lane.len();
            let info = format!("  {:>3} lane", lane_len);
            let visible = row_visible(w, info.chars().count(), lane_len);
            let anchor = if state.on_lane { state.lane_selected } else { state.lane_pos };
            let start = row_window_start(lane_len, anchor, visible);
            let on_lane = state.on_lane;
            let mut spans: Vec<Span> = Vec::with_capacity(visible + 8);
            spans.push(Span::styled(
                format!("{}", if on_lane { theme::PLAYHEAD } else { ' ' }),
                if on_lane { theme::chrome_hi() } else { theme::chrome() },
            ));
            spans.push(Span::styled(
                String::from(" @ "),
                if on_lane { theme::chrome_hi() } else { theme::chrome() },
            ));
            spans.push(Span::styled(
                if start > 0 { "‹" } else { " " }.to_string(),
                theme::dim(),
            ));
            for i in start..start + visible {
                if i > start && (i - start).is_multiple_of(4) {
                    spans.push(Span::raw(" "));
                }
                let glyph = match state.lane[i] {
                    Some(idx) => (b'a' + idx as u8) as char,
                    None => '·',
                };
                let style = if on_lane && i == state.lane_selected {
                    theme::selected()
                } else if state.playing && i == state.lane_pos {
                    theme::flash(theme::clock())
                } else if i == state.lane_pos {
                    theme::signal(theme::clock())
                } else if let Some(idx) = state.lane[i] {
                    theme::signal(theme::channel_color(idx))
                } else {
                    theme::dim()
                };
                spans.push(Span::styled(glyph.to_string(), style));
            }
            spans.push(Span::styled(
                if start + visible < lane_len { "›" } else { " " }.to_string(),
                theme::dim(),
            ));
            spans.push(Span::styled(info, if on_lane { theme::value() } else { theme::dim() }));
            lines.push(Line::from(spans));
        }

        // V-line selection: which track rows are inside the span
        let vline_span = (mode == "visual_line")
            .then(|| {
                let a = state.visual_track_anchor.unwrap_or(state.current_track);
                (a.min(state.current_track), a.max(state.current_track))
            });
        let in_vline = |ti: usize| vline_span.is_some_and(|(a, b)| ti >= a && ti <= b);

        // ── track rows ──────────────────────────────────────────────────
        for (ti, trk) in state.tracks.iter().enumerate() {
            let is_cur = ti == state.current_track && !state.on_lane;
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
            // marked tracks (X) wear a star; rows something in the rig is
            // LISTENING to (a voice playing the notes, an envelope
            // triggered by the output) wear ▸ — paste an empty row onto a
            // listened position and you can see why it went quiet
            let marked = state.marks.get(ti).copied().unwrap_or(false);
            let listened = entries.iter().any(|e| {
                e.consumes_notes & (1u8 << ti.min(7)) != 0 && ti < 8
                    || state.mod_base.is_some_and(|base| {
                        let ch = base + ti;
                        ch < 64 && e.consumes_channels & (1u64 << ch) != 0
                    })
            });
            let tag = if marked {
                '*'
            } else if listened {
                '▸'
            } else {
                ' '
            };
            spans.push(Span::styled(
                format!("t{}{}", ti + 1, tag),
                if in_vline(ti) {
                    theme::selected()
                } else if trk.muted {
                    theme::dim()
                } else {
                    theme::signal(cable)
                },
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
                // the active value layer picks the glyph and base color:
                // one grammar, many lanes (docs/plans/sequencer-v2.md §1)
                let (glyph, value_color) = layer_cell(state.layer, trk, step);
                // playhead + wake (CLOCK hue), trigger flash on the live cell
                let style = if is_cur && i == state.selected {
                    // your edit cursor, visible in the overview too
                    theme::selected()
                } else if in_vline(ti) {
                    theme::flash(theme::cv())
                } else if state.playing && i == tstep && !trk.muted {
                    if on {
                        theme::flash(hue)
                    } else {
                        theme::signal(theme::clock())
                    }
                } else if !state.playing && i == tstep {
                    // paused: a still CLOCK-hue marker says "you are here"
                    theme::signal(theme::clock())
                } else if state.playing
                    && !trk.muted
                    && state.trails.get(ti).is_some_and(|tr| tr.contains(&i))
                {
                    theme::signal(theme::clock())
                } else if trk.muted {
                    theme::dim()
                } else if on {
                    theme::signal(value_color)
                } else {
                    theme::dim()
                };
                // a patched-in mod binding wears an underline in the
                // source's cable color (one color at both ends, the law)
                let style = match &step.bind {
                    Some(b) => {
                        let cable = crate::routing::SourceAddr::parse(&b.source)
                            .map(|a| crate::routing::cable_color(entries, &a))
                            .unwrap_or_else(theme::clock);
                        style
                            .add_modifier(ratatui::style::Modifier::UNDERLINED)
                            .underline_color(cable)
                    }
                    None => style,
                };
                let recency = state
                    .trails
                    .get(ti)
                    .and_then(|tr| tr.iter().position(|&p| p == i));
                let shown = if state.playing && !trk.muted {
                    match recency {
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
                        // scaled tracks read in degrees from the root
                        match i32::from(step.note) - 60 {
                            _ if trk.scale.is_none() => midi_note_name(step.note),
                            0 => String::from("r"),
                            d => format!("r{:+}", d),
                        }
                    } else {
                        String::from("·")
                    }
                }
                TrackMode::Modulation => format!("{:+.2}", step.mod_value),
            };
            let mut val_style = if sel {
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
            if let Some(b) = &step.bind {
                let cable = crate::routing::SourceAddr::parse(&b.source)
                    .map(|a| crate::routing::cable_color(entries, &a))
                    .unwrap_or_else(theme::clock);
                val_style = val_style
                    .add_modifier(ratatui::style::Modifier::UNDERLINED)
                    .underline_color(cable);
            }
            vals.push(Span::styled(format!("{:^cell$}", val), val_style));

            // third row: the active layer's value, numeric. A live cable
            // on this layer's param shows base→effective, breathing with
            // the source, in the cable's color.
            let live = step.bind.as_ref().zip(live_binds.get(&i)).filter(|(b, _)| {
                b.target == state.layer
                    || (b.target == state::BindTarget::Note
                        && state.layer == state::BindTarget::Note)
            });
            if let Some((b, &v)) = live {
                let off = bind_offsets(Some(b), Some(v));
                let cable = crate::routing::SourceAddr::parse(&b.source)
                    .map(|a| crate::routing::cable_color(entries, &a))
                    .unwrap_or_else(theme::clock);
                let text = match b.target {
                    state::BindTarget::Note => {
                        format!("{:+}", off.degrees)
                    }
                    state::BindTarget::Velocity => format!(
                        "{}→{}",
                        step.velocity,
                        (i32::from(step.velocity) + off.velocity).clamp(1, 127)
                    ),
                    state::BindTarget::Prob => format!(
                        "{}→{}%",
                        step.prob,
                        (i32::from(step.prob) + off.prob).clamp(0, 100)
                    ),
                    state::BindTarget::Mod => format!(
                        "{:+.2}",
                        (step.mod_value + off.mod_value).clamp(-1.0, 1.0)
                    ),
                };
                vels.push(Span::styled(
                    format!("{:^cell$}", text),
                    theme::signal(cable),
                ));
                continue;
            }
            let (text, color) = match state.layer {
                state::BindTarget::Note | state::BindTarget::Velocity => (
                    if step.active && trk.mode == TrackMode::Note {
                        if state.layer == state::BindTarget::Velocity {
                            format!("{}", step.velocity)
                        } else {
                            theme::meter_char(f32::from(step.velocity) / 127.0).to_string()
                        }
                    } else {
                        String::from("·")
                    },
                    theme::pitch_color(step.note),
                ),
                state::BindTarget::Prob => (
                    if step.active {
                        format!("{}%", step.prob)
                    } else {
                        String::from("·")
                    },
                    if step.prob < 100 { theme::clock() } else { theme::note() },
                ),
                state::BindTarget::Mod => (
                    format!("{:+.2}", step.mod_value),
                    theme::cv_ramp(step.mod_value),
                ),
            };
            vels.push(Span::styled(
                format!("{:^cell$}", text),
                if step.active || state.layer == state::BindTarget::Mod {
                    theme::signal(color)
                } else {
                    theme::dim()
                },
            ));
        }
        // pattern-slot chip at the right edge of the numbers row: the
        // active slot highlighted, slots holding content lit, empties dim
        {
            let used: usize = (first..(first + visible).min(trk.length)).count() * cell;
            if w > used + 2 + 2 * NUM_SLOTS {
                nums.push(Span::raw(" ".repeat(w - used - 2 * NUM_SLOTS)));
                for i in 0..NUM_SLOTS {
                    let letter = slot_letter(i);
                    let filled = trk.slots[i]
                        .as_ref()
                        .is_some_and(|pd| pd.steps.iter().any(|st| *st != Step::default()));
                    let style = if i == trk.active_slot {
                        theme::selected()
                    } else if filled {
                        theme::value()
                    } else {
                        theme::dim()
                    };
                    nums.push(Span::styled(format!("{} ", letter), style));
                }
            }
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
        // the cursor's cable, spelled out
        if !state.on_lane {
            if let Some(b) = state.track().steps.get(state.selected).and_then(|st| st.bind.as_ref()) {
                let target = match b.target {
                    state::BindTarget::Note => "note",
                    state::BindTarget::Velocity => "vel",
                    state::BindTarget::Prob => "prob",
                    state::BindTarget::Mod => "mod",
                };
                match live_binds.get(&state.selected) {
                    Some(v) => msg_parts.push(format!(
                        "{}←{} ×{:.2} = {:+.2}",
                        target, b.source, b.amount, v
                    )),
                    None => msg_parts.push(format!(
                        "{}←{} ×{:.2} (unresolved)",
                        target, b.source, b.amount
                    )),
                }
            }
        }
        // performance state first: recording + macros waiting on the clock
        if let Some((idx, n)) = rec {
            msg_parts.push(format!("rec @{} [{}]", (b'a' + idx as u8) as char, n));
        }
        for p in &state.pending_macros {
            msg_parts.push(format!("…@{}", (b'a' + p.idx as u8) as char));
        }
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

        // ── command-bar completion menu (just above the modeline) ───────
        if let Some((items, sel)) = &ex_menu {
            if !items.is_empty() {
                let max_rows = 8usize.min(items.len());
                let first = sel.map_or(0, |s| s.saturating_sub(max_rows - 1));
                let shown = &items[first..(first + max_rows).min(items.len())];
                let mw = shown
                    .iter()
                    .map(|r| r.chars().count())
                    .max()
                    .unwrap_or(8)
                    .max(8) as u16
                    + 3;
                let h = shown.len() as u16 + 1;
                let r = ratatui::layout::Rect::new(
                    0,
                    area.height.saturating_sub(1 + h),
                    mw.min(area.width),
                    h.min(area.height),
                );
                f.render_widget(ratatui::widgets::Clear, r);
                let mut rows: Vec<Line> = shown
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let style = if Some(first + i) == *sel {
                            theme::selected()
                        } else {
                            theme::value()
                        };
                        Line::from(Span::styled(format!(" {} ", item), style))
                    })
                    .collect();
                rows.push(Line::from(Span::styled(
                    format!(" {} ·Tab·↓ ", items.len()),
                    theme::dim(),
                )));
                f.render_widget(Paragraph::new(rows), r);
            }
        }

        // ── bind-source picker overlay (B on a step) ────────────────────
        if let Some((rows, sel)) = picker {
            let h = (rows.len() as u16 + 2).min(area.height);
            let pw = rows.iter().map(|r| r.len()).max().unwrap_or(10).max(20) as u16 + 4;
            let r = ratatui::layout::Rect::new(
                (area.width.saturating_sub(pw)) / 2,
                (area.height.saturating_sub(h)) / 2,
                pw.min(area.width),
                h,
            );
            f.render_widget(ratatui::widgets::Clear, r);
            let items: Vec<ratatui::widgets::ListItem> = rows
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    let style = if i == sel { theme::selected() } else { theme::value() };
                    ratatui::widgets::ListItem::new(row.clone()).style(style)
                })
                .collect();
            let list = ratatui::widgets::List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" bind step ", theme::chrome_hi())),
            );
            f.render_widget(list, r);
        }
    })?;

    Ok(())
}

fn sequencer_help() -> Vec<Line<'static>> {
    vec![
        Line::from("━━━ SEQ ━━━  (docs/sequencer.md has the full tour)"),
        Line::from(""),
        Line::from("Motions (word = run of active steps):"),
        Line::from("  h/l w/b/e 0/$ f#/t#   steps (counts: 5l, yt8)"),
        Line::from("  j/k [/] gg/G gt#      tracks · k from t1 = macro lane"),
        Line::from(""),
        Line::from("Operators: y/d/c{motion} · yy/dd/cc track · Y/D/C to $"),
        Line::from("  p/P paste INTO track · gp/gP paste as new track"),
        Line::from(""),
        Line::from("Normal: Enter/i insert · v visual · V track-line"),
        Line::from("  x cut · ~ toggle · . repeat · o/O new track"),
        Line::from("  X mark track (multi-edit) · gX clear marks"),
        Line::from("  m mute · M note/mod mode · >>/<< rotate"),
        Line::from("  'n 'v 'p 'm value layer · \"a-\"h pattern slot (\"A save)"),
        Line::from("  gc/gC cycle mode · F refill · u/^r undo/redo"),
        Line::from("  cycles: →fwd ←rev ↔pong ?rand ~drunk ½skip ◎spiral #prime"),
        Line::from(""),
        Line::from("Macros: qa…q record · @a fire (quantized) · @@ again"),
        Line::from("  lane: @a assign · x clear · y/p · #L length · D wipe"),
        Line::from(""),
        Line::from("Insert: Enter/space toggle · k/K j/J value ±fine/coarse"),
        Line::from("  N set value · prob layer: 1-9,0 = 10-100%"),
        Line::from("  #P/#L/#R euclid · P/L/R re-apply/rotate"),
        Line::from(""),
        Line::from("Ex: :w/:e/:q · :set bpm/pulses/length/rotation/cycle/root"),
        Line::from("  :scale <name>|off|<file.scl> · :fill <kind> · :macro a = …"),
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

/// Grid cell (glyph + base color) for a step under the active value layer.
/// The note layer is the classic view; the others show their value as a
/// meter so a whole lane reads at a glance.
fn layer_cell(
    layer: state::BindTarget,
    trk: &Track,
    step: &Step,
) -> (char, ratatui::style::Color) {
    use crate::theme;
    let off_glyph = match trk.mode {
        TrackMode::Note => theme::STEP_OFF,
        TrackMode::Modulation => theme::MOD_OFF,
    };
    match layer {
        state::BindTarget::Note => {
            let glyph = match (trk.mode, step.active) {
                (TrackMode::Note, true) => theme::STEP_ON,
                (TrackMode::Note, false) => theme::STEP_OFF,
                (TrackMode::Modulation, true) => theme::MOD_ON,
                (TrackMode::Modulation, false) => theme::MOD_OFF,
            };
            let color = match trk.mode {
                // pitch-class wheel: see the melody from across the room
                TrackMode::Note => theme::pitch_color(step.note),
                TrackMode::Modulation => theme::cv_ramp(step.mod_value),
            };
            (glyph, color)
        }
        state::BindTarget::Velocity => {
            if step.active {
                (theme::meter_char(f32::from(step.velocity) / 127.0), theme::note())
            } else {
                (off_glyph, theme::note())
            }
        }
        state::BindTarget::Prob => {
            if step.active {
                let c = theme::meter_char(f32::from(step.prob) / 100.0);
                // anything under 100% wears the clock hue: dice are in play
                let color = if step.prob < 100 { theme::clock() } else { theme::note() };
                (c, color)
            } else {
                (off_glyph, theme::note())
            }
        }
        state::BindTarget::Mod => (
            if step.active || trk.mode == TrackMode::Modulation {
                theme::meter_char((step.mod_value + 1.0) / 2.0)
            } else {
                off_glyph
            },
            theme::cv_ramp(step.mod_value),
        ),
    }
}

/// One-glyph cycle-mode tag for the info column ("" = forward, the default).
fn cycle_glyph(cycle: state::CycleMode) -> &'static str {
    match cycle {
        state::CycleMode::Forward => "",
        state::CycleMode::Reverse => " ←",
        state::CycleMode::PingPong => " ↔",
        state::CycleMode::Random => " ?",
        state::CycleMode::Drunk => " ~",
        state::CycleMode::EveryOther => " ½",
        state::CycleMode::Spiral => " ◎",
        state::CycleMode::PrimeJump => " #",
    }
}

/// The info column text for a track row (drawn and measured identically).
fn row_info(trk: &Track) -> String {
    let slot = if trk.active_slot > 0 {
        format!(" \"{}", slot_letter(trk.active_slot))
    } else {
        String::new()
    };
    let scale = match &trk.scale {
        Some(sc) => {
            let tag: String = sc.name.chars().take(4).collect();
            format!(" ♪{}", tag)
        }
        None => String::new(),
    };
    format!(
        "  {:>3} P{} R{}{}{}{}{}{}",
        trk.length,
        trk.pulses,
        trk.rotation,
        slot,
        cycle_glyph(trk.cycle),
        scale,
        if trk.mode == TrackMode::Modulation { " ⌁" } else { "" },
        if trk.muted { " M" } else { "" },
    )
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
            steps: trimmed_steps(&trk.steps),
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
        macros: s
            .macros
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                m.as_ref().map(|m| state::MacroParam {
                    id: ((b'a' + i as u8) as char).to_string(),
                    quant: m.quant,
                    cmds: m.cmds.clone(),
                })
            })
            .collect(),
        lane: s
            .lane
            .iter()
            .map(|slot| {
                slot.map(|i| ((b'a' + i as u8) as char).to_string()).unwrap_or_default()
            })
            .collect(),
        lane_len: Some(s.lane.len()),
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
/// imports), a bare name re-resolves against the library. Cents from a
/// hand-edited/corrupt file are validated against the Scale invariants —
/// garbage falls back to the name, then to chromatic, never to NaN Hz.
fn scale_from_params(tp: &state::TrackParam) -> Option<crate::theory::scales::Scale> {
    if !tp.scale_cents.is_empty() {
        let period = tp.scale_period.unwrap_or(1200.0);
        let degrees = &tp.scale_cents;
        let valid = period.is_finite()
            && period > 0.0
            && degrees.iter().all(|d| d.is_finite() && *d >= 0.0 && *d < period)
            && degrees[0].abs() < 1e-9
            && degrees.windows(2).all(|w| w[1] > w[0]);
        if valid {
            return Some(crate::theory::scales::Scale {
                name: tp.scale.clone().unwrap_or_else(|| String::from("imported")),
                degrees: degrees.clone(),
                period,
            });
        }
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
    // held gates must close under their old channels before the world moves
    flush_held_notes(s);
    s.tracks.clear();
    s.current_steps.clear();
    s.last_notes.clear();
    s.marks.clear();
    s.trails.clear();
    for tp in params.tracks.iter().take(crate::NUM_TRACKS) {
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
        s.trails.push(Vec::new());
    }
    s.current_track = s.current_track.min(s.tracks.len().saturating_sub(1));
    let len = s.tracks[s.current_track].length;
    s.selected = s.selected.min(len.saturating_sub(1));

    // Macros + lane travel with the tracks (legacy saves simply have none).
    let mut macros: Vec<Option<Macro>> = vec![None; 26];
    for mp in &params.macros {
        let idx = mp
            .id
            .chars()
            .next()
            .filter(char::is_ascii_lowercase)
            .map(|ch| (ch as u8 - b'a') as usize);
        if let Some(i) = idx {
            macros[i] = Some(Macro { cmds: mp.cmds.clone(), quant: mp.quant });
        }
    }
    s.macros = macros;
    let lane_len = params
        .lane_len
        .unwrap_or_else(|| params.lane.len().max(DEFAULT_LANE_LEN))
        .clamp(1, NUM_STEPS);
    let mut lane: Vec<Option<usize>> = vec![None; lane_len];
    for (i, slot) in params.lane.iter().take(lane_len).enumerate() {
        lane[i] = slot
            .chars()
            .next()
            .filter(char::is_ascii_lowercase)
            .map(|ch| (ch as u8 - b'a') as usize);
    }
    s.lane = lane;
    s.lane_pos = 0;
    s.lane_selected = s.lane_selected.min(lane_len - 1);
    s.pending_macros.clear();
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
        if let Some(bpm) = params.bpm {
            // corrupt/hand-edited saves must not freeze the transport
            if bpm.is_finite() {
                s.bpm = bpm.clamp(20.0, 300.0);
            }
        }
        if let Some(playing) = params.playing { s.playing = playing; }
        if !params.tracks.is_empty() {
            apply_tracks(&mut s, &params);
        } else {
            // Fallback: load flat v1 fields into a CLEAN first track (the
            // fresh-session demo melody must not bleed through a shorter
            // saved pattern), with the same clamps apply_tracks uses
            let trk = &mut s.tracks[0];
            *trk = Track::empty();
            if let Some(p) = params.euclidean_pulses { trk.pulses = p.min(NUM_STEPS); }
            if let Some(l) = params.euclidean_length { trk.length = l.clamp(1, NUM_STEPS); }
            if let Some(r) = params.euclidean_rotation { trk.rotation = r.min(255); }
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
    // `"` pressed: next key (a-h / A-H) switches / saves a pattern slot.
    // From V-line, the slot switch applies to the whole span (a scene
    // change in two keys: V-select, "b)
    let mut pending_quote = false;
    let mut quote_span: Option<(usize, usize)> = None;
    // `q` pressed with no recording active: next key (a-z) starts one
    let mut pending_record = false;
    // after gc/gC, bare c/C keeps cycling through playhead modes for a
    // second — tap through all eight without re-prefixing
    let mut cycle_window: Option<Instant> = None;
    // `@` pressed: next key (a-z) fires a macro (assigns, on the lane)
    let mut pending_at = false;
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
    // live cable display: manifest entries (cable colors, resolution) +
    // a read-only modbus handle, refreshed about once a second
    let mut ui_manifest = Manifest::open().ok();
    let mut ui_entries: Vec<crate::shm::ManifestEntry> = Vec::new();
    let mut ui_entries_at: Option<Instant> = None;
    let mut ui_modbus = ModulationBus::open().ok();
    let mut ex = crate::excmd::ExLine::default();
    // command-bar audition: the previewed change lives in a scratch
    // history — arrows apply it live, Esc/typing reverts, Enter commits
    let mut preview: Option<(History, String)> = None;
    // `B`: the source picker patches a mod cable into the current step
    let mut picker = crate::picker::Picker::default();
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
                if let Some(bpm) = params.bpm {
                    if bpm.is_finite() {
                        s.bpm = bpm.clamp(20.0, 300.0);
                    }
                }
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
        
        // Drain undo entries produced by thread-side macro firings into
        // the history, so `u` reverts a fired macro like any edit
        {
            let mut s = state.lock().unwrap();
            if let Some(flash) = s.macro_flash.take() {
                undo_msg = Some(flash);
                undo_time = Some(Instant::now());
            }
            if !s.macro_outbox.is_empty() {
                let drained: Vec<Command> = s.macro_outbox.drain(..).collect();
                drop(s);
                for cmd in drained {
                    // fired macros never re-record into a live q-recording
                    history.push_raw(cmd);
                }
            }
        }

        // Auto-execute gt on timeout (only when digits were collected;
        // never while a modal prompt owns the keyboard)
        if gt_target.is_some() && (ex.is_active() || picker.is_active()) {
            gt_target = None;
            gt_input.clear();
            gt_last_key = None;
        }
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

        // Auto-execute f#/t# on timeout (only when digits were collected;
        // never while a modal prompt owns the keyboard — a cf8 must not
        // clear steps and flip to insert mode under an open ex line)
        if (ex.is_active() || picker.is_active()) && pending_find.is_some() {
            pending_find = None;
        }
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
        if ui_entries_at.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
            if ui_manifest.is_none() {
                ui_manifest = Manifest::open().ok();
            }
            if let Some(m) = &ui_manifest {
                ui_entries = m.entries();
            }
            // the bus may not have existed when this pane spawned (panes
            // start in parallel) — keep retrying or the live cable values
            // stay frozen forever
            if ui_modbus.is_none() {
                ui_modbus = ModulationBus::open().ok();
            }
            ui_entries_at = Some(Instant::now());
        }
        // live values for the current track's patched-in steps
        let live_binds: std::collections::HashMap<usize, f32> = current_state
            .track()
            .steps
            .iter()
            .enumerate()
            .filter_map(|(i, st)| {
                let b = st.bind.as_ref()?;
                let addr = crate::routing::SourceAddr::parse(&b.source)?;
                let ch = crate::routing::resolve(&ui_entries, &addr)?;
                let bus = ui_modbus.as_ref()?;
                Some((i, bus.get(ch)))
            })
            .collect();
        let picker_rows = picker.is_active().then(|| picker.rows());
        let ex_menu: Option<(Vec<String>, Option<usize>)> = if ex.is_active() {
            let (items, sel) = ex.menu();
            (!items.is_empty()).then(|| (items.to_vec(), sel))
        } else {
            None
        };
        draw_ui(&mut terminal, &current_state, &mode, &submode, &input_buffer, &pending_count, &gt_target, &gt_input, show_help, &status_msg, pending_hint, picker_rows, ex_menu, &ui_entries, &live_binds, history.rec_status())?;

        if event::poll(Duration::from_millis(50))? {
            let ev = event::read()?;
            if let Event::Mouse(m) = ev {
                use crossterm::event::{MouseButton, MouseEventKind};
                // the picker / ex line own the input while open — a click
                // must not move the cursor under them
                if picker.is_active() || ex.is_active() {
                    continue;
                }
                if matches!(m.kind, MouseEventKind::Down(_)) {
                    // a click is a context switch: abandon half-typed chords
                    pending_count = None;
                    pending_g = false;
                    pending_op = None;
                    pending_find = None;
                    pending_angle = None;
                    pending_layer = false;
                    pending_quote = false;
                    pending_record = false;
                    pending_at = false;
                }
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
                        let w = terminal.size().map(|r| r.width as usize).unwrap_or(60);
                        // the macro lane sits at y = 1, track rows at 2..=n+1
                        if y == 1 {
                            s.on_lane = true;
                            let lane_len = s.lane.len();
                            if lane_len > 0 {
                                let rel = (m.column as usize).saturating_sub(5);
                                let idx_in_window = rel - rel / 5;
                                let info_len =
                                    format!("  {:>3} lane", lane_len).chars().count();
                                let visible = row_visible(w, info_len, lane_len);
                                let start =
                                    row_window_start(lane_len, s.lane_selected, visible);
                                s.lane_selected = (start + idx_in_window).min(lane_len - 1);
                            }
                        } else if (2..=n + 1).contains(&y) {
                            s.on_lane = false;
                            s.current_track = y - 2;
                            // map column back to a step: label(4) + ‹(1),
                            // then 4 cells + 1 space repeating, window
                            // starts where the draw started it
                            let trk_len = s.track().length;
                            let rel = (m.column as usize).saturating_sub(5);
                            let idx_in_window = rel - rel / 5;
                            // identical geometry to the renderer
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

                // a macro fired during the poll window must land in history
                // BEFORE whatever this key is about to push
                {
                    let mut s = state.lock().unwrap();
                    if !s.macro_outbox.is_empty() {
                        let drained: Vec<Command> = s.macro_outbox.drain(..).collect();
                        drop(s);
                        for cmd in drained {
                            history.push_raw(cmd);
                        }
                    }
                }

                // The bind-source picker captures every key while open
                if picker.is_active() {
                    if let crate::picker::PickerEvent::Chosen(addr) = picker.handle_key(key.code)
                    {
                        let target = state.lock().unwrap().layer;
                        let action = Action::BindStep {
                            target,
                            source: addr.map(|a| a.to_string()),
                        };
                        undo_msg = exec_action(&state, &mut history, &action);
                        undo_time = Some(Instant::now());
                        last_change = Some(action);
                    }
                    continue;
                }

                // Ex command line captures every key while open
                if ex.is_active() {
                    let macro_ids: Vec<String> = {
                        let s = state.lock().unwrap();
                        s.macros
                            .iter()
                            .enumerate()
                            .filter(|(_, m)| m.is_some())
                            .map(|(i, _)| ((b'a' + i as u8) as char).to_string())
                            .collect()
                    };
                    let completer = seq_completer(macro_ids);
                    let ev = ex.handle_key(key.code, &completer);
                    let cmd = match ev {
                        crate::excmd::ExEvent::Preview(line) => {
                            // arrow keys audition: apply live, revertible
                            end_preview(&state, &mut history, &mut preview, false);
                            if let Some(scratch) = start_preview(&state, &line, fill_seed) {
                                undo_msg = Some(format!("({})", line));
                                undo_time = Some(Instant::now());
                                preview = Some((scratch, line));
                            }
                            continue;
                        }
                        crate::excmd::ExEvent::Pending | crate::excmd::ExEvent::Cancelled => {
                            // typing/Tab/Esc: the audition reverts cleanly
                            end_preview(&state, &mut history, &mut preview, false);
                            continue;
                        }
                        crate::excmd::ExEvent::Submit(cmd) => cmd,
                    };
                    // Enter on an auditioned line: keep what you hear
                    if preview.is_some() {
                        let matches = preview
                            .as_ref()
                            .is_some_and(|(_, l)| crate::excmd::parse(l) == cmd);
                        end_preview(&state, &mut history, &mut preview, matches);
                        if matches {
                            if let crate::excmd::ExCommand::Unknown(c) = &cmd {
                                // committed fills feed F's repeat bookkeeping
                                if let Some(rest) = c.strip_prefix("fill ") {
                                    let (k, a) = match rest.split_once(char::is_whitespace) {
                                        Some((k, a)) => (k, a.trim()),
                                        None => (rest, ""),
                                    };
                                    if let Some(kind) = state::FillKind::parse(k) {
                                        let default = match kind {
                                            state::FillKind::Mutate => 0.3,
                                            state::FillKind::Density => 0.5,
                                            _ => 0.0,
                                        };
                                        last_fill = Some((kind, a.parse().unwrap_or(default)));
                                    }
                                }
                            }
                            undo_msg = Some(String::from("kept"));
                            undo_time = Some(Instant::now());
                            continue;
                        }
                    }
                    {
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
                                    if let Some(bpm) = p.bpm {
                                        if bpm.is_finite() {
                                            s.bpm = bpm.clamp(20.0, 300.0);
                                        }
                                    }
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
                                    "macro" => {
                                        let mut s = state.lock().unwrap();
                                        if rest.is_empty() {
                                            let defined: Vec<String> = s
                                                .macros
                                                .iter()
                                                .enumerate()
                                                .filter(|(_, m)| m.is_some())
                                                .map(|(i, _)| format!("@{}", (b'a' + i as u8) as char))
                                                .collect();
                                            undo_msg = Some(if defined.is_empty() {
                                                String::from("No macros (qa…q records; :macro a = mute 2 | pat 1 b)")
                                            } else {
                                                defined.join(" ")
                                            });
                                        } else {
                                            let (id, tail) = match rest.split_once(char::is_whitespace) {
                                                Some((a, b)) => (a, b.trim()),
                                                None => (rest.as_str(), ""),
                                            };
                                            let idx = (id.len() == 1)
                                                .then(|| id.chars().next())
                                                .flatten()
                                                .filter(char::is_ascii_lowercase)
                                                .map(|ch| (ch as u8 - b'a') as usize);
                                            match idx {
                                                None => {
                                                    undo_msg = Some(format!("Bad macro id: {} (a-z)", id));
                                                }
                                                Some(idx) if tail.is_empty() => {
                                                    undo_msg = Some(match &s.macros[idx] {
                                                        Some(m) => format!(
                                                            "@{} [{}]: {}",
                                                            id,
                                                            m.quant.name(),
                                                            m.cmds
                                                                .iter()
                                                                .map(fmt_macro_cmd)
                                                                .collect::<Vec<_>>()
                                                                .join(" | ")
                                                        ),
                                                        None => format!("@{} is empty", id),
                                                    });
                                                }
                                                Some(idx) => {
                                                    let spec = tail.strip_prefix('=').map(str::trim);
                                                    let text = spec.unwrap_or(tail);
                                                    undo_msg = Some(match parse_macro_dsl(text) {
                                                        Ok((cmds, quant)) => {
                                                            let m = s.macros[idx]
                                                                .take()
                                                                .unwrap_or_default();
                                                            let new = Macro {
                                                                cmds: if cmds.is_empty() {
                                                                    m.cmds
                                                                } else {
                                                                    cmds
                                                                },
                                                                quant: quant.unwrap_or(m.quant),
                                                            };
                                                            let desc = format!(
                                                                "@{} [{}]: {} command{}",
                                                                id,
                                                                new.quant.name(),
                                                                new.cmds.len(),
                                                                if new.cmds.len() == 1 { "" } else { "s" }
                                                            );
                                                            s.macros[idx] = Some(new);
                                                            desc
                                                        }
                                                        Err(e) => e,
                                                    });
                                                }
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

                // ':' opens the ex command line (any mode, when no prompt
                // is active) — and abandons every half-typed chord, so a
                // stray `q:`/`@:`/`d:` can't fire on the key after Enter
                if key.code == KeyCode::Char(':') && submode.is_empty() && gt_target.is_none() {
                    pending_count = None;
                    pending_g = false;
                    pending_op = None;
                    pending_find = None;
                    pending_angle = None;
                    pending_layer = false;
                    pending_quote = false;
                    pending_record = false;
                    pending_at = false;
                    ex.open();
                    {
                        let macro_ids: Vec<String> = {
                            let s = state.lock().unwrap();
                            s.macros
                                .iter()
                                .enumerate()
                                .filter(|(_, m)| m.is_some())
                                .map(|(i, _)| ((b'a' + i as u8) as char).to_string())
                                .collect()
                        };
                        ex.refresh(&seq_completer(macro_ids));
                    }
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
                    pending_record = false;
                    pending_at = false;
                    cycle_window = None;
                    continue;
                }

                // the gc repeat window: bare c/C keeps cycling for 1s
                if let Some(t0) = cycle_window {
                    if t0.elapsed() > Duration::from_secs(1) {
                        cycle_window = None;
                    } else if mode == "normal" {
                        if let KeyCode::Char(c @ ('c' | 'C')) = key.code {
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
                            cycle_window = Some(Instant::now());
                            continue;
                        }
                        cycle_window = None;
                    }
                }

                // a half-typed >>/<< chord dies on any other key — `>h>`
                // must not count as `>>`
                if pending_angle.is_some()
                    && !matches!(key.code, KeyCode::Char('>') | KeyCode::Char('<'))
                {
                    pending_angle = None;
                }

                // `q`: start/stop macro recording (normal mode). Recording
                // captures semantic commands, not keystrokes — see
                // docs/plans/sequencer-v2.md §7.
                if pending_record {
                    pending_record = false;
                    if let KeyCode::Char(c @ 'a'..='z') = key.code {
                        history.rec_start((c as u8 - b'a') as usize);
                        undo_msg = Some(format!("rec @{} — every edit records; q stops", c));
                        undo_time = Some(Instant::now());
                    }
                    continue;
                }
                if key.code == KeyCode::Char('q') && mode == "normal" && submode.is_empty() {
                    pending_count = None;
                    if let Some((idx, cmds)) = history.rec_stop() {
                        let mut s = state.lock().unwrap();
                        let quant =
                            s.macros[idx].as_ref().map(|m| m.quant).unwrap_or_default();
                        let n = cmds.len();
                        s.macros[idx] = Some(Macro { cmds, quant });
                        undo_msg = Some(format!(
                            "@{} recorded ({} command{})",
                            (b'a' + (idx as u8).min(25)) as char,
                            n,
                            if n == 1 { "" } else { "s" }
                        ));
                        undo_time = Some(Instant::now());
                    } else {
                        pending_record = true;
                    }
                    continue;
                }

                // `@`: fire a macro live (quantized) — or assign it to the
                // slot under the cursor when on the macro lane
                if pending_at {
                    pending_at = false;
                    let idx = match key.code {
                        KeyCode::Char(c @ 'a'..='z') => Some((c as u8 - b'a') as usize),
                        KeyCode::Char('@') => state.lock().unwrap().last_macro,
                        _ => None,
                    };
                    if let Some(idx) = idx {
                        let mut s = state.lock().unwrap();
                        if s.on_lane {
                            let old = s.lane.clone();
                            let sel = s.lane_selected.min(s.lane.len().saturating_sub(1));
                            if !s.lane.is_empty() {
                                s.lane[sel] = Some(idx);
                                let new = s.lane.clone();
                                if new != old {
                                    history.push(Command::SetLane { old, new });
                                }
                                undo_msg = Some(format!(
                                    "lane[{}] = @{}",
                                    sel + 1,
                                    (b'a' + idx as u8) as char
                                ));
                            }
                        } else {
                            let quant = s
                                .macros
                                .get(idx)
                                .and_then(|m| m.as_ref())
                                .filter(|m| !m.cmds.is_empty())
                                .map(|m| m.quant);
                            undo_msg = Some(match quant {
                                Some(q) => {
                                    s.pending_macros.push(PendingMacro { idx, at: None });
                                    s.last_macro = Some(idx);
                                    format!("…@{} ({})", (b'a' + idx as u8) as char, q.name())
                                }
                                None => format!(
                                    "@{} is empty (qa…q records)",
                                    (b'a' + idx as u8) as char
                                ),
                            });
                        }
                        undo_time = Some(Instant::now());
                    }
                    continue;
                }
                if key.code == KeyCode::Char('@') && mode == "normal" && submode.is_empty() {
                    pending_count = None;
                    pending_at = true;
                    continue;
                }

                // The macro lane has its own small dialect (cursor on the
                // lane row): h/l/0/$ move, @a assigns, x/d clear, y/p
                // yank/paste, D clears the lane, #L sets its length, j leaves.
                if mode == "normal" && state.lock().unwrap().on_lane {
                    match key.code {
                        KeyCode::Char(c) if c.is_ascii_digit() => {
                            if c == '0' && pending_count.is_none() {
                                state.lock().unwrap().lane_selected = 0;
                            } else {
                                pending_count.get_or_insert_with(String::new).push(c);
                            }
                        }
                        KeyCode::Char('h') | KeyCode::Left => {
                            let n: usize =
                                pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            let len = s.lane.len().max(1);
                            s.lane_selected = (s.lane_selected + len - (n % len)) % len;
                        }
                        KeyCode::Char('l') | KeyCode::Right => {
                            let n: usize =
                                pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            let len = s.lane.len().max(1);
                            s.lane_selected = (s.lane_selected + n) % len;
                        }
                        KeyCode::Char('$') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            s.lane_selected = s.lane.len().saturating_sub(1);
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            pending_count = None;
                            state.lock().unwrap().on_lane = false;
                        }
                        KeyCode::Char('G') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            s.on_lane = false;
                            s.current_track = s.tracks.len() - 1;
                        }
                        KeyCode::Char('@') => {
                            pending_count = None;
                            pending_at = true;
                        }
                        KeyCode::Char('x') | KeyCode::Char('d') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.lane_selected.min(s.lane.len().saturating_sub(1));
                            if !s.lane.is_empty() && s.lane[sel].is_some() {
                                let old = s.lane.clone();
                                s.lane_register = Some(vec![s.lane[sel]]);
                                s.lane[sel] = None;
                                let new = s.lane.clone();
                                history.push(Command::SetLane { old, new });
                            }
                        }
                        KeyCode::Char('D') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            if s.lane.iter().any(Option::is_some) {
                                let old = s.lane.clone();
                                for slot in s.lane.iter_mut() {
                                    *slot = None;
                                }
                                let new = s.lane.clone();
                                history.push(Command::SetLane { old, new });
                                undo_msg = Some(String::from("Lane cleared"));
                                undo_time = Some(Instant::now());
                            }
                        }
                        KeyCode::Char('y') => {
                            pending_count = None;
                            let mut s = state.lock().unwrap();
                            let sel = s.lane_selected.min(s.lane.len().saturating_sub(1));
                            if !s.lane.is_empty() {
                                s.lane_register = Some(vec![s.lane[sel]]);
                                undo_msg = Some(String::from("Yanked lane slot"));
                                undo_time = Some(Instant::now());
                            }
                        }
                        KeyCode::Char('p') => {
                            let times: usize =
                                pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            if let Some(reg) = s.lane_register.clone() {
                                if !reg.is_empty() && !s.lane.is_empty() {
                                    let old = s.lane.clone();
                                    let start = s.lane_selected.min(s.lane.len() - 1);
                                    let total = (reg.len() * times.max(1))
                                        .min(s.lane.len() - start);
                                    for i in 0..total {
                                        s.lane[start + i] = reg[i % reg.len()];
                                    }
                                    let new = s.lane.clone();
                                    if new != old {
                                        history.push(Command::SetLane { old, new });
                                    }
                                }
                            } else {
                                undo_msg = Some(String::from("Lane register is empty"));
                                undo_time = Some(Instant::now());
                            }
                        }
                        // #L sets the lane length in bars (1-128)
                        KeyCode::Char('L') => {
                            if let Some(n) =
                                pending_count.take().and_then(|s| s.parse::<usize>().ok())
                            {
                                let mut s = state.lock().unwrap();
                                let n = n.clamp(1, NUM_STEPS);
                                if n != s.lane.len() {
                                    let old = s.lane.clone();
                                    s.lane.resize(n, None);
                                    s.lane_selected = s.lane_selected.min(n - 1);
                                    let new = s.lane.clone();
                                    history.push(Command::SetLane { old, new });
                                    undo_msg = Some(format!("lane length = {}", n));
                                    undo_time = Some(Instant::now());
                                }
                            }
                        }
                        // transport + undo still work from the lane
                        KeyCode::Char(' ') => {
                            pending_count = None;
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
                            if transport_ui.is_none() {
                                transport_ui = ShmTransport::open().ok();
                            }
                            if let Some(ref mut t) = transport_ui {
                                t.set_playing(false);
                            }
                            state.lock().unwrap().playing = false;
                        }
                        KeyCode::Char('u') => {
                            let count = pending_count
                                .take()
                                .and_then(|c| c.parse().ok())
                                .unwrap_or(1)
                                .max(1);
                            let mut s = state.lock().unwrap();
                            undo_msg =
                                Some(history_status("Undo", count, || history.undo(&mut s)));
                            undo_time = Some(Instant::now());
                        }
                        _ => {
                            pending_count = None;
                        }
                    }
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
                    let span = quote_span.take();
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
                        {
                            let mut s = state.lock().unwrap();
                            undo_msg = apply_selected(&mut s, &mut history, &action, span);
                            if span.is_some() {
                                s.visual_track_anchor = None;
                            }
                        }
                        if span.is_some() && mode == "visual_line" {
                            mode = String::from("normal");
                        }
                        undo_time = Some(Instant::now());
                        last_change = Some(action);
                    }
                    continue;
                }
                if key.code == KeyCode::Char('"') && (mode == "normal" || mode == "visual_line")
                {
                    pending_count = None;
                    pending_quote = true;
                    quote_span = (mode == "visual_line").then(|| {
                        let s = state.lock().unwrap();
                        let a = s.visual_track_anchor.unwrap_or(s.current_track);
                        (a.min(s.current_track), a.max(s.current_track))
                    });
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
                            // keep tapping c to keep cycling
                            cycle_window = Some(Instant::now());
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
                        // gB unplugs the step's mod cable
                        KeyCode::Char('B') => {
                            pending_count = None;
                            let target = state.lock().unwrap().layer;
                            let action = Action::BindStep { target, source: None };
                            undo_msg = exec_action(&state, &mut history, &action);
                            undo_time = Some(Instant::now());
                            last_change = Some(action);
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
                            let span = Some(span);
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
                        // B patches a mod cable into the current step's
                        // active layer (the picker chooses the source)
                        KeyCode::Char('B') => {
                            pending_count = None;
                            let sources = Manifest::open()
                                .map(|m| crate::routing::live_sources(&m.entries()))
                                .unwrap_or_default();
                            let s = state.lock().unwrap();
                            let current = s.track().steps[s.selected]
                                .bind
                                .as_ref()
                                .and_then(|b| crate::routing::SourceAddr::parse(&b.source));
                            drop(s);
                            picker.open(sources, current.as_ref());
                        }
                        // ( / ) dial the bound source's influence
                        KeyCode::Char(c @ ('(' | ')')) => {
                            let n: i32 = pending_count
                                .take()
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(1);
                            let delta = 0.05 * n as f32 * if c == '(' { -1.0 } else { 1.0 };
                            let action = Action::BindAmount { delta };
                            undo_msg = exec_action(&state, &mut history, &action);
                            undo_time = Some(Instant::now());
                            last_change = Some(action);
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
                        // M toggles note/modulation mode (@ fires macros)
                        KeyCode::Char('M') => {
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

                        // Track navigation (k from the top row reaches the
                        // macro lane; j leaves it again)
                        KeyCode::Char('k') | KeyCode::Up => {
                            let n: usize = pending_count.take().and_then(|s| s.parse().ok()).unwrap_or(1);
                            let mut s = state.lock().unwrap();
                            if s.current_track == 0 {
                                s.on_lane = true;
                            } else {
                                s.current_track = s.current_track.saturating_sub(n);
                                s.selected = 0;
                            }
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
        assert!(matches!(s.register, Some(Register::Tracks(_))));
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
    fn phase_survives_bpm_changes_and_clock_regression() {
        // steady clock at 1000 samples/step
        let p0 = advance_phase(None, 4000, 0, 1000);
        assert_eq!(p0 as u64, 4);
        // bpm change (samples_per_step halves): future rescales, no jump
        let p1 = advance_phase(Some(p0), 4500, 4000, 500);
        assert_eq!(p1 as u64, 5);
        // the clock goes BACKWARD (mixer respawn): rebase, never freeze
        let p2 = advance_phase(Some(p1), 1000, 4500, 500);
        assert_eq!(p2 as u64, 2, "rebased to the new clock");
        let p3 = advance_phase(Some(p2), 1500, 1000, 500);
        assert!(p3 > p2, "and keeps advancing after the rebase");
        // paused transport: clock frozen, phase frozen
        let p4 = advance_phase(Some(p3), 1500, 1500, 500);
        assert_eq!(p4, p3);
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
    fn bind_step_patches_and_unpatches() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        let bind = Action::BindStep {
            target: state::BindTarget::Velocity,
            source: Some(String::from("envelope/0/ch1")),
        };
        apply_action(&mut s, &mut h, &bind);
        let b = s.tracks[0].steps[0].bind.as_ref().expect("bound");
        assert_eq!(b.target, state::BindTarget::Velocity);
        assert!((b.amount - 1.0).abs() < 1e-6);
        // dial the amount, then rebind the same source: amount survives
        apply_action(&mut s, &mut h, &Action::BindAmount { delta: -0.45 });
        apply_action(&mut s, &mut h, &bind);
        let b = s.tracks[0].steps[0].bind.as_ref().expect("still bound");
        assert!((b.amount - 0.55).abs() < 1e-4, "rebind keeps the dialed amount");
        // clamp
        for _ in 0..100 {
            apply_action(&mut s, &mut h, &Action::BindAmount { delta: 0.5 });
        }
        assert!((s.tracks[0].steps[0].bind.as_ref().map(|b| b.amount).unwrap_or(0.0) - 2.0).abs() < 1e-6);
        // unbind + undo round trip — fresh history so the sweep-coalescing
        // rule (all the edits above were the same step, same second)
        // doesn't fold this into the earlier edits
        let mut h2 = History::new();
        apply_action(
            &mut s,
            &mut h2,
            &Action::BindStep { target: state::BindTarget::Velocity, source: None },
        );
        assert!(s.tracks[0].steps[0].bind.is_none());
        assert!(h2.undo(&mut s).is_some());
        assert!(s.tracks[0].steps[0].bind.is_some(), "undo replugs the cable");
        // amount on an unbound step explains itself
        let mut s2 = state_with_tracks(1);
        let msg = apply_action(&mut s2, &mut h, &Action::BindAmount { delta: 0.1 });
        assert_eq!(msg.as_deref(), Some("No binding on this step (B binds)"));
    }

    #[test]
    fn multi_select_yank_and_block_paste() {
        let mut s = state_with_tracks(6);
        let mut h = History::new();
        for (i, t) in s.tracks.iter_mut().enumerate() {
            t.steps[0].note = 50 + i as u8;
        }
        // yank rows 1, 3, 4 via marks (non-consecutive)
        s.marks = vec![true, false, true, true, false, false];
        s.current_track = 0;
        let msg = apply_selected(&mut s, &mut h, &Action::OpTrack(Operator::Yank), None);
        assert_eq!(msg.as_deref(), Some("3 tracks yanked"));
        assert_eq!(h.undo(&mut s), None, "yank is never a change");
        // block paste at row 4: rows 4,5,6 get the yanked patterns
        s.marks = vec![false; 6];
        s.current_track = 3;
        s.selected = 0;
        apply_selected(&mut s, &mut h, &Action::Paste { before: false, times: 1 }, None);
        assert_eq!(s.tracks[3].steps[0].note, 50);
        assert_eq!(s.tracks[4].steps[0].note, 52);
        assert_eq!(s.tracks[5].steps[0].note, 53);
        assert!(h.undo(&mut s).is_some());
        assert_eq!(s.tracks[4].steps[0].note, 54, "one u reverts the block");
        // gp inserts all three as new tracks
        s.current_track = 0;
        apply_selected(&mut s, &mut h, &Action::PasteAsTrack { before: false, times: 1 }, None);
        // capped at 8: 6 + 2 fit
        assert_eq!(s.tracks.len(), 8);
        assert_eq!(s.tracks[1].steps[0].note, 50);
        assert_eq!(s.tracks[2].steps[0].note, 52);
    }

    #[test]
    fn multi_delete_yanks_what_it_removes() {
        let mut s = state_with_tracks(4);
        let mut h = History::new();
        for (i, t) in s.tracks.iter_mut().enumerate() {
            t.steps[0].note = 60 + i as u8;
        }
        apply_selected(&mut s, &mut h, &Action::OpTrack(Operator::Delete), Some((1, 2)));
        assert_eq!(s.tracks.len(), 2);
        match &s.register {
            Some(Register::Tracks(ts)) => {
                assert_eq!(ts.len(), 2);
                assert_eq!(ts[0].steps[0].note, 61, "row order preserved");
                assert_eq!(ts[1].steps[0].note, 62);
            }
            other => panic!("register should hold the deleted tracks, got {:?}", other.is_some()),
        }
    }

    // ── macros + the lane ───────────────────────────────────────────────

    #[test]
    fn recording_captures_everything_undoable() {
        let mut s = state_with_tracks(2);
        let mut h = History::new();
        h.rec_start(0);
        s.current_track = 1;
        apply_action(&mut s, &mut h, &Action::ToggleMute);
        apply_action(&mut s, &mut h, &Action::SwitchSlot(2));
        apply_action(&mut s, &mut h, &Action::SetCycle(state::CycleMode::Reverse));
        // step edits record too now — as absolute state
        s.selected = 5;
        apply_action(&mut s, &mut h, &Action::ToggleStep);
        apply_action(
            &mut s,
            &mut h,
            &Action::Adjust { layer: state::BindTarget::Velocity, steps: 1, coarse: false },
        );
        let (idx, cmds) = h.rec_stop().expect("was recording");
        assert_eq!(idx, 0);
        assert_eq!(cmds[0], state::MacroCmd::SetMute { track: 1, muted: true });
        assert_eq!(cmds[1], state::MacroCmd::SwitchPattern { track: 1, slot: 2 });
        assert_eq!(
            cmds[2],
            state::MacroCmd::SetCycle { track: 1, mode: state::CycleMode::Reverse }
        );
        assert!(matches!(
            cmds[3],
            state::MacroCmd::SetActive { track: 1, step: 5, active: true }
        ));
        assert!(matches!(cmds[4], state::MacroCmd::SetSteps { track: 1, start: 5, .. }));
        assert_eq!(cmds.len(), 5);
    }

    #[test]
    fn recorded_edits_replay_exactly_and_sweeps_merge() {
        let mut s = state_with_tracks(1);
        let mut h = History::new();
        h.rec_start(3);
        s.selected = 2;
        apply_action(&mut s, &mut h, &Action::ToggleStep);
        // a velocity sweep: many presses, ONE recorded command
        for _ in 0..5 {
            apply_action(
                &mut s,
                &mut h,
                &Action::Adjust { layer: state::BindTarget::Velocity, steps: 1, coarse: false },
            );
        }
        let (_, cmds) = h.rec_stop().expect("recording");
        assert_eq!(cmds.len(), 2, "sweep merged: {cmds:?}");
        let velocity_after = s.tracks[0].steps[2].velocity;
        // wipe the work, then fire the macro: the riff comes back
        s.tracks[0].steps[2] = Step::default();
        s.macros[3] = Some(Macro { cmds, quant: state::Quant::Now });
        fire_macro_now(&mut s, 3, 0);
        assert!(s.tracks[0].steps[2].active, "toggle replayed");
        assert_eq!(s.tracks[0].steps[2].velocity, velocity_after, "sweep replayed absolutely");
        assert_eq!(s.macro_flash.as_deref(), Some("@d"));
        // firing again: state already matches → honest no-change flash
        fire_macro_now(&mut s, 3, 0);
        assert_eq!(s.macro_flash.as_deref(), Some("@d: no change"));
    }

    #[test]
    fn fired_macros_do_not_leak_into_a_recording() {
        let mut h = History::new();
        h.rec_start(0);
        // a thread-fired macro's undo lands via push_raw
        h.push_raw(Command::ToggleMute { track: 0, was_muted: false });
        assert_eq!(h.rec_status(), Some((0, 0)), "push_raw is invisible to rec");
        h.push(Command::ToggleMute { track: 0, was_muted: false });
        assert_eq!(h.rec_status(), Some((0, 1)));
    }

    #[test]
    fn fire_macro_applies_and_groups_undo() {
        let mut s = state_with_tracks(3);
        let mut h = History::new();
        s.macros[0] = Some(Macro {
            cmds: vec![
                state::MacroCmd::SetMute { track: 0, muted: true },
                state::MacroCmd::SetMute { track: 2, muted: true },
                state::MacroCmd::SwitchPattern { track: 1, slot: 1 },
            ],
            quant: state::Quant::Now,
        });
        fire_macro_now(&mut s, 0, 99);
        assert!(s.tracks[0].muted && s.tracks[2].muted);
        assert_eq!(s.tracks[1].active_slot, 1);
        assert_eq!(s.last_macro, Some(0));
        // outbox drains into history as one entry
        let drained: Vec<Command> = s.macro_outbox.drain(..).collect();
        assert_eq!(drained.len(), 1);
        for cmd in drained {
            h.push(cmd);
        }
        assert_eq!(h.undo(&mut s), Some("Macro"));
        assert!(!s.tracks[0].muted && !s.tracks[2].muted);
        assert_eq!(s.tracks[1].active_slot, 0);
    }

    #[test]
    fn fire_macro_skips_noops_and_bad_tracks() {
        let mut s = state_with_tracks(1);
        s.macros[3] = Some(Macro {
            cmds: vec![
                state::MacroCmd::SetMute { track: 9, muted: true }, // out of range
                state::MacroCmd::SetMute { track: 0, muted: false }, // already false
            ],
            quant: state::Quant::Bar,
        });
        fire_macro_now(&mut s, 3, 1);
        assert!(s.macro_outbox.is_empty(), "nothing changed, nothing to undo");
    }

    #[test]
    fn macro_transpose_and_rotate_whole_track() {
        let mut s = state_with_tracks(1);
        let cmd = state::MacroCmd::TransposeTrack { track: 0, by: 7 };
        let undo = apply_macro_cmd(&mut s, &cmd, 0).expect("changed");
        assert_eq!(s.tracks[0].steps[0].note, 67);
        undo.undo(&mut s);
        assert_eq!(s.tracks[0].steps[0].note, 60);
        let rot = state::MacroCmd::RotateTrack { track: 0, by: 1 };
        apply_macro_cmd(&mut s, &rot, 0);
        assert!(!s.tracks[0].steps[0].active && s.tracks[0].steps[1].active);
    }

    #[test]
    fn quant_boundaries_land_on_the_grid() {
        use state::Quant;
        assert_eq!(quant_boundary(Quant::Now, 37, None), 37);
        assert_eq!(quant_boundary(Quant::Beat, 37, None), 40);
        assert_eq!(quant_boundary(Quant::Beat, 40, None), 44, "strictly after");
        assert_eq!(quant_boundary(Quant::Bar, 37, None), 48);
        assert_eq!(quant_boundary(Quant::PatternEnd, 37, Some(12)), 48);
        assert_eq!(quant_boundary(Quant::PatternEnd, 37, None), 48, "falls back to bar");
    }

    #[test]
    fn macro_dsl_round_trips() {
        let text = "pat 2 b | mute 3 | unmute 1 | cycle 2 pingpong | xpose 1 +7 | rot 4 -2 | scale 1 dorian | fill 2 density 0.4 | bpm 90 | quant beat";
        let (cmds, quant) = parse_macro_dsl(text).expect("parses");
        assert_eq!(cmds.len(), 9);
        assert_eq!(quant, Some(state::Quant::Beat));
        // formatting then reparsing yields the same commands
        let formatted: Vec<String> = cmds.iter().map(fmt_macro_cmd).collect();
        let (cmds2, _) = parse_macro_dsl(&formatted.join(" | ")).expect("reparses");
        assert_eq!(cmds, cmds2);
        // errors are specific
        assert!(parse_macro_dsl("pat 2 z").is_err(), "slot z out of range");
        assert!(parse_macro_dsl("cycle 1 sideways").is_err());
        assert!(parse_macro_dsl("scale 1 nonsense").is_err());
        assert!(parse_macro_dsl("pat 0 a").is_err(), "tracks are 1-based");
    }

    #[test]
    fn macros_and_lane_persist() {
        let mut s = state_with_tracks(2);
        s.macros[1] = Some(Macro {
            cmds: vec![state::MacroCmd::SetMute { track: 0, muted: true }],
            quant: state::Quant::PatternEnd,
        });
        // every MacroCmd variant must survive TOML
        s.macros[2] = Some(Macro {
            cmds: vec![
                state::MacroCmd::SwitchPattern { track: 1, slot: 3 },
                state::MacroCmd::SetMute { track: 0, muted: false },
                state::MacroCmd::SetCycle { track: 1, mode: state::CycleMode::Drunk },
                state::MacroCmd::TransposeTrack { track: 0, by: -2 },
                state::MacroCmd::RotateTrack { track: 1, by: 3 },
                state::MacroCmd::SetScale { track: 0, scale: String::new() },
                state::MacroCmd::Fill { track: 1, kind: state::FillKind::Density, arg: 0.4 },
                state::MacroCmd::SetBpm { bpm: 93.5 },
                state::MacroCmd::SetSteps {
                    track: 0,
                    start: 2,
                    steps: vec![state::StepParam {
                        active: true,
                        note: 71,
                        velocity: 90,
                        mod_value: 0.0,
                        prob: 60,
                        bind: None,
                    }],
                },
                state::MacroCmd::SetActive { track: 1, step: 7, active: true },
                state::MacroCmd::SetEuclid { track: 0, pulses: 3, length: 12, rotation: 1 },
                state::MacroCmd::SetMode { track: 1, mode: TrackMode::Modulation },
            ],
            quant: state::Quant::Beat,
        });
        s.lane = vec![Some(1), None, Some(1), None, None, None];
        let params = snapshot_params(&s);
        let toml = state::to_toml_string(&params).expect("serializes");
        let back: state::SequencerParams = toml::from_str(&toml).expect("parses");
        let mut s2 = SequencerState::default();
        apply_tracks(&mut s2, &back);
        assert_eq!(s2.macros[1], s.macros[1]);
        assert_eq!(s2.macros[2], s.macros[2], "all eight command shapes round-trip");
        assert!(s2.macros[0].is_none());
        assert_eq!(s2.lane, s.lane);
    }

    #[test]
    fn deleting_a_track_keeps_marks_and_flushes_gates() {
        let mut s = state_with_tracks(3);
        let mut h = History::new();
        s.marks = vec![false, false, true]; // mark t3
        s.last_notes = vec![Some(60), Some(64), Some(67)]; // all sounding
        s.current_track = 0;
        apply_action(&mut s, &mut h, &Action::OpTrack(Operator::Delete));
        assert_eq!(s.marks, vec![false, true], "mark followed its track");
        // rows ARE the source bytes, so every held note flushes under its
        // pre-shift row before the re-wire
        assert_eq!(s.pending_offs, vec![(60, 0), (64, 1), (67, 2)]);
        assert!(s.last_notes.iter().all(Option::is_none));
    }

    #[test]
    fn routing_is_positional_and_paste_carries_no_routing() {
        // THE contract, settled after much blood: t-numbers are the rig's
        // jacks. A yanked track carries steps + settings, never routing;
        // whatever lands on a row sounds through that row's listeners,
        // and a track that slides to row 4 inherits t4's routing (in the
        // default patch: silence). Pure position, no hidden identity.
        let mut s = state_with_tracks(4);
        let mut h = History::new();
        s.tracks[2].steps[0].note = 41; // the bassline at t3
        s.current_track = 0;
        apply_action(&mut s, &mut h, &Action::OpTrack(Operator::Yank));
        s.current_track = 1;
        apply_action(&mut s, &mut h, &Action::PasteAsTrack { before: false, times: 1 });
        assert_eq!(s.tracks.len(), 5);
        // the copy SITS at row 3 (= broadcasts as t3: row is the routing);
        // the bassline slid to row 4 (= broadcasts as t4 now)
        assert_eq!(s.tracks[2].steps[0].note, 60, "row 3 holds the copy");
        assert_eq!(s.tracks[3].steps[0].note, 41, "row 4 holds the bassline");
        // shifting flushed every held note under its pre-shift row so no
        // gate hangs across the re-wire
        assert!(s.last_notes.iter().all(Option::is_none));
        // undo restores the board exactly
        assert!(h.undo(&mut s).is_some());
        assert_eq!(s.tracks.len(), 4);
        assert_eq!(s.tracks[2].steps[0].note, 41);
    }

    #[test]
    fn track_count_caps_at_the_modbus_claim() {
        let mut s = state_with_tracks(crate::NUM_TRACKS);
        let mut h = History::new();
        let msg = apply_action(&mut s, &mut h, &Action::NewTrack { before: false });
        assert_eq!(s.tracks.len(), crate::NUM_TRACKS, "o refuses at the cap");
        assert!(msg.is_some_and(|m| m.contains("limit")));
        s.register = Some(Register::Tracks(vec![Track::new()]));
        apply_action(&mut s, &mut h, &Action::PasteAsTrack { before: false, times: 3 });
        assert_eq!(s.tracks.len(), crate::NUM_TRACKS, "gp refuses at the cap");
        // a corrupt save with too many tracks loads clamped
        let mut params = snapshot_params(&s);
        params.tracks.extend(params.tracks.clone());
        let mut s2 = SequencerState::default();
        apply_tracks(&mut s2, &params);
        assert_eq!(s2.tracks.len(), crate::NUM_TRACKS);
    }

    #[test]
    fn corrupt_scale_cents_fall_back_safely() {
        let mut s = state_with_tracks(1);
        s.tracks[0].scale = crate::theory::scales::lookup("dorian");
        let mut params = snapshot_params(&s);
        // corrupt: non-ascending cents and a garbage period
        params.tracks[0].scale_cents = vec![0.0, 700.0, 300.0];
        params.tracks[0].scale_period = Some(f64::NAN);
        let mut s2 = SequencerState::default();
        apply_tracks(&mut s2, &params);
        // cents rejected → the name re-resolves against the library
        let sc = s2.tracks[0].scale.as_ref().expect("name fallback");
        assert_eq!(sc.name, "dorian");
        // with a bogus name too, it falls all the way to chromatic
        params.tracks[0].scale = Some(String::from("not a scale"));
        let mut s3 = SequencerState::default();
        apply_tracks(&mut s3, &params);
        assert!(s3.tracks[0].scale.is_none());
    }

    #[test]
    fn saved_tracks_trim_default_tails() {
        let s = state_with_tracks(1); // 16-step pattern, nothing past it
        let params = snapshot_params(&s);
        assert!(
            params.tracks[0].steps.len() <= 16,
            "saved {} step tables for a 16-step pattern",
            params.tracks[0].steps.len()
        );
        // and the round trip still restores the full grid
        let mut s2 = SequencerState::default();
        apply_tracks(&mut s2, &params);
        assert_eq!(s2.tracks[0].steps.len(), NUM_STEPS);
        assert_eq!(s2.tracks[0].steps[..16], s.tracks[0].steps[..16]);
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
