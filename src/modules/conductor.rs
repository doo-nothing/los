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

/// Seed the fresh session's curated default patch: the fx rack arrives
/// pre-cabled. Modules load these state files as they spawn.
///
/// The routing story (see docs/tour.md):
///   strips → send A (25%) → DELAY → its strip on the console (the return)
///   strips → send B (20%) → BANK  → its strip, morph swept by MATHs 1 ch1
fn write_house_patch() {
    // delay 0: a tasteful echo on send A — wet-only (the dry path is
    // the console itself), a little regeneration, a whisper of wash
    let delay = state::DelayParams {
        format: state::STATE_FORMAT,
        time: Some(0.160),
        regen: Some(0.30),
        shim: Some(0.0),
        wash: Some(0.15),
        dry: Some(0.0),
        taps: Some(8),
        input: Some(String::from("send/0")),
        ..Default::default()
    };
    let _ = state::save_module_state("delay", 0, &delay);

    // filterbank 0: on send B, wet-only, spectrum slowly breathing —
    // morph rides MATHs 1's looping channel 1
    let bank = state::FilterbankParams {
        format: state::STATE_FORMAT,
        morph: Some(0.0),
        split: Some(0.4),
        dry: Some(0.0),
        decay: Some(0.35),
        input: Some(String::from("send/1")),
        morph_src: Some(String::from("envelope/1/ch1")),
        ..Default::default()
    };
    let _ = state::save_module_state("filterbank", 0, &bank);

    // MATHs 1: channel 1 cycling slowly = the LFO driving the bank's
    // morph; the other channels stay stock for patching
    let mut ch1 = state::EnvelopeChannelParams {
        rise: 0.72,
        fall: 0.72,
        loop_mode: true,
        ..Default::default()
    };
    ch1.shape = 0.5;
    let env1 = state::EnvelopeParams {
        format: state::STATE_FORMAT,
        channels: vec![
            ch1,
            state::EnvelopeChannelParams::default(),
            state::EnvelopeChannelParams::default(),
            state::EnvelopeChannelParams::default(),
        ],
        logic_outputs: Default::default(),
    };
    let _ = state::save_module_state("envelope", 1, &env1);
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
    tmux_cmd(&["set-option", "-w", "-t", win, "pane-border-format", " #{pane_title} "])?;

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
    tmux_cmd(&["respawn-pane", "-k", "-t", &base, &format!("{} delay 0", exe)])?;

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
    // land on the console, not the rack
    tmux_cmd_ok(&["select-window", "-t", "los:modules"]);

    install_transport_keys(&exe);
    install_shell_theme();
    install_resize_hook(&exe, true);

    // Attach (blocks until detached; use raw .status())
    let _ = Command::new("tmux")
        .args(["attach-session", "-t", "los"])
        .status();

    Ok(())
}

pub fn load_session(state_path: &str) -> Result<()> {
    state::ensure_dirs()?;

    // Read the state file
    let st = state::from_toml_file::<state::SessionState>(std::path::Path::new(state_path))?;
    anyhow::ensure!(
        st.meta.format >= state::STATE_FORMAT,
        "{} is a v{} state file; v{} (routing source addresses) is a clean break — re-save your session",
        state_path,
        st.meta.format.max(1),
        state::STATE_FORMAT
    );

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

    let _ = Command::new("tmux")
        .args(["attach-session", "-t", "los"])
        .status();

    Ok(())
}

/// Reload module panes and layout from a saved state file without
/// killing the conductor (which lives in the "conductor" window).
fn reload_modules_from_state(state_path: &std::path::Path) -> Result<()> {
    // Read the state file
    let st = state::from_toml_file::<state::SessionState>(state_path)?;
    anyhow::ensure!(
        st.meta.format >= state::STATE_FORMAT,
        "{} is a v{} state file; v{} is a clean break — re-save your session",
        state_path.display(),
        st.meta.format.max(1),
        state::STATE_FORMAT
    );

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
