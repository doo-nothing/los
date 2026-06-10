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

use anyhow::{Context, Result, bail};
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
            bail!("tmux {} exited with code {:?}", args.join(" "), output.status.code());
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

fn exe_path() -> Result<String> {
    Ok(std::env::current_exe()?
        .to_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "los".into()))
}

fn list_session_panes(session: &str, window: &str) -> Result<Vec<(usize, String)>> {
    let stdout = tmux_cmd(&["list-panes", "-t", &format!("{}:{}", session, window), "-F", "#{pane_index} #{pane_id}"])?;
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
    let stdout = tmux_cmd(&["display-message", "-p", "-t", &format!("{}:{}", session, window), "#{window_layout}"])?;
    Ok(stdout.trim().to_string())
}

fn get_active_pane_index(session: &str, window: &str) -> Result<usize> {
    let stdout = tmux_cmd(&["list-panes", "-t", &format!("{}:{}", session, window), "-F", "#{pane_index} #{pane_active}"])?;
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

    // Create all required panes
    for _ in 1..panes_data.len() {
        tmux_cmd(&["split-window", "-t", &format!("{}:{}", session, win)])?;
    }

    // Enable pane borders
    tmux_cmd(&["set-option", "-t", &format!("{}:{}", session, win), "pane-border-status", "top"])?;
    tmux_cmd(&["set-option", "-t", &format!("{}:{}", session, win), "pane-border-format", " #{pane_title} "])?;

    // Discover panes and spawn modules
    let panes = list_session_panes(session, win)?;
    let exe = exe_path()?;

    for (i, (_, pane_id)) in panes.iter().enumerate() {
        if i >= panes_data.len() { break; }
        let (cmd, label) = panes_data[i];

        let _ = tmux_cmd(&["select-pane", "-t", pane_id, "-T", label]);

        let full_cmd = format!("{} {}", exe, cmd);
        if let Err(e) = tmux_cmd(&["respawn-pane", "-k", "-t", pane_id, &full_cmd]) {
            // A single pane failure shouldn't prevent other modules from starting.
            // The pane might still have its default shell; user can check it.
            eprintln!("[spawn] failed to respawn pane {} ({}): {}", pane_id, label, e);
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
        "bind-key", "p",
        "if-shell", "-F", in_los,
        &format!("run-shell -b '{} ctl toggle'", exe),
        "previous-window",
    ]);
    tmux_cmd_ok(&[
        "bind-key", "s",
        "if-shell", "-F", in_los,
        &format!("run-shell -b '{} ctl stop'", exe),
        "choose-tree -Zs",
    ]);
}

/// Module types the conductor (and `los add`) can spawn at runtime.
pub const ADDABLE_MODULES: &[&str] = &["voice", "envelope", "sequencer", "scope", "tone", "badge"];

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
        "split-window", "-t", "los:modules", "-P", "-F", "#{pane_id}",
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
    let stdout = tmux_cmd(&["list-panes", "-t", "los:modules", "-F", "#{pane_id}\t#{pane_title}"])?;
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
    t(&["set-option", "-w", "-t", "los:modules", "pane-border-style", "fg=#4a4438,bg=#070605"]);
    t(&["set-option", "-w", "-t", "los:modules", "pane-active-border-style", "fg=#e3a818,bold,bg=#1b1610"]);
    t(&["set-option", "-w", "-t", "los:modules", "pane-border-format", " #{pane_title} "]);
    t(&["set-option", "-t", "los", "status-style", "fg=#9a7b2d,bg=#0d0b08"]);
    t(&["set-option", "-t", "los", "status-left", "#[fg=#e3a818,bold] los #[fg=#9a7b2d]· "]);
    t(&["set-option", "-t", "los", "status-right", "#[fg=#c45dd4]♪ #{session_name} "]);
    t(&["set-option", "-t", "los", "window-status-current-style", "fg=#e8dcc8,bold"]);
    t(&["set-option", "-t", "los", "window-status-style", "fg=#7d7363"]);
    // Active-pane clarity: inactive panes sink into near-black with dimmed
    // ink; the active pane sits on a clearly lifted warm charcoal with full
    // ink — plus the bold amber border. Unmissable.
    t(&["set-option", "-w", "-t", "los:modules", "window-style", "fg=#8d8170,bg=#070605"]);
    t(&["set-option", "-w", "-t", "los:modules", "window-active-style", "fg=#e8dcc8,bg=#1b1610"]);
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
/// Content-aware split sizes for the house layout: `(top_block, row2,
/// left_col)` in cells, given the window size. MIX and the VOICE|MATHs row
/// have intrinsic content heights (~10 and ~15 lines with pane titles), so
/// on tall windows they stay content-sized and the slack flows to the
/// elastic panes (SEQ, and the badge/scope column). Small windows fall
/// back to proportional splits so nothing collapses.
fn house_dims(w: usize, h: usize) -> (usize, usize, usize) {
    let top = ((h * 3) / 5).clamp(6, 26);
    let row2 = ((top * 3) / 5).clamp(3, 15);
    let col = (w / 4).clamp(20.min(w / 2), 48);
    (top, row2, col)
}

