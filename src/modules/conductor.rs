use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

#[allow(dead_code)]
fn shell_escape(s: &str) -> String {
    // Simple escaping: wrap in single quotes, escape embedded single quotes
    if s.contains('\'') {
        let escaped = s.replace('\'', "'\"'\"'");
        format!("'{}'", escaped)
    } else {
        format!("'{}'", s)
    }
}

use anyhow::{bail, Context, Result};
use crossterm::{
    event::{self, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{enable_raw_mode, EnterAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Terminal,
};

use crate::shm::{Manifest, ShmTransport};
use crate::state;

// ── tmux command helpers ───────────────────────────────────────────────────

/// Run a tmux command, check exit code, and capture stderr.
/// Returns stdout on success, bails with stderr on failure.
fn tmux_cmd(args: &[&str]) -> Result<String> {
    let output = Command::new("tmux")
        .args(args)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .with_context(|| format!("failed to run tmux {}", args.join(" ")))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        if stderr.is_empty() {
            bail!(
                "tmux {} exited with code {:?}",
                args.join(" "),
                output.status.code()
            );
        } else {
            bail!("tmux {}: {}", args.join(" "), stderr);
        }
    }

    Ok(stdout)
}

/// Run a tmux command and ignore failure (log to stderr but don't bail).
fn tmux_cmd_ok(args: &[&str]) {
    if let Err(e) = tmux_cmd(args) {
        eprintln!("[tmux warning] {}", e);
    }
}

// ── session creation ───────────────────────────────────────────────────────

/// Create the detached `los` session sized to the launching terminal.
/// A bare `new-session -d` defaults to 80x24, so every layout computation
/// would happen in a tiny window and only get proportionally rescaled on
/// attach — content-aware sizing needs the real geometry up front.
fn new_los_session(window_name: &str) -> Result<()> {
    let mut args: Vec<String> = ["new-session", "-d", "-s", "los", "-n", window_name]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Ok((w, h)) = crossterm::terminal::size() {
        args.extend(["-x".into(), w.to_string(), "-y".into(), h.to_string()]);
    }
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    tmux_cmd(&args_ref).map(|_| ())
}

fn exe_path() -> Result<String> {
    Ok(std::env::current_exe()?
        .to_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "los".into()))
}

fn list_session_panes(session: &str, window: &str) -> Result<Vec<(usize, String)>> {
    let stdout = tmux_cmd(&[
        "list-panes",
        "-t",
        &format!("{}:{}", session, window),
        "-F",
        "#{pane_index} #{pane_id}",
    ])?;
    let mut panes: Vec<(usize, String)> = stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let idx = parts.next()?.parse::<usize>().ok()?;
            let id = parts.next()?.to_string();
            Some((idx, id))
        })
        .collect();
    panes.sort_by_key(|(idx, _)| *idx);
    Ok(panes)
}

fn get_window_layout(session: &str, window: &str) -> Result<String> {
    let stdout = tmux_cmd(&[
        "display-message",
        "-p",
        "-t",
        &format!("{}:{}", session, window),
        "#{window_layout}",
    ])?;
    Ok(stdout.trim().to_string())
}

fn get_active_pane_index(session: &str, window: &str) -> Result<usize> {
    let stdout = tmux_cmd(&[
        "list-panes",
        "-t",
        &format!("{}:{}", session, window),
        "-F",
        "#{pane_index} #{pane_active}",
    ])?;
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(idx), Some("1")) = (parts.next(), parts.next()) {
            return idx.parse().context("invalid pane index");
        }
    }
    // pane-base-index is 1, so pane 0 is invalid. Default to first pane.
    Ok(1)
}

// ── tmux layout string parser ──────────────────────────────────────────────
//
// Tmux layout strings embed pane IDs and a checksum that are unique to each
// server instance. When a session is killed and recreated, pane IDs change and
// the checksum becomes invalid, causing `select-layout` to fail.
//
// We parse the layout string, strip pane IDs from leaf cells (they're
// session-specific), recompute the checksum over the ID-free body, and
// return a portable layout string that can be applied to any window with the
// same number of panes.

fn spawn_session_panes(panes_data: &[(&str, &str)]) -> Result<()> {
    let session = "los";
    let win = "modules";

    // Create window
    tmux_cmd(&["new-window", "-t", session, "-n", win])?;

    // Create all required panes. Rebalance after every split: repeated
    // bare splits halve the same pane and hit "no space for new pane" in
    // small windows; the saved layout is applied over this afterwards.
    for _ in 1..panes_data.len() {
        tmux_cmd(&["split-window", "-t", &format!("{}:{}", session, win)])?;
        tmux_cmd_ok(&[
            "select-layout",
            "-t",
            &format!("{}:{}", session, win),
            "tiled",
        ]);
    }

    // Enable pane borders
    tmux_cmd(&[
        "set-option",
        "-t",
        &format!("{}:{}", session, win),
        "pane-border-status",
        "top",
    ])?;
    tmux_cmd(&[
        "set-option",
        "-t",
        &format!("{}:{}", session, win),
        "pane-border-format",
        " #{pane_title} ",
    ])?;

    // Discover panes and spawn modules
    let panes = list_session_panes(session, win)?;
    let exe = exe_path()?;

    for (i, (_, pane_id)) in panes.iter().enumerate() {
        if i >= panes_data.len() {
            break;
        }
        let (cmd, label) = panes_data[i];

        let _ = tmux_cmd(&["select-pane", "-t", pane_id, "-T", label]);

        let full_cmd = format!("{} {}", exe, cmd);
        if let Err(e) = tmux_cmd(&["respawn-pane", "-k", "-t", pane_id, &full_cmd]) {
            // A single pane failure shouldn't prevent other modules from starting.
            // The pane might still have its default shell; user can check it.
            eprintln!(
                "[spawn] failed to respawn pane {} ({}): {}",
                pane_id, label, e
            );
        }
    }

    // Select first pane by ID (reliable from outside tmux)
    if let Some((_, pane_id)) = panes.first() {
        tmux_cmd(&["select-pane", "-t", pane_id])?;
    }

    Ok(())
}

/// Install global transport keys on the tmux prefix: `Ctrl-b p` toggles
/// play/pause and `Ctrl-b s` stops — but only inside the los session.
/// Outside it, the if-shell falls through to the stock tmux behavior
/// (previous-window / session chooser), so other sessions are unaffected.
fn install_transport_keys(exe: &str) {
    let in_los = "#{==:#{session_name},los}";
    tmux_cmd_ok(&[
        "bind-key",
        "p",
        "if-shell",
        "-F",
        in_los,
        &format!("run-shell -b '{} ctl toggle'", exe),
        "previous-window",
    ]);
    tmux_cmd_ok(&[
        "bind-key",
        "s",
        "if-shell",
        "-F",
        in_los,
        &format!("run-shell -b '{} ctl stop'", exe),
        "choose-tree -Zs",
    ]);
}

/// Re-apply the content-aware house sizes after a client resize. tmux only
/// rescales panes proportionally, which slowly distorts the content-sized
/// rows as the terminal changes. Acts ONLY while the session still has the
/// untouched house pane set (six panes, stock titles) and was created
/// fresh (`@los_house 1`) — loaded states and custom arrangements are left
/// to tmux's proportional rescale.
pub fn relayout() -> Result<()> {
    let house = tmux_cmd(&["show-options", "-t", "los", "-v", "@los_house"]).unwrap_or_default();
    if house.trim() != "1" {
        return Ok(());
    }
    let win = "los:modules";
    let panes = tmux_cmd(&["list-panes", "-t", win, "-F", "#{pane_id}\t#{pane_title}"])?;
    let by_title: std::collections::HashMap<&str, &str> = panes
        .lines()
        .filter_map(|l| l.split_once('\t').map(|(id, t)| (t, id)))
        .collect();
    if by_title.len() != HOUSE_TITLES.len()
        || !HOUSE_TITLES.iter().all(|t| by_title.contains_key(t))
    {
        return Ok(()); // not the house anymore — leave it be
    }
    let dims = tmux_cmd(&[
        "display-message",
        "-p",
        "-t",
        win,
        "#{window_width} #{window_height}",
    ])?;
    let mut it = dims
        .split_whitespace()
        .filter_map(|n| n.parse::<usize>().ok());
    let (w, h) = (it.next().unwrap_or(120), it.next().unwrap_or(40));
    let (row, seq_h, col, badge_h) = house_dims(w, h);
    // fixed panes get their content sizes; the SCOPE column and the
    // MATHs|MIX row absorb the slack
    tmux_cmd_ok(&[
        "resize-pane",
        "-t",
        by_title["SEQ"],
        "-y",
        &seq_h.to_string(),
    ]);
    tmux_cmd_ok(&["resize-pane", "-t", by_title["los"], "-x", &col.to_string()]);
    tmux_cmd_ok(&[
        "resize-pane",
        "-t",
        by_title["los"],
        "-y",
        &badge_h.to_string(),
    ]);
    tmux_cmd_ok(&[
        "resize-pane",
        "-t",
        by_title["VOICE 0"],
        "-y",
        &row.to_string(),
    ]);
    Ok(())
}

/// Re-run the house sizing whenever the attached client resizes.
/// `house` marks whether the session has the fresh house layout (loaded
/// states keep their own arrangement and skip the recompute).
fn install_resize_hook(exe: &str, house: bool) {
    tmux_cmd_ok(&[
        "set-option",
        "-t",
        "los",
        "@los_house",
        if house { "1" } else { "0" },
    ]);
    tmux_cmd_ok(&[
        "set-hook",
        "-t",
        "los",
        "client-resized",
        &format!("run-shell -b '{} relayout'", exe),
    ]);
}

/// Canonical module name for a pane title or CLI word. Display titles
/// (SEQ, MIX, MATHs, los) and aliases (sto, seq) all map to the one name
/// that save, load, and dispatch agree on — pane titles are the save
/// format's source of truth, so this is what keeps the round-trip closed.
pub fn canonical_module(name: &str) -> Option<&'static str> {
    Some(match name.to_lowercase().as_str() {
        "sequencer" | "seq" => "sequencer",
        "voice" | "sto" => "voice",
        "mixer" | "mix" => "mixer",
        "scope" => "scope",
        "envelope" | "maths" => "envelope",
        "badge" | "los" => "badge",
        "tone" => "tone",
        "template" | "example" => "template",
        "delay" | "288" | "mdp" => "delay",
        "filterbank" | "bank" | "296" | "296e" => "filterbank",
        "tape" | "recorder" | "4track" => "tape",
        "swarm" | "brass" | "cs80" => "swarm",
        "conductor" => "conductor",
        _ => return None,
    })
}

/// Module types the conductor (and `los add`) can spawn at runtime.
pub const ADDABLE_MODULES: &[&str] = &[
    "voice",
    "envelope",
    "sequencer",
    "scope",
    "tone",
    "badge",
    "template",
    "delay",
    "filterbank",
    "tape",
    "swarm",
];

