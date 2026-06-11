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
token!(
    amber,
    0x9a,
    0x7b,
    0x2d,
    136,
    "chrome: wordmarks, labels, rules"
);
token!(
    amber_hi,
    0xe3,
    0xa8,
    0x18,
    172,
    "chrome emphasis: active headers"
);

// ── signal hues (the promises) ──────────────────────────────────────────────
token!(
    note,
    0xe0,
    0x76,
    0x3a,
    166,
    "NOTE: pitch, velocity, note steps"
);
token!(cv, 0x3f, 0xc9, 0xb0, 79, "CV: modulation, bindings, ghosts");
token!(audio, 0x8f, 0xbf, 0x4d, 107, "AUDIO: meters, waveforms");
token!(
    clock,
    0xc4,
    0x5d,
    0xd4,
    170,
    "CLOCK: transport, playhead, BPM"
);
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
/// Map a raw peak amplitude onto meter ladder height perceptually:
/// -48 dBFS sits at the bottom, 0 dBFS at the top. Linear amplitude
/// wastes the ladder — audible-but-modest signals (-20 dB ≈ 0.1) barely
/// lit two cells; in dB they stand more than half tall, like a console.
pub fn meter_frac(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * amplitude.max(1e-6).log10();
    ((db + 48.0) / 48.0).clamp(0.0, 1.0)
}

/// One cell of a vertical LED meter ladder, `row` 0 at the top. Lit
/// cells fill from the bottom with an eighth-block tip, and the ladder
/// wears console zones: AUDIO green through the body, hot amber above
/// ~70%, clip red at the very top. Unlit cells are a faint notch so the
/// ladder reads as hardware even at silence.
pub fn meter_cell(level: f32, row: usize, rows: usize) -> (char, Style) {
    const TIP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let rows = rows.max(1);
    let from_bottom = rows - 1 - row.min(rows - 1);
    let lit = level.clamp(0.0, 1.0) * rows as f32 - from_bottom as f32;
    let zone = (from_bottom as f32 + 1.0) / rows as f32;
    let hue = if zone > 0.92 {
        alert()
    } else if zone > 0.70 {
        amber_hi()
    } else {
        audio()
    };
    if lit >= 1.0 {
        ('█', signal(hue))
    } else if lit > 0.0 {
        (TIP[((lit * 8.0) as usize).min(7)], signal(hue))
    } else {
        ('╵', dim())
    }
}

/// The cap of a vertical fader on its rail, half-cell precision:
/// returns the knob glyph when `value`'s position lands in this cell
/// ('▀' upper half, '▄' lower half), or None — draw the rail. Twice the
/// resolution of a one-char-per-row tick, and it reads as a cap, not a
/// dot.
pub fn knob_cell(value: f32, row: usize, rows: usize) -> Option<char> {
    let rows = rows.max(1);
    let halves = rows * 2;
    let pos = ((1.0 - value.clamp(0.0, 1.0)) * (halves - 1) as f32).round() as usize;
    if pos / 2 == row.min(rows - 1) {
        Some(if pos.is_multiple_of(2) { '▀' } else { '▄' })
    } else {
        None
    }
}

/// The rail a fader knob rides on.
pub const RAIL: char = '│';

pub fn meter_char(level: f32) -> char {
    let idx = (level.clamp(0.0, 1.0) * 7.0).round() as usize;
    METER[idx.min(7)]
}

/// Identity palette for modulation sources — patch-cable colors. Colors are
/// allocated by **modbus channel slot** (palette[channel % 12]): collision-
/// free while ≤12 sources share a screen, stable all session, and both ends
/// of a cable compute the same hue independently. (Cross-window reuse is
/// fine — windows never share a screen.)
const SOURCE_PALETTE: [(u8, u8, u8, u8); 12] = [
    (0xe0, 0x76, 0x3a, 166), // coral
    (0x3f, 0xc9, 0xb0, 79),  // teal
    (0xd9, 0xa4, 0x41, 179), // gold
    (0xc4, 0x5d, 0xd4, 170), // orchid
    (0x8f, 0xbf, 0x4d, 107), // moss
    (0x5f, 0xa8, 0xd3, 74),  // sky
    (0xd3, 0x6a, 0x8a, 168), // rose
    (0x9a, 0xa8, 0x6a, 143), // sage
    (0x70, 0xc2, 0x9a, 72),  // jade
    (0xb8, 0x8c, 0x5e, 137), // tan
    (0x7f, 0x9e, 0xc4, 110), // slate
    (0xc9, 0x86, 0xc9, 175), // mauve
];