fn build_house_layout(exe: &str) -> Result<()> {
    let win = "los:modules";
    tmux_cmd(&["new-window", "-t", "los", "-n", "modules"])?;
    tmux_cmd(&["set-option", "-w", "-t", win, "pane-border-status", "top"])?;
    tmux_cmd(&["set-option", "-w", "-t", win, "pane-border-format", " #{pane_title} "])?;

    let dims = tmux_cmd(&[
        "display-message", "-p", "-t", win, "#{window_width} #{window_height}",
    ])?;
    let mut it = dims.split_whitespace().filter_map(|n| n.parse::<usize>().ok());
    let (w, h) = (it.next().unwrap_or(120), it.next().unwrap_or(40));
    let (top, row2, col) = house_dims(w, h);

    // the window's first pane becomes SEQ (bottom) after the splits
    let seq = tmux_cmd(&["list-panes", "-t", win, "-F", "#{pane_id}"])?
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    // top block (rows 1+2) above SEQ, content-sized; row 1 = mixer
    let row1 = tmux_cmd(&[
        "split-window", "-t", &seq, "-v", "-b", "-l", &top.to_string(), "-P", "-F",
        "#{pane_id}",
        &format!("{} mixer 0", exe),
    ])?;
    let row1 = row1.trim().to_string();
    // row 2 under row 1: voice | maths
    let voice = tmux_cmd(&[
        "split-window", "-t", &row1, "-v", "-l", &row2.to_string(), "-P", "-F",
        "#{pane_id}",
        &format!("{} voice 0", exe),
    ])?;
    let voice = voice.trim().to_string();
    let maths = tmux_cmd(&[
        "split-window", "-t", &voice, "-h", "-l", "50%", "-P", "-F", "#{pane_id}",
        &format!("{} envelope 0", exe),
    ])?;
    // row 1 left column: badge over scope
    let badge = tmux_cmd(&[
        "split-window", "-t", &row1, "-h", "-b", "-l", &col.to_string(), "-P", "-F",
        "#{pane_id}",
        &format!("{} badge 0", exe),
    ])?;
    let badge = badge.trim().to_string();
    let scope = tmux_cmd(&[
        "split-window", "-t", &badge, "-v", "-l", "50%", "-P", "-F", "#{pane_id}",
        &format!("{} scope 0", exe),
    ])?;
    tmux_cmd(&["respawn-pane", "-k", "-t", &seq, &format!("{} sequencer 0", exe)])?;

    for (id, title) in [
        (seq.as_str(), "SEQ"),
        (row1.as_str(), "MIX"),
        (voice.as_str(), "VOICE"),
        (maths.trim(), "MATHs"),
        (badge.as_str(), "los"),
        (scope.trim(), "SCOPE"),
    ] {
        tmux_cmd_ok(&["select-pane", "-t", id, "-T", title]);
    }
    tmux_cmd_ok(&["select-pane", "-t", &seq]);
    Ok(())
}

