//! `los badge` — the faceplate. A small optional pane (on by default) that
//! renders the Los mark with a beat-synced breathing-dither animation and a
//! one-line session readout. Pure flavor, signal-true: the dither wave rolls
//! with the transport and the whole badge sleeps when the music stops.
//! (design-language.md §7; non-instrument module — exempt from the editing
//! contract, honors Space/?/m only.)

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{enable_raw_mode, EnterAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Terminal,
};

use crate::shm::{Manifest, ShmTransport};
use crate::theme;
use crate::state;

/// The mark. Solid cells get re-textured by the breath wave.
const LOGO: [&str; 4] = [
    "▗▖ ▄▄▄   ▄▄▄",
    "▐▌█   █ ▀▄▄ ",
    "▐▌▀▄▄▄▀ ▄▄▄▀",
    "▐▙▄▄▖       ",
];

const DITHER: [char; 3] = ['░', '▒', '▓'];

#[derive(Clone, Copy, PartialEq)]
enum Mood {
    Breathe,
    Pulse,
}

/// Texture one logo row for the current breath phase.
///
/// `phase` 0..1 is the position within the beat; a wave of lighter dither
/// rolls left→right through the solid cells once per beat. `energy` 0..1
/// scales how deep the wave bites (sleeping badge = 0 = untouched glyphs).
fn breathe_row(row: &str, phase: f32, energy: f32, row_idx: usize, mood: Mood) -> String {
    let cols = row.chars().count().max(1);
    match mood {
        Mood::Breathe => row
            .chars()
            .enumerate()
            .map(|(x, c)| {
                if c == ' ' || energy <= 0.0 {
                    return c;
                }
                // distance from the rolling wavefront, wrapped
                let front = phase * cols as f32;
                let d = ((x as f32 - front).rem_euclid(cols as f32)) / cols as f32;
                // wave envelope: deepest right behind the front
                let depth = (1.0 - d * 3.0).max(0.0) * energy;
                if depth > 0.66 {
                    DITHER[0]
                } else if depth > 0.4 {
                    DITHER[1]
                } else if depth > 0.18 {
                    DITHER[2]
                } else {
                    c
                }
            })
            .collect(),
        Mood::Pulse => {
            // one-frame shear on the downbeat: odd rows slide a cell
            if phase < 0.08 && energy > 0.0 && row_idx % 2 == 1 {
                format!(" {}", &row[..row.len().saturating_sub(1)])
            } else {
                row.to_string()
            }
        }
    }
}

/// Deterministic sparse artifact field: tiny dim specks drifting slowly up
/// and to the right behind the mark — dust in the phosphor. Density scales
/// with energy so a stopped session's background goes still and empty.
fn artifact_at(x: u16, y: u16, frame: u32, energy: f32) -> Option<char> {
    // drift: the field scrolls as frame advances
    let fx = x as u32 + frame / 6;
    let fy = y as u32 + frame / 24;
    // cheap integer hash
    let mut h = fx.wrapping_mul(374_761_393) ^ fy.wrapping_mul(668_265_263);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    let v = (h >> 17) % 1000;
    let density = (8.0 + 22.0 * energy) as u32; // ~0.8%..3% of cells
    if v < density {
        Some(match v % 3 {
            0 => '·',
            1 => '░',
            _ => '∙',
        })
    } else {
        None
    }
}

/// Most recent save-state name, for the readout line.
fn session_name() -> String {
    std::fs::read_dir(state::states_dir())
        .ok()
        .and_then(|rd| {
            let mut entries: Vec<_> = rd
                .flatten()
                .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".toml")))
                .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, e)))
                .collect();
            entries.sort_by_key(|(t, _)| *t);
            entries.last().map(|(_, e)| {
                e.file_name()
                    .to_string_lossy()
                    .trim_end_matches(".toml")
                    .to_string()
            })
        })
        .unwrap_or_else(|| String::from("unsaved"))
}

