// Los — a modular groovebox that lives in your terminal
// Copyright (C) 2026 doo-nothing / AU Supply
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version. See LICENSE.

//! Walking a song's macro lane into absolute time.
//!
//! The sequencer's macro lane is one slot per bar; a bar is 16 steps of
//! 16th notes, so `240 / bpm` seconds (sequencer.rs: `samples_per_step =
//! 60/bpm*rate/4`, `STEPS_PER_BAR = 16`). A macro fires *at* its bar
//! line, so a [`crate::state::MacroCmd::SetBpm`] inside it owns that bar
//! and everything after — that's what makes the walk worth doing offline:
//! `los audit --song` needs to know where each bar of a render starts
//! without booting a session.

use crate::state::{MacroCmd, SequencerParams, SessionState};

/// One bar of the macro lane walked into absolute time.
#[derive(Debug, Clone, PartialEq)]
pub struct BarInfo {
    /// Bar index, 0-based.
    pub index: usize,
    /// Where the bar line falls, seconds from the start of the song.
    pub start_secs: f64,
    /// The bpm that owns this bar (after any macro at its bar line).
    pub bpm: f64,
    /// The lane letter fired at this bar line, if any.
    pub macro_fired: Option<char>,
    /// Section number: increments at each bar whose lane slot is
    /// non-empty (section 0 = bars before any macro fires).
    pub section: usize,
}

/// The whole lane, bar by bar, in absolute time.
#[derive(Debug, Clone, PartialEq)]
pub struct SongTimeline {
    pub bars: Vec<BarInfo>,
    /// Total song length in seconds (end of the last bar).
    pub total_secs: f64,
}

/// Walk a sequencer's macro lane into a [`SongTimeline`].
///
/// Mirrors the sequencer's runtime semantics: `lane_len` bars (default =
/// `lane.len().max(1)`, clamped 1–128), initial bpm `seq.bpm` (default
/// 120), and a lane letter applies its macro's `SetBpm` (if any) before
/// the bar it sits on plays.
#[must_use]
pub fn timeline(seq: &SequencerParams) -> SongTimeline {
    let lane_len = seq
        .lane_len
        .unwrap_or_else(|| seq.lane.len().max(1))
        .clamp(1, 128);
    let mut bpm = seq.bpm.unwrap_or(120.0);
    let mut start_secs = 0.0;
    let mut section = 0usize;
    let bars: Vec<BarInfo> = (0..lane_len)
        .map(|index| {
            let slot = seq.lane.get(index).map(String::as_str).unwrap_or("");
            let macro_fired = slot.chars().next();
            if let Some(c) = macro_fired {
                section += 1;
                if let Some(new_bpm) = macro_bpm(seq, c) {
                    bpm = new_bpm;
                }
            }
            let bar = BarInfo {
                index,
                start_secs,
                bpm,
                macro_fired,
                section,
            };
            start_secs += 240.0 / bpm;
            bar
        })
        .collect();
    SongTimeline {
        bars,
        total_secs: start_secs,
    }
}

/// The bpm a macro letter sets, if its command list contains a `SetBpm`
/// (the last one wins, matching replay order).
fn macro_bpm(seq: &SequencerParams, letter: char) -> Option<f64> {
    let m = seq.macros.iter().find(|m| m.id == letter.to_string())?;
    m.cmds.iter().rev().find_map(|cmd| match cmd {
        MacroCmd::SetBpm { bpm } => Some(*bpm),
        MacroCmd::SwitchPattern { .. }
        | MacroCmd::SetMute { .. }
        | MacroCmd::SetCycle { .. }
        | MacroCmd::TransposeTrack { .. }
        | MacroCmd::RotateTrack { .. }
        | MacroCmd::SetScale { .. }
        | MacroCmd::Fill { .. }
        | MacroCmd::SetSteps { .. }
        | MacroCmd::SetActive { .. }
        | MacroCmd::SetEuclid { .. }
        | MacroCmd::SetMode { .. }
        | MacroCmd::SetTiming { .. } => None,
    })
}

