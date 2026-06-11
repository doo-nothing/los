//! Shared undo/redo for param-list modules (voice, envelope, mixer, scope).
//!
//! Each module maps its editable fields onto numbered *slots* via
//! [`ParamUndo`]; edits are recorded as old/new [`ParamValue`] pairs.
//! Consecutive edits to the same slot **coalesce into one entry** while they
//! arrive within [`COALESCE_WINDOW`] — one `u` reverts a whole h/l sweep,
//! vi's "one insert session = one undo" feel (docs/keybindings.md).
//!
//! The sequencer keeps its own richer Command history (tracks, registers,
//! step ranges) and implements the same coalescing rule for note sweeps.

use std::time::{Duration, Instant};

pub const COALESCE_WINDOW: Duration = Duration::from_millis(1000);
const HISTORY_CAP: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    F32(f32),
    Usize(usize),
    U8(u8),
    Bool(bool),
    Src(Option<String>),
}

/// A module state whose editable fields are addressable by slot number.
pub trait ParamUndo {
    fn get_param(&self, slot: usize) -> Option<ParamValue>;
    fn set_param(&mut self, slot: usize, value: ParamValue);
}

#[derive(Debug, Clone)]
struct Edit {
    desc: &'static str,
    slot: usize,
    old: ParamValue,
    new: ParamValue,
    at: Instant,
}

#[derive(Debug, Default)]
pub struct ParamHistory {
    entries: Vec<Edit>,
    index: usize,
}

impl ParamHistory {
    /// Record an edit captured around a mutation. No-ops are skipped; an
    /// edit of the same slot hot on the heels of the previous one merges
    /// into it (the sweep rule).
    pub fn record(&mut self, slot: usize, desc: &'static str, old: ParamValue, new: ParamValue) {
        if old == new {
            return;
        }
        if self.index == self.entries.len() {
            if let Some(last) = self.entries.last_mut() {
                if last.slot == slot && last.at.elapsed() < COALESCE_WINDOW {
                    last.new = new;
                    last.at = Instant::now();
                    // sweep returned to the start: drop the no-op entry
                    if last.old == last.new {
                        self.entries.pop();
                        self.index = self.entries.len();
                    }
                    return;
                }
            }
        }
        self.entries.truncate(self.index);
        self.entries.push(Edit {
            desc,
            slot,
            old,
            new,
            at: Instant::now(),
        });
        if self.entries.len() > HISTORY_CAP {
            self.entries.remove(0);
        }
        self.index = self.entries.len();
    }

    pub fn undo<S: ParamUndo>(&mut self, s: &mut S) -> Option<&'static str> {
        if self.index == 0 {
            return None;
        }
        self.index -= 1;
        let e = &self.entries[self.index];
        s.set_param(e.slot, e.old.clone());
        Some(e.desc)
    }

    pub fn redo<S: ParamUndo>(&mut self, s: &mut S) -> Option<&'static str> {
        if self.index >= self.entries.len() {
            return None;
        }
        let e = &self.entries[self.index];
        s.set_param(e.slot, e.new.clone());
        self.index += 1;
        Some(e.desc)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Run an undo or redo op up to `count` times, stopping early when history
/// runs out; returns the status-bar message (same contract as the
/// sequencer's history_status).
pub fn history_status(
    label: &str,
    count: usize,
    mut op: impl FnMut() -> Option<&'static str>,
) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        vals: [f32; 4],
    }

    impl ParamUndo for Fake {
        fn get_param(&self, slot: usize) -> Option<ParamValue> {
            self.vals.get(slot).map(|v| ParamValue::F32(*v))
        }
        fn set_param(&mut self, slot: usize, value: ParamValue) {
            if let (Some(v), ParamValue::F32(f)) = (self.vals.get_mut(slot), value) {
                *v = f;
            }
        }
    }

    fn edit(s: &mut Fake, h: &mut ParamHistory, slot: usize, to: f32) {
        let old = s.get_param(slot).unwrap();
        s.vals[slot] = to;
        h.record(slot, "Adjust", old, ParamValue::F32(to));
    }

    #[test]
    fn sweep_coalesces_into_one_entry() {
        let mut s = Fake::default();
        let mut h = ParamHistory::default();
        for i in 1..=5 {
            edit(&mut s, &mut h, 0, i as f32 * 0.1);
        }
        assert_eq!(h.len(), 1, "rapid same-slot edits merge");
        assert_eq!(h.undo(&mut s), Some("Adjust"));
        assert_eq!(s.vals[0], 0.0, "one undo reverts the whole sweep");
        assert!(h.redo(&mut s).is_some());
        assert!((s.vals[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn different_slots_do_not_coalesce() {
        let mut s = Fake::default();
        let mut h = ParamHistory::default();
        edit(&mut s, &mut h, 0, 0.5);
        edit(&mut s, &mut h, 1, 0.7);
        assert_eq!(h.len(), 2);
        h.undo(&mut s);
        assert_eq!(s.vals[1], 0.0);
        assert_eq!(s.vals[0], 0.5, "first edit still applied");
    }

    #[test]
    fn round_trip_sweep_drops_entry() {
        let mut s = Fake::default();
        let mut h = ParamHistory::default();
        edit(&mut s, &mut h, 0, 0.3);
        edit(&mut s, &mut h, 0, 0.0); // back where it started
        assert_eq!(h.len(), 0, "no-op sweep leaves no history");
        assert!(h.undo(&mut s).is_none());
    }

    #[test]
    fn noop_edit_not_recorded() {
        let mut h = ParamHistory::default();
        h.record(0, "Adjust", ParamValue::F32(0.5), ParamValue::F32(0.5));
        assert_eq!(h.len(), 0);
    }

    #[test]
    fn new_edit_after_undo_truncates_redo() {
        let mut s = Fake::default();
        let mut h = ParamHistory::default();
        edit(&mut s, &mut h, 0, 0.5);
        edit(&mut s, &mut h, 1, 0.5);
        h.undo(&mut s);
        edit(&mut s, &mut h, 2, 0.5);
        assert!(h.redo(&mut s).is_none(), "redo branch discarded");
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn src_values_round_trip() {
        #[derive(Default)]
        struct S {
            src: Option<String>,
        }
        impl ParamUndo for S {
            fn get_param(&self, _: usize) -> Option<ParamValue> {
                Some(ParamValue::Src(self.src.clone()))
            }
            fn set_param(&mut self, _: usize, v: ParamValue) {
                if let ParamValue::Src(s) = v {
                    self.src = s;
                }
            }
        }
        let mut s = S::default();
        let mut h = ParamHistory::default();
        let old = s.get_param(0).unwrap();
        s.src = Some("envelope/0/ch1".into());
        h.record(0, "Bind", old, s.get_param(0).unwrap());
        assert_eq!(h.undo(&mut s), Some("Bind"));
        assert_eq!(s.src, None);
        h.redo(&mut s);
        assert_eq!(s.src.as_deref(), Some("envelope/0/ch1"));
    }

    #[test]
    fn history_status_messages() {
        let mut s = Fake::default();
        let mut h = ParamHistory::default();
        assert_eq!(
            history_status("Undo", 1, || h.undo(&mut s)),
            "Nothing to undo"
        );
        edit(&mut s, &mut h, 0, 0.5);
        edit(&mut s, &mut h, 1, 0.5);
        assert_eq!(
            history_status("Undo", 5, || h.undo(&mut s)),
            "Undo ×2: Adjust"
        );
    }
}
