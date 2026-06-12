// Los — a modular groovebox that lives in your terminal
// Copyright (C) 2026 doo-nothing / AU Supply
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version. See LICENSE.

//! Offline validation for session/song files: `los check`, and the gate
//! `los load` runs before it touches tmux.
//!
//! The point is feedback an author — human or agent — can act on without
//! booting a session: every problem in the file is reported in one pass
//! (never just the first), each message says what range or spelling would
//! be valid, and misspelled keys get a suggestion instead of serde's
//! silent ignore-unknown-fields default (which is right for loading old
//! saves, and exactly wrong for catching typos in hand-written songs).
//!
//! Ranges here mirror the runtime clamps inside each module; the comments
//! on [`check_sequencer`] and friends cite the clamp sites so drift
//! between validator and engine stays findable.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use crate::routing::{output_labels, SourceAddr};
use crate::state::{
    DelayParams, EnvelopeParams, FilterbankParams, MacroCmd, MixerParams, ScopeParams,
    SequencerParams, SessionState, StepParam, SwarmParams, TapeParams, TemplateParams, TrackMode,
    VoiceParams, STATE_FORMAT,
};
use crate::theory;

/// One problem found in a state file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Where in the file, e.g. `sequencer 0: tracks[1].swing`.
    pub location: String,
    pub message: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.location.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.location, self.message)
        }
    }
}

/// Everything wrong (and everything suspicious) with a state file.
/// Errors block `los load`; warnings print and proceed.
#[derive(Debug, Default)]
pub struct Report {
    pub errors: Vec<Issue>,
    pub warnings: Vec<Issue>,
}

impl Report {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }

    fn error(&mut self, location: impl Into<String>, message: impl Into<String>) {
        self.errors.push(Issue {
            location: location.into(),
            message: message.into(),
        });
    }

    fn warn(&mut self, location: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(Issue {
            location: location.into(),
            message: message.into(),
        });
    }

    /// All errors, one per line — what `los load` prints when it refuses.
    #[must_use]
    pub fn render_errors(&self) -> String {
        self.errors
            .iter()
            .map(|i| format!("  error: {i}\n"))
            .collect()
    }
}

/// Validate a state file on disk. An unreadable file is itself an error.
#[must_use]
pub fn validate_file(path: &Path) -> Report {
    match std::fs::read_to_string(path) {
        Ok(text) => validate_str(&text),
        Err(e) => {
            let mut r = Report::default();
            r.error("", format!("cannot read {}: {e}", path.display()));
            r
        }
    }
}

/// Validate state-file TOML text. Collects every issue; never panics.
#[must_use]
pub fn validate_str(text: &str) -> Report {
    let mut r = Report::default();
    // toml 0.8's Display already renders a caret-marked source snippet
    // with line numbers — pass it through untouched.
    let value: toml::Value = match toml::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            r.error("", format!("TOML parse error:\n{e}"));
            return r;
        }
    };
    let Some(st) = decode::<SessionState>(&value, "", &mut r) else {
        return r;
    };
    validate_session(&st, &mut r);
    r
}

// ── session-level checks ─────────────────────────────────────────────────────

/// Pending cross-module check: a source address that names a sequencer
/// track whose existence/mode we can only judge once every pane is decoded.
struct PendingTrackRef {
    addr: SourceAddr,
    location: String,
    /// true = note events wanted (voice/swarm `notes_src`), false = a
    /// trigger/gate (envelope `trigger_src`) — both come from note events.
    wants_notes: bool,
}

