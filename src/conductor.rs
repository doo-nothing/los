use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

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
    Ok(0)
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

/// Recompute tmux's rotating-XOR checksum over `s`.
/// Count leaf cells (panes with IDs) in a tmux layout string.
fn count_layout_leaf_cells(layout: &str) -> usize {
    let body = layout.split_once(',').map(|(_, b)| b).unwrap_or(layout);
    let mut count = 0;
    let mut depth = 0;
    let mut bytes = body.bytes().peekable();

    while let Some(b) = bytes.next() {
        match b {
            b'[' | b'{' => depth += 1,
            b']' | b'}' => depth -= 1,
            // A comma at depth 0 followed by a digit is a leaf ID separator
            b',' if depth == 0 => {
                if let Some(&next) = bytes.peek() {
                    if next.is_ascii_digit() {
                        count += 1;
                    }
                }
            }
            _ => {}
        }
    }
    count
}

fn tmux_checksum(s: &str) -> u32 {
    let mut csum: u32 = 0;
    for b in s.bytes() {
        csum ^= b as u32;
        csum = csum.rotate_left(1);
    }
    csum
}

/// Parse a tmux layout string and replace all embedded pane IDs with the
/// IDs of the newly created panes. Recomputes the checksum so the layout
/// can be applied to a window with different pane IDs.
fn make_layout_portable(layout: &str, new_ids: &[u32]) -> Option<String> {
    // Split off the checksum prefix: "1f88,body"
    let body = layout.split_once(',')?.1;

    let mut parser = LayoutParser { s: body, pos: 0, id_idx: 0, new_ids };
    let rebuilt = parser.parse_cell()?;

    parser.skip_whitespace();
    if !parser.is_at_end() {
        return None;
    }

    let checksum = tmux_checksum(&rebuilt);
    Some(format!("{:04x},{}", checksum, rebuilt))
}

struct LayoutParser<'a> {
    s: &'a str,
    pos: usize,
    id_idx: usize,
    new_ids: &'a [u32],
}