/// Spawn a new module instance in a fresh pane of the modules window.
/// Picks the next free instance number from the manifest when not given.
pub fn add_module(module: &str, instance: Option<usize>) -> Result<()> {
    anyhow::ensure!(
        ADDABLE_MODULES.contains(&module),
        "unknown module {module} (addable: {})",
        ADDABLE_MODULES.join(", ")
    );
    let instance = instance.unwrap_or_else(|| {
        Manifest::open()
            .map(|m| {
                m.entries()
                    .iter()
                    .filter(|e| e.module_name == module)
                    .map(|e| e.instance + 1)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    });
    let exe = exe_path()?;
    let title = format!("{} {}", capitalize(module), instance);
    // split, set the title, spawn, retile
    let pane_id = tmux_cmd(&[
        "split-window",
        "-t",
        "los:modules",
        "-P",
        "-F",
        "#{pane_id}",
        &format!("{} {} {}", exe, module, instance),
    ])?;
    tmux_cmd_ok(&["select-pane", "-t", pane_id.trim(), "-T", &title]);
    tmux_cmd_ok(&["select-layout", "-t", "los:modules", "tiled"]);
    Ok(())
}

/// Cleanly remove a running module: save its state, then kill its pane
/// (process exit unregisters it from the manifest).
pub fn remove_module(module: &str, instance: usize) -> Result<()> {
    if let Some(pid) = Manifest::open().ok().and_then(|m| {
        m.entries()
            .iter()
            .find(|e| e.module_name == module && e.instance == instance)
            .map(|e| e.pid)
    }) {
        state::send_save_signal(pid);
        std::thread::sleep(Duration::from_millis(300));
    }
    let title = format!("{} {}", capitalize(module), instance);
    let stdout = tmux_cmd(&[
        "list-panes",
        "-t",
        "los:modules",
        "-F",
        "#{pane_id}\t#{pane_title}",
    ])?;
    for line in stdout.lines() {
        if let Some((pane_id, pane_title)) = line.split_once('\t') {
            if pane_title.trim() == title {
                tmux_cmd(&["kill-pane", "-t", pane_id])?;
                tmux_cmd_ok(&["select-layout", "-t", "los:modules", "tiled"]);
                return Ok(());
            }
        }
    }
    anyhow::bail!("no pane titled '{}' found", title)
}

/// Theme the tmux shell itself (design-language.md §8): dim ink borders,
/// amber active edge, brand-voice status bar. Session-scoped options only —
/// other tmux sessions keep their look.
fn install_shell_theme() {
    let t = |args: &[&str]| tmux_cmd_ok(args);
    // Mouse: click focuses panes (tmux native) and events pass through to
    // the modules (wheel = adjust, click = select, drag = slide).
    t(&["set-option", "-t", "los", "mouse", "on"]);
    t(&[
        "set-option",
        "-w",
        "-t",
        "los:modules",
        "pane-border-style",
        "fg=#4a4438,bg=#070605",
    ]);
    t(&[
        "set-option",
        "-w",
        "-t",
        "los:modules",
        "pane-active-border-style",
        "fg=#e3a818,bold,bg=#1b1610",
    ]);
    t(&[
        "set-option",
        "-w",
        "-t",
        "los:modules",
        "pane-border-format",
        " #{pane_title} ",
    ]);
    t(&[
        "set-option",
        "-t",
        "los",
        "status-style",
        "fg=#9a7b2d,bg=#0d0b08",
    ]);
    t(&[
        "set-option",
        "-t",
        "los",
        "status-left",
        "#[fg=#e3a818,bold] los #[fg=#9a7b2d]· ",
    ]);
    t(&[
        "set-option",
        "-t",
        "los",
        "status-right",
        "#[fg=#c45dd4]♪ #{session_name} ",
    ]);
    t(&[
        "set-option",
        "-t",
        "los",
        "window-status-current-style",
        "fg=#e8dcc8,bold",
    ]);
    t(&[
        "set-option",
        "-t",
        "los",
        "window-status-style",
        "fg=#7d7363",
    ]);
    // Active-pane clarity: inactive panes sink into near-black with dimmed
    // ink; the active pane sits on a clearly lifted warm charcoal with full
    // ink — plus the bold amber border. Unmissable.
    t(&[
        "set-option",
        "-w",
        "-t",
        "los:modules",
        "window-style",
        "fg=#8d8170,bg=#070605",
    ]);
    t(&[
        "set-option",
        "-w",
        "-t",
        "los:modules",
        "window-active-style",
        "fg=#e8dcc8,bg=#1b1610",
    ]);
}

/// Build the default window layout:
///   ┌──────┬───────────────┐
///   │ los  │               │
///   ├──────┤      MIX      │
///   │SCOPE │               │
///   ├──────┴──┬────────────┤
///   │  VOICE  │   MATHs    │
///   ├─────────┴────────────┤
///   │         SEQ          │
///   └──────────────────────┘
/// The house pane set. One source of truth: build_house_layout titles its
/// panes from this set, relayout() recognizes the untouched house by it,
/// and the save round-trip test walks it. Add a pane here when the house
/// grows.
pub const HOUSE_TITLES: [&str; 7] = ["SEQ", "VOICE 0", "VOICE 1", "MATHs", "MIX", "los", "SCOPE"];

/// The fx rack window's pane titles ("fx" window, behind the console):
/// the two fx processors, two modulators to wiggle them, and a second
/// sequencer whose tracks (sources 8–15) can step fx params without
/// touching the voices.
pub const FX_TITLES: [&str; 5] = ["DELAY", "BANK", "MATHs 1", "MATHs 2", "SEQ 1"];

/// The record window's pane titles: the deck and a MATHs for
/// automation patching (tape faders/speed take `@` bindings).
pub const TAPE_TITLES: [&str; 2] = ["TAPE", "MATHs 3"];

/// Content-aware split sizes for the house layout: `(row1, seq, left_col,
/// badge)` in cells, given the window size. SEQ snaps to its content
/// (≤15: header + 8 tracks + detail strip + modeline); the rest splits
/// roughly evenly between row 1 (badge+scope column | voices) and row 2
/// (MATHs | MIX) — the badge is fixed and the scope takes the column's
/// remainder, so badge + scope always equal the voices' height. Pinned
/// modelines keep the breathing room inside each pane tidy.
fn house_dims(w: usize, h: usize) -> (usize, usize, usize, usize) {
    let seq = ((h * 28) / 100).clamp(5, crate::sequencer::CONTENT_LINES);
    let top = h.saturating_sub(seq + 1);
    let row1 = ((top * 52) / 100).clamp(8.min(top.max(1)), 28);
    let col = (w / 4).clamp(20.min(w / 2), 48);
    let badge = ((h * 16) / 100).clamp(4, 10);
    (row1, seq, col, badge)
}

fn build_house_layout(exe: &str) -> Result<()> {
    let win = "los:modules";
    tmux_cmd(&["new-window", "-t", "los", "-n", "modules"])?;
    tmux_cmd(&["set-option", "-w", "-t", win, "pane-border-status", "top"])?;
    tmux_cmd(&[
        "set-option",
        "-w",
        "-t",
        win,
        "pane-border-format",
        " #{pane_title} ",
    ])?;

    let dims = tmux_cmd(&[
        "display-message",
        "-p",
        "-t",
        win,
        "#{window_width} #{window_height}",
    ])?;
    let mut it = dims
        .split_whitespace()
        .filter_map(|n| n.parse::<usize>().ok());
    let (w, h) = (it.next().unwrap_or(120), it.next().unwrap_or(40));
    let (row, seq_h, col, badge_h) = house_dims(w, h);

    // the base pane ends up as the MATHs row (respawned at the end — the
    // base pane is the one split-window never gives a command to)
    let base = tmux_cmd(&["list-panes", "-t", win, "-F", "#{pane_id}"])?
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    // SEQ pinned at the bottom, snapped to its content height
    let seq = tmux_cmd(&[
        "split-window",
        "-t",
        &base,
        "-v",
        "-l",
        &seq_h.to_string(),
        "-P",
        "-F",
        "#{pane_id}",
        &format!("{} sequencer 0", exe),
    ])?;
    let seq = seq.trim().to_string();
    // row 1 above the MATHs|MIX row: badge+scope column and the voices,
    // all one height
    let row1 = tmux_cmd(&[
        "split-window",
        "-t",
        &base,
        "-v",
        "-b",
        "-l",
        &row.to_string(),
        "-P",
        "-F",
        "#{pane_id}",
        &format!("{} voice 0", exe),
    ])?;
    let row1 = row1.trim().to_string();
    // left column inside row 1: badge fixed on top, scope the remainder —
    // together they are exactly as tall as the voices
    let badge = tmux_cmd(&[
        "split-window",
        "-t",
        &row1,
        "-h",
        "-b",
        "-l",
        &col.to_string(),
        "-P",
        "-F",
        "#{pane_id}",
        &format!("{} badge 0", exe),
    ])?;
    let badge = badge.trim().to_string();
    let scope_h = row.saturating_sub(badge_h + 1).max(2);
    let scope = tmux_cmd(&[
        "split-window",
        "-t",
        &badge,
        "-v",
        "-l",
        &scope_h.to_string(),
        "-P",
        "-F",
        "#{pane_id}",
        &format!("{} scope 0", exe),
    ])?;
    // halve what remains of the row AFTER the badge column is carved, so
    // both voices come out the same width
    let voice1 = tmux_cmd(&[
        "split-window",
        "-t",
        &row1,
        "-h",
        "-l",
        "50%",
        "-P",
        "-F",
        "#{pane_id}",
        &format!("{} voice 1", exe),
    ])?;
    let mix = tmux_cmd(&[
        "split-window",
        "-t",
        &base,
        "-h",
        "-l",
        "50%",
        "-P",
        "-F",
        "#{pane_id}",
        &format!("{} mixer 0", exe),
    ])?;
    tmux_cmd(&[
        "respawn-pane",
        "-k",
        "-t",
        &base,
        &format!("{} envelope 0", exe),
    ])?;

    // titles must stay in sync with HOUSE_TITLES (relayout and the save
    // round-trip recognize the house by them)
    for (id, title) in [
        (seq.as_str(), "SEQ"),
        (row1.as_str(), "VOICE 0"),
        (voice1.trim(), "VOICE 1"),
        (base.as_str(), "MATHs"),
        (mix.trim(), "MIX"),
        (badge.as_str(), "los"),
        (scope.trim(), "SCOPE"),
    ] {
        tmux_cmd_ok(&["select-pane", "-t", id, "-T", title]);
    }
    tmux_cmd_ok(&["select-pane", "-t", &seq]);
    Ok(())
}

/// Seed the fresh session's curated default patch — "the house drone".
///
/// A slow, melodic, evolving piece (à la Alessandro Cortini) that plays
/// the moment the session opens and demonstrates the rig's depth:
/// pattern slots a–d on the melody, macros sequenced by the macro lane
/// (the 16-bar form runs itself), probability and ratchets for life,
/// a drunk modulation track sweeping the filterbank's window, polymetric
/// ping-pong bass, and the fx chain wired so everything breathes:
///
///   voices → send A (30%) ───────────────► DELAY ─► console
///   voices → send B (25%) ─► BANK ─► console ─► send A (30%) ─► DELAY
///   MATHs 1 ch1 (looping) ─► bank morph · seq t2 (drunk) ─► bank wcent
fn write_house_patch() {
    let h = house_patch_params();
    let _ = state::save_module_state("sequencer", 0, &h.sequencer0);
    let _ = state::save_module_state("delay", 0, &h.delay0);
    let _ = state::save_module_state("filterbank", 0, &h.filterbank0);
    let _ = state::save_module_state("envelope", 1, &h.envelope1);
    let _ = state::save_module_state("voice", 0, &h.voice0);
    let _ = state::save_module_state("voice", 1, &h.voice1);
    let _ = state::save_module_state("envelope", 0, &h.envelope0);
    let _ = state::save_module_state("envelope", 3, &h.envelope3);
    let _ = state::save_module_state("tape", 0, &h.tape0);
}

/// The house drone's module params, one field per state file the fresh
/// session seeds. Pure data — [`write_house_patch`] saves it to tmp
/// state, [`house_session_state`] wraps it into a loadable session
/// (committed at examples/house-drone.toml).
pub struct HousePatch {
    pub sequencer0: state::SequencerParams,
    pub delay0: state::DelayParams,
    pub filterbank0: state::FilterbankParams,
    pub envelope0: state::EnvelopeParams,
    pub envelope1: state::EnvelopeParams,
    pub envelope3: state::EnvelopeParams,
    pub voice0: state::VoiceParams,
    pub voice1: state::VoiceParams,
    pub tape0: state::TapeParams,
}

/// Build the house drone (see [`write_house_patch`] for the tour).
pub fn house_patch_params() -> HousePatch {
    use state::{DelayTapParam, EnvelopeChannelParams, MacroCmd, MacroParam, Quant, SlotParam,
        StepParam, TrackParam};
    use state::{CycleMode, DelayUnit, TrackMode};

    // ── helpers: a silent step, a note, an ornament ────────────────────
    let off = || StepParam {
        active: false,
        note: 60,
        velocity: 100,
        mod_value: 0.0,
        prob: 100,
        bind: None,
        delay: 0.0,
        delay_unit: DelayUnit::Ms,
        delay_prob: 100,
        repeats: 1,
        repeat_prob: 100,
    };
    let note = |n: u8, v: u8| StepParam { active: true, note: n, velocity: v, ..off() };
    let maybe = |n: u8, v: u8, p: u8| StepParam { prob: p, ..note(n, v) };

    // ── sequencer 0: the piece (A minor, 74 BPM) ───────────────────────
    // t1 — the melody voice 0 plays. Four pattern slots, switched by the
    // macro lane over a 16-bar form:
    //   a: the theme   b: fuller arp   c: high shimmer   d: breakdown
    let mut a = vec![off(); 16];
    a[0] = note(57, 92); // A3 — the anchor
    a[3] = maybe(60, 68, 85); // C4, usually
    a[6] = note(64, 78); // E4
    a[10] = maybe(55, 62, 70); // G3, often
    a[12] = note(57, 84);
    a[14] = StepParam { delay: 45.0, delay_prob: 80, ..maybe(59, 55, 45) }; // late B3 ornament

    let mut b = vec![off(); 16];
    for (i, n, v) in [(0usize, 57u8, 120u8), (2, 60, 86), (4, 64, 104), (6, 67, 80),
        (8, 69, 112), (10, 67, 78), (12, 64, 96), (14, 60, 72)]
    {
        b[i] = note(n, v);
    }
    b[6].prob = 75;
    b[10].prob = 60;
    // a pushed echo-double of the E4: ~60% late, usually — the loose
    // repeat (the delay line answers it again at 152 ms)
    b[13] = StepParam { delay: 60.0, delay_unit: DelayUnit::Pct, delay_prob: 85, ..maybe(64, 52, 70) };

    let mut c = vec![off(); 16];
    c[0] = maybe(69, 58, 90); // A4
    c[5] = maybe(72, 48, 65); // C5
    c[8] = maybe(76, 52, 75); // E5
    c[11] = StepParam { delay: 60.0, delay_prob: 100, ..maybe(71, 40, 50) }; // pushed B4
    c[14] = maybe(64, 45, 55);

    let mut d = vec![off(); 16];
    d[0] = note(45, 44); // A2 — just the root, hushed
    d[8] = maybe(52, 30, 80); // E3, barely there

    // slot e — the ghost: one breath per pattern, for the outro floor
    let mut gh = vec![off(); 16];
    gh[0] = note(45, 34);

    let t1 = TrackParam {
        steps: a,
        length: Some(16),
        pulses: None,
        rotation: None,
        muted: false,
        mode: TrackMode::Note,
        cycle: CycleMode::Forward,
        scale: None,
        scale_cents: vec![],
        scale_period: None,
        root: None,
        active_slot: 0,
        slots: vec![
            SlotParam { slot: 1, steps: b, length: Some(16), pulses: None, rotation: None },
            SlotParam { slot: 2, steps: c, length: Some(16), pulses: None, rotation: None },
            SlotParam { slot: 3, steps: d, length: Some(16), pulses: None, rotation: None },
            SlotParam { slot: 4, steps: gh, length: Some(16), pulses: None, rotation: None },
        ],
        swing: 50,
        groove: None,
        humanize: 2.5,
        ratchet_decay: 35,
    };

    // t2 — a drunk modulation track wandering the filterbank's window
    // (wcent is bound to sequencer/0/t2 below): the spectrum strolls.
    let walk = [0.18, 0.62, 0.35, 0.8, 0.5, 0.28, 0.7, 0.42];
    let t2 = TrackParam {
        steps: walk
            .iter()
            .map(|m| StepParam { active: true, mod_value: *m, ..off() })
            .collect(),
        length: Some(8),
        pulses: None,
        rotation: None,
        muted: false,
        mode: TrackMode::Modulation,
        cycle: CycleMode::Drunk,
        scale: None,
        scale_cents: vec![],
        scale_period: None,
        root: None,
        active_slot: 0,
        slots: vec![],
        swing: 50,
        groove: None,
        humanize: 0.0,
        ratchet_decay: 0,
    };

    // t3 — the bass voice 1 plays: 12 steps against t1's 16 (polymeter),
    // ping-ponging, with a rare ratcheted approach note.
    let mut bass = vec![off(); 12];
    bass[0] = note(33, 100); // A1
    bass[4] = maybe(40, 72, 85); // E2
    bass[8] = maybe(43, 58, 60); // G2
    bass[10] = StepParam { delay: 55.0, delay_unit: DelayUnit::Pct, delay_prob: 100, ..maybe(38, 56, 45) }; // pushed D2 lean-in

    // the peak riff: denser, hotter — macro f swaps it in, a/e/h swap out
    let mut bass_peak = vec![off(); 12];
    bass_peak[0] = note(33, 115); // A1, leaning in
    bass_peak[2] = maybe(33, 84, 70);
    bass_peak[4] = note(40, 104); // E2
    bass_peak[6] = maybe(43, 80, 80); // G2
    bass_peak[8] = note(45, 96); // A2
    bass_peak[10] = maybe(38, 92, 85); // D2
    let t3 = TrackParam {
        steps: bass,
        length: Some(12),
        pulses: None,
        rotation: None,
        muted: false,
        mode: TrackMode::Note,
        cycle: CycleMode::PingPong,
        scale: None,
        scale_cents: vec![],
        scale_period: None,
        root: None,
        active_slot: 0,
        slots: vec![SlotParam {
            slot: 1,
            steps: bass_peak,
            length: Some(12),
            pulses: None,
            rotation: None,
        }],
        swing: 54,
        groove: None,
        humanize: 1.5,
        ratchet_decay: -25, // bass ratchets crescendo into the beat
    };

    // macros: the form's verbs. The lane fires them bar by bar, so the
    // piece evolves on its own — and they're yours to fire with @a–@d.
    // macros: the song's verbs — each one is a SECTION (slot, mutes,
    // bass behavior, tempo). The lane walks them through a 128-bar form
    // (~7 minutes with the tempo moves), and they're yours live on @a–@h.
    let mac = |id: &str, cmds: Vec<MacroCmd>| MacroParam {
        id: id.into(),
        quant: Quant::Bar,
        cmds,
    };
    let macros = vec![
        // a — the theme: both voices, ping-pong bass, home tempo
        mac("a", vec![
            MacroCmd::SwitchPattern { track: 0, slot: 0 },
            MacroCmd::SwitchPattern { track: 2, slot: 0 },
            MacroCmd::SetMute { track: 0, muted: false },
            MacroCmd::SetMute { track: 2, muted: false },
            MacroCmd::SetCycle { track: 2, mode: CycleMode::PingPong },
            MacroCmd::SetBpm { bpm: 74.0 },
        ]),
        // b — the build: fuller arp, a touch faster
        mac("b", vec![
            MacroCmd::SwitchPattern { track: 0, slot: 1 },
            MacroCmd::SetMute { track: 0, muted: false },
            MacroCmd::SetBpm { bpm: 78.0 },
        ]),
        // c — shimmer: high sparse line over reversed bass
        mac("c", vec![
            MacroCmd::SwitchPattern { track: 0, slot: 2 },
            MacroCmd::SetMute { track: 0, muted: false },
            MacroCmd::SetCycle { track: 2, mode: CycleMode::Reverse },
            MacroCmd::SetBpm { bpm: 74.0 },
        ]),
        // d — thin: root drones only, slowing
        mac("d", vec![
            MacroCmd::SwitchPattern { track: 0, slot: 3 },
            MacroCmd::SetMute { track: 0, muted: false },
            MacroCmd::SetBpm { bpm: 70.0 },
        ]),
        // e — BASS ONLY: melody out, bass walks forward, slow
        mac("e", vec![
            MacroCmd::SwitchPattern { track: 2, slot: 0 },
            MacroCmd::SetMute { track: 0, muted: true },
            MacroCmd::SetMute { track: 2, muted: false },
            MacroCmd::SetCycle { track: 2, mode: CycleMode::Forward },
            MacroCmd::SetBpm { bpm: 64.0 },
        ]),
        // f — the peak: fast, arp on, the bass riff in, drunk motion
        mac("f", vec![
            MacroCmd::SwitchPattern { track: 0, slot: 1 },
            MacroCmd::SwitchPattern { track: 2, slot: 1 },
            MacroCmd::SetMute { track: 0, muted: false },
            MacroCmd::SetCycle { track: 2, mode: CycleMode::Drunk },
            MacroCmd::SetBpm { bpm: 90.0 },
        ]),
        // g — the outro: one ghost note alone, very slow
        mac("g", vec![
            MacroCmd::SwitchPattern { track: 0, slot: 4 },
            MacroCmd::SetMute { track: 0, muted: false },
            MacroCmd::SetMute { track: 2, muted: true },
            MacroCmd::SetBpm { bpm: 58.0 },
        ]),
        // h — the swell return: shimmer with the bass back
        mac("h", vec![
            MacroCmd::SwitchPattern { track: 0, slot: 2 },
            MacroCmd::SwitchPattern { track: 2, slot: 0 },
            MacroCmd::SetMute { track: 0, muted: false },
            MacroCmd::SetMute { track: 2, muted: false },
            MacroCmd::SetCycle { track: 2, mode: CycleMode::PingPong },
            MacroCmd::SetBpm { bpm: 70.0 },
        ]),
    ];
    // the 128-bar form (one lane slot per bar):
    //   intro a ×12 · build b ×16 · shimmer c ×12 · thin d ×12 ·
    //   BASS ONLY e ×12 · theme a ×8 · PEAK f ×20 · return h ×12 ·
    //   thin d ×12 · outro g ×12
    let mut lane = vec![String::new(); 128];
    for (bar, m) in [
        (0, "a"),
        (12, "b"),
        (28, "c"),
        (40, "d"),
        (52, "e"),
        (64, "a"),
        (72, "f"),
        (92, "h"),
        (104, "d"),
        (116, "g"),
    ] {
        lane[bar] = m.to_string();
    }

    let seq = state::SequencerParams {
        bpm: Some(74.0),
        playing: Some(true), // the house opens already breathing
        euclidean_pulses: None,
        euclidean_length: None,
        euclidean_rotation: None,
        steps: vec![],
        tracks: vec![t1, t2, t3],
        macros,
        lane,
        lane_len: Some(128),
    };

    // ── delay 0: a tasteful echo on send A — wet-only, real feedback,
    // tap levels decaying with a bloom on tap 8, pans widening with age
    let levels = [0.85, 0.5, 0.65, 0.4, 0.55, 0.35, 0.45, 0.75];
    let pans = [-0.15, 0.18, -0.25, 0.3, -0.35, 0.4, -0.45, 0.5];
    let delay = state::DelayParams {
        format: state::STATE_FORMAT,
        time: Some(0.152),
        regen: Some(0.5),
        shim: Some(0.0),
        wash: Some(0.12),
        dry: Some(0.0),
        taps: Some(8),
        input: Some(String::from("send/0")),
        tap: (0..8)
            .map(|i| DelayTapParam {
                level: levels[i],
                pan: pans[i],
                phase: String::from("+"),
                pan_src: None,
                level_src: None,
            })
            .collect(),
        ..Default::default()
    };

    // ── filterbank 0: on send B, wet-only, twice alive — morph breathes
    // under MATHs 1's looping channel, and the 296-style window strolls
    // the spectrum on the sequencer's drunk t2
    // the bank must be a CHARACTER, not seasoning: a narrow resonant
    // window strolling the spectrum, hard odd|even split, long follower
    // decay, and the per-band time spread smearing everything it passes
    // — then the whole animation echoes into the delay via its return
    let bank = state::FilterbankParams {
        format: state::STATE_FORMAT,
        morph: Some(0.0),
        wwidth: Some(0.38),
        split: Some(0.6),
        spread: Some(0.3),
        dry: Some(0.0),
        decay: Some(0.5),
        input: Some(String::from("send/1")),
        morph_src: Some(String::from("envelope/1/ch1")),
        wcent_src: Some(String::from("sequencer/0/t2")),
        ..Default::default()
    };

    // ── MATHs 1: channel 1 cycling slowly — the LFO behind the morph
    let ch1 = EnvelopeChannelParams {
        rise: 0.72,
        fall: 0.72,
        loop_mode: true,
        ..Default::default()
    };
    let env1 = state::EnvelopeParams {
        format: state::STATE_FORMAT,
        channels: vec![
            ch1,
            EnvelopeChannelParams::default(),
            EnvelopeChannelParams::default(),
            EnvelopeChannelParams::default(),
        ],
        logic_outputs: Default::default(),
    };

    // ── voices: melody soft-edged, bass heavy on the sub osc. Format-2
    // states replace bindings wholesale, so the stock cables (amp ←
    // MATHs ch 2i+1, notes ← track 2i+1) ride along explicitly.
    let voice0 = state::VoiceParams {
        format: state::STATE_FORMAT,
        shape: Some(0.35),
        sub: Some(0.25),
        lpg: Some(0.55),
        amp_src: Some(String::from("envelope/0/ch1")),
        notes_src: Some(String::from("sequencer/0/t1")),
        ..Default::default()
    };
    let voice1 = state::VoiceParams {
        format: state::STATE_FORMAT,
        shape: Some(0.1),
        sub: Some(0.85),
        lpg: Some(0.35),
        amp_src: Some(String::from("envelope/0/ch3")),
        notes_src: Some(String::from("sequencer/0/t3")),
        ..Default::default()
    };

    // ── MATHs 0: the amp envelopes. The bass swell was so slow the sub
    // never opened inside a step — both channels now bloom fast and
    // ring long. Triggers ride along explicitly (ch_i ← t_i, the stock
    // cables) for the same format-2 reason as the voices.
    let amp_ch = |rise: f32, fall: f32, track: &str| EnvelopeChannelParams {
        rise,
        fall,
        trigger_src: Some(format!("sequencer/0/{}", track)),
        ..Default::default()
    };
    let env0 = state::EnvelopeParams {
        format: state::STATE_FORMAT,
        channels: vec![
            amp_ch(0.3, 0.62, "t1"),  // melody: quick bloom, singing tail
            amp_ch(0.5, 0.5, "t2"),
            amp_ch(0.24, 0.8, "t3"),  // bass: speaks NOW, rings long
            amp_ch(0.5, 0.5, "t4"),
        ],
        logic_outputs: Default::default(),
    };

    // ── MATHs 3 (the record window's modulator): channel 1 loops
    // minutes-long with the attenuverter pulled down, ready to patch
    // (it shipped bound to the delay's wash once — and its random boot
    // phase kept filling the form's quiet sections; the composition
    // owns the dynamics now, the cable is yours to plug)
    let env3 = state::EnvelopeParams {
        format: state::STATE_FORMAT,
        channels: vec![
            EnvelopeChannelParams {
                rise: 0.88,
                fall: 0.88,
                loop_mode: true,
                attenuverter: 0.35,
                ..Default::default()
            },
            EnvelopeChannelParams::default(),
            EnvelopeChannelParams::default(),
            EnvelopeChannelParams::default(),
        ],
        logic_outputs: Default::default(),
    };

    // ── tape 0: ready to record. Track 1 armed to the mix, the loop
    // wrapped around the drone's 16-bar form — roll the transport, hit
    // r on the TAPE pane, and 52 seconds later you have a take.
    // loop OFF: r captures the form once, top to tail (~7 min); flip
    // L on for OP-1-style sound-on-sound layering instead
    let bar_frames = (60.0 / 74.0 * 4.0 * 48_000.0) as u64;
    let tape = state::TapeParams {
        format: state::STATE_FORMAT,
        speed: Some(1.0),
        loop_on: Some(false),
        loop_in: Some(0),
        loop_out: Some(bar_frames * 128),
        speed_src: None,
        tracks: (0..crate::tape::TRACKS)
            .map(|i| state::TapeTrackParam {
                input: None, // the mix
                fader: 0.8,
                pan: 0.0,
                armed: i == 0,
                muted: false,
                reversed: false,
                monitor: true,
                fader_src: None,
                pan_src: None,
                auto: vec![],
            })
            .collect(),
    };

    HousePatch {
        sequencer0: seq,
        delay0: delay,
        filterbank0: bank,
        envelope0: env0,
        envelope1: env1,
        envelope3: env3,
        voice0,
        voice1,
        tape0: tape,
    }
}

/// The house drone as a loadable session file — the worked example
/// committed at examples/house-drone.toml. The pane roster mirrors
/// [`create_session`]'s three windows (modules / fx / tape); layouts
/// stay empty so `los load` falls back to tiled. Regenerate the file
/// with: cargo test regen_house_example -- --ignored
pub fn house_session_state() -> Result<state::SessionState> {
    let h = house_patch_params();
    let pane = |module: &str, instance: usize| state::PaneState {
        module: module.into(),
        instance,
        patch: None,
        patch_inline: None,
    };
    fn patched<T: serde::Serialize>(
        module: &str,
        instance: usize,
        params: &T,
    ) -> Result<state::PaneState> {
        Ok(state::PaneState {
            module: module.into(),
            instance,
            patch: None,
            patch_inline: Some(
                toml::Value::try_from(params).context("serializing house params")?,
            ),
        })
    }
    Ok(state::SessionState {
        meta: state::Meta {
            name: String::from("house-drone"),
            created: String::new(),
            format: state::STATE_FORMAT,
        },
        tmux: state::TmuxState::default(),
        windows: vec![
            state::WindowState {
                name: String::from("modules"),
                layout: String::new(),
                active_pane: 0,
                panes: vec![
                    patched("sequencer", 0, &h.sequencer0)?,
                    patched("voice", 0, &h.voice0)?,
                    patched("voice", 1, &h.voice1)?,
                    patched("envelope", 0, &h.envelope0)?,
                    pane("mixer", 0),
                    pane("scope", 0),
                    pane("badge", 0),
                ],
            },
            state::WindowState {
                name: String::from("fx"),
                layout: String::new(),
                active_pane: 0,
                panes: vec![
                    patched("delay", 0, &h.delay0)?,
                    patched("filterbank", 0, &h.filterbank0)?,
                    patched("envelope", 1, &h.envelope1)?,
                    pane("envelope", 2),
                    pane("sequencer", 1),
                ],
            },
            state::WindowState {
                name: String::from("tape"),
                layout: String::new(),
                active_pane: 0,
                panes: vec![
                    patched("tape", 0, &h.tape0)?,
                    patched("envelope", 3, &h.envelope3)?,
                ],
            },
        ],
    })
}

/// Rewrite every float in a TOML tree as its shortest f32-faithful
/// decimal. Params are f32 at runtime; without this the example file
/// reads like `0.15199999511241913` instead of `0.152`.
pub fn round_floats_to_f32(v: &mut toml::Value) {
    match v {
        toml::Value::Float(f) => {
            if let Ok(p) = format!("{}", *f as f32).parse::<f64>() {
                *f = p;
            }
        }
        toml::Value::Array(items) => items.iter_mut().for_each(round_floats_to_f32),
        toml::Value::Table(table) => table
            .iter_mut()
            .for_each(|(_, v)| round_floats_to_f32(v)),
        toml::Value::String(_)
        | toml::Value::Integer(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_) => {}
    }
}

/// The fx rack window:
///   ┌────────────┬──────────┐
///   │   DELAY    │ MATHs 1  │
///   ├────────────┼──────────┤
///   │   BANK     │ MATHs 2  │
///   │            ├──────────┤
///   │            │  SEQ 1   │
///   └────────────┴──────────┘
fn build_fx_window(exe: &str) -> Result<()> {
    let win = "los:fx";
    tmux_cmd(&["new-window", "-t", "los", "-n", "fx"])?;
    tmux_cmd(&["set-option", "-w", "-t", win, "pane-border-status", "top"])?;
    tmux_cmd(&[
        "set-option",
        "-w",
        "-t",
        win,
        "pane-border-format",
        " #{pane_title} ",
    ])?;

    let base = tmux_cmd(&["list-panes", "-t", win, "-F", "#{pane_id}"])?
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    // right column: the modulators (MATHs 1 / MATHs 2 / SEQ 1)
    let maths1 = tmux_cmd(&[
        "split-window",
        "-t",
        &base,
        "-h",
        "-l",
        "38%",
        "-P",
        "-F",
        "#{pane_id}",
        &format!("{} envelope 1", exe),
    ])?;
    let maths1 = maths1.trim().to_string();
    let maths2 = tmux_cmd(&[
        "split-window",
        "-t",
        &maths1,
        "-v",
        "-l",
        "62%",
        "-P",
        "-F",
        "#{pane_id}",
        &format!("{} envelope 2", exe),
    ])?;
    let maths2 = maths2.trim().to_string();
    let seq1 = tmux_cmd(&[
        "split-window",
        "-t",
        &maths2,
        "-v",
        "-l",
        "45%",
        "-P",
        "-F",
        "#{pane_id}",
        &format!("{} sequencer 1", exe),
    ])?;
    // left column: DELAY over BANK
    let bank = tmux_cmd(&[
        "split-window",
        "-t",
        &base,
        "-v",
        "-l",
        "50%",
        "-P",
        "-F",
        "#{pane_id}",
        &format!("{} filterbank 0", exe),
    ])?;
    tmux_cmd(&[
        "respawn-pane",
        "-k",
        "-t",
        &base,
        &format!("{} delay 0", exe),
    ])?;

    // titles must stay in sync with FX_TITLES (the save round-trip
    // parses them back into module names + instances)
    for (id, title) in [
        (base.as_str(), "DELAY"),
        (bank.trim(), "BANK"),
        (maths1.as_str(), "MATHs 1"),
        (maths2.as_str(), "MATHs 2"),
        (seq1.trim(), "SEQ 1"),
    ] {
        tmux_cmd_ok(&["select-pane", "-t", id, "-T", title]);
    }
    tmux_cmd_ok(&["select-pane", "-t", &base]);
    Ok(())
}

/// The record window: the deck across the left, MATHs 3 riding along
/// for automation cables.
fn build_tape_window(exe: &str) -> Result<()> {
    let win = "los:tape";
    tmux_cmd(&["new-window", "-t", "los", "-n", "tape"])?;
    tmux_cmd(&["set-option", "-w", "-t", win, "pane-border-status", "top"])?;
    tmux_cmd(&["set-option", "-w", "-t", win, "pane-border-format", " #{pane_title} "])?;
    let base = tmux_cmd(&["list-panes", "-t", win, "-F", "#{pane_id}"])?
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    let maths = tmux_cmd(&[
        "split-window",
        "-t",
        &base,
        "-h",
        "-l",
        "30%",
        "-P",
        "-F",
        "#{pane_id}",
        &format!("{} envelope 3", exe),
    ])?;
    tmux_cmd(&["respawn-pane", "-k", "-t", &base, &format!("{} tape 0", exe)])?;
    for (id, title) in [(base.as_str(), "TAPE"), (maths.trim(), "MATHs 3")] {
        tmux_cmd_ok(&["select-pane", "-t", id, "-T", title]);
    }
    tmux_cmd_ok(&["select-pane", "-t", &base]);
    Ok(())
}

pub fn create_session() -> Result<()> {
    state::ensure_dirs()?;
    let _ = tmux_cmd(&["kill-session", "-t", "los"]);
    write_house_patch();

    // Create conductor window
    new_los_session("conductor")?;

    // Start conductor TUI in its pane
    let panes = list_session_panes("los", "conductor")?;
    let exe = exe_path()?;
    if let Some((_, pane_id)) = panes.first() {
        tmux_cmd(&[
            "respawn-pane",
            "-k",
            "-t",
            pane_id,
            &format!("{} conductor", exe),
        ])?;
    }

    // Spawn module panes (each with instance 0 by default)
    // The house layout (SEQ content-sized; rows 1 and 2 share the rest;
    // badge + scope stack to exactly the voices' height):
    //   ┌───────┬─────────┬─────────┐
    //   │ los   │ VOICE 0 │ VOICE 1 │
    //   ├───────┤         │         │
    //   │ SCOPE │         │         │
    //   ├───────┴────┬────┴─────────┤
    //   │   MATHs    │     MIX      │
    //   ├────────────┴──────────────┤
    //   │            SEQ            │
    //   └───────────────────────────┘
    build_house_layout(&exe)?;
    build_fx_window(&exe)?;
    build_tape_window(&exe)?;
    // land on the console, not the rack
    tmux_cmd_ok(&["select-window", "-t", "los:modules"]);

    // the house patch opens already breathing: press play once the
    // mixer brings the transport up (background — don't hold up attach)
    std::thread::spawn(|| {
        for _ in 0..40 {
            if let Ok(mut t) = crate::shm::ShmTransport::open() {
                t.set_playing(true);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    });

    install_transport_keys(&exe);
    install_shell_theme();
    install_resize_hook(&exe, true);

    // Attach (blocks until detached; use raw .status())
    let _ = Command::new("tmux")
        .args(["attach-session", "-t", "los"])
        .status();

    Ok(())
}

/// How [`spawn_session_from_state`] leaves the spawned session.
pub struct SpawnOpts {
    /// Attach the calling terminal (interactive load) or stay detached
    /// (headless render).
    pub attach: bool,
    /// Force every sequencer's `playing` to false in the written state,
    /// so the session boots waiting for an explicit transport start.
    pub force_stopped: bool,
}

pub fn load_session(state_path: &str) -> Result<()> {
    spawn_session_from_state(
        state_path,
        &SpawnOpts {
            attach: true,
            force_stopped: false,
        },
    )
}

/// Force a sequencer's inline params to boot stopped — render owns the
/// transport start; the sequencer would otherwise push `playing = true`
/// into the shared transport at startup (sequencer.rs).
fn force_sequencer_stopped(value: &mut toml::Value) {
    if let toml::Value::Table(table) = value {
        table.insert(String::from("playing"), toml::Value::Boolean(false));
    }
}

/// Everything `los load` does up to (optionally) attaching: validate,
/// kill any existing `los` session, write the per-module tmp state,
/// spawn the conductor + module panes detached, apply layout.
pub fn spawn_session_from_state(state_path: &str, opts: &SpawnOpts) -> Result<()> {
    state::ensure_dirs()?;

    // Validate before touching tmux: a bad file must never cost a
    // running session. Same report `los check` prints (format-version
    // refusal included).
    let report = crate::validate::validate_file(std::path::Path::new(state_path));
    for issue in &report.warnings {
        eprintln!("[load] warning: {issue}");
    }
    anyhow::ensure!(
        report.is_clean(),
        "{state_path} failed validation:\n{}fix these (or run `los check {state_path}`)",
        report.render_errors()
    );

    // Read the state file
    let st = state::from_toml_file::<state::SessionState>(std::path::Path::new(state_path))?;

    // Kill existing session (ignore error if none exists)
    let _ = tmux_cmd(&["kill-session", "-t", "los"]);

    // Create conductor window
    new_los_session("conductor")?;

    let exe = exe_path()?;

    // Start conductor TUI
    let panes = list_session_panes("los", "conductor")?;
    if let Some((_, pane_id)) = panes.first() {
        tmux_cmd(&[
            "respawn-pane",
            "-k",
            "-t",
            pane_id,
            &format!("{} conductor", exe),
        ])?;
    }

    // Write module state files from the loaded session
    let module_windows: Vec<&state::WindowState> = st
        .windows
        .iter()
        .filter(|w| w.name != "conductor")
        .collect();

    for win in &module_windows {
        for pane in &win.panes {
            if let Some(ref inline) = pane.patch_inline {
                let mut value = inline.clone();
                if opts.force_stopped && canonical_module(&pane.module) == Some("sequencer") {
                    force_sequencer_stopped(&mut value);
                }
                let path = state::module_state_path(&pane.module, pane.instance);
                let toml_str =
                    toml::to_string_pretty(&value).context("serializing module params")?;
                state::write_state_file(&path, &toml_str)?;
            }
        }
    }

    // Build module list for spawning with real labels and instance numbers
    let all_panes: Vec<(String, String)> = module_windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| {
            let label = format!("{} {}", capitalize(&p.module), p.instance);
            let cmd = format!("{} {}", p.module, p.instance);
            (cmd, label)
        })
        .collect();
    let all_panes_ref: Vec<(&str, &str)> = all_panes
        .iter()
        .map(|(c, l)| (c.as_str(), l.as_str()))
        .collect();

    if !all_panes_ref.is_empty() {
        spawn_session_panes(&all_panes_ref)?;
    }

    let mut layout_applied = false;
    for win in &st.windows {
        if win.name == "modules" && !win.layout.is_empty() {
            match tmux_cmd(&["select-layout", "-t", "los:modules", &win.layout]) {
                Ok(_) => layout_applied = true,
                Err(e) => {
                    eprintln!(
                        "[load_session] layout failed ({}), falling back to tiled",
                        e
                    );
                }
            }
        }
    }
    if !layout_applied {
        tmux_cmd_ok(&["select-layout", "-t", "los:modules", "tiled"]);
    }

    if !st.tmux.window_size.is_empty() {
        tmux_cmd_ok(&[
            "set-option",
            "-t",
            "los:modules",
            "window-size",
            &st.tmux.window_size,
        ]);
    }

    for win in &st.windows {
        if win.name == "modules" {
            tmux_cmd_ok(&[
                "select-pane",
                "-t",
                &format!("los:modules.{}", win.active_pane),
            ]);
        }
    }

    install_transport_keys(&exe);
    install_shell_theme();
    install_resize_hook(&exe, false);

    if opts.attach {
        let _ = Command::new("tmux")
            .args(["attach-session", "-t", "los"])
            .status();
    }

    Ok(())
}

/// A render session is throwaway: kill it however we leave.
struct RenderSession;

impl Drop for RenderSession {
    fn drop(&mut self) {
        let _ = tmux_cmd(&["kill-session", "-t", "los"]);
    }
}

/// `los render <song.toml> <out.wav>` — spawn the song in a detached
/// throwaway session, record the master mix from bar 0, tear down.
///
/// Realtime by construction: the engine is a fleet of live processes
/// around a device-driven clock (the mixer's cpal callback advances the
/// transport), so a 3-minute song takes 3 minutes and is audible on the
/// default output device while it renders. True offline rendering would
/// mean a pull-driven graph in one process — an engine re-architecture,
/// not a flag here.
///
/// Known jitter: the mixer polls the arm file every ≤500 ms and may
/// momentarily create the transport playing — up to a few hundred ms of
/// pre-roll can precede bar 0. `los audit --song` flags any resulting
/// length mismatch.
pub fn render(song_path: &str, out_path: &str, secs: Option<f32>, tail: f32) -> Result<()> {
    state::ensure_dirs()?;
    anyhow::ensure!(
        !crate::tmux::session_exists("los"),
        "a 'los' tmux session is already running — save and close it first \
         (render spawns and kills a throwaway one)"
    );

    // Fresh control plane: a SIGHUP'd previous session leaves manifest,
    // modbus, event ring, and transport SHM behind, and an inherited
    // bump allocator can refuse every registration (= a silent render).
    crate::shm::unlink_control_plane();

    // Duration: explicit --secs, or the macro lane walked into seconds
    // plus a tail for the delay/filterbank to ring out.
    // (spawn_session_from_state validates; failures land before spawn.)
    let secs = match secs {
        Some(s) => s,
        None => {
            let st =
                state::from_toml_file::<state::SessionState>(std::path::Path::new(song_path))?;
            let seq = crate::song::sequencer_params(&st).ok_or_else(|| {
                anyhow::anyhow!(
                    "{song_path} has no sequencer 0 params to derive a duration from — pass --secs"
                )
            })?;
            crate::song::timeline(&seq).total_secs as f32 + tail
        }
    };
    anyhow::ensure!(secs > 0.0, "render duration must be positive");

    let abs = if std::path::Path::new(out_path).is_absolute() {
        std::path::PathBuf::from(out_path)
    } else {
        std::env::current_dir()?.join(out_path)
    };
    let abs = abs.to_string_lossy().to_string();
    let done = format!("{abs}.done");
    let _ = std::fs::remove_file(&abs);
    let _ = std::fs::remove_file(&done);

    eprintln!("[render] {song_path} → {abs} ({secs:.0}s, realtime — audible while it runs)");
    spawn_session_from_state(
        song_path,
        &SpawnOpts {
            attach: false,
            force_stopped: true,
        },
    )?;
    let _guard = RenderSession;

    // Arm the tape before the transport moves: the mixer polls the arm
    // file (≤500 ms) and creates the WAV the moment recording starts.
    std::fs::write(
        state::tmp_dir().join("record.arm"),
        format!("{secs}\n{abs}"),
    )?;

    // Boot wait. The transport shm may pre-date this session (stale) or
    // be created playing by the mixer (shm.rs creates flag = 1) — keep
    // forcing it stopped until recording is rolling, so bar 0 is bar 0.
    let boot_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !std::path::Path::new(&abs).exists() {
        if let Ok(mut t) = ShmTransport::open() {
            t.set_playing(false);
        }
        anyhow::ensure!(
            std::time::Instant::now() < boot_deadline,
            "mixer never started recording — is the audio device available?"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // Roll: rebase the clock to zero (the sequencer rebases its phase on
    // clock regression and bar 0's lane slot fires) and start.
    let mut transport = ShmTransport::open().context("transport vanished before start")?;
    transport.set_clock(0);
    transport.set_playing(true);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f32(secs + 30.0);
    while !std::path::Path::new(&done).exists() {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "render never finished — tape marker {done} missing"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = std::fs::remove_file(&done);
    eprintln!("[render] done: {abs}");
    eprintln!("[render] hear it in numbers: los audit {abs} --song {song_path}");
    Ok(())
}

/// Reload module panes and layout from a saved state file without
/// killing the conductor (which lives in the "conductor" window).
fn reload_modules_from_state(state_path: &std::path::Path) -> Result<()> {
    // Same gate as load_session: refuse a file `los check` would flag.
    let report = crate::validate::validate_file(state_path);
    for issue in &report.warnings {
        eprintln!("[reload] warning: {issue}");
    }
    anyhow::ensure!(
        report.is_clean(),
        "{} failed validation:\n{}fix these (or run `los check {}`)",
        state_path.display(),
        report.render_errors(),
        state_path.display()
    );

    // Read the state file
    let st = state::from_toml_file::<state::SessionState>(state_path)?;

    // Kill the existing modules window (all module panes), keeping conductor
    let _ = tmux_cmd(&["kill-window", "-t", "los:modules"]);

    // Write module state files from the loaded session
    let module_windows: Vec<&state::WindowState> = st
        .windows
        .iter()
        .filter(|w| w.name != "conductor")
        .collect();

    for win in &module_windows {
        for pane in &win.panes {
            if let Some(ref inline) = pane.patch_inline {
                let path = state::module_state_path(&pane.module, pane.instance);
                let toml_str =
                    toml::to_string_pretty(inline).context("serializing module params")?;
                state::write_state_file(&path, &toml_str)?;
            }
        }
    }

    // Build module list for spawning with real labels and instance numbers
    let all_panes: Vec<(String, String)> = module_windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| {
            let label = format!("{} {}", capitalize(&p.module), p.instance);
            let cmd = format!("{} {}", p.module, p.instance);
            (cmd, label)
        })
        .collect();
    let all_panes_ref: Vec<(&str, &str)> = all_panes
        .iter()
        .map(|(c, l)| (c.as_str(), l.as_str()))
        .collect();

    if !all_panes_ref.is_empty() {
        spawn_session_panes(&all_panes_ref)?;
    }

    let mut layout_applied = false;
    for win in &st.windows {
        if win.name == "modules" && !win.layout.is_empty() {
            match tmux_cmd(&["select-layout", "-t", "los:modules", &win.layout]) {
                Ok(_) => layout_applied = true,
                Err(e) => eprintln!("[reload] layout failed ({}), falling back to tiled", e),
            }
        }
    }
    if !layout_applied {
        tmux_cmd_ok(&["select-layout", "-t", "los:modules", "tiled"]);
    }

    for win in &st.windows {
        if win.name == "modules" {
            tmux_cmd_ok(&[
                "select-pane",
                "-t",
                &format!("los:modules.{}", win.active_pane),
            ]);
        }
    }

    Ok(())
}

// ── conductor TUI ───────────────────────────────────────────────────────────

pub fn run_conductor() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut selected: usize = 0;
    let mut show_help = false;
    let mut count = crate::keys::Count::default();
    let mut pending_g = false;
    let mut pending_d = false;
    // Some(filename) while waiting for y/n delete confirmation
    let mut confirm_delete: Option<String> = None;
    // Modules view (Tab toggles): manifest-driven list + lifecycle keys
    let mut view_modules = false;
    let mut modules: Vec<crate::shm::ManifestEntry> = Vec::new();
    let mut msel: usize = 0;
    let mut add_picker: Option<usize> = None;
    let mut confirm_remove: Option<(String, usize)> = None;
    let mut manifest_ro: Option<Manifest> = Manifest::open().ok();
    // Global transport handle for Space = play/pause (lazily reopened)
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();

    // Shared state entries list
    let mut entries: Vec<String> = Vec::new();
    let mut needs_refresh = true;

    loop {
        if view_modules {
            if manifest_ro.is_none() {
                manifest_ro = Manifest::open().ok();
            }
            modules = manifest_ro
                .as_ref()
                .map(|m| m.entries())
                .unwrap_or_default();
            modules.sort_by(|a, b| (&a.module_name, a.instance).cmp(&(&b.module_name, b.instance)));
            msel = msel.min(modules.len().saturating_sub(1));
            needs_refresh = true; // live view: redraw each tick
        }
        if needs_refresh || entries.is_empty() {
            entries.clear();
            if let Ok(dir) = std::fs::read_dir(state::states_dir()) {
                for entry in dir.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        if name.ends_with(".toml") {
                            entries.push(name.to_string());
                        }
                    }
                }
            }
            entries.sort();
            // Clamp selection to list bounds
            if entries.is_empty() {
                selected = 0;
            } else if selected >= entries.len() {
                selected = entries.len().saturating_sub(1);
            }

            needs_refresh = false;

            terminal.draw(|f| {
                let area = f.area();
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .margin(1)
                    .constraints([Constraint::Length(3), Constraint::Min(0)])
                    .split(area);

                let header = if let Some(ref name) = confirm_delete {
                    format!("Delete {}? (y/n)", name)
                } else if let Some((ref m, i)) = confirm_remove {
                    format!("Remove {} {}? (y/n)", m, i)
                } else if pending_d {
                    String::from("LOS Conductor — d…")
                } else if view_modules {
                    String::from("LOS Conductor — Modules (a add, x remove, Tab states)")
                } else {
                    String::from("LOS Conductor — States (Tab modules)")
                };
                let header_style = if confirm_delete.is_some() || confirm_remove.is_some() {
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(ratatui::style::Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(ratatui::style::Modifier::BOLD)
                };
                let title = Paragraph::new(header)
                    .style(header_style)
                    .block(Block::default().borders(Borders::ALL));
                f.render_widget(title, chunks[0]);

                if view_modules {
                    let items: Vec<ListItem> = modules
                        .iter()
                        .enumerate()
                        .map(|(i, e)| {
                            let outputs = match e.mod_base {
                                Some(base) => {
                                    let labels = crate::routing::output_labels(&e.module_name);
                                    let shown: Vec<&str> =
                                        labels.iter().take(e.mod_count).copied().collect();
                                    format!(
                                        "  outputs ch{}-{}: {}",
                                        base,
                                        base + e.mod_count - 1,
                                        shown.join(",")
                                    )
                                }
                                None => String::new(),
                            };
                            let audio = if e.audio_shm.is_some() {
                                "  [audio]"
                            } else {
                                ""
                            };
                            let text = format!(
                                "{} {}  pid {}{}{}",
                                e.module_name, e.instance, e.pid, audio, outputs
                            );
                            let style = if i == msel {
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(ratatui::style::Modifier::BOLD)
                            } else {
                                Style::default()
                            };
                            ListItem::new(text).style(style)
                        })
                        .collect();
                    let list = List::new(items).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Running Modules"),
                    );
                    f.render_widget(list, chunks[1]);

                    if let Some(sel) = add_picker {
                        let rows: Vec<ListItem> = ADDABLE_MODULES
                            .iter()
                            .enumerate()
                            .map(|(i, m)| {
                                let style = if i == sel {
                                    Style::default().fg(Color::Black).bg(Color::Yellow)
                                } else {
                                    Style::default().fg(Color::White)
                                };
                                ListItem::new(*m).style(style)
                            })
                            .collect();
                        let h = (ADDABLE_MODULES.len() as u16 + 2).min(area.height);
                        let r = ratatui::layout::Rect::new(
                            (area.width.saturating_sub(24)) / 2,
                            (area.height.saturating_sub(h)) / 2,
                            24.min(area.width),
                            h,
                        );
                        f.render_widget(ratatui::widgets::Clear, r);
                        let list = List::new(rows).block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::Yellow))
                                .title("Add module"),
                        );
                        f.render_widget(list, r);
                    }
                } else if entries.is_empty() {
                    let empty = Paragraph::new("No saved states. Press ? for help.")
                        .style(Style::default().fg(Color::Gray));
                    f.render_widget(empty, chunks[1]);
                } else {
                    let items: Vec<ListItem> = entries
                        .iter()
                        .enumerate()
                        .map(|(i, name)| {
                            let style = if i == selected {
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(ratatui::style::Modifier::BOLD)
                            } else {
                                Style::default()
                            };
                            ListItem::new(name.as_str()).style(style)
                        })
                        .collect();

                    let list = List::new(items)
                        .block(Block::default().borders(Borders::ALL).title("Saved States"))
                        .highlight_style(Style::default().fg(Color::Yellow));
                    f.render_widget(list, chunks[1]);
                }
            })?;
        }

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if show_help {
                    if let KeyCode::Char('?') | KeyCode::Char('q') = key.code {
                        show_help = false;
                        needs_refresh = true;
                    }
                    continue;
                }

                // Pending delete confirmation: y deletes, anything else cancels
                if let Some(name) = confirm_delete.take() {
                    if key.code == KeyCode::Char('y') {
                        let path = state::states_dir().join(&name);
                        let _ = std::fs::remove_file(&path);
                    }
                    needs_refresh = true;
                    continue;
                }

                // Pending module-removal confirmation
                if let Some((module, inst)) = confirm_remove.take() {
                    if key.code == KeyCode::Char('y') {
                        if let Err(e) = remove_module(&module, inst) {
                            eprintln!("[conductor] remove failed: {}", e);
                        }
                    }
                    needs_refresh = true;
                    continue;
                }

                // Add-module type picker
                if let Some(sel) = add_picker {
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => {
                            add_picker = Some((sel + 1).min(ADDABLE_MODULES.len() - 1));
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            add_picker = Some(sel.saturating_sub(1));
                        }
                        KeyCode::Enter => {
                            add_picker = None;
                            if let Err(e) = add_module(ADDABLE_MODULES[sel], None) {
                                eprintln!("[conductor] add failed: {}", e);
                            }
                        }
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('a') => {
                            add_picker = None;
                        }
                        _ => {}
                    }
                    needs_refresh = true;
                    continue;
                }

                // Tab toggles between states and modules views
                if key.code == KeyCode::Tab {
                    view_modules = !view_modules;
                    count.clear();
                    needs_refresh = true;
                    continue;
                }

                // Modules view keys
                if view_modules {
                    match key.code {
                        KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
                        KeyCode::Char('j') | KeyCode::Down => {
                            msel = (msel + count.take()).min(modules.len().saturating_sub(1));
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            msel = msel.saturating_sub(count.take());
                        }
                        KeyCode::Char('g') => {
                            if pending_g {
                                pending_g = false;
                                msel = 0;
                            } else {
                                pending_g = true;
                            }
                        }
                        KeyCode::Char('G') => {
                            msel = modules.len().saturating_sub(1);
                        }
                        KeyCode::Char('a') => {
                            count.clear();
                            add_picker = Some(0);
                        }
                        KeyCode::Char('x') => {
                            count.clear();
                            if let Some(e) = modules.get(msel) {
                                if e.module_name == "mixer" || e.module_name == "conductor" {
                                    // removing the mixer kills audio output; keep it manual
                                } else {
                                    confirm_remove = Some((e.module_name.clone(), e.instance));
                                }
                            }
                        }
                        KeyCode::Char(' ') => {
                            if transport_ui.is_none() {
                                transport_ui = ShmTransport::open().ok();
                            }
                            if let Some(ref mut t) = transport_ui {
                                t.toggle_playing();
                            }
                        }
                        KeyCode::Char('?') => {
                            show_help = true;
                        }
                        _ => {
                            count.clear();
                            pending_g = false;
                        }
                    }
                    needs_refresh = true;
                    continue;
                }
                if !matches!(key.code, KeyCode::Char('g')) {
                    pending_g = false;
                }
                if !matches!(key.code, KeyCode::Char('d')) {
                    pending_d = false;
                }

                match key.code {
                    KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
                    KeyCode::Char('j') | KeyCode::Down => {
                        let n = count.take();
                        selected = (selected + n).min(entries.len().saturating_sub(1));
                        needs_refresh = true;
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        let n = count.take();
                        selected = selected.saturating_sub(n);
                        needs_refresh = true;
                    }
                    KeyCode::Char('g') => {
                        count.clear();
                        if pending_g {
                            pending_g = false;
                            selected = 0;
                        } else {
                            pending_g = true;
                        }
                        needs_refresh = true;
                    }
                    KeyCode::Char('G') => {
                        count.clear();
                        selected = entries.len().saturating_sub(1);
                        needs_refresh = true;
                    }
                    KeyCode::Char('s') => {
                        // Every module window in the session (the fx rack
                        // included), each with its pane order + instances
                        let window_names: Vec<String> =
                            tmux_cmd(&["list-windows", "-t", "los", "-F", "#{window_name}"])
                                .map(|out| {
                                    out.lines()
                                        .map(|l| l.trim().to_string())
                                        .filter(|w| !w.is_empty() && w != "conductor")
                                        .collect()
                                })
                                .unwrap_or_else(|_| vec![String::from("modules")]);
                        let mut per_window: Vec<(String, Vec<(String, usize)>)> = Vec::new();
                        for wname in &window_names {
                            let mut pane_info: Vec<(String, usize)> = Vec::new();
                            if let Ok(titles) = tmux_cmd(&[
                                "list-panes",
                                "-t",
                                &format!("los:{}", wname),
                                "-F",
                                "#{pane_title}",
                            ]) {
                                for title in titles.lines() {
                                    let t = title.trim();
                                    let parts: Vec<&str> = t.split_whitespace().collect();
                                    if parts.is_empty() {
                                        continue;
                                    }
                                    // canonicalize: house titles (SEQ, MIX,
                                    // BANK, los) must save as spawnable
                                    // module names
                                    let Some(module_name) = canonical_module(parts[0]) else {
                                        continue;
                                    };
                                    let instance =
                                        parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                                    pane_info.push((module_name.to_string(), instance));
                                }
                            }
                            per_window.push((wname.clone(), pane_info));
                        }
                        // Fallback to default order if tmux query fails or returns nothing
                        if per_window.iter().all(|(_, p)| p.is_empty()) {
                            per_window = vec![(
                                String::from("modules"),
                                vec![
                                    ("sequencer".into(), 0),
                                    ("voice".into(), 0),
                                    ("mixer".into(), 0),
                                    ("scope".into(), 0),
                                    ("envelope".into(), 0),
                                ],
                            )];
                        }
                        let pane_info: Vec<(String, usize)> =
                            per_window.iter().flat_map(|(_, p)| p.clone()).collect();

                        // Read PIDs from manifest + pid files
                        let manifest = Manifest::open().ok();
                        let mut pids = Vec::new();
                        for (mod_name, instance) in &pane_info {
                            let pid = manifest
                                .as_ref()
                                .and_then(|m| {
                                    m.entries()
                                        .iter()
                                        .find(|e| {
                                            e.module_name == *mod_name && e.instance == *instance
                                        })
                                        .map(|e| e.pid)
                                })
                                .or_else(|| state::read_pid_file(mod_name, *instance));
                            if let Some(p) = pid {
                                pids.push(p);
                            }
                        }

                        // Send SIGUSR1 to all module processes
                        for pid in &pids {
                            state::send_save_signal(*pid);
                        }

                        // Wait for modules to write their state files
                        std::thread::sleep(Duration::from_millis(500));

                        // Collect module state files from tmp, window by
                        // window, each with its own tmux layout
                        let mut windows = Vec::new();
                        for (wname, infos) in &per_window {
                            let mut panes = Vec::new();
                            for (mod_name, instance) in infos.iter() {
                                let path = state::module_state_path(mod_name, *instance);
                                let inline = if path.exists() {
                                    std::fs::read_to_string(&path)
                                        .ok()
                                        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
                                } else {
                                    None
                                };
                                panes.push(state::PaneState {
                                    module: mod_name.to_string(),
                                    instance: *instance,
                                    patch: None,
                                    patch_inline: inline,
                                });
                            }
                            windows.push(state::WindowState {
                                name: wname.clone(),
                                layout: get_window_layout("los", wname).unwrap_or_default(),
                                active_pane: get_active_pane_index("los", wname).unwrap_or(1),
                                panes,
                            });
                        }

                        // Prompt for filename
                        let now = chrono_or_fallback();
                        let default_name = format!("session-{}", now);
                        let filename = if let Ok(name) =
                            prompt_string(&mut terminal, "Save as:", &default_name)
                        {
                            format!("{}.toml", name)
                        } else {
                            format!("{}.toml", default_name)
                        };
                        let save_path = state::states_dir().join(&filename);

                        let session_state = state::SessionState {
                            meta: state::Meta {
                                name: filename.trim_end_matches(".toml").to_string(),
                                created: now,
                                format: state::STATE_FORMAT,
                            },
                            tmux: state::TmuxState::default(),
                            windows,
                        };

                        if let Ok(toml_str) = state::to_toml_string(&session_state) {
                            let _ = state::write_state_file(&save_path, &toml_str);
                        }
                        needs_refresh = true;
                    }
                    KeyCode::Enter | KeyCode::Char('l') if selected < entries.len() => {
                        // Full module reload while keeping the conductor alive.
                        count.clear();
                        let path = state::states_dir().join(&entries[selected]);
                        if let Err(e) = reload_modules_from_state(&path) {
                            eprintln!("[conductor] reload failed: {}", e);
                        }
                        needs_refresh = true;
                    }
                    KeyCode::Char('d') if selected < entries.len() => {
                        // dd chord: first d arms, second d asks for confirmation
                        count.clear();
                        if pending_d {
                            pending_d = false;
                            confirm_delete = Some(entries[selected].clone());
                        } else {
                            pending_d = true;
                        }
                        needs_refresh = true;
                    }
                    KeyCode::Char(' ') => {
                        if transport_ui.is_none() {
                            transport_ui = ShmTransport::open().ok();
                        }
                        if let Some(ref mut t) = transport_ui {
                            t.toggle_playing();
                        }
                    }
                    KeyCode::Char('?') => {
                        count.clear();
                        show_help = true;
                        needs_refresh = true;
                    }
                    _ => {
                        count.clear();
                    }
                }
            }
        }

        if show_help {
            terminal.draw(|f| {
                let help_text = vec![
                    Line::from("━━━ Conductor Help ━━━"),
                    Line::from(""),
                    Line::from("  j/k, ↑/↓  Navigate state list (counts: 3j)"),
                    Line::from("  gg / G     First / last state"),
                    Line::from("  s          Save current session state"),
                    Line::from("  Enter / l  Load selected state (full session reload)"),
                    Line::from("  dd         Delete selected state (asks y/n)"),
                    Line::from("  Tab        Switch states ⇄ modules view"),
                    Line::from("  a          Add module (modules view)"),
                    Line::from("  x          Remove module (modules view, asks y/n)"),
                    Line::from("  space      Play/pause (global)"),
                    Line::from("  ?          Toggle this help"),
                    Line::from("  Close pane: tmux prefix + x"),
                ];
                let help = Paragraph::new(help_text)
                    .style(Style::default().fg(Color::White))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Cyan))
                            .title("Help"),
                    );
                f.render_widget(help, f.area());
            })?;

            // Wait for key to close help
            loop {
                if event::poll(Duration::from_millis(100))? {
                    if let Event::Key(k) = event::read()? {
                        if let KeyCode::Char('?') | KeyCode::Char('q') = k.code {
                            show_help = false;
                            needs_refresh = true;
                            break;
                        }
                    }
                }
            }
        }
    }
}