/// Find sequencer 0's inline patch in a session file and decode it.
///
/// Returns `None` when the file declares no sequencer 0 pane or the pane
/// carries no `patch_inline` (callers should have run
/// [`crate::validate::validate_file`] first, which reports decode
/// problems properly).
#[must_use]
pub fn sequencer_params(st: &SessionState) -> Option<SequencerParams> {
    st.windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .find(|p| {
            crate::conductor::canonical_module(&p.module) == Some("sequencer") && p.instance == 0
        })
        .and_then(|p| p.patch_inline.clone())
        .and_then(|v| v.try_into::<SequencerParams>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{MacroParam, Quant};

    fn set_bpm_macro(id: &str, bpm: f64) -> MacroParam {
        MacroParam {
            id: id.into(),
            quant: Quant::Bar,
            cmds: vec![MacroCmd::SetBpm { bpm }],
        }
    }

    #[test]
    fn timeline_walks_bpm_changes() {
        let seq = SequencerParams {
            bpm: Some(120.0),
            macros: vec![set_bpm_macro("a", 120.0), set_bpm_macro("b", 60.0)],
            lane: vec!["a".into(), "".into(), "b".into(), "".into()],
            lane_len: Some(4),
            ..Default::default()
        };
        let tl = timeline(&seq);
        assert_eq!(tl.bars.len(), 4);
        let starts: Vec<f64> = tl.bars.iter().map(|b| b.start_secs).collect();
        assert_eq!(starts, vec![0.0, 2.0, 4.0, 8.0]);
        assert_eq!(tl.total_secs, 12.0);
        // sections increment at bars 0 and 2 (the non-empty lane slots)
        let sections: Vec<usize> = tl.bars.iter().map(|b| b.section).collect();
        assert_eq!(sections, vec![1, 1, 2, 2]);
        assert_eq!(tl.bars[0].macro_fired, Some('a'));
        assert_eq!(tl.bars[1].macro_fired, None);
        assert_eq!(tl.bars[2].macro_fired, Some('b'));
        assert_eq!(tl.bars[2].bpm, 60.0);
    }

    #[test]
    fn timeline_empty_lane_uses_initial_bpm() {
        let seq = SequencerParams {
            bpm: Some(120.0),
            lane: vec![],
            lane_len: Some(3),
            ..Default::default()
        };
        let tl = timeline(&seq);
        assert_eq!(tl.bars.len(), 3);
        assert!(tl.bars.iter().all(|b| b.bpm == 120.0));
        assert!(tl.bars.iter().all(|b| b.section == 0));
        assert_eq!(tl.total_secs, 6.0);
    }

    #[test]
    fn timeline_defaults_when_unset() {
        // no lane, no lane_len, no bpm: one bar at 120
        let seq = SequencerParams::default();
        let tl = timeline(&seq);
        assert_eq!(tl.bars.len(), 1);
        assert_eq!(tl.bars[0].bpm, 120.0);
        assert_eq!(tl.total_secs, 2.0);
    }

    #[test]
    fn timeline_section_zero_before_first_macro() {
        let seq = SequencerParams {
            bpm: Some(120.0),
            macros: vec![set_bpm_macro("a", 60.0)],
            lane: vec!["".into(), "a".into()],
            lane_len: Some(2),
            ..Default::default()
        };
        let tl = timeline(&seq);
        assert_eq!(tl.bars[0].section, 0);
        assert_eq!(tl.bars[1].section, 1);
        assert_eq!(tl.bars[1].bpm, 60.0);
        assert_eq!(tl.total_secs, 2.0 + 4.0);
    }

    #[test]
    fn sequencer_params_finds_instance_zero() {
        use crate::state::{Meta, PaneState, SessionState, TmuxState, WindowState, STATE_FORMAT};
        let inline: toml::Value = toml::from_str("bpm = 96.0\nlane = [\"a\"]").unwrap();
        let st = SessionState {
            meta: Meta {
                name: "t".into(),
                created: String::new(),
                format: STATE_FORMAT,
            },
            tmux: TmuxState::default(),
            windows: vec![WindowState {
                name: "modules".into(),
                layout: String::new(),
                active_pane: 0,
                panes: vec![
                    PaneState {
                        module: "voice".into(),
                        instance: 0,
                        patch: None,
                        patch_inline: None,
                    },
                    PaneState {
                        module: "sequencer".into(),
                        instance: 0,
                        patch: None,
                        patch_inline: Some(inline),
                    },
                ],
            }],
        };
        let seq = sequencer_params(&st).expect("sequencer params");
        assert_eq!(seq.bpm, Some(96.0));
        assert_eq!(seq.lane, vec!["a".to_string()]);
    }

    #[test]
    fn sequencer_params_none_without_inline_patch() {
        use crate::state::{Meta, SessionState, TmuxState};
        let st = SessionState {
            meta: Meta {
                name: "t".into(),
                created: String::new(),
                format: 2,
            },
            tmux: TmuxState::default(),
            windows: vec![],
        };
        assert!(sequencer_params(&st).is_none());
    }
}
