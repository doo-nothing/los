//! Receiver-side routing: source addresses and their resolution.
//!
//! A *source address* names a modulation output as `module/instance/output`,
//! e.g. `sequencer/0/t3` or `envelope/0/sum`. Inputs store addresses (in
//! params and state files); the live modbus channel index is resolved through
//! the manifest, so a restarted module (which claims a fresh channel range)
//! keeps its bindings working.

use std::fmt;

use crate::shm::ManifestEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceAddr {
    pub module: String,
    pub instance: usize,
    pub output: String,
}

impl SourceAddr {
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split('/');
        let module = parts.next()?.to_string();
        let instance = parts.next()?.parse().ok()?;
        let output = parts.next()?.to_string();
        if module.is_empty() || output.is_empty() || parts.next().is_some() {
            return None;
        }
        Some(Self { module, instance, output })
    }
}

impl fmt::Display for SourceAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.module, self.instance, self.output)
    }
}

/// Output labels per module type, in claimed-channel order.
pub fn output_labels(module: &str) -> &'static [&'static str] {
    match module {
        "sequencer" => &["t1", "t2", "t3", "t4", "t5", "t6", "t7", "t8"],
        "envelope" => &[
            "ch1", "ch2", "ch3", "ch4", "ch5", "ch6", "sum", "or", "and", "inv", "eor", "eoc",
        ],
        _ => &[],
    }
}

/// Resolve an address to a live modbus channel through the manifest.
pub fn resolve(entries: &[ManifestEntry], addr: &SourceAddr) -> Option<usize> {
    let e = entries
        .iter()
        .find(|e| e.module_name == addr.module && e.instance == addr.instance)?;
    let base = e.mod_base?;
    let off = output_labels(&e.module_name)
        .iter()
        .position(|l| *l == addr.output)?;
    if off < e.mod_count {
        Some(base + off)
    } else {
        None
    }
}

/// For a `sequencer/N/tM` address, the track index its note events carry in
/// their `source` field (notes routing). None for non-sequencer sources.
pub fn note_source_track(addr: &SourceAddr) -> Option<u8> {
    if addr.module != "sequencer" {
        return None;
    }
    let off = output_labels("sequencer")
        .iter()
        .position(|l| *l == addr.output)?;
    Some(off as u8)
}

/// All live, claimable sources in the session, sorted — feeds the `@` picker
/// and the scope's channel browser.
pub fn live_sources(entries: &[ManifestEntry]) -> Vec<SourceAddr> {
    let mut out: Vec<SourceAddr> = entries
        .iter()
        .filter(|e| e.mod_base.is_some())
        .flat_map(|e| {
            output_labels(&e.module_name)
                .iter()
                .take(e.mod_count)
                .map(|label| SourceAddr {
                    module: e.module_name.clone(),
                    instance: e.instance,
                    output: (*label).to_string(),
                })
                .collect::<Vec<_>>()
        })
        .collect();
    out.sort_by_key(|a| (a.module.clone(), a.instance, resolve(entries, a)));
    out
}

/// Reverse lookup: the address whose output lives on `channel`, if any.
pub fn label_for_channel(entries: &[ManifestEntry], channel: usize) -> Option<SourceAddr> {
    for e in entries {
        let Some(base) = e.mod_base else { continue };
        if channel >= base && channel < base + e.mod_count {
            let off = channel - base;
            let label = output_labels(&e.module_name).get(off)?;
            return Some(SourceAddr {
                module: e.module_name.clone(),
                instance: e.instance,
                output: (*label).to_string(),
            });
        }
    }
    None
}