/// Identity color for a claimed modbus channel.
pub fn channel_color(channel: usize) -> Color {
    let (r, g, b, idx) = SOURCE_PALETTE[channel % SOURCE_PALETTE.len()];
    if truecolor() {
        Color::Rgb(r, g, b)
    } else {
        Color::Indexed(idx)
    }
}

/// Fallback identity color when an address can't be resolved to a live
/// channel (source module not running): stable hash into the same palette.
pub fn source_color(addr: &str) -> Color {
    let h: usize = addr.bytes().map(|b| b as usize).sum::<usize>() % SOURCE_PALETTE.len();
    channel_color(h)
}

/// Pitch-class wheel (sanctioned rainbow, note cells only): each of the 12
/// pitch classes keeps a fixed hue in every octave; brightness rises with
/// octave so register reads too.
pub fn pitch_color(note: u8) -> Color {
    // Dusty, de-primaried wheel (default theme): distinct pitch classes,
    // saturation capped so a busy melody reads as glow, not carnival.
    const WHEEL: [(u8, u8, u8, u8); 12] = [
        (0xc4, 0x6a, 0x52, 173), // C  terracotta
        (0xc4, 0x7e, 0x52, 172), // C# clay
        (0xc4, 0x94, 0x52, 178), // D  ochre
        (0xc4, 0xab, 0x52, 179), // D# honey
        (0xa8, 0xad, 0x58, 143), // E  olive
        (0x84, 0xb0, 0x6a, 108), // F  sage
        (0x5f, 0xb0, 0x8a, 72),  // F# sea green
        (0x52, 0xb0, 0xa4, 73),  // G  sea glass
        (0x58, 0xa3, 0xb8, 74),  // G# dusty cyan
        (0x6a, 0x8c, 0xc4, 103), // A  dusty blue
        (0x8d, 0x7c, 0xc4, 104), // A# heather
        (0xb4, 0x6a, 0xa8, 133), // B  plum
    ];
    let (r, g, b, idx) = WHEEL[(note % 12) as usize];
    if truecolor() {
        // brightness rises with octave: 0.65 at the bottom → 1.0 up top
        let f = 0.65 + 0.35 * ((note / 12) as f32 / 10.0).min(1.0);
        Color::Rgb(
            (r as f32 * f) as u8,
            (g as f32 * f) as u8,
            (b as f32 * f) as u8,
        )
    } else {
        Color::Indexed(idx)
    }
}

/// CV intensity ramp for mod-track cells: value −1..+1 → deep to bright
/// teal. A different color *family* from pitch, so note tracks and mod
/// tracks never blur.
pub fn cv_ramp(value: f32) -> Color {
    let v = ((value.clamp(-1.0, 1.0) + 1.0) / 2.0).max(0.15);
    if truecolor() {
        Color::Rgb(
            (0x3f as f32 * v) as u8,
            (0xc9 as f32 * v) as u8,
            (0xb0 as f32 * v) as u8,
        )
    } else {
        cv()
    }
}

fn shade(c: Color, f: f32) -> Color {
    match c {
        Color::Rgb(r, g, b) => Color::Rgb(
            (r as f32 * f) as u8,
            (g as f32 * f) as u8,
            (b as f32 * f) as u8,
        ),
        other => other,
    }
}

/// Pad `lines` with blanks so the next `bottom` lines render flush with
/// the pane's bottom edge — vim-style: content at the top, the command /
/// status block pinned to the bottom, flex in between. No-op when the
/// pane is already full.
pub fn anchor_bottom(lines: &mut Vec<Line<'static>>, height: usize, bottom: usize) {
    while lines.len() + bottom < height {
        lines.push(Line::default());
    }
}