fn chrono_or_fallback() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", now.as_secs())
}

/// Show a text input prompt in the TUI, return the entered string.
fn prompt_string(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    prompt: &str,
    default: &str,
) -> Result<String> {
    let mut buf = String::new();
    loop {
        terminal.draw(|f| {
            let area = f.area();
            let text = format!(
                "{} [{}]",
                prompt,
                if buf.is_empty() { default } else { &buf }
            );
            let para = Paragraph::new(text)
                .style(Style::default().fg(Color::Yellow))
                .block(Block::default().borders(Borders::ALL).title("Input"));
            f.render_widget(para, area);
        })?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Enter => {
                        if buf.is_empty() {
                            return Ok(default.to_string());
                        }
                        return Ok(buf);
                    }
                    KeyCode::Esc => return Ok(default.to_string()),
                    KeyCode::Char(c) if c.is_ascii_alphanumeric() || c == '-' || c == '_' => {
                        buf.push(c);
                    }
                    KeyCode::Backspace => {
                        buf.pop();
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
    fn house_titles_round_trip_to_spawnable_modules() {
        // every house pane title must canonicalize to a real module, or
        // save/load silently spawns dead panes
        for title in HOUSE_TITLES {
            let word = title.split_whitespace().next().unwrap_or(title);
            let m = canonical_module(word);
            assert!(m.is_some(), "house title {title} must map to a module");
        }
        // labels from add_module ("Voice 1") and plain names work too
        assert_eq!(canonical_module("Voice"), Some("voice"));
        assert_eq!(canonical_module("envelope"), Some("envelope"));
        assert_eq!(canonical_module("maths"), Some("envelope"));
        assert_eq!(canonical_module("nonsense"), None);
    }

    #[test]
    fn house_patch_round_trips_and_is_composed() {
        let _ = crate::state::ensure_dirs();
        write_house_patch();

        let seq: crate::state::SequencerParams =
            crate::state::load_module_state("sequencer", 0).expect("seq state");
        assert_eq!(seq.bpm, Some(74.0));
        assert_eq!(seq.playing, Some(true), "the house opens breathing");
        assert_eq!(seq.tracks.len(), 3);
        assert_eq!(seq.tracks[0].slots.len(), 4, "melody carries slots b, c, d + the ghost");
        assert_eq!(seq.tracks[1].mode, crate::state::TrackMode::Modulation);
        assert_eq!(seq.tracks[2].length, Some(12), "polymetric bass");
        assert_eq!(seq.tracks[2].slots.len(), 1, "the bass carries its peak riff");
        assert_eq!(seq.macros.len(), 8, "eight section verbs");
        assert_eq!(seq.lane.len(), 128, "the long form (~7 minutes)");
        assert!(seq.lane.iter().any(|m| m == "e"), "a bass-only section");
        assert!(seq.lane.iter().any(|m| m == "f"), "a fast peak");
        assert!(seq.lane.iter().any(|m| m == "g"), "a sparse outro");
        assert!(
            seq.macros.iter().any(|m| m
                .cmds
                .iter()
                .any(|c| matches!(c, crate::state::MacroCmd::SetBpm { .. }))),
            "tempo moves are part of the form"
        );
        assert!(
            seq.tracks[0].steps.iter().any(|st| st.active && st.prob < 100),
            "probability is in play"
        );
        let slot_b = &seq.tracks[0].slots[0].steps;
        assert!(
            slot_b.iter().chain(seq.tracks[2].steps.iter()).any(|st| st.active && st.delay > 0.0),
            "pushed micro-delay notes carry the loose-repeat feel"
        );
        assert!(
            seq.tracks[0].steps.iter().all(|st| st.repeats <= 1),
            "no substep ratchets in the drone (too tight at 74 BPM)"
        );

        let delay: crate::state::DelayParams =
            crate::state::load_module_state("delay", 0).expect("delay state");
        assert_eq!(delay.input.as_deref(), Some("send/0"));
        assert_eq!(delay.tap.len(), 8);
        assert!(delay.regen.unwrap() > 0.3, "audible feedback out of the box");
        assert!(delay.tap[0].level > delay.tap[5].level, "tap levels decay");

        let tape: crate::state::TapeParams =
            crate::state::load_module_state("tape", 0).expect("tape state");
        assert_eq!(tape.loop_on, Some(false), "record-once over the form");
        assert!(tape.tracks[0].armed, "track 1 ready to record");
        assert!(!tape.tracks[1].armed);

        let bank: crate::state::FilterbankParams =
            crate::state::load_module_state("filterbank", 0).expect("bank state");
        assert_eq!(bank.input.as_deref(), Some("send/1"));
        assert_eq!(bank.morph_src.as_deref(), Some("envelope/1/ch1"));
        assert_eq!(bank.wcent_src.as_deref(), Some("sequencer/0/t2"));
        assert!(bank.wwidth.unwrap() < 1.0, "the window is engaged");
    }

    #[test]
    fn house_geometry_invariants_across_sizes() {
        // the net for layout fragility: every reasonable window size must
        // produce a workable house — no zero panes, badge + scope fit
        // inside row 1, SEQ never exceeds its content, row 2 exists
        for h in 12..150 {
            for w in (30..260).step_by(7) {
                let (row1, seq, col, badge) = house_dims(w, h);
                assert!(seq <= crate::sequencer::CONTENT_LINES, "{w}x{h}");
                assert!(row1 >= 1 && seq >= 1 && col >= 1 && badge >= 1, "{w}x{h}");
                assert!(
                    col <= w / 2 + 1,
                    "column never eats half the window: {w}x{h}"
                );
                if h >= 30 {
                    assert!(badge + 2 <= row1, "scope keeps a window at {w}x{h}");
                    assert!(row1 + seq + 2 < h, "MATHs|MIX row exists at {w}x{h}");
                }
            }
        }
        // and the SEQ snap is DERIVED from the sequencer's real content:
        // 8 default tracks + header + detail strip + rules + modeline
        assert_eq!(crate::sequencer::CONTENT_LINES, crate::NUM_TRACKS + 8);
    }

    #[test]
    fn house_dims_adapt_to_window() {
        // tall window: SEQ snaps to content; rows 1 and 2 share the rest
        // about evenly; badge + scope stack to the voices' height
        let (row1, seq, _, badge) = house_dims(180, 80);
        assert_eq!(seq, 16, "SEQ snaps to its content");
        assert_eq!(row1, 28, "voices row takes a real share");
        assert_eq!(badge, 10, "badge caps at content height");
        let scope_h = row1 - badge - 1;
        assert!(scope_h >= 8, "scope still gets a usable window");
        let row2 = 80 - seq - 1 - row1 - 1;
        assert!(row2 >= 14, "MATHs|MIX keep content height plus slack");
        // mid window (the gif terminal): proportional
        let (row1, seq, col, badge) = house_dims(140, 40);
        assert_eq!(seq, 11);
        assert_eq!(row1, 14);
        assert_eq!(col, 35);
        assert_eq!(badge, 6);
        // small window: everything stays usable, nothing collapses
        let (row1, seq, col, badge) = house_dims(60, 20);
        assert!((6..12).contains(&row1));
        assert!((5..10).contains(&seq));
        assert!((15..=30).contains(&col));
        assert!(badge >= 4);
        // degenerate sizes never panic or go to zero
        let (row1, seq, col, badge) = house_dims(4, 4);
        assert!(row1 >= 1 && seq >= 1 && col >= 1 && badge >= 1);
    }

    #[test]
    fn test_shell_escape_no_quotes() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn test_shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\"'\"'s'");
    }

    #[test]
    fn test_shell_escape_path() {
        let path = "/Users/jake/.config/los/states/YES.toml";
        assert_eq!(
            shell_escape(path),
            "'/Users/jake/.config/los/states/YES.toml'"
        );
    }

    // ── save/load state format tests ─────────────────────────────────────

    #[test]
    fn test_session_state_with_reordered_panes() {
        let state = state::SessionState {
            meta: state::Meta {
                name: "reordered".into(),
                created: "123".into(),
                format: state::STATE_FORMAT,
            },
            tmux: state::TmuxState::default(),
            windows: vec![state::WindowState {
                name: "modules".into(),
                layout: "test,100x50,0,0[50x50,0,0,1,50x50,0,0,2]".into(),
                active_pane: 3,
                panes: vec![
                    state::PaneState {
                        module: "voice".into(),
                        instance: 0,
                        patch: None,
                        patch_inline: None,
                    },
                    state::PaneState {
                        module: "envelope".into(),
                        instance: 0,
                        patch: None,
                        patch_inline: None,
                    },
                    state::PaneState {
                        module: "sequencer".into(),
                        instance: 0,
                        patch: None,
                        patch_inline: None,
                    },
                ],
            }],
        };

        let toml = state::to_toml_string(&state).expect("serialize");
        let loaded: state::SessionState = toml::from_str(&toml).expect("deserialize");

        assert_eq!(loaded.windows[0].panes[0].module, "voice");
        assert_eq!(loaded.windows[0].panes[1].module, "envelope");
        assert_eq!(loaded.windows[0].panes[2].module, "sequencer");
        assert_eq!(loaded.windows[0].active_pane, 3);
    }

    #[test]
    fn test_active_pane_with_base_index_1() {
        // tmux default pane-base-index is 1, so active_pane=5 means the 5th pane
        let state = state::SessionState {
            meta: state::Meta {
                name: "base-index-1".into(),
                created: "123".into(),
                format: state::STATE_FORMAT,
            },
            tmux: state::TmuxState::default(),
            windows: vec![state::WindowState {
                name: "modules".into(),
                layout: "".into(),
                active_pane: 5,
                panes: vec![
                    state::PaneState {
                        module: "a".into(),
                        instance: 0,
                        patch: None,
                        patch_inline: None,
                    },
                    state::PaneState {
                        module: "b".into(),
                        instance: 0,
                        patch: None,
                        patch_inline: None,
                    },
                    state::PaneState {
                        module: "c".into(),
                        instance: 0,
                        patch: None,
                        patch_inline: None,
                    },
                    state::PaneState {
                        module: "d".into(),
                        instance: 0,
                        patch: None,
                        patch_inline: None,
                    },
                    state::PaneState {
                        module: "e".into(),
                        instance: 0,
                        patch: None,
                        patch_inline: None,
                    },
                ],
            }],
        };

        let toml = state::to_toml_string(&state).expect("serialize");
        let loaded: state::SessionState = toml::from_str(&toml).expect("deserialize");

        assert_eq!(loaded.windows[0].active_pane, 5);
    }

    #[test]
    fn test_layout_string_preserved_raw() {
        // Layout strings with old pane IDs must be passed directly to tmux.
        // We must NOT parse, modify, or recompute checksums.
        let raw_layout = "f1d4,100x50,0,0[50x50,0,0,1,50x50,0,0,2]";
        let state = state::SessionState {
            meta: state::Meta {
                name: "layout-preserve".into(),
                created: "123".into(),
                format: state::STATE_FORMAT,
            },
            tmux: state::TmuxState::default(),
            windows: vec![state::WindowState {
                name: "modules".into(),
                layout: raw_layout.into(),
                active_pane: 1,
                panes: vec![
                    state::PaneState {
                        module: "a".into(),
                        instance: 0,
                        patch: None,
                        patch_inline: None,
                    },
                    state::PaneState {
                        module: "b".into(),
                        instance: 0,
                        patch: None,
                        patch_inline: None,
                    },
                ],
            }],
        };

        let toml = state::to_toml_string(&state).expect("serialize");
        let loaded: state::SessionState = toml::from_str(&toml).expect("deserialize");

        assert_eq!(loaded.windows[0].layout, raw_layout);
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn force_stopped_overrides_sequencer_playing() {
        let params = state::SequencerParams {
            playing: Some(true),
            bpm: Some(74.0),
            ..Default::default()
        };
        let mut v = toml::Value::try_from(&params).expect("to value");
        force_sequencer_stopped(&mut v);
        let back: state::SequencerParams = v.try_into().expect("from value");
        assert_eq!(back.playing, Some(false));
        assert_eq!(back.bpm, Some(74.0), "only `playing` is touched");
    }

    #[test]
    fn force_stopped_inserts_when_absent() {
        let mut v = toml::Value::try_from(state::SequencerParams::default()).expect("to value");
        force_sequencer_stopped(&mut v);
        let back: state::SequencerParams = v.try_into().expect("from value");
        assert_eq!(back.playing, Some(false));
    }
}

#[cfg(test)]
mod house_example_tests {
    use super::*;

    fn example_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/house-drone.toml")
    }

    /// Floats through f32 and back, so a file holding the shortest
    /// decimal ("0.152") compares equal to live params holding the
    /// f64-widened f32 (0.15199999511…).
    fn normalize(v: &mut toml::Value) {
        match v {
            toml::Value::Float(f) => *f = (*f as f32) as f64,
            toml::Value::Array(items) => items.iter_mut().for_each(normalize),
            toml::Value::Table(table) => table.iter_mut().for_each(|(_, v)| normalize(v)),
            toml::Value::String(_)
            | toml::Value::Integer(_)
            | toml::Value::Boolean(_)
            | toml::Value::Datetime(_) => {}
        }
    }

    /// Regenerate examples/house-drone.toml from the code. Run with:
    ///   cargo test regen_house_example -- --ignored
    #[test]
    #[ignore = "writes examples/house-drone.toml; run explicitly after changing the house patch"]
    fn regen_house_example() {
        let st = house_session_state().expect("house session");
        let mut val = toml::Value::try_from(&st).expect("to value");
        round_floats_to_f32(&mut val);
        let body = toml::to_string_pretty(&val).expect("to toml");
        let header = "\
# The house drone — the patch a fresh `los new` session seeds, as a
# loadable song file. ~7.4 minutes: three sequencer tracks (melody with
# five pattern slots, a drunk modulation walk, polymetric bass), eight
# section macros (a–h) walked by a 128-bar macro lane with tempo moves,
# voices into a tap delay (send A) and a 296-style filterbank (send B).
#
# GENERATED from house_patch_params() in src/modules/conductor.rs —
# edit there, then: cargo test regen_house_example -- --ignored
#
# Play it:    los load examples/house-drone.toml
# Check it:   los check examples/house-drone.toml

";
        std::fs::write(example_path(), format!("{header}{body}")).expect("write example");
    }

    #[test]
    fn house_example_matches_the_code() {
        let text = std::fs::read_to_string(example_path())
            .expect("examples/house-drone.toml missing — run: cargo test regen_house_example -- --ignored");
        let on_disk: state::SessionState = toml::from_str(&text).expect("parse example");
        let mut disk_val = toml::Value::try_from(&on_disk).expect("disk to value");
        let live = house_session_state().expect("house session");
        let mut live_val = toml::Value::try_from(&live).expect("live to value");
        normalize(&mut disk_val);
        normalize(&mut live_val);
        assert_eq!(
            disk_val, live_val,
            "examples/house-drone.toml is stale — regen: cargo test regen_house_example -- --ignored"
        );
    }

    #[test]
    fn house_example_validates_clean() {
        let text = std::fs::read_to_string(example_path()).expect("example file");
        let report = crate::validate::validate_str(&text);
        assert!(report.errors.is_empty(), "house example has errors: {:?}", report.errors);
    }
}
