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
    let modules = [
        ("sequencer 0", "Sequencer 0"),
        ("voice 0", "Voice 0"),
        ("mixer 0", "Mixer 0"),
        ("scope 0", "Scope 0"),
        ("envelope 0", "Envelope 0"),
    ];
    spawn_session_panes(&modules)?;

    // Apply default tiled layout
    tmux_cmd(&["select-layout", "-t", "los:modules", "tiled"])?;

    install_transport_keys(&exe);

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
    // Global transport handle for Space = play/pause (lazily reopened)
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();
    
    // Shared state entries list
    let mut entries: Vec<String> = Vec::new();
    let mut needs_refresh = true;
    
    loop {
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
                
                let title = Paragraph::new("LOS Conductor")
                    .style(Style::default().fg(Color::Cyan).add_modifier(ratatui::style::Modifier::BOLD))
                    .block(Block::default().borders(Borders::ALL));
                f.render_widget(title, chunks[0]);
                
                if entries.is_empty() {
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
                
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        if selected + 1 < entries.len() {
                            selected += 1;
                        }
                        needs_refresh = true;
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        selected = selected.saturating_sub(1);
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
                    KeyCode::Char('l') if selected < entries.len() => {
                        // Full module reload while keeping the conductor alive.
                        let path = state::states_dir().join(&entries[selected]);
                        if let Err(e) = reload_modules_from_state(&path) {
                            eprintln!("[conductor] reload failed: {}", e);
                        }
                        needs_refresh = true;
                    }
                    KeyCode::Char('d') if selected < entries.len() => {
                        // Delete selected state
                        let path = state::states_dir().join(&entries[selected]);
                        let _ = std::fs::remove_file(&path);
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
                        show_help = true;
                        needs_refresh = true;
                    }
                    _ => {}
                }
            }
        }
        
        if show_help {
            terminal.draw(|f| {
                let help_text = vec![
                    Line::from("━━━ Conductor Help ━━━"),
                    Line::from(""),
                    Line::from("  j/k, ↑/↓  Navigate state list"),
                    Line::from("  s          Save current session state"),
                    Line::from("  l          Load selected state (full session reload)"),
                    Line::from("  d          Delete selected state"),
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