/// Bar width for a pane `w` columns wide with `reserved` columns of
/// labels/readouts beside the bar. Scales with the pane (instead of
/// pinning to a fixed width) so wide panes fill with slider instead of
/// dead space. Renderers and mouse hit-tests MUST both use this so drag
/// geometry stays in sync.
pub fn bar_width(w: usize, reserved: usize) -> usize {
    (w.saturating_sub(reserved)).clamp(8, 36)
}

/// The bar (sliders, take six): half-height fill `▄` that GLOWS — a
/// truecolor gradient rising toward the tip — with a quarter-block tip for
/// half-cell resolution, a faint `▁` rail, and a bone ghost `▴` at the live
/// modulated position. Pass the binding's [`source_color`] as `hue` (amber
/// when unbound): the bar wears its cable's color.
pub fn bar(set: f32, live: Option<f32>, width: usize, hue: Color) -> Vec<Span<'static>> {
    let width = width.max(4);
    let subs = (set.clamp(0.0, 1.0) * (2 * width) as f32).round() as usize;
    let ghost = live.map(|v| ((v.clamp(0.0, 1.0)) * (width - 1) as f32).round() as usize);
    let fill_cells = (subs as f32 / 2.0).max(1.0);
    (0..width)
        .map(|i| {
            if ghost == Some(i) {
                return Span::styled(GHOST.to_string(), value());
            }
            let lit = subs.saturating_sub(i * 2).min(2);
            match lit {
                2 => {
                    let f = 0.55 + 0.45 * ((i + 1) as f32 / fill_cells).min(1.0);
                    Span::styled("▄".to_string(), Style::default().fg(shade(hue, f)))
                }
                1 => Span::styled("▖".to_string(), Style::default().fg(hue)),
                _ => Span::styled("▁".to_string(), dim()),
            }
        })
        .collect()
}

/// Plain-string form of [`bar`] for tests.
pub fn bar_str(set: f32, live: Option<f32>, width: usize) -> String {
    bar(set, live, width, amber())
        .iter()
        .map(|s| s.content.clone())
        .collect()
}

/// A segmented switch:/// A segmented switch: every option visible, the active one carved out in
/// an inverse block — a hardware toggle, not a blind cycle.
pub fn segments(options: &[&str], active: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(options.len() * 2);
    for (i, opt) in options.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("·".to_string(), dim()));
        }
        if i == active {
            spans.push(Span::styled(
                format!("▐{}▌", opt),
                Style::default().fg(bg()).bg(ink()),
            ));
        } else {
            spans.push(Span::styled(format!(" {} ", opt), dim()));
        }
    }
    spans
}