pub fn run(instance: usize) -> Result<()> {
    state::write_pid_file("badge", instance);
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let _ = manifest.register("badge", instance, None, 0);

    for attempt in 0..20 {
        match enable_raw_mode() {
            Ok(()) => break,
            Err(e) => {
                if attempt < 19 {
                    std::thread::sleep(Duration::from_millis(200));
                } else {
                    return Err(anyhow::anyhow!("Failed to enable raw mode: {}", e));
                }
            }
        }
    }
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut transport: Option<ShmTransport> = ShmTransport::open().ok();
    let mut mood = Mood::Breathe;
    let mut frame: u32 = 0;
    // energy eases toward 1 when playing, 0 when stopped (the sleep rule)
    let mut energy = 0.0f32;
    let mut name = session_name();
    let mut name_refresh = 0u32;

    loop {
        if transport.is_none() {
            transport = ShmTransport::open().ok();
        }
        let (playing, bpm, clock) = transport
            .as_ref()
            .map(|t| (t.playing(), t.bpm(), t.clock()))
            .unwrap_or((false, 120.0, 0));

        // ease energy: wake fast, sleep slow (~2s)
        let target = if playing { 1.0 } else { 0.0 };
        energy += (target - energy) * if playing { 0.3 } else { 0.04 };

        if name_refresh == 0 {
            name_refresh = 40; // ~every 3s
            name = session_name();
        }
        name_refresh -= 1;

        let samples_per_beat = (60.0 / bpm.max(1.0) * 48_000.0) as u64;
        let phase = if samples_per_beat > 0 {
            (clock % samples_per_beat) as f32 / samples_per_beat as f32
        } else {
            0.0
        };

        frame = frame.wrapping_add(1);
        terminal.draw(|f| {
            let area = f.area();
            let logo_style = if energy > 0.05 {
                Style::default().fg(theme::amber_hi())
            } else {
                theme::dim()
            };

            // background: drifting phosphor dust (dim, behind everything)
            let mut dust: Vec<Line> = Vec::with_capacity(area.height as usize);
            for y in 0..area.height {
                let row: String = (0..area.width)
                    .map(|x| artifact_at(x, y, frame, energy).unwrap_or(' '))
                    .collect();
                dust.push(Line::from(Span::styled(row, theme::dim())));
            }
            f.render_widget(Paragraph::new(dust), area);

            // the mark + readout, centered, over the dust
            let pad_top = area.height.saturating_sub(LOGO.len() as u16 + 2) / 2;
            let pad_left = " ".repeat((area.width.saturating_sub(17) / 2) as usize);
            let mut lines: Vec<Line> = Vec::with_capacity(LOGO.len() + 2);
            for (i, row) in LOGO.iter().enumerate() {
                let textured = breathe_row(row, phase, energy, i, mood);
                lines.push(Line::from(vec![
                    Span::raw(pad_left.clone()),
                    Span::styled(textured, logo_style),
                ]));
            }
            // downbeat accent: the underline flares on the first 10% of the beat
            let downbeat = playing && phase < 0.10;
            lines.push(Line::from(Span::styled(
                format!("{}{}", pad_left, "─".repeat(17)),
                if downbeat {
                    Style::default().fg(theme::amber_hi())
                } else {
                    Style::default().fg(theme::amber())
                },
            )));
            lines.push(Line::from(vec![
                Span::raw(pad_left.clone()),
                Span::styled(name.clone(), theme::value()),
                Span::raw(" "),
                Span::styled(
                    theme::transport_echo(bpm, playing, None),
                    theme::signal(theme::clock()),
                ),
            ]));
            let logo_area = ratatui::layout::Rect::new(
                area.x,
                area.y + pad_top,
                area.width,
                (LOGO.len() as u16 + 2).min(area.height.saturating_sub(pad_top)),
            );
            f.render_widget(Paragraph::new(lines), logo_area);
        })?;

        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char(' ') => {
                        if let Some(ref mut t) = transport {
                            t.toggle_playing();
                        }
                    }
                    KeyCode::Char('m') => {
                        mood = match mood {
                            Mood::Breathe => Mood::Pulse,
                            Mood::Pulse => Mood::Breathe,
                        };
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breathe_preserves_spaces_and_length() {
        for phase in [0.0, 0.3, 0.7, 0.99] {
            let out = breathe_row(LOGO[1], phase, 1.0, 1, Mood::Breathe);
            assert_eq!(out.chars().count(), LOGO[1].chars().count());
            for (a, b) in LOGO[1].chars().zip(out.chars()) {
                if a == ' ' {
                    assert_eq!(b, ' ', "spaces never textured");
                }
            }
        }
    }

    #[test]
    fn zero_energy_means_stillness() {
        let out = breathe_row(LOGO[2], 0.5, 0.0, 2, Mood::Breathe);
        assert_eq!(out, LOGO[2], "sleeping badge leaves the mark untouched");
    }

    #[test]
    fn wave_actually_textures_at_full_energy() {
        let out = breathe_row(LOGO[1], 0.25, 1.0, 1, Mood::Breathe);
        assert_ne!(out, LOGO[1], "breathing visibly changes the glyphs");
        assert!(out.chars().any(|c| DITHER.contains(&c)));
    }
}
