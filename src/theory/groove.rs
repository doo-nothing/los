//! Groove templates: per-bar micro-timing offset curves.
//!
//! A groove is 16 offsets, one per 16th of a 4/4 bar, each a fraction
//! of a step (±0.5 = half a step early/late). The sequencer adds the
//! offset for `gstep % 16` to every fire on the track, on top of swing
//! and per-step delay (docs/plans/sequencer-timing.md). Templates are
//! data, not code, so `:groove` can menu and audition them like scales.

/// One groove template.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Groove {
    pub name: &'static str,
    /// What `:groove`'s menu shows next to the name.
    pub hint: &'static str,
    /// Per-16th offsets in step fractions, applied at `gstep % 16`.
    pub offsets: [f32; 16],
}

/// The built-in library. `straight` is the identity (and what `None`
/// means); it's listed so the menu always has an escape hatch.
pub const LIBRARY: &[Groove] = &[
    Groove {
        name: "straight",
        hint: "no groove — the grid as written",
        offsets: [0.0; 16],
    },
    Groove {
        name: "lilt",
        hint: "sine push/pull across the bar — gentle sway",
        // half-cycle sine: rushes the front of the bar, drags the back
        offsets: [
            0.0, 0.04, 0.07, 0.09, 0.10, 0.09, 0.07, 0.04, 0.0, -0.04, -0.07, -0.09, -0.10,
            -0.09, -0.07, -0.04,
        ],
    },
    Groove {
        name: "drag3",
        hint: "beat 3 lays back — laid-back backbeat",
        offsets: [
            0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.12, 0.06, 0.02, 0.0, 0.0, 0.0, 0.0, 0.0,
        ],
    },
    Groove {
        name: "push24",
        hint: "beats 2 and 4 rush — urgent snare",
        offsets: [
            0.0, 0.0, 0.0, 0.0, -0.08, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, -0.08, 0.0, 0.0, 0.0,
        ],
    },
    Groove {
        name: "sway",
        hint: "8th-note lean — every off-8th drags",
        offsets: [
            0.0, 0.0, 0.10, 0.0, 0.0, 0.0, 0.10, 0.0, 0.0, 0.0, 0.10, 0.0, 0.0, 0.0, 0.10, 0.0,
        ],
    },
    Groove {
        name: "limp",
        hint: "alternating push/drag 16ths — broken machine",
        offsets: [
            0.0, 0.08, -0.05, 0.08, 0.0, 0.08, -0.05, 0.08, 0.0, 0.08, -0.05, 0.08, 0.0, 0.08,
            -0.05, 0.08,
        ],
    },
    Groove {
        name: "rushin",
        hint: "everything but the downbeats early — nervous",
        offsets: [
            0.0, -0.06, -0.06, -0.06, 0.0, -0.06, -0.06, -0.06, 0.0, -0.06, -0.06, -0.06, 0.0,
            -0.06, -0.06, -0.06,
        ],
    },
    Groove {
        name: "molasses",
        hint: "the whole bar drags deeper and deeper — tape slow-down",
        offsets: [
            0.0, 0.01, 0.02, 0.03, 0.04, 0.05, 0.07, 0.08, 0.10, 0.11, 0.12, 0.14, 0.15, 0.16,
            0.18, 0.20,
        ],
    },
];

/// Look a groove up by name (case-insensitive).
pub fn find(name: &str) -> Option<&'static Groove> {
    LIBRARY.iter().find(|g| g.name.eq_ignore_ascii_case(name))
}

/// The track's timing offset (in step fractions) for a global step.
pub fn offset(groove: Option<&str>, gstep: u64) -> f32 {
    groove
        .and_then(find)
        .map(|g| g.offsets[(gstep % 16) as usize])
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_invariants_hold() {
        for g in LIBRARY {
            assert!(!g.name.is_empty() && !g.hint.is_empty());
            assert_eq!(
                g.name,
                g.name.to_lowercase(),
                "names are lowercase for the menu/completer"
            );
            for (i, &o) in g.offsets.iter().enumerate() {
                assert!(
                    (-0.5..=0.5).contains(&o),
                    "{} offset {} out of half-step range: {}",
                    g.name,
                    i,
                    o
                );
            }
        }
        let names: std::collections::HashSet<_> = LIBRARY.iter().map(|g| g.name).collect();
        assert_eq!(names.len(), LIBRARY.len(), "names are unique");
    }

    #[test]
    fn find_and_offset() {
        assert!(find("LILT").is_some(), "lookup is case-insensitive");
        assert_eq!(offset(None, 5), 0.0);
        assert_eq!(offset(Some("straight"), 9), 0.0);
        assert_eq!(offset(Some("drag3"), 8), 0.12, "beat 3 lays back");
        assert_eq!(offset(Some("drag3"), 24), 0.12, "wraps per bar");
        assert_eq!(offset(Some("nope"), 0), 0.0, "unknown name is straight");
    }
}