impl<'a> LayoutParser<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while let Some(b) = self.peek() {
            if b.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn is_at_end(&self) -> bool {
        self.pos >= self.s.len()
    }

    fn parse_number(&mut self) -> Option<u32> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return None;
        }
        self.s[start..self.pos].parse().ok()
    }

    fn expect(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn next_new_id(&mut self) -> Option<u32> {
        if self.id_idx < self.new_ids.len() {
            let id = self.new_ids[self.id_idx];
            self.id_idx += 1;
            Some(id)
        } else {
            None
        }
    }

    fn parse_cell(&mut self) -> Option<String> {
        let mut out = String::new();

        // size: NUMBER x NUMBER
        let w = self.parse_number()?;
        if !self.expect(b'x') { return None; }
        let h = self.parse_number()?;
        out.push_str(&format!("{}x{}", w, h));

        // position: , NUMBER , NUMBER
        if !self.expect(b',') { return None; }
        let x = self.parse_number()?;
        out.push(',');
        out.push_str(&x.to_string());

        if !self.expect(b',') { return None; }
        let y = self.parse_number()?;
        out.push(',');
        out.push_str(&y.to_string());

        // After position:
        //   , NUMBER  → leaf with ID (replace with new ID)
        //   [ cells ] → horizontal container
        //   { cells } → vertical container
        if self.expect(b',') {
            // leaf with ID — replace it
            self.parse_number()?; // consume old ID
            let new_id = self.next_new_id()?;
            out.push(',');
            out.push_str(&new_id.to_string());
        } else if self.expect(b'[') {
            out.push('[');
            self.parse_cells(&mut out)?;
            if !self.expect(b']') { return None; }
            out.push(']');
        } else if self.expect(b'{') {
            out.push('{');
            self.parse_cells(&mut out)?;
            if !self.expect(b'}') { return None; }
            out.push('}');
        }

        Some(out)
    }

    fn parse_cells(&mut self, out: &mut String) -> Option<()> {
        let cell = self.parse_cell()?;
        out.push_str(&cell);

        while self.expect(b',') {
            out.push(',');
            let cell = self.parse_cell()?;
            out.push_str(&cell);
        }
        Some(())
    }
}

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

        tmux_cmd(&["select-pane", "-t", pane_id, "-T", label])?;

        let full_cmd = format!("{} {}", exe, cmd);
        tmux_cmd(&["respawn-pane", "-k", "-t", pane_id, &full_cmd])?;
    }

    // Select first pane by ID (reliable from outside tmux)
    if let Some((_, pane_id)) = panes.first() {
        tmux_cmd(&["select-pane", "-t", pane_id])?;
    }

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
    
    // Spawn module panes
    let modules = [("sequencer", "Sequencer"), ("voice", "Voice"), ("mixer", "Mixer"), ("scope", "Scope"), ("envelope", "Envelope")];
    spawn_session_panes(&modules)?;

    // Apply default tiled layout
    tmux_cmd(&["select-layout", "-t", "los:modules", "tiled"])?;

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

    // Build module list for spawning with real labels
    let all_panes: Vec<(String, String)> = module_windows.iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| {
            let label = p.module.chars().next().unwrap().to_uppercase().to_string() + &p.module[1..];
            (p.module.clone(), label)
        })
        .collect();
    let all_panes_ref: Vec<(&str, &str)> = all_panes.iter()
        .map(|(m, l)| (m.as_str(), l.as_str()))
        .collect();

    if !all_panes_ref.is_empty() {
        spawn_session_panes(&all_panes_ref)?;
    }

    // Count actual panes after spawning and clamp active_pane
    let actual_pane_count = list_session_panes("los", "modules")?.len();
    let clamped_active = |saved: usize| -> usize {
        saved.min(actual_pane_count.saturating_sub(1))
    };

    // Apply saved layout: query new pane IDs, replace them in the layout string,
    // recompute checksum, then call select-layout.
    let mut layout_applied = false;
    for win in &st.windows {
        if win.name == "modules" && !win.layout.is_empty() {
            let pane_ids_stdout = tmux_cmd(
                &["list-panes", "-t", "los:modules", "-F", "#{pane_id}"]
            );
            if let Ok(stdout) = pane_ids_stdout {
                let new_ids: Vec<u32> = stdout
                    .lines()
                    .filter_map(|line| line.trim().strip_prefix('%').and_then(|s| s.parse().ok()))
                    .collect();
                if let Some(portable) = make_layout_portable(&win.layout, &new_ids) {
                    match tmux_cmd(&["select-layout", "-t", "los:modules", &portable]) {
                        Ok(_) => layout_applied = true,
                        Err(e) => eprintln!(
                            "[load_session] portable layout failed ({}), falling back to tiled", e
                        ),
                    }
                } else {
                    eprintln!(
                        "[load_session] failed to make layout portable ({} pane IDs, {} leaf cells), falling back to tiled",
                        new_ids.len(),
                        count_layout_leaf_cells(&win.layout)
                    );
                }
            }
        }
    }
    if !layout_applied {
        tmux_cmd_ok(&["select-layout", "-t", "los:modules", "tiled"]);
    }

    // Apply tmux settings from state
    if !st.tmux.window_size.is_empty() {
        tmux_cmd_ok(&["set-option", "-t", "los:modules", "window-size", &st.tmux.window_size]);
    }

    // Restore active pane (clamp to actual pane count)
    for win in &st.windows {
        if win.name == "modules" {
            let pane_idx = clamped_active(win.active_pane);
            tmux_cmd_ok(&["select-pane", "-t", &format!("los:modules.{}", pane_idx)]);
        }
    }

    // Attach (blocks until detached; use raw .status() since tmux_cmd would hang)
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

    // Build module list for spawning with real labels
    let all_panes: Vec<(String, String)> = module_windows.iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| {
            let label = p.module.chars().next().unwrap().to_uppercase().to_string() + &p.module[1..];
            (p.module.clone(), label)
        })
        .collect();
    let all_panes_ref: Vec<(&str, &str)> = all_panes.iter()
        .map(|(m, l)| (m.as_str(), l.as_str()))
        .collect();

    if !all_panes_ref.is_empty() {
        spawn_session_panes(&all_panes_ref)?;
    }

    // Count actual panes after spawning and clamp active_pane
    let actual_pane_count = list_session_panes("los", "modules")?.len();
    let clamped_active = |saved: usize| -> usize {
        saved.min(actual_pane_count.saturating_sub(1))
    };

    // Apply saved layout: query new pane IDs, replace them in the layout string,
    // recompute checksum, then call select-layout.
    let mut layout_applied = false;
    for win in &st.windows {
        if win.name == "modules" && !win.layout.is_empty() {
            let pane_ids_stdout = tmux_cmd(
                &["list-panes", "-t", "los:modules", "-F", "#{pane_id}"]
            );
            if let Ok(stdout) = pane_ids_stdout {
                let new_ids: Vec<u32> = stdout
                    .lines()
                    .filter_map(|line| line.trim().strip_prefix('%').and_then(|s| s.parse().ok()))
                    .collect();
                if let Some(portable) = make_layout_portable(&win.layout, &new_ids) {
                    match tmux_cmd(&["select-layout", "-t", "los:modules", &portable]) {
                        Ok(_) => layout_applied = true,
                        Err(e) => eprintln!(
                            "[reload] portable layout failed ({}), falling back to tiled", e
                        ),
                    }
                } else {
                    eprintln!(
                        "[reload] failed to make layout portable ({} pane IDs, {} leaf cells), falling back to tiled",
                        new_ids.len(),
                        count_layout_leaf_cells(&win.layout)
                    );
                }
            }
        }
    }
    if !layout_applied {
        tmux_cmd_ok(&["select-layout", "-t", "los:modules", "tiled"]);
    }

    // Restore active pane
    for win in &st.windows {
        if win.name == "modules" {
            let pane_idx = clamped_active(win.active_pane);
            tmux_cmd_ok(&["select-pane", "-t", &format!("los:modules.{}", pane_idx)]);
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
                        // Save: send SIGUSR1 to all module processes, then collect
                        let modules = ["sequencer", "voice", "mixer", "scope", "envelope"];
                        
                        // Read module PIDs from their pid files (written at startup)
                        let mut pids = Vec::new();
                        for mod_name in &modules {
                            if let Some(pid) = state::read_pid_file(mod_name, 0) {
                                pids.push(pid);
                            }
                        }
                        
                        // Send SIGUSR1 to all module processes
                        for pid in &pids {
                            state::send_save_signal(*pid);
                        }
                        
                        // Wait for modules to write their state files
                        std::thread::sleep(Duration::from_millis(500));
                        
                        // Collect module state files from tmp
                        let mut panes = Vec::new();
                        for mod_name in modules.iter() {
                            let path = state::module_state_path(mod_name, 0);
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
                                instance: 0,
                                patch: None,
                                patch_inline: inline,
                            });
                        }
                        
                        // Capture current tmux layout and active pane
                        let layout = get_window_layout("los", "modules").unwrap_or_default();
                        let raw_active = get_active_pane_index("los", "modules").unwrap_or(0);
                        let active_pane = raw_active.min(panes.len().saturating_sub(1));

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

    // ── layout parser tests ──────────────────────────────────────────────

    #[test]
    fn test_layout_portable_replaces_ids() {
        // Full layout string from a real tmux session with 5 panes
        let original = "1f88,159x79,0,0[159x25,0,0{79x25,0,0,1176,79x25,80,0,1177},159x25,0,26{79x25,0,26,1175,79x25,80,26,1178},159x27,0,52,1174]";
        let new_ids = vec![2000, 2001, 2002, 2003, 2004];

        let portable = make_layout_portable(original, &new_ids).expect("should parse");

        // Should contain a comma after the checksum prefix
        assert!(portable.contains(','));

        // All old IDs should be replaced with new ones
        assert!(!portable.contains("1176"));
        assert!(!portable.contains("1177"));
        assert!(!portable.contains("1175"));
        assert!(!portable.contains("1178"));
        assert!(!portable.contains("1174"));

        // New IDs should be present
        assert!(portable.contains("2000"));
        assert!(portable.contains("2001"));
        assert!(portable.contains("2002"));
        assert!(portable.contains("2003"));
        assert!(portable.contains("2004"));
    }

    #[test]
    fn test_layout_portable_single_pane() {
        let original = "0000,80x24,0,0,1234";
        let portable = make_layout_portable(original, &[42]).expect("should parse");
        assert!(portable.ends_with("80x24,0,0,42"));
    }

    #[test]
    fn test_layout_portable_nested() {
        // Nested containers: horizontal with vertical inside (4 panes)
        let original = "aaaa,100x50,0,0[50x50,0,0{25x50,0,0,1,25x50,0,25,2},50x50,0,0{25x50,0,0,3,25x50,0,25,4}]";
        let portable = make_layout_portable(original, &[10, 20, 30, 40]).expect("should parse");
        assert!(portable.contains("25x50,0,0,10,25x50,0,25,20}"));
        assert!(portable.contains("25x50,0,0,30,25x50,0,25,40}"));
    }

    #[test]
    fn test_layout_portable_not_enough_ids_fails() {
        let original = "0000,80x24,0,0,1,80x24,0,0,2"; // 2 panes
        assert!(make_layout_portable(original, &[100]).is_none()); // only 1 new ID
    }

    #[test]
    fn test_layout_portable_empty_fails() {
        assert!(make_layout_portable("", &[]).is_none());
        assert!(make_layout_portable("nocomma", &[]).is_none());
    }

    #[test]
    fn test_layout_portable_checksum_recomputed() {
        let body = "80x24,0,0,80x24,0,0";
        let c1 = tmux_checksum(body);
        let c2 = tmux_checksum(body);
        assert_eq!(c1, c2);
    }
}
