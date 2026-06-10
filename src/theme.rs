//! The "phosphor & ink" design language (docs/plans/design-language.md).
//!
//! Single source of truth for color tokens, the glyph vocabulary, and the
//! shared pane anatomy (header / rule / status). Color is a promise: the
//! four signal hues mean what they mean everywhere, chrome is amber, values
//! are bone, state is brightness. Truecolor when the terminal offers it,
//! xterm-256 otherwise.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// True when the terminal advertises 24-bit color (COLORTERM passes through
/// tmux when configured; otherwise we fall back to the 256 palette, which
/// is authored to land on the same hues).
fn truecolor() -> bool {
    std::env::var("COLORTERM")
        .map(|v| v.contains("truecolor") || v.contains("24bit"))
        .unwrap_or(false)
}

macro_rules! token {
    ($name:ident, $r:expr, $g:expr, $b:expr, $idx:expr, $doc:expr) => {
        #[doc = $doc]
        pub fn $name() -> Color {
            if truecolor() {
                Color::Rgb($r, $g, $b)
            } else {
                Color::Indexed($idx)
            }
        }
    };
}

// ── ink ─────────────────────────────────────────────────────────────────────
token!(bg, 0x0d, 0x0b, 0x08, 232, "background: warm near-black");
token!(ink, 0xe8, 0xdc, 0xc8, 223, "values & content: warm bone");
token!(ink_dim, 0x7d, 0x73, 0x63, 101, "inactive content");
token!(amber, 0x9a, 0x7b, 0x2d, 136, "chrome: wordmarks, labels, rules");
token!(amber_hi, 0xe3, 0xa8, 0x18, 172, "chrome emphasis: active headers");

// ── signal hues (the promises) ──────────────────────────────────────────────
token!(note, 0xe0, 0x76, 0x3a, 166, "NOTE: pitch, velocity, note steps");
token!(cv, 0x3f, 0xc9, 0xb0, 79, "CV: modulation, bindings, ghosts");
token!(audio, 0x8f, 0xbf, 0x4d, 107, "AUDIO: meters, waveforms");
token!(clock, 0xc4, 0x5d, 0xd4, 170, "CLOCK: transport, playhead, BPM");
token!(alert, 0xd4, 0x50, 0x2e, 160, "errors, clipping — sparingly");

// ── styles ──────────────────────────────────────────────────────────────────

pub fn chrome() -> Style {
    Style::default().fg(amber())
}

pub fn chrome_hi() -> Style {
    Style::default().fg(amber_hi()).add_modifier(Modifier::BOLD)
}

pub fn value() -> Style {
    Style::default().fg(ink())
}

pub fn dim() -> Style {
    Style::default().fg(ink_dim())
}

/// Selected item: inverse bone block.
pub fn selected() -> Style {
    Style::default().fg(bg()).bg(ink())
}

/// One-frame trigger flash: inverse in the signal's hue.
pub fn flash(hue: Color) -> Style {
    Style::default().fg(bg()).bg(hue)
}

pub fn signal(hue: Color) -> Style {
    Style::default().fg(hue)
}

// ── glyph vocabulary ────────────────────────────────────────────────────────

pub const STEP_ON: char = '●';
pub const STEP_OFF: char = '○';
pub const MOD_ON: char = '◆';
pub const MOD_OFF: char = '◇';
pub const PLAYHEAD: char = '▶';
pub const STOPPED: char = '■';
/// playhead wake, oldest → newest
pub const WAKE: [char; 3] = ['░', '▒', '▓'];
pub const BIND: char = '⌁';
pub const GHOST: char = '▴';
pub const BPM: char = '♪';
pub const AUDIO_GLYPH: char = '∿';
pub const METER: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
pub const SEP: &str = " · ";
pub const GATE_HI: char = '◉';
pub const GATE_LO: char = '◎';
pub const RISE_ARROW: char = '↗';
pub const FALL_ARROW: char = '↘';
pub const SUSTAIN_BAR: char = '―';

/// A meter cell for a 0..1 level.
pub fn meter_char(level: f32) -> char {
    let idx = (level.clamp(0.0, 1.0) * 7.0).round() as usize;
    METER[idx.min(7)]
}