fn validate_session(st: &SessionState, r: &mut Report) {
    if st.meta.format < STATE_FORMAT {
        r.error(
            "meta.format",
            format!(
                "this is a v{} state file; v{STATE_FORMAT} (routing source addresses) \
                 is a clean break — re-save your session",
                st.meta.format.max(1)
            ),
        );
    }

    // Roster: every (module, instance) the file declares. The conductor
    // window is the conductor's own pane — load_session filters it out,
    // so validation does too.
    let mut declared: BTreeSet<(String, usize)> = BTreeSet::new();
    let mut panes: Vec<(&'static str, usize, String, Option<&toml::Value>)> = Vec::new();
    for (wi, win) in st.windows.iter().enumerate() {
        if win.name == "conductor" {
            continue;
        }
        for (pi, pane) in win.panes.iter().enumerate() {
            let loc = format!("windows[{wi}].panes[{pi}]");
            let Some(canon) = crate::conductor::canonical_module(&pane.module) else {
                let hint = suggest_from(&pane.module, MODULE_NAMES.iter().copied())
                    .map(|s| format!(" — did you mean '{s}'?"))
                    .unwrap_or_default();
                r.error(loc, format!("unknown module '{}'{hint}", pane.module));
                continue;
            };
            if canon == "conductor" {
                continue;
            }
            if !declared.insert((canon.to_string(), pane.instance)) {
                r.error(
                    loc.clone(),
                    format!("duplicate pane: {canon} {} appears twice", pane.instance),
                );
            }
            if pane.patch.is_some() {
                r.warn(
                    loc.clone(),
                    "pane sets `patch` (a named patch reference) — load only applies \
                     `patch_inline`; put the params inline instead",
                );
            }
            panes.push((canon, pane.instance, loc, pane.patch_inline.as_ref()));
        }
    }

    if panes.is_empty() {
        r.error("windows", "no module panes — nothing would spawn");
    }
    if !declared.iter().any(|(m, _)| m == "mixer") {
        r.warn(
            "windows",
            "no mixer pane: the mixer owns the audio device and the clock — \
             without one nothing sounds and nothing advances",
        );
    }

    // Typed decode + per-module checks. Decoded sequencers are kept for
    // the cross-module track checks afterwards.
    let mut seqs: BTreeMap<usize, SequencerParams> = BTreeMap::new();
    let mut pending: Vec<PendingTrackRef> = Vec::new();
    for (canon, instance, _, inline) in &panes {
        let Some(value) = inline else { continue };
        let loc = format!("{canon} {instance}");
        match *canon {
            "sequencer" => {
                if let Some(p) = decode::<SequencerParams>(value, &loc, r) {
                    check_sequencer(&p, &loc, &declared, r);
                    seqs.insert(*instance, p);
                }
            }
            "voice" => {
                if let Some(p) = decode::<VoiceParams>(value, &loc, r) {
                    check_voice(&p, &loc, &declared, r, &mut pending);
                }
            }
            "swarm" => {
                if let Some(p) = decode::<SwarmParams>(value, &loc, r) {
                    check_swarm(&p, &loc, &declared, r, &mut pending);
                }
            }
            "envelope" => {
                if let Some(p) = decode::<EnvelopeParams>(value, &loc, r) {
                    check_envelope(&p, &loc, &declared, r, &mut pending);
                }
            }
            "mixer" => {
                if let Some(p) = decode::<MixerParams>(value, &loc, r) {
                    check_mixer(&p, &loc, &declared, r);
                }
            }
            "delay" => {
                if let Some(p) = decode::<DelayParams>(value, &loc, r) {
                    check_delay(&p, &loc, &declared, r);
                }
            }
            "filterbank" => {
                if let Some(p) = decode::<FilterbankParams>(value, &loc, r) {
                    check_filterbank(&p, &loc, &declared, r);
                }
            }
            "tape" => {
                if let Some(p) = decode::<TapeParams>(value, &loc, r) {
                    check_tape(&p, &loc, &declared, r);
                }
            }
            "template" => {
                if let Some(p) = decode::<TemplateParams>(value, &loc, r) {
                    check_template(&p, &loc, &declared, r);
                }
            }
            "scope" => {
                // ScopeParams is all view state; decode to catch typos.
                let _ = decode::<ScopeParams>(value, &loc, r);
            }
            // No params structs: state is ephemeral or none.
            "tone" | "badge" | "conductor" => {}
            other => r.error(&loc, format!("no validator for module '{other}'")),
        }
    }

    check_track_refs(&pending, &seqs, r);
}

/// Cross-module pass: addresses that name sequencer tracks, judged
/// against the sequencer params the file actually declares.
fn check_track_refs(
    pending: &[PendingTrackRef],
    seqs: &BTreeMap<usize, SequencerParams>,
    r: &mut Report,
) {
    for p in pending {
        if p.addr.module != "sequencer" {
            continue;
        }
        let Some(sp) = seqs.get(&p.addr.instance) else {
            continue;
        };
        // An empty tracks list means the sequencer keeps its defaults
        // (all 8 tracks exist) — nothing to judge.
        if sp.tracks.is_empty() {
            continue;
        }
        let Some(idx) = output_labels("sequencer")
            .iter()
            .position(|l| *l == p.addr.output)
        else {
            continue; // grammar errors were already reported
        };
        if idx >= sp.tracks.len() {
            r.warn(
                p.location.clone(),
                format!(
                    "\"{}\" points past the {} track(s) sequencer {} declares — \
                     it will never fire",
                    p.addr,
                    sp.tracks.len(),
                    p.addr.instance
                ),
            );
        } else if p.wants_notes && sp.tracks[idx].mode == TrackMode::Modulation {
            r.warn(
                p.location.clone(),
                format!(
                    "\"{}\" is a modulation-mode track — it writes CV to the modbus \
                     and emits no note events",
                    p.addr
                ),
            );
        }
    }
}

// ── per-module checks ────────────────────────────────────────────────────────

/// Mirrors the sequencer's load clamps: bpm 20–300 (sequencer.rs `:set
/// bpm` and macro paths), `apply_tracks` (length/swing/humanize/decay/
/// root/active_slot), `step_from_param` (prob/repeats), and the macro +
/// lane decoding (single lowercase letters, undefined letters = None).
fn check_sequencer(
    p: &SequencerParams,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
) {
    if let Some(bpm) = p.bpm {
        if !(20.0..=300.0).contains(&bpm) {
            r.error(loc, format!("bpm {bpm} is out of range 20–300"));
        }
    }
    if p.tracks.len() > crate::NUM_TRACKS {
        r.error(
            loc,
            format!(
                "{} tracks declared — the sequencer holds {} (extras are dropped)",
                p.tracks.len(),
                crate::NUM_TRACKS
            ),
        );
    }
    check_steps(&p.steps, "steps", loc, declared, r);

    for (ti, t) in p.tracks.iter().enumerate() {
        let tl = format!("tracks[{ti}]");
        if let Some(len) = t.length {
            if len == 0 || len > 128 {
                r.error(loc, format!("{tl}.length {len} is out of range 1–128"));
            }
        }
        if let Some(root) = t.root {
            if root > 127 {
                r.error(loc, format!("{tl}.root {root} is out of MIDI range 0–127"));
            }
        }
        if t.active_slot > 7 {
            r.error(
                loc,
                format!("{tl}.active_slot {} is out of range 0–7 (a–h)", t.active_slot),
            );
        }
        if !(50..=75).contains(&t.swing) {
            r.error(
                loc,
                format!(
                    "{tl}.swing {} is out of range 50–75 (50 = straight, 75 = maximal)",
                    t.swing
                ),
            );
        }
        if !(0.0..=30.0).contains(&t.humanize) {
            r.error(
                loc,
                format!("{tl}.humanize {} ms is out of range 0–30", t.humanize),
            );
        }
        if !(-100..=100).contains(&t.ratchet_decay) {
            r.error(
                loc,
                format!(
                    "{tl}.ratchet_decay {} is out of range -100–100",
                    t.ratchet_decay
                ),
            );
        }
        if t.scale_cents.is_empty() {
            if let Some(name) = t.scale.as_deref() {
                if !name.is_empty() && theory::scales::lookup(name).is_none() {
                    let hint = suggest_from(name, theory::scales::names())
                        .map(|s| format!(" — did you mean '{s}'?"))
                        .unwrap_or_default();
                    r.error(loc, format!("{tl}.scale '{name}' is not a known scale{hint}"));
                }
            }
        }
        if let Some(name) = t.groove.as_deref() {
            if !name.is_empty() && !theory::groove::LIBRARY.iter().any(|g| g.name == name) {
                let hint =
                    suggest_from(name, theory::groove::LIBRARY.iter().map(|g| g.name))
                        .map(|s| format!(" — did you mean '{s}'?"))
                        .unwrap_or_default();
                r.error(loc, format!("{tl}.groove '{name}' is not a known groove{hint}"));
            }
        }
        check_steps(&t.steps, &format!("{tl}.steps"), loc, declared, r);
        for sp in &t.slots {
            if sp.slot > 7 {
                r.error(
                    loc,
                    format!("{tl}.slots: slot {} is out of range 0–7 (a–h)", sp.slot),
                );
                continue;
            }
            if sp.slot == t.active_slot {
                // apply_tracks overwrites the active slot with the inline
                // pattern (sequencer.rs `slots[active_slot] = None`).
                r.warn(
                    loc,
                    format!(
                        "{tl}.slots: slot {} is the active slot — its data here is \
                         discarded; the active pattern lives inline in the track",
                        sp.slot
                    ),
                );
            }
            if let Some(len) = sp.length {
                if len == 0 || len > 128 {
                    r.error(
                        loc,
                        format!("{tl}.slots[{}].length {len} is out of range 1–128", sp.slot),
                    );
                }
            }
            check_steps(
                &sp.steps,
                &format!("{tl}.slots[{}].steps", sp.slot),
                loc,
                declared,
                r,
            );
        }
    }

    // Macros: single letters a–z, unique. The lane stores bare letters;
    // an undefined letter decodes to a slot that silently never fires.
    let track_count = if p.tracks.is_empty() {
        crate::NUM_TRACKS
    } else {
        p.tracks.len()
    };
    let mut macro_ids: BTreeSet<char> = BTreeSet::new();
    for (mi, m) in p.macros.iter().enumerate() {
        let ml = format!("macros[{mi}]");
        let id = m.id.chars().next();
        match id {
            Some(c) if c.is_ascii_lowercase() && m.id.len() == 1 => {
                if !macro_ids.insert(c) {
                    r.error(loc, format!("{ml}: duplicate macro id '{c}'"));
                }
            }
            _ => {
                r.error(
                    loc,
                    format!("{ml}: id '{}' must be a single letter a–z", m.id),
                );
            }
        }
        for (ci, cmd) in m.cmds.iter().enumerate() {
            check_macro_cmd(cmd, p, track_count, &format!("{ml}.cmds[{ci}]"), loc, declared, r);
        }
    }

    let lane_len = p.lane_len.unwrap_or_else(|| p.lane.len().max(1));
    if let Some(ll) = p.lane_len {
        if ll == 0 || ll > 128 {
            r.error(loc, format!("lane_len {ll} is out of range 1–128"));
        }
        if p.lane.len() > ll {
            r.warn(
                loc,
                format!(
                    "lane has {} entries but lane_len is {ll} — the extras never play",
                    p.lane.len()
                ),
            );
        }
    }
    let mut lane_has_entries = false;
    for (bar, slot) in p.lane.iter().take(lane_len).enumerate() {
        if slot.is_empty() {
            continue;
        }
        lane_has_entries = true;
        let c = slot.chars().next().unwrap_or(' ');
        if !(c.is_ascii_lowercase() && slot.len() == 1) {
            r.error(
                loc,
                format!("lane[{bar}] '{slot}' must be \"\" (empty) or a single letter a–z"),
            );
        } else if !macro_ids.contains(&c) {
            r.error(
                loc,
                format!("lane[{bar}] fires macro '{c}' but no macro '{c}' is defined"),
            );
        }
    }
    if lane_has_entries && p.lane.first().is_none_or(String::is_empty) {
        r.warn(
            loc,
            "lane[0] is empty: the form opens in whatever state the track params \
             describe — an opening macro at bar 0 makes the start explicit \
             (and `los render` starts from bar 0)",
        );
    }
}

/// Macro commands carry track/slot indices the sequencer bounds-checks at
/// fire time (silently skipping); here they're surfaced as errors.
fn check_macro_cmd(
    cmd: &MacroCmd,
    p: &SequencerParams,
    track_count: usize,
    cl: &str,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
) {
    let check_track = |track: usize, r: &mut Report| {
        if track >= track_count {
            r.error(
                loc,
                format!("{cl}: track {track} is out of range — {track_count} track(s) declared"),
            );
            false
        } else {
            true
        }
    };
    match cmd {
        MacroCmd::SwitchPattern { track, slot } => {
            let track_ok = check_track(*track, r);
            if *slot > 7 {
                r.error(loc, format!("{cl}: slot {slot} is out of range 0–7 (a–h)"));
            } else if track_ok && !p.tracks.is_empty() {
                let t = &p.tracks[*track];
                let occupied =
                    t.active_slot == *slot || t.slots.iter().any(|s| s.slot == *slot);
                if !occupied {
                    let letter = (b'a' + *slot as u8) as char;
                    r.warn(
                        loc,
                        format!(
                            "{cl}: switches track {track} to empty slot '{letter}' — \
                             silence until something else switches it back"
                        ),
                    );
                }
            }
        }
        MacroCmd::SetMute { track, muted: _ } => {
            check_track(*track, r);
        }
        MacroCmd::SetCycle { track, mode: _ } => {
            check_track(*track, r);
        }
        MacroCmd::TransposeTrack { track, by: _ } => {
            check_track(*track, r);
        }
        MacroCmd::RotateTrack { track, by: _ } => {
            check_track(*track, r);
        }
        MacroCmd::SetScale { track, scale } => {
            check_track(*track, r);
            if !scale.is_empty() && theory::scales::lookup(scale).is_none() {
                let hint = suggest_from(scale, theory::scales::names())
                    .map(|s| format!(" — did you mean '{s}'?"))
                    .unwrap_or_default();
                r.error(loc, format!("{cl}: scale '{scale}' is not a known scale{hint}"));
            }
        }
        MacroCmd::Fill {
            track,
            kind: _,
            arg: _,
        } => {
            check_track(*track, r);
        }
        MacroCmd::SetBpm { bpm } => {
            if !(20.0..=300.0).contains(bpm) {
                r.error(loc, format!("{cl}: bpm {bpm} is out of range 20–300"));
            }
        }
        MacroCmd::SetSteps {
            track,
            start,
            steps,
        } => {
            check_track(*track, r);
            if start + steps.len() > 128 {
                r.error(
                    loc,
                    format!(
                        "{cl}: steps {start}..{} run past the 128-step grid",
                        start + steps.len()
                    ),
                );
            }
            check_steps(steps, &format!("{cl}.steps"), loc, declared, r);
        }
        MacroCmd::SetActive {
            track,
            step,
            active: _,
        } => {
            check_track(*track, r);
            if *step >= 128 {
                r.error(loc, format!("{cl}: step {step} is out of the 128-step grid"));
            }
        }
        MacroCmd::SetEuclid {
            track,
            pulses,
            length,
            rotation: _,
        } => {
            check_track(*track, r);
            if *length == 0 || *length > 128 {
                r.error(loc, format!("{cl}: length {length} is out of range 1–128"));
            }
            if pulses > length {
                r.warn(
                    loc,
                    format!("{cl}: {pulses} pulses in {length} steps — every step fires"),
                );
            }
        }
        MacroCmd::SetMode { track, mode: _ } => {
            check_track(*track, r);
        }
        MacroCmd::SetTiming {
            track,
            swing,
            groove,
            humanize,
            decay,
        } => {
            check_track(*track, r);
            if !(50..=75).contains(swing) {
                r.error(loc, format!("{cl}: swing {swing} is out of range 50–75"));
            }
            if !(0.0..=30.0).contains(humanize) {
                r.error(loc, format!("{cl}: humanize {humanize} ms is out of range 0–30"));
            }
            if !(-100..=100).contains(decay) {
                r.error(loc, format!("{cl}: decay {decay} is out of range -100–100"));
            }
            if !groove.is_empty() && !theory::groove::LIBRARY.iter().any(|g| g.name == groove) {
                r.error(loc, format!("{cl}: groove '{groove}' is not a known groove"));
            }
        }
    }
}

/// Step values, shared by track steps, slot patterns and SetSteps
/// payloads. Mirrors `step_from_param` (prob/repeats clamps) — except
/// nothing clamps velocity, so a 0 on an active step plays silence.
fn check_steps(
    steps: &[StepParam],
    what: &str,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
) {
    if steps.len() > 128 {
        r.error(
            loc,
            format!("{what} has {} steps — the grid holds 128", steps.len()),
        );
    }
    for (i, s) in steps.iter().enumerate() {
        let sl = format!("{what}[{i}]");
        if s.note > 127 {
            r.error(loc, format!("{sl}.note {} is out of MIDI range 0–127", s.note));
        }
        if s.prob > 100 {
            r.error(loc, format!("{sl}.prob {}% is out of range 0–100", s.prob));
        }
        if s.delay_prob > 100 {
            r.error(
                loc,
                format!("{sl}.delay_prob {}% is out of range 0–100", s.delay_prob),
            );
        }
        if s.repeat_prob > 100 {
            r.error(
                loc,
                format!("{sl}.repeat_prob {}% is out of range 0–100", s.repeat_prob),
            );
        }
        if s.repeats == 0 || s.repeats > 8 {
            r.error(loc, format!("{sl}.repeats {} is out of range 1–8", s.repeats));
        }
        if s.delay < 0.0 {
            r.error(loc, format!("{sl}.delay {} must be >= 0", s.delay));
        }
        if s.active && s.velocity == 0 {
            r.warn(loc, format!("{sl} is active with velocity 0 — it plays silence"));
        }
        if let Some(b) = &s.bind {
            check_src_str(&b.source, &format!("{sl}.bind.source"), loc, declared, r);
        }
    }
}

/// Voice knobs all live in 0–1 (voice.rs render clamps).
fn check_voice(
    p: &VoiceParams,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
    pending: &mut Vec<PendingTrackRef>,
) {
    range01(p.shape, "shape", loc, r);
    range01(p.sub, "sub", loc, r);
    range01(p.fm, "fm", loc, r);
    range01(p.level, "level", loc, r);
    range01(p.lpg, "lpg", loc, r);
    for (field, src) in [
        ("shape_src", &p.shape_src),
        ("sub_src", &p.sub_src),
        ("fm_src", &p.fm_src),
        ("level_src", &p.level_src),
        ("amp_src", &p.amp_src),
    ] {
        check_src(src, field, loc, declared, r);
    }
    check_notes_src(&p.notes_src, loc, declared, r, pending);
}

/// Swarm: chord by name (swarm.rs CHORDS), knobs in 0–1.
fn check_swarm(
    p: &SwarmParams,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
    pending: &mut Vec<PendingTrackRef>,
) {
    const CHORDS: [&str; 8] = ["uni", "oct", "5th", "sus4", "min", "maj", "min7", "maj7"];
    if let Some(name) = p.chord.as_deref() {
        if !CHORDS.contains(&name) {
            let hint = suggest_from(name, CHORDS.iter().copied())
                .map(|s| format!(" — did you mean '{s}'?"))
                .unwrap_or_default();
            r.error(
                loc,
                format!("chord '{name}' is not one of {}{hint}", CHORDS.join(", ")),
            );
        }
    }
    range01(p.detune, "detune", loc, r);
    range01(p.cutoff, "cutoff", loc, r);
    range01(p.res, "res", loc, r);
    range01(p.swell, "swell", loc, r);
    range01(p.glide, "glide", loc, r);
    range01(p.level, "level", loc, r);
    for (field, src) in [
        ("detune_src", &p.detune_src),
        ("cutoff_src", &p.cutoff_src),
        ("res_src", &p.res_src),
        ("swell_src", &p.swell_src),
        ("level_src", &p.level_src),
        ("amp_src", &p.amp_src),
    ] {
        check_src(src, field, loc, declared, r);
    }
    check_notes_src(&p.notes_src, loc, declared, r, pending);
}

/// Envelope: up to 6 channels (envelope.rs MAX_CHANNELS); rise/fall/
/// shape/pluck 0–1, attenuverter/offset -1–1.
fn check_envelope(
    p: &EnvelopeParams,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
    pending: &mut Vec<PendingTrackRef>,
) {
    if p.channels.len() > 6 {
        r.error(
            loc,
            format!("{} channels declared — MATHs holds 6", p.channels.len()),
        );
    }
    for (i, ch) in p.channels.iter().enumerate() {
        let chl = format!("channels[{i}]");
        // rise/fall/shape are deliberately unchecked: the load path takes
        // them raw (envelope.rs apply_params) and real saves carry values
        // past the knob range (`:set rise 2s` style time entry).
        if !(0.0..=1.0).contains(&ch.pluck) {
            r.warn(loc, format!("{chl}.pluck {} is outside the knob range 0–1", ch.pluck));
        }
        if !(-1.0..=1.0).contains(&ch.attenuverter) {
            r.warn(
                loc,
                format!(
                    "{chl}.attenuverter {} is outside the knob range -1–1",
                    ch.attenuverter
                ),
            );
        }
        if !(-1.0..=1.0).contains(&ch.offset) {
            r.warn(
                loc,
                format!("{chl}.offset {} is outside the knob range -1–1", ch.offset),
            );
        }
        for (field, src) in [
            ("signal_src", &ch.signal_src),
            ("rise_src", &ch.rise_src),
            ("fall_src", &ch.fall_src),
            ("shape_src", &ch.shape_src),
            ("atten_src", &ch.atten_src),
        ] {
            check_src(src, &format!("{chl}.{field}"), loc, declared, r);
        }
        // trigger_src has a sentinel: absent = any note, "off" = never
        // (envelope.rs Trigger::from_param).
        if ch.trigger_src.as_deref() != Some("off") {
            if let Some(addr) =
                check_src(&ch.trigger_src, &format!("{chl}.trigger_src"), loc, declared, r)
            {
                pending.push(PendingTrackRef {
                    addr,
                    location: format!("{loc}: {chl}.trigger_src"),
                    wants_notes: true,
                });
            }
        }
    }
}

/// Mixer strips (mixer.rs `set` clamps): level/drive/sends 0–1, pan
/// -1–1, EQ gains -15–15 dB, eq_freq 0–1, width 0–2.
fn check_mixer(
    p: &MixerParams,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
) {
    range01(p.master, "master", loc, r);
    for (field, v) in [
        ("master_drive", p.master_drive),
        ("master_send_a", p.master_send_a),
        ("master_send_b", p.master_send_b),
        ("master_eq_freq", p.master_eq_freq),
    ] {
        if !(0.0..=1.0).contains(&v) {
            r.error(loc, format!("{field} {v} is out of range 0–1"));
        }
    }
    for (field, v) in [
        ("master_eq_lo", p.master_eq_lo),
        ("master_eq_mid", p.master_eq_mid),
        ("master_eq_hi", p.master_eq_hi),
    ] {
        if !(-15.0..=15.0).contains(&v) {
            r.error(loc, format!("{field} {v} dB is out of range -15–15"));
        }
    }
    if !(0.0..=2.0).contains(&p.master_width) {
        r.error(
            loc,
            format!("master_width {} is out of range 0–2", p.master_width),
        );
    }
    for (i, t) in p.tracks.iter().enumerate() {
        let tl = format!("tracks[{i}]");
        for (field, v) in [
            ("level", t.level),
            ("drive", t.drive),
            ("send_a", t.send_a),
            ("send_b", t.send_b),
            ("eq_freq", t.eq_freq),
        ] {
            if !(0.0..=1.0).contains(&v) {
                r.error(loc, format!("{tl}.{field} {v} is out of range 0–1"));
            }
        }
        if !(-1.0..=1.0).contains(&t.pan) {
            r.error(loc, format!("{tl}.pan {} is out of range -1–1", t.pan));
        }
        for (field, v) in [("eq_lo", t.eq_lo), ("eq_mid", t.eq_mid), ("eq_hi", t.eq_hi)] {
            if !(-15.0..=15.0).contains(&v) {
                r.error(loc, format!("{tl}.{field} {v} dB is out of range -15–15"));
            }
        }
        for (field, src) in [
            ("level_src", &t.level_src),
            ("pan_src", &t.pan_src),
            ("drive_src", &t.drive_src),
            ("lo_src", &t.lo_src),
            ("mid_src", &t.mid_src),
            ("freq_src", &t.freq_src),
            ("hi_src", &t.hi_src),
            ("send_a_src", &t.send_a_src),
            ("send_b_src", &t.send_b_src),
        ] {
            check_src(src, &format!("{tl}.{field}"), loc, declared, r);
        }
    }
}

/// Delay (delay.rs param clamps + dsp.rs TIME_MIN/TIME_MAX): time
/// 0.001–0.250 s, mixes 0–1, taps 1–8, phase glyph "+" / "·" / "−".
fn check_delay(
    p: &DelayParams,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
) {
    if let Some(t) = p.time {
        if !(0.001..=0.250).contains(&t) {
            r.error(
                loc,
                format!("time {t} s is out of range 0.001–0.250 (per stage)"),
            );
        }
    }
    range01(p.regen, "regen", loc, r);
    range01(p.shim, "shim", loc, r);
    range01(p.wash, "wash", loc, r);
    range01(p.dry, "dry", loc, r);
    if let Some(taps) = p.taps {
        if taps == 0 || taps > 8 {
            r.error(loc, format!("taps {taps} is out of range 1–8"));
        }
    }
    if p.tap.len() > 8 {
        r.error(loc, format!("{} tap entries — the 288 has 8", p.tap.len()));
    }
    for (i, t) in p.tap.iter().enumerate() {
        let tl = format!("tap[{i}]");
        if !(0.0..=1.0).contains(&t.level) {
            r.error(loc, format!("{tl}.level {} is out of range 0–1", t.level));
        }
        if !(-1.0..=1.0).contains(&t.pan) {
            r.error(loc, format!("{tl}.pan {} is out of range -1–1", t.pan));
        }
        if !["+", "·", "−"].contains(&t.phase.as_str()) {
            r.error(
                loc,
                format!(
                    "{tl}.phase '{}' must be \"+\" (normal), \"·\" (off) or \"−\" (inverted)",
                    t.phase
                ),
            );
        }
        check_src(&t.pan_src, &format!("{tl}.pan_src"), loc, declared, r);
        check_src(&t.level_src, &format!("{tl}.level_src"), loc, declared, r);
    }
    check_input(&p.input, "input", loc, declared, r);
    for (field, src) in [
        ("time_src", &p.time_src),
        ("regen_src", &p.regen_src),
        ("shim_src", &p.shim_src),
        ("wash_src", &p.wash_src),
        ("dry_src", &p.dry_src),
    ] {
        check_src(src, field, loc, declared, r);
    }
}

/// Filterbank (filterbank.rs param clamps): everything 0–1, 16 bands,
/// xfer by name (XFERS).
fn check_filterbank(
    p: &FilterbankParams,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
) {
    const XFERS: [&str; 4] = ["off", "o→e", "e→o", "both"];
    for (field, bank) in [("bank_a", &p.bank_a), ("bank_b", &p.bank_b)] {
        if bank.len() > 16 {
            r.error(loc, format!("{field} has {} faders — 16 bands", bank.len()));
        }
        for (i, v) in bank.iter().enumerate() {
            if !(0.0..=1.0).contains(v) {
                r.error(loc, format!("{field}[{i}] {v} is out of range 0–1"));
            }
        }
    }
    range01(p.morph, "morph", loc, r);
    range01(p.wcent, "wcent", loc, r);
    range01(p.wwidth, "wwidth", loc, r);
    range01(p.spread, "spread", loc, r);
    range01(p.split, "split", loc, r);
    range01(p.dry, "dry", loc, r);
    range01(p.decay, "decay", loc, r);
    if let Some(x) = p.xfer.as_deref() {
        if !XFERS.contains(&x) {
            r.error(
                loc,
                format!("xfer '{x}' must be one of: {}", XFERS.join(", ")),
            );
        }
    }
    if p.band_srcs.len() > 16 {
        r.error(
            loc,
            format!("band_srcs has {} entries — 16 bands", p.band_srcs.len()),
        );
    }
    for (i, src) in p.band_srcs.iter().enumerate() {
        if !src.is_empty() {
            check_src_str(src, &format!("band_srcs[{i}]"), loc, declared, r);
        }
    }
    check_input(&p.input, "input", loc, declared, r);
    for (field, src) in [
        ("morph_src", &p.morph_src),
        ("freeze_src", &p.freeze_src),
        ("wcent_src", &p.wcent_src),
        ("wwidth_src", &p.wwidth_src),
        ("spread_src", &p.spread_src),
        ("split_src", &p.split_src),
        ("dry_src", &p.dry_src),
        ("decay_src", &p.decay_src),
    ] {
        check_src(src, field, loc, declared, r);
    }
}

/// Tape (tape.rs clamps): speed 0.25–2.0, six tracks, fader 0–1,
/// pan -1–1; `input` None = the mix (print bus).
fn check_tape(p: &TapeParams, loc: &str, declared: &BTreeSet<(String, usize)>, r: &mut Report) {
    if let Some(speed) = p.speed {
        if !(0.25..=2.0).contains(&speed) {
            r.error(loc, format!("speed {speed} is out of range 0.25–2.0"));
        }
    }
    if let (Some(true), Some(li), Some(lo_)) = (p.loop_on, p.loop_in, p.loop_out) {
        if lo_ <= li {
            r.warn(
                loc,
                format!("loop_out {lo_} is not past loop_in {li} — the loop is empty"),
            );
        }
    }
    if p.tracks.len() > 6 {
        r.error(
            loc,
            format!("{} tracks declared — the deck has 6", p.tracks.len()),
        );
    }
    check_src(&p.speed_src, "speed_src", loc, declared, r);
    for (i, t) in p.tracks.iter().enumerate() {
        let tl = format!("tracks[{i}]");
        if !(0.0..=1.0).contains(&t.fader) {
            r.error(loc, format!("{tl}.fader {} is out of range 0–1", t.fader));
        }
        if !(-1.0..=1.0).contains(&t.pan) {
            r.error(loc, format!("{tl}.pan {} is out of range -1–1", t.pan));
        }
        check_input(&t.input, &format!("{tl}.input"), loc, declared, r);
        check_src(&t.fader_src, &format!("{tl}.fader_src"), loc, declared, r);
        check_src(&t.pan_src, &format!("{tl}.pan_src"), loc, declared, r);
    }
}

/// Template (template.rs SHAPES): shape by name.
fn check_template(
    p: &TemplateParams,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
) {
    const SHAPES: [&str; 4] = ["sine", "tri", "saw", "sqr"];
    if let Some(name) = p.shape.as_deref() {
        if !SHAPES.contains(&name) {
            r.error(
                loc,
                format!("shape '{name}' must be one of: {}", SHAPES.join(", ")),
            );
        }
    }
    for (field, src) in [
        ("rate_src", &p.rate_src),
        ("depth_src", &p.depth_src),
        ("pitch_src", &p.pitch_src),
        ("level_src", &p.level_src),
    ] {
        check_src(src, field, loc, declared, r);
    }
}

// ── shared field checks ──────────────────────────────────────────────────────

fn range01(v: Option<f32>, field: &str, loc: &str, r: &mut Report) {
    if let Some(v) = v {
        if !(0.0..=1.0).contains(&v) {
            r.error(loc, format!("{field} {v} is out of range 0–1"));
        }
    }
}

/// Modules that publish modulation outputs ([`output_labels`]).
const SOURCE_MODULES: [&str; 6] = [
    "sequencer",
    "envelope",
    "template",
    "swarm",
    "delay",
    "filterbank",
];

/// Modules that publish audio rings an fx/tape input can claim.
const AUDIO_MODULES: [&str; 6] = ["voice", "swarm", "tone", "template", "delay", "filterbank"];

/// Canonical module names, for misspelled-pane suggestions.
const MODULE_NAMES: [&str; 13] = [
    "sequencer",
    "voice",
    "mixer",
    "scope",
    "envelope",
    "badge",
    "tone",
    "template",
    "delay",
    "filterbank",
    "tape",
    "swarm",
    "conductor",
];

/// An optional `*_src` field: grammar, known output, declared instance.
/// Returns the parsed address so callers can queue cross-module checks.
fn check_src(
    src: &Option<String>,
    field: &str,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
) -> Option<SourceAddr> {
    src.as_deref()
        .and_then(|s| check_src_str(s, field, loc, declared, r))
}

fn check_src_str(
    src: &str,
    field: &str,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
) -> Option<SourceAddr> {
    let Some(addr) = SourceAddr::parse(src) else {
        r.error(
            loc,
            format!(
                "{field} \"{src}\" is not a source address — the form is \
                 module/instance/output, e.g. \"envelope/0/ch1\" or \"sequencer/0/t2\""
            ),
        );
        return None;
    };
    let labels = output_labels(&addr.module);
    if labels.is_empty() {
        r.error(
            loc,
            format!(
                "{field} \"{src}\": {} has no modulation outputs (sources: {})",
                addr.module,
                SOURCE_MODULES.join(", ")
            ),
        );
        return None;
    }
    if !labels.contains(&addr.output.as_str()) {
        let hint = suggest_from(&addr.output, labels.iter().copied())
            .map(|s| format!(" — did you mean '{s}'?"))
            .unwrap_or_default();
        r.error(
            loc,
            format!(
                "{field} \"{src}\": {} has no output '{}' (outputs: {}){hint}",
                addr.module,
                addr.output,
                labels.join(", ")
            ),
        );
        return None;
    }
    if !declared.contains(&(addr.module.clone(), addr.instance)) {
        // Legal but dead: bindings resolve through the live manifest, so
        // this only fires once the module exists (`los add`). In a song
        // file it is almost always a typo'd instance number.
        r.warn(
            loc,
            format!(
                "{field} \"{src}\": no {} {} pane in this file — the binding stays \
                 dead until that module is added",
                addr.module, addr.instance
            ),
        );
    }
    Some(addr)
}

/// `notes_src`: only sequencer tracks emit note events
/// (routing::note_source_track is None for everything else).
fn check_notes_src(
    src: &Option<String>,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
    pending: &mut Vec<PendingTrackRef>,
) {
    let Some(s) = src.as_deref() else { return };
    let Some(addr) = check_src_str(s, "notes_src", loc, declared, r) else {
        return;
    };
    if addr.module != "sequencer" {
        r.error(
            loc,
            format!(
                "notes_src \"{s}\" must name a sequencer track (sequencer/N/tM) — \
                 only sequencer tracks emit note events"
            ),
        );
        return;
    }
    pending.push(PendingTrackRef {
        addr,
        location: format!("{loc}: notes_src"),
        wants_notes: true,
    });
}

/// An audio `input` field: 2-segment `module/instance`, either a mixer
/// virtual (send/0, send/1, mix/0 — mixer.rs registers these) or a
/// declared audio-producing module.
fn check_input(
    input: &Option<String>,
    field: &str,
    loc: &str,
    declared: &BTreeSet<(String, usize)>,
    r: &mut Report,
) {
    let Some(s) = input.as_deref() else { return };
    let mut parts = s.split('/');
    let (module, instance) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(i), None) if !m.is_empty() => match i.parse::<usize>() {
            Ok(i) => (m, i),
            Err(_) => {
                r.error(
                    loc,
                    format!(
                        "{field} \"{s}\" is not an audio input — the form is \
                         module/instance, e.g. \"voice/0\" or \"send/0\""
                    ),
                );
                return;
            }
        },
        _ => {
            r.error(
                loc,
                format!(
                    "{field} \"{s}\" is not an audio input — the form is \
                     module/instance, e.g. \"voice/0\" or \"send/0\""
                ),
            );
            return;
        }
    };
    let virtuals = [("send", 0), ("send", 1), ("mix", 0)];
    if virtuals.contains(&(module, instance)) {
        return;
    }
    if !AUDIO_MODULES.contains(&module) {
        r.error(
            loc,
            format!(
                "{field} \"{s}\": {module} makes no audio (audio sources: {}, \
                 plus send/0, send/1, mix/0)",
                AUDIO_MODULES.join(", ")
            ),
        );
        return;
    }
    if !declared.contains(&(module.to_string(), instance)) {
        r.warn(
            loc,
            format!(
                "{field} \"{s}\": no {module} {instance} pane in this file — \
                 the input stays silent until that module is added"
            ),
        );
    }
}