/// Plain-string form of [`segments`] for tests.
pub fn segments_str(options: &[&str], active: usize) -> String {
    segments(options, active)
        .iter()
        .map(|s| s.content.clone())
        .collect()
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
    Line::from(Span::styled(
        "─".repeat(width),
        Style::default().fg(amber()),
    ))
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
    fn meter_frac_is_perceptual() {
        assert_eq!(meter_frac(0.0), 0.0);
        assert_eq!(meter_frac(1.0), 1.0);
        let quiet = meter_frac(0.1); // -20 dBFS
        assert!(quiet > 0.5, "-20 dB stands over half tall: {}", quiet);
        assert!(meter_frac(0.003) < 0.05, "below -48 dB vanishes");
        assert!(meter_frac(0.5) > meter_frac(0.25));
    }

    #[test]
    fn meter_ladder_fills_from_bottom_with_zones() {
        for r in 0..8 {
            assert_eq!(meter_cell(0.0, r, 8).0, '╵', "silence is all notches");
            assert_eq!(meter_cell(1.0, r, 8).0, '█', "full scale all lit");
        }
        assert_eq!(meter_cell(1.0, 0, 8).1, signal(alert()), "top is the clip cell");
        assert_eq!(meter_cell(1.0, 7, 8).1, signal(audio()), "bottom is green");
        assert_eq!(meter_cell(1.0, 1, 8).1, signal(amber_hi()), "below clip runs hot");
        assert_eq!(meter_cell(0.5, 4, 8).0, '█');
        assert_eq!(meter_cell(0.5, 3, 8).0, '╵');
    }

    #[test]
    fn knob_rides_the_rail_half_cell() {
        assert_eq!(knob_cell(1.0, 0, 8), Some('▀'));
        assert_eq!(knob_cell(0.0, 7, 8), Some('▄'));
        assert_eq!(knob_cell(1.0, 4, 8), None);
        for v in [0.0_f32, 0.13, 0.5, 0.8, 1.0] {
            let hits = (0..8).filter(|r| knob_cell(v, *r, 8).is_some()).count();
            assert_eq!(hits, 1, "exactly one cell carries the knob at {}", v);
        }
    }

    #[test]
    fn segments_carve_out_the_active_option() {
        assert_eq!(
            segments_str(&["main", "sub", "mix"], 0),
            "▐main▌· sub · mix "
        );
        assert_eq!(segments_str(&["a", "b"], 1), " a ·▐b▌");
    }

    #[test]
    fn bar_fills_with_tip_rail_and_ghost() {
        assert_eq!(bar_str(1.0, None, 4), "▄▄▄▄");
        assert_eq!(bar_str(0.0, None, 4), "▁▁▁▁", "empty = faint rail");
        let g = bar_str(0.5, None, 8);
        assert_eq!(&g[..], "▄▄▄▄▁▁▁▁");
        assert_eq!(
            bar_str(0.31, None, 8).chars().nth(2),
            Some('▖'),
            "quarter-block tip = half-cell resolution"
        );
        let g = bar_str(0.25, Some(1.0), 8);
        assert_eq!(g.chars().last(), Some(GHOST), "ghost at the live position");
        assert_eq!(g.chars().count(), 8);
    }

    #[test]
    fn channel_colors_are_distinct_within_palette() {
        let distinct: std::collections::HashSet<String> =
            (0..12).map(|c| format!("{:?}", channel_color(c))).collect();
        assert_eq!(distinct.len(), 12, "12 slots, 12 hues, zero collisions");
        assert_eq!(
            channel_color(0),
            channel_color(12),
            "wraps past the palette"
        );
    }

    #[test]
    fn pitch_wheel_fixed_class_brighter_octaves() {
        assert_eq!(pitch_color(60), pitch_color(60));
        // same class, different octave: same family, different brightness
        let (c3, c5) = (pitch_color(48), pitch_color(72));
        assert_ne!(format!("{:?}", c3), format!("{:?}", c5));
        // different classes differ
        assert_ne!(
            format!("{:?}", pitch_color(60)),
            format!("{:?}", pitch_color(67))
        );
    }

    #[test]
    fn cv_ramp_scales_with_value() {
        assert_ne!(
            format!("{:?}", cv_ramp(-1.0)),
            format!("{:?}", cv_ramp(1.0))
        );
        assert_eq!(format!("{:?}", cv_ramp(0.5)), format!("{:?}", cv_ramp(0.5)));
    }

    #[test]
    fn source_colors_are_stable_and_spread() {
        assert_eq!(
            source_color("sequencer/0/t1"),
            source_color("sequencer/0/t1")
        );
        let distinct: std::collections::HashSet<String> = (1..=8)
            .map(|i| format!("{:?}", source_color(&format!("sequencer/0/t{}", i))))
            .collect();
        assert!(
            distinct.len() >= 4,
            "neighboring tracks mostly differ: {}",
            distinct.len()
        );
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

    #[test]
    fn bar_width_scales_with_pane() {
        assert_eq!(bar_width(60, 24), 36, "wide panes get wide bars");
        assert_eq!(bar_width(100, 24), 36, "capped so readouts stay near");
        assert_eq!(bar_width(30, 24), 8, "floor for tiny panes");
        assert_eq!(bar_width(10, 24), 8, "never underflows");
    }

    #[test]
    fn anchor_bottom_fills_exactly() {
        let mut lines: Vec<Line> = vec![Line::default(); 5];
        anchor_bottom(&mut lines, 20, 6);
        assert_eq!(lines.len(), 14, "5 content + 9 filler + 6 bottom = 20");
        // already-full pane: untouched
        let mut full: Vec<Line> = vec![Line::default(); 30];
        anchor_bottom(&mut full, 20, 6);
        assert_eq!(full.len(), 30);
    }
}