/// A minimal fader `width` cells wide: a dim track `─`, a bone thumb `┃` at
/// the set position, and the teal ghost `▴` riding the live (modulated)
/// position — the §5 mod-feedback rule without block-gauge bulk.
pub fn fader(set: f32, live: Option<f32>, width: usize) -> Vec<Span<'static>> {
    let width = width.max(3);
    let pos = |v: f32| ((v.clamp(0.0, 1.0)) * (width - 1) as f32).round() as usize;
    let thumb = pos(set);
    let ghost = live.map(pos);
    (0..width)
        .map(|i| {
            if ghost == Some(i) && i != thumb {
                Span::styled(GHOST.to_string(), signal(cv()))
            } else if i == thumb {
                Span::styled("┃".to_string(), value())
            } else {
                Span::styled("─".to_string(), dim())
            }
        })
        .collect()
}

/// Plain-string form of [`fader`] for tests and width math.
pub fn fader_str(set: f32, live: Option<f32>, width: usize) -> String {
    fader(set, live, width).iter().map(|s| s.content.clone()).collect()
}

// ── pane anatomy ────────────────────────────────────────────────────────────

/// Header line: `WORDMARK ·context·` left, transport echo right (CLOCK hue).
pub fn header(wordmark: &str, context: &str, right: &str, width: usize) -> Line<'static> {
    let left = if context.is_empty() {
        format!("{} ", wordmark)
    } else {
        format!("{} ·{}· ", wordmark, context)
    };
    let pad = width
        .saturating_sub(left.chars().count())
        .saturating_sub(right.chars().count());
    Line::from(vec![
        Span::styled(wordmark.to_string(), chrome_hi()),
        Span::styled(left[wordmark.len()..].to_string(), chrome()),
        Span::raw(" ".repeat(pad)),
        Span::styled(right.to_string(), signal(clock())),
    ])
}

/// The transport echo for headers: `♪120 ▶` / `♪120 ■`.
pub fn transport_echo(bpm: f32, playing: bool, position: Option<&str>) -> String {
    let p = if playing { PLAYHEAD } else { STOPPED };
    match position {
        Some(pos) => format!("{}{:.0} {}{}", BPM, bpm, p, pos),
        None => format!("{}{:.0} {}", BPM, bpm, p),
    }
}

/// Section rule: a dim amber hairline.
pub fn rule(width: usize) -> Line<'static> {
    Line::from(Span::styled("─".repeat(width), Style::default().fg(amber())))
}

/// Status line: mode label + message left, right-aligned tail.
pub fn status(mode: &str, msg: &str, right: &str, width: usize) -> Line<'static> {
    let left = if msg.is_empty() {
        mode.to_string()
    } else {
        format!("{}{}{}", mode, SEP, msg)
    };
    let pad = width
        .saturating_sub(left.chars().count())
        .saturating_sub(right.chars().count());
    Line::from(vec![
        Span::styled(mode.to_string(), chrome()),
        Span::styled(left[mode.len()..].to_string(), value()),
        Span::raw(" ".repeat(pad)),
        Span::styled(right.to_string(), dim()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fader_renders_thumb_and_ghost() {
        let g = fader_str(0.5, None, 9);
        assert_eq!(g, "────┃────");
        let g = fader_str(0.0, Some(1.0), 5);
        assert_eq!(g, "┃───▴", "thumb at zero, ghost riding high");
        let g = fader_str(1.0, Some(1.0), 5);
        assert_eq!(g, "────┃", "ghost under the thumb yields to it");
        assert_eq!(fader_str(0.5, Some(0.75), 8).chars().count(), 8);
    }

    #[test]
    fn meter_maps_levels() {
        assert_eq!(meter_char(0.0), '▁');
        assert_eq!(meter_char(1.0), '█');
        assert_eq!(meter_char(0.5), '▅'); // 3.5 rounds up
    }

    #[test]
    fn header_and_status_fit_width() {
        let h = header("SEQ", "t1/8", "♪120 ▶09/16", 50);
        assert_eq!(h.width(), 50);
        let s = status("NORMAL", "3d…", ":w bass", 50);
        assert_eq!(s.width(), 50);
    }

    #[test]
    fn transport_echo_forms() {
        assert_eq!(transport_echo(120.0, true, Some("09/16")), "♪120 ▶09/16");
        assert_eq!(transport_echo(90.4, false, None), "♪90 ■");
    }

    #[test]
    fn tokens_resolve_in_both_depths() {
        // can't toggle COLORTERM safely in-process; just exercise the calls
        let _ = (bg(), ink(), ink_dim(), amber(), amber_hi());
        let _ = (note(), cv(), audio(), clock(), alert());
    }
}