// ── typed decode with unknown-key detection ─────────────────────────────────

/// Deserialize through `serde_ignored` so misspelled keys — which serde
/// would silently drop, by design, for old-save compatibility — become
/// named errors with a did-you-mean.
fn decode<T: serde::de::DeserializeOwned>(
    value: &toml::Value,
    loc: &str,
    r: &mut Report,
) -> Option<T> {
    let mut unknown: Vec<String> = Vec::new();
    let result: Result<T, toml::de::Error> =
        serde_ignored::deserialize(value.clone(), |path: serde_ignored::Path<'_>| {
            unknown.push(path.to_string());
        });
    for path in &unknown {
        let key = path.rsplit('.').next().unwrap_or(path);
        let hint = if key.chars().all(|c| c.is_ascii_digit()) {
            String::new()
        } else {
            suggest_from(key, KNOWN_KEYS.iter().copied())
                .map(|s| format!(" — did you mean '{s}'?"))
                .unwrap_or_default()
        };
        r.error(
            loc,
            format!("unknown key '{path}'{hint} (unknown keys are silently ignored at load)"),
        );
    }
    match result {
        Ok(t) => Some(t),
        Err(e) => {
            r.error(loc, format!("params do not decode: {e}"));
            None
        }
    }
}

/// Every field name in the state schema (state.rs), flat — candidates
/// for unknown-key suggestions. Drift here only weakens the hints, never
/// the detection.
const KNOWN_KEYS: &[&str] = &[
    "active", "active_pane", "active_slot", "active_window", "amount", "amp_src", "and_enabled",
    "arg", "armed", "atten_src", "attenuverter", "auto", "band_srcs", "bank_a", "bank_b", "bind",
    "bpm", "by", "channel", "channels", "chord", "cmds", "created", "cutoff", "cutoff_src",
    "cycle", "decay", "decay_src", "delay", "delay_prob", "delay_unit", "depth", "depth_src",
    "detune", "detune_src", "drive", "drive_src", "dry", "dry_src", "eq_freq", "eq_hi", "eq_lo",
    "eq_mid", "euclidean_length", "euclidean_pulses", "euclidean_rotation", "fader", "fader_src",
    "fall", "fall_src", "fm", "fm_src", "format", "freeze", "freeze_src", "freq", "freq_src",
    "gain", "gate", "gate_mode", "glide", "groove", "hi_src", "humanize", "id", "input",
    "instance", "kind", "lane", "lane_len", "layout", "length", "level", "level_src", "lo_src",
    "logic_outputs", "loop_in", "loop_mode", "loop_on", "loop_out", "lpg", "macros", "master",
    "master_drive", "master_eq_freq", "master_eq_hi", "master_eq_lo", "master_eq_mid",
    "master_send_a", "master_send_b", "master_width", "meta", "mid_src", "mod_value",
    "modbus_channel", "mode", "module", "monitor", "morph", "morph_src", "muted", "mute", "name",
    "note", "notes_src", "offset", "or_enabled", "output", "pan", "pan_src", "panes", "patch",
    "patch_inline", "phase", "pitch", "pitch_src", "playing", "pluck", "prob", "pulses", "quant",
    "rate", "rate_src", "ratchet_decay", "regen", "regen_src", "repeat_prob", "repeats", "res",
    "res_src", "reversed", "rise", "rise_src", "root", "rotation", "scale", "scale_cents",
    "scale_period", "send_a", "send_a_src", "send_b", "send_b_src", "session_name", "shape",
    "shape_src", "shim", "shim_src", "signal_src", "slot", "slots", "solo", "source", "speed",
    "speed_src", "split", "split_src", "spread", "spread_src", "start", "step", "steps", "sub",
    "sub_src", "sum_enabled", "swell", "swell_src", "swing", "tap", "taps", "time", "time_src",
    "tmux", "track", "tracks", "trigger_level", "trigger_src", "unipolar", "velocity", "wash",
    "wash_src", "wcent", "wcent_src", "windows", "window_size",
];