pub fn create_session() -> Result<()> {
    state::ensure_dirs()?;
    let _ = tmux_cmd(&["kill-session", "-t", "los"]);

    // Create conductor window
    tmux_cmd(&["new-session", "-d", "-s", "los", "-n", "conductor"])?;
    
    // Start conductor TUI in its pane
    let panes = list_session_panes("los", "conductor")?;
    let exe = exe_path()?;
    if let Some((_, pane_id)) = panes.first() {
        tmux_cmd(&["respawn-pane", "-k", "-t", pane_id, &format!("{} conductor", exe)])?;
    }
    
    // Spawn module panes (each with instance 0 by default)
    // The house layout:
    //   ┌───────┬─────────┬─────────┬───────┐
    //   │ los   │         │         │       │
    //   ├───────┤  VOICE  │  MATHs  │  MIX  │
    //   │ SCOPE │         │         │       │
    //   ├───────┴─────────┴─────────┴───────┤
    //   │               SEQ                 │
    //   └───────────────────────────────────┘
    build_house_layout(&exe)?;

    install_transport_keys(&exe);
    install_shell_theme();

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
    tmux_cmd(&["new-session", "-d", "-s", "los", "-n", "conductor"])?;

    let exe = exe_path()?;

    // Start conductor TUI
    let panes = list_session_panes("los", "conductor")?;
    if let Some((_, pane_id)) = panes.first() {
        tmux_cmd(&["respawn-pane", "-k", "-t", pane_id, &format!("{} conductor", exe)])?;
    }

    // Write module state files from the loaded session
    let module_windows: Vec<&state::WindowState> = st.windows.iter()
        .filter(|w| w.name != "conductor")
        .collect();

    for win in &module_windows {
        for pane in &win.panes {
            if let Some(ref inline) = pane.patch_inline {
                let path = state::module_state_path(&pane.module, pane.instance);
                let toml_str = toml::to_string_pretty(inline)
                    .context("serializing module params")?;
                state::write_state_file(&path, &toml_str)?;
            }
        }
    }

    // Build module list for spawning with real labels and instance numbers
    let all_panes: Vec<(String, String)> = module_windows.iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| {
            let label = format!("{} {}", capitalize(&p.module), p.instance);
            let cmd = format!("{} {}", p.module, p.instance);
            (cmd, label)
        })
        .collect();
    let all_panes_ref: Vec<(&str, &str)> = all_panes.iter()
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
                    eprintln!("[load_session] layout failed ({}), falling back to tiled", e);
                }
            }
        }
    }
    if !layout_applied {
        tmux_cmd_ok(&["select-layout", "-t", "los:modules", "tiled"]);
    }

    if !st.tmux.window_size.is_empty() {
        tmux_cmd_ok(&["set-option", "-t", "los:modules", "window-size", &st.tmux.window_size]);
    }

    for win in &st.windows {
        if win.name == "modules" {
            tmux_cmd_ok(&["select-pane", "-t", &format!("los:modules.{}", win.active_pane)]);
        }
    }

    install_transport_keys(&exe);
    install_shell_theme();

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
    let module_windows: Vec<&state::WindowState> = st.windows.iter()
        .filter(|w| w.name != "conductor")
        .collect();

    for win in &module_windows {
        for pane in &win.panes {
            if let Some(ref inline) = pane.patch_inline {
                let path = state::module_state_path(&pane.module, pane.instance);
                let toml_str = toml::to_string_pretty(inline)
                    .context("serializing module params")?;
                state::write_state_file(&path, &toml_str)?;
            }
        }
    }

    // Build module list for spawning with real labels and instance numbers
    let all_panes: Vec<(String, String)> = module_windows.iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| {
            let label = format!("{} {}", capitalize(&p.module), p.instance);
            let cmd = format!("{} {}", p.module, p.instance);
            (cmd, label)
        })
        .collect();
    let all_panes_ref: Vec<(&str, &str)> = all_panes.iter()
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
            tmux_cmd_ok(&["select-pane", "-t", &format!("los:modules.{}", win.active_pane)]);
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
            modules = manifest_ro.as_ref().map(|m| m.entries()).unwrap_or_default();
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
                    Style::default().fg(Color::Red).add_modifier(ratatui::style::Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan).add_modifier(ratatui::style::Modifier::BOLD)
                };
                let title = Paragraph::new(header)
                    .style(header_style)
                    .block(Block::default().borders(Borders::ALL));
                f.render_widget(title, chunks[0]);
                
                if view_modules {
                    let items: Vec<ListItem> = modules.iter().enumerate().map(|(i, e)| {
                        let outputs = match e.mod_base {
                            Some(base) => {
                                let labels = crate::routing::output_labels(&e.module_name);
                                let shown: Vec<&str> = labels.iter().take(e.mod_count).copied().collect();
                                format!("  outputs ch{}-{}: {}", base, base + e.mod_count - 1, shown.join(","))
                            }
                            None => String::new(),
                        };
                        let audio = if e.audio_shm.is_some() { "  [audio]" } else { "" };
                        let text = format!("{} {}  pid {}{}{}", e.module_name, e.instance, e.pid, audio, outputs);
                        let style = if i == msel {
                            Style::default().fg(Color::Yellow).add_modifier(ratatui::style::Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        ListItem::new(text).style(style)
                    }).collect();
                    let list = List::new(items)
                        .block(Block::default().borders(Borders::ALL).title("Running Modules"));
                    f.render_widget(list, chunks[1]);

                    if let Some(sel) = add_picker {
                        let rows: Vec<ListItem> = ADDABLE_MODULES.iter().enumerate().map(|(i, m)| {
                            let style = if i == sel {
                                Style::default().fg(Color::Black).bg(Color::Yellow)
                            } else {
                                Style::default().fg(Color::White)
                            };
                            ListItem::new(*m).style(style)
                        }).collect();
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
                    let items: Vec<ListItem> = entries.iter().enumerate().map(|(i, name)| {
                        let style = if i == selected {
                            Style::default().fg(Color::Yellow).add_modifier(ratatui::style::Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        ListItem::new(name.as_str()).style(style)
                    }).collect();
                    
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
                        // Query tmux for actual pane order and discover module instances
                        let mut pane_info: Vec<(String, usize)> = Vec::new();
                        if let Ok(titles) = tmux_cmd(&["list-panes", "-t", "los:modules", "-F", "#{pane_title}"]) {
                            for title in titles.lines() {
                                let t = title.trim();
                                let parts: Vec<&str> = t.split_whitespace().collect();
                                if parts.is_empty() { continue; }
                                let module_name = parts[0].to_lowercase();
                                let instance = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                                pane_info.push((module_name, instance));
                            }
                        }
                        // Fallback to default order if tmux query fails or returns nothing
                        if pane_info.is_empty() {
                            pane_info = vec![
                                ("sequencer".into(), 0),
                                ("voice".into(), 0),
                                ("mixer".into(), 0),
                                ("scope".into(), 0),
                                ("envelope".into(), 0),
                            ];
                        }

                        // Read PIDs from manifest + pid files
                        let manifest = Manifest::open().ok();
                        let mut pids = Vec::new();
                        for (mod_name, instance) in &pane_info {
                            let pid = manifest.as_ref().and_then(|m| {
                                m.entries().iter().find(|e| {
                                    e.module_name == *mod_name && e.instance == *instance
                                }).map(|e| e.pid)
                            }).or_else(|| {
                                state::read_pid_file(mod_name, *instance)
                            });
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

                        // Collect module state files from tmp in pane order
                        let mut panes = Vec::new();
                        for (mod_name, instance) in pane_info.iter() {
                            let path = state::module_state_path(mod_name, *instance);
                            let exists = path.exists();
                            let inline = if exists {
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
                        
                        // Capture current tmux layout and active pane
                        let layout = get_window_layout("los", "modules").unwrap_or_default();
                        let active_pane = get_active_pane_index("los", "modules").unwrap_or(1);

                        // Prompt for filename
                        let now = chrono_or_fallback();
                        let default_name = format!("session-{}", now);
                        let filename = if let Ok(name) = prompt_string(&mut terminal, "Save as:", &default_name) {
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
                            windows: vec![state::WindowState {
                                name: "modules".into(),
                                layout,
                                active_pane,
                                panes,
                            }],
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
                    .block(Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan))
                        .title("Help"));
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
fn prompt_string(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, prompt: &str, default: &str) -> Result<String> {
    let mut buf = String::new();
    loop {
        terminal.draw(|f| {
            let area = f.area();
            let text = format!("{} [{}]", prompt, if buf.is_empty() { default } else { &buf });
            let para = Paragraph::new(text)
                .style(Style::default().fg(Color::Yellow))
                .block(Block::default().borders(Borders::ALL).title("Input"));
            f.render_widget(para, area);
        })?;
        
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Enter => {
                        if buf.is_empty() { return Ok(default.to_string()); }
                        return Ok(buf);
                    }
                    KeyCode::Esc => return Ok(default.to_string()),
                    KeyCode::Char(c) if c.is_ascii_alphanumeric() || c == '-' || c == '_' => {
                        buf.push(c);
                    }
                    KeyCode::Backspace => { buf.pop(); }
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
    fn house_dims_adapt_to_window() {
        // tall window: rows stay content-sized, slack flows to SEQ
        let (top, row2, _) = house_dims(180, 80);
        assert_eq!(top, 26, "top block caps at content height");
        assert_eq!(row2, 15, "voice/maths row caps at content height");
        // mid window (the gif terminal): proportional
        let (top, row2, col) = house_dims(140, 40);
        assert_eq!(top, 24);
        assert_eq!(row2, 14);
        assert_eq!(col, 35);
        // small window: everything stays usable, nothing collapses
        let (top, row2, col) = house_dims(60, 20);
        assert!((6..20).contains(&top));
        assert!((3..top).contains(&row2));
        assert!((15..=30).contains(&col));
        // degenerate sizes never panic or go to zero
        let (top, row2, col) = house_dims(4, 4);
        assert!(top >= 1 && row2 >= 1 && col >= 1);
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
        assert_eq!(shell_escape(path), "'/Users/jake/.config/los/states/YES.toml'");
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
                    state::PaneState { module: "a".into(), instance: 0, patch: None, patch_inline: None },
                    state::PaneState { module: "b".into(), instance: 0, patch: None, patch_inline: None },
                    state::PaneState { module: "c".into(), instance: 0, patch: None, patch_inline: None },
                    state::PaneState { module: "d".into(), instance: 0, patch: None, patch_inline: None },
                    state::PaneState { module: "e".into(), instance: 0, patch: None, patch_inline: None },
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
                    state::PaneState { module: "a".into(), instance: 0, patch: None, patch_inline: None },
                    state::PaneState { module: "b".into(), instance: 0, patch: None, patch_inline: None },
                ],
            }],
        };

        let toml = state::to_toml_string(&state).expect("serialize");
        let loaded: state::SessionState = toml::from_str(&toml).expect("deserialize");

        assert_eq!(loaded.windows[0].layout, raw_layout);
    }
}