/// The cable color for an address: identity hue of its live claimed
/// channel, falling back to a stable hash when the source isn't running.
pub fn cable_color(entries: &[ManifestEntry], addr: &SourceAddr) -> ratatui::style::Color {
    match resolve(entries, addr) {
        Some(ch) => crate::theme::channel_color(ch),
        None => crate::theme::source_color(&addr.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(module: &str, instance: usize, mod_base: Option<usize>, mod_count: usize) -> ManifestEntry {
        ManifestEntry {
            module_name: module.to_string(),
            instance,
            pid: 0,
            audio_shm: None,
            mod_base,
            mod_count,
        }
    }

    #[test]
    fn parse_and_display_roundtrip() {
        let a = SourceAddr::parse("sequencer/0/t3").unwrap();
        assert_eq!(a.module, "sequencer");
        assert_eq!(a.instance, 0);
        assert_eq!(a.output, "t3");
        assert_eq!(a.to_string(), "sequencer/0/t3");

        assert!(SourceAddr::parse("nope").is_none());
        assert!(SourceAddr::parse("a/b/c").is_none(), "instance must be a number");
        assert!(SourceAddr::parse("a/0/c/d").is_none(), "too many segments");
        assert!(SourceAddr::parse("a/0/").is_none(), "empty output");
    }

    #[test]
    fn resolve_through_manifest() {
        let entries = vec![
            entry("sequencer", 0, Some(0), 8),
            entry("envelope", 0, Some(8), 8),
            entry("voice", 0, None, 0),
        ];
        let t3 = SourceAddr::parse("sequencer/0/t3").unwrap();
        assert_eq!(resolve(&entries, &t3), Some(2));
        let sum = SourceAddr::parse("envelope/0/sum").unwrap();
        assert_eq!(resolve(&entries, &sum), Some(14), "sum at offset 6 of the claim");
        let eor = SourceAddr::parse("envelope/0/eor").unwrap();
        assert_eq!(resolve(&entries, &eor), None, "eor (offset 10) is beyond an 8-channel claim");
        let full = vec![entry("envelope", 0, Some(8), 12)];
        assert_eq!(resolve(&full, &eor), Some(18), "eor resolves under a full 12-output claim");
        let missing = SourceAddr::parse("envelope/1/ch1").unwrap();
        assert_eq!(resolve(&entries, &missing), None, "instance not running");
        let bad = SourceAddr::parse("voice/0/out").unwrap();
        assert_eq!(resolve(&entries, &bad), None, "voice claims no channels");
    }

    #[test]
    fn resolution_survives_restart_with_new_base() {
        let addr = SourceAddr::parse("envelope/0/ch2").unwrap();
        let before = vec![entry("envelope", 0, Some(8), 8)];
        let after = vec![entry("envelope", 0, Some(16), 8)]; // restarted, new claim
        assert_eq!(resolve(&before, &addr), Some(9));
        assert_eq!(resolve(&after, &addr), Some(17), "same address, new channel");
    }

    #[test]
    fn live_sources_lists_all_outputs() {
        let entries = vec![
            entry("sequencer", 0, Some(0), 8),
            entry("envelope", 0, Some(8), 8),
            entry("scope", 0, None, 0),
        ];
        let sources = live_sources(&entries);
        assert_eq!(sources.len(), 16, "claim count bounds the listed outputs");
        assert!(sources.iter().any(|a| a.to_string() == "sequencer/0/t8"));
        assert!(sources.iter().any(|a| a.to_string() == "envelope/0/ch5"));
    }

    #[test]
    fn label_for_channel_reverse_lookup() {
        let entries = vec![entry("envelope", 0, Some(8), 12)];
        assert_eq!(label_for_channel(&entries, 14).unwrap().to_string(), "envelope/0/sum");
        assert_eq!(label_for_channel(&entries, 19).unwrap().to_string(), "envelope/0/eoc");
        assert!(label_for_channel(&entries, 3).is_none());
    }

    #[test]
    fn cable_color_resolves_live_and_falls_back() {
        let entries = vec![entry("sequencer", 0, Some(0), 8)];
        let t3 = SourceAddr::parse("sequencer/0/t3").unwrap();
        assert_eq!(
            format!("{:?}", cable_color(&entries, &t3)),
            format!("{:?}", crate::theme::channel_color(2)),
            "live source uses its channel slot"
        );
        let ghost = SourceAddr::parse("envelope/9/ch1").unwrap();
        // not running: stable hash fallback, same answer twice
        assert_eq!(
            format!("{:?}", cable_color(&entries, &ghost)),
            format!("{:?}", cable_color(&entries, &ghost))
        );
    }

    #[test]
    fn note_source_track_from_address() {
        let t5 = SourceAddr::parse("sequencer/0/t5").unwrap();
        assert_eq!(note_source_track(&t5), Some(4));
        let env = SourceAddr::parse("envelope/0/ch1").unwrap();
        assert_eq!(note_source_track(&env), None);
    }
}