// ── did-you-mean ─────────────────────────────────────────────────────────────

/// Closest candidate within edit distance 2 (and not an exact match).
fn suggest_from<'a>(key: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    candidates
        .into_iter()
        .map(|c| (edit_distance(key, c), c))
        .filter(|(d, _)| *d > 0 && *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// Plain Levenshtein — keys are short, no need for cleverness.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let best = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
            cur.push(best);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small valid song: sequencer (1 track, 2 macros, 4-bar lane),
    /// voice bound to it, envelope, mixer. Tests mutate this by string
    /// replacement so each one shows exactly the mistake it checks.
    fn base() -> String {
        r#"
[meta]
name = "t"
format = 2

[[windows]]
name = "modules"

[[windows.panes]]
module = "sequencer"
instance = 0

[windows.panes.patch_inline]
bpm = 96.0
lane = ["a", "", "b", ""]
lane_len = 4

[[windows.panes.patch_inline.tracks]]
length = 16
swing = 50
steps = [{ active = true, note = 60, velocity = 100 }]

[[windows.panes.patch_inline.tracks.slots]]
slot = 1
steps = [{ active = true, note = 62, velocity = 90 }]

[[windows.panes.patch_inline.macros]]
id = "a"
cmds = [{ switch_pattern = { track = 0, slot = 0 } }]

[[windows.panes.patch_inline.macros]]
id = "b"
cmds = [{ set_bpm = { bpm = 120.0 } }]

[[windows.panes]]
module = "voice"
instance = 0

[windows.panes.patch_inline]
shape = 0.5
amp_src = "envelope/0/ch1"
notes_src = "sequencer/0/t1"

[[windows.panes]]
module = "envelope"
instance = 0

[[windows.panes]]
module = "mixer"
instance = 0
"#
        .to_string()
    }

    fn has_error(r: &Report, needle: &str) -> bool {
        r.errors.iter().any(|i| i.to_string().contains(needle))
    }

    fn has_warning(r: &Report, needle: &str) -> bool {
        r.warnings.iter().any(|i| i.to_string().contains(needle))
    }

    #[test]
    fn clean_file_passes() {
        let r = validate_str(&base());
        assert!(r.errors.is_empty(), "unexpected errors: {:?}", r.errors);
        assert!(r.warnings.is_empty(), "unexpected warnings: {:?}", r.warnings);
    }

    #[test]
    fn parse_error_is_reported() {
        let r = validate_str("this is not = = toml");
        assert!(!r.is_clean());
        assert!(has_error(&r, "TOML parse error"));
    }

    #[test]
    fn v1_format_is_refused_like_load() {
        let r = validate_str("[meta]\nname = \"old\"\n");
        assert!(has_error(&r, "clean break"));
    }

    #[test]
    fn misspelled_key_gets_a_suggestion() {
        let song = base().replace("shape = 0.5", "shap = 0.5");
        let r = validate_str(&song);
        assert!(has_error(&r, "unknown key 'shap'"), "{:?}", r.errors);
        assert!(has_error(&r, "did you mean 'shape'"), "{:?}", r.errors);
    }

    #[test]
    fn unknown_module_gets_a_suggestion() {
        let song = base().replace("module = \"voice\"", "module = \"vocie\"");
        let r = validate_str(&song);
        assert!(has_error(&r, "unknown module 'vocie'"), "{:?}", r.errors);
        assert!(has_error(&r, "did you mean 'voice'"), "{:?}", r.errors);
    }

    #[test]
    fn duplicate_pane_is_an_error() {
        let song = base().replace(
            "[[windows.panes]]\nmodule = \"envelope\"",
            "[[windows.panes]]\nmodule = \"mixer\"\ninstance = 0\n\n[[windows.panes]]\nmodule = \"envelope\"",
        );
        let r = validate_str(&song);
        assert!(has_error(&r, "duplicate pane: mixer 0"), "{:?}", r.errors);
    }

    #[test]
    fn missing_mixer_is_a_warning() {
        let song = base().replace("[[windows.panes]]\nmodule = \"mixer\"\ninstance = 0\n", "");
        let r = validate_str(&song);
        assert!(has_warning(&r, "no mixer pane"), "{:?}", r.warnings);
    }

    #[test]
    fn address_grammar_and_resolution() {
        // bad grammar
        let r = validate_str(&base().replace("\"envelope/0/ch1\"", "\"envelope-ch1\""));
        assert!(has_error(&r, "not a source address"), "{:?}", r.errors);
        // unknown output, with suggestion
        let r = validate_str(&base().replace("\"envelope/0/ch1\"", "\"envelope/0/ch7\""));
        assert!(has_error(&r, "no output 'ch7'"), "{:?}", r.errors);
        // undeclared instance: legal live (resolves once the module is
        // added), so a warning — not a load-blocking error
        let r = validate_str(&base().replace("\"envelope/0/ch1\"", "\"envelope/3/ch1\""));
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(has_warning(&r, "no envelope 3 pane"), "{:?}", r.warnings);
        // module with no modulation outputs
        let r = validate_str(&base().replace("\"envelope/0/ch1\"", "\"mixer/0/ch1\""));
        assert!(has_error(&r, "no modulation outputs"), "{:?}", r.errors);
    }

    #[test]
    fn notes_src_must_be_a_sequencer_track() {
        let song = base().replace("notes_src = \"sequencer/0/t1\"", "notes_src = \"envelope/0/ch1\"");
        let r = validate_str(&song);
        assert!(
            has_error(&r, "only sequencer tracks emit note events"),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn notes_src_past_declared_tracks_warns() {
        let song = base().replace("notes_src = \"sequencer/0/t1\"", "notes_src = \"sequencer/0/t5\"");
        let r = validate_str(&song);
        assert!(has_warning(&r, "will never fire"), "{:?}", r.warnings);
    }

    #[test]
    fn notes_src_on_modulation_track_warns() {
        let song = base().replace("length = 16\nswing = 50", "length = 16\nswing = 50\nmode = \"modulation\"");
        let r = validate_str(&song);
        assert!(has_warning(&r, "modulation-mode track"), "{:?}", r.warnings);
    }

    #[test]
    fn lane_must_fire_defined_macros() {
        let song = base().replace("lane = [\"a\", \"\", \"b\", \"\"]", "lane = [\"a\", \"\", \"q\", \"\"]");
        let r = validate_str(&song);
        assert!(
            has_error(&r, "fires macro 'q' but no macro 'q' is defined"),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn empty_lane_opening_bar_warns() {
        let song = base().replace("lane = [\"a\", \"\", \"b\", \"\"]", "lane = [\"\", \"a\", \"b\", \"\"]");
        let r = validate_str(&song);
        assert!(has_warning(&r, "lane[0] is empty"), "{:?}", r.warnings);
    }

    #[test]
    fn switch_pattern_bounds_and_empty_slot() {
        // out-of-range slot
        let song = base().replace("{ track = 0, slot = 0 }", "{ track = 0, slot = 9 }");
        let r = validate_str(&song);
        assert!(has_error(&r, "slot 9 is out of range"), "{:?}", r.errors);
        // out-of-range track
        let song = base().replace("{ track = 0, slot = 0 }", "{ track = 4, slot = 0 }");
        let r = validate_str(&song);
        assert!(has_error(&r, "track 4 is out of range"), "{:?}", r.errors);
        // empty (but in-range) slot is a warning
        let song = base().replace("{ track = 0, slot = 0 }", "{ track = 0, slot = 3 }");
        let r = validate_str(&song);
        assert!(has_warning(&r, "empty slot 'd'"), "{:?}", r.warnings);
    }

    #[test]
    fn out_of_range_values_are_errors() {
        let r = validate_str(&base().replace("bpm = 96.0", "bpm = 400.0"));
        assert!(has_error(&r, "bpm 400 is out of range 20–300"), "{:?}", r.errors);
        let r = validate_str(&base().replace("swing = 50", "swing = 90"));
        assert!(has_error(&r, "swing 90 is out of range 50–75"), "{:?}", r.errors);
        let r = validate_str(&base().replace("shape = 0.5", "shape = 1.5"));
        assert!(has_error(&r, "shape 1.5 is out of range 0–1"), "{:?}", r.errors);
        let r = validate_str(&base().replace(
            "{ active = true, note = 60, velocity = 100 }",
            "{ active = true, note = 60, velocity = 100, repeats = 0 }",
        ));
        assert!(has_error(&r, "repeats 0 is out of range 1–8"), "{:?}", r.errors);
    }

    #[test]
    fn silent_active_step_warns() {
        let song = base().replace(
            "{ active = true, note = 60, velocity = 100 }",
            "{ active = true, note = 60, velocity = 0 }",
        );
        let r = validate_str(&song);
        assert!(has_warning(&r, "plays silence"), "{:?}", r.warnings);
    }

    #[test]
    fn scale_and_groove_names_are_checked() {
        let song = base().replace("swing = 50", "swing = 50\nscale = \"mynor\"");
        let r = validate_str(&song);
        assert!(has_error(&r, "'mynor' is not a known scale"), "{:?}", r.errors);
        let song = base().replace("swing = 50", "swing = 50\ngroove = \"lilty\"");
        let r = validate_str(&song);
        assert!(has_error(&r, "'lilty' is not a known groove"), "{:?}", r.errors);
        assert!(has_error(&r, "did you mean 'lilt'"), "{:?}", r.errors);
    }

    #[test]
    fn audio_inputs_are_checked() {
        // a delay pane whose input names a module that makes no audio
        let song = base()
            + "\n[[windows.panes]]\nmodule = \"delay\"\ninstance = 0\n\n[windows.panes.patch_inline]\ninput = \"sequencer/0\"\n";
        let r = validate_str(&song);
        assert!(has_error(&r, "makes no audio"), "{:?}", r.errors);
        // mixer virtuals are always fine
        let song = base()
            + "\n[[windows.panes]]\nmodule = \"delay\"\ninstance = 0\n\n[windows.panes.patch_inline]\ninput = \"send/0\"\n";
        let r = validate_str(&song);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        // undeclared audio module is a warning (resolves after `los add`)
        let song = base()
            + "\n[[windows.panes]]\nmodule = \"delay\"\ninstance = 0\n\n[windows.panes.patch_inline]\ninput = \"voice/2\"\n";
        let r = validate_str(&song);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(has_warning(&r, "no voice 2 pane"), "{:?}", r.warnings);
    }

    #[test]
    fn envelope_trigger_off_sentinel_is_valid() {
        let song = base().replace(
            "[[windows.panes]]\nmodule = \"envelope\"\ninstance = 0",
            "[[windows.panes]]\nmodule = \"envelope\"\ninstance = 0\n\n[windows.panes.patch_inline]\nchannels = [{ rise = 0.5, fall = 1.6, shape = 0.5, loop_mode = false, attenuverter = 1.0, trigger_src = \"off\" }]",
        );
        let r = validate_str(&song);
        // "off" is the Trigger::Off sentinel; rise/fall past the knob
        // range load raw — neither may block a real save from loading
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn named_patch_reference_warns() {
        let song = base().replace(
            "module = \"envelope\"\ninstance = 0",
            "module = \"envelope\"\ninstance = 0\npatch = \"my-envelope\"",
        );
        let r = validate_str(&song);
        assert!(has_warning(&r, "load only applies `patch_inline`"), "{:?}", r.warnings);
    }

    #[test]
    fn all_problems_are_collected_not_just_the_first() {
        let song = base()
            .replace("bpm = 96.0", "bpm = 400.0")
            .replace("swing = 50", "swing = 90")
            .replace("\"envelope/0/ch1\"", "\"envelope/0/ch7\"");
        let r = validate_str(&song);
        assert!(r.errors.len() >= 3, "expected 3+ errors, got {:?}", r.errors);
    }

    #[test]
    fn edit_distance_sanity() {
        assert_eq!(edit_distance("velocity", "velocity"), 0);
        assert_eq!(edit_distance("vellocity", "velocity"), 1);
        assert_eq!(edit_distance("shap", "shape"), 1);
        assert!(edit_distance("totally", "different") > 2);
    }
}
