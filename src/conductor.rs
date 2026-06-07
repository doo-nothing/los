use std::io;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
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

// ── session creation ───────────────────────────────────────────────────────

fn exe_path() -> Result<String> {
    Ok(std::env::current_exe()?
        .to_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "los".into()))
}

fn list_session_panes(session: &str, window: &str) -> Result<Vec<(usize, String)>> {
    let output = Command::new("tmux")
        .args(["list-panes", "-t", &format!("{}:{}", session, window), "-F", "#{pane_index} #{pane_id}"])
        .output()?;
    let mut panes: Vec<(usize, String)> = String::from_utf8(output.stdout)?
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

fn spawn_session_panes(panes_data: &[(&str, &str)]) -> Result<()> {
    let session = "los";
    let win = "modules";
    
    // Create window
    Command::new("tmux")
        .args(["new-window", "-t", session, "-n", win])
        .output()?;
    
    // Split into required number of panes
    for _ in 1..panes_data.len() {
        Command::new("tmux")
            .args(["split-window", "-t", &format!("{}:{}", session, win)])
            .output()?;
    }
    Command::new("tmux")
        .args(["select-layout", "-t", &format!("{}:{}", session, win), "tiled"])
        .output()?;
    
    // Enable pane borders
    Command::new("tmux")
        .args(["set-option", "-t", &format!("{}:{}", session, win), "pane-border-status", "top"])
        .output()?;
    Command::new("tmux")
        .args(["set-option", "-t", &format!("{}:{}", session, win), "pane-border-format", " #{pane_title} "])
        .output()?;
    
    // Discover panes and spawn modules
    let panes = list_session_panes(session, win)?;
    let exe = exe_path()?;
    
    for (i, (_, pane_id)) in panes.iter().enumerate() {
        if i >= panes_data.len() { break; }
        let (cmd, label) = panes_data[i];
        
        Command::new("tmux")
            .args(["select-pane", "-t", pane_id, "-T", label])
            .output()?;
        
        let full_cmd = format!("{} {}", exe, cmd);
        Command::new("tmux")
            .args(["respawn-pane", "-k", "-t", pane_id, &full_cmd])
            .output()?;
    }
    
    // Select first pane
    if let Some((_, first_pane_id)) = panes.first() {
        Command::new("tmux")
            .args(["select-pane", "-t", first_pane_id])
            .output()?;
    }
    
    Ok(())
}

pub fn create_session() -> Result<()> {
    state::ensure_dirs()?;
    let _ = Command::new("tmux").args(["kill-session", "-t", "los"]).output();

    // Create conductor window
    Command::new("tmux")
        .args(["new-session", "-d", "-s", "los", "-n", "conductor"])
        .output()?;
    
    // Start conductor TUI in its pane
    let panes = list_session_panes("los", "conductor")?;
    let exe = exe_path()?;
    if let Some((_, pane_id)) = panes.first() {
        Command::new("tmux")
            .args(["respawn-pane", "-k", "-t", pane_id, &format!("{} conductor", exe)])
            .output()?;
    }
    
    // Spawn module panes
    let modules = [("sequencer", "Sequencer"), ("voice", "Voice"), ("mixer", "Mixer"), ("scope", "Scope")];
    spawn_session_panes(&modules)?;
    
    // Select modules window, first pane, and attach
    Command::new("tmux")
        .args(["select-window", "-t", "los:modules"])
        .output()?;
    Command::new("tmux")
        .args(["select-pane", "-t", "los:modules.0"])
        .output()?;
    Command::new("tmux")
        .args(["attach-session", "-t", "los"])
        .status()?;
    
    Ok(())
}

pub fn load_session(state_path: &str) -> Result<()> {
    state::ensure_dirs()?;
    
    // Read the state file
    let st = state::from_toml_file::<state::SessionState>(std::path::Path::new(state_path))?;
    
    // Kill existing session
    let _ = Command::new("tmux").args(["kill-session", "-t", "los"]).output();
    
    // Create conductor window
    Command::new("tmux")
        .args(["new-session", "-d", "-s", "los", "-n", "conductor"])
        .output()?;
    
    let exe = exe_path()?;
    
    // Start conductor TUI
    let panes = list_session_panes("los", "conductor")?;
    if let Some((_, pane_id)) = panes.first() {
        Command::new("tmux")
            .args(["respawn-pane", "-k", "-t", pane_id, &format!("{} conductor", exe)])
            .output()?;
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
    
    // Build module list for spawning
    let all_panes: Vec<(&str, &str)> = module_windows.iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| (p.module.as_str(), "Module"))
        .collect();
    
    if !all_panes.is_empty() {
        spawn_session_panes(&all_panes)?;
    }
    
    // Apply tmux settings from state
    if !st.tmux.window_size.is_empty() {
        let _ = Command::new("tmux")
            .args(["set-option", "-t", "los:modules", "window-size", &st.tmux.window_size])
            .output();
    }
    
    // Select active window, saved pane (or first), and attach
    let active_win = if st.tmux.active_window.is_empty() { "modules" } else { &st.tmux.active_window };
    Command::new("tmux")
        .args(["select-window", "-t", &format!("los:{}", active_win)])
        .output()?;
    let pane_idx = st.tmux.active_pane;
    Command::new("tmux")
        .args(["select-pane", "-t", &format!("los:{}.{}", active_win, pane_idx)])
        .output()?;
    Command::new("tmux")
        .args(["attach-session", "-t", "los"])
        .status()?;
    
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
                        let modules = ["sequencer", "voice", "mixer", "scope"];
                        
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
                        
                        // Prompt for filename
                        let now = chrono_or_fallback();
                        let default_name = format!("session-{}", now);
                        let filename = if let Ok(name) = prompt_string(&mut terminal, "Save as:", &default_name) {
                            format!("{}.toml", name)
                        } else {
                            format!("{}.toml", default_name)
                        };
                        let save_path = state::states_dir().join(&filename);
                        
                        // Capture active pane from modules window
                        let active_pane = Command::new("tmux")
                            .args(["list-panes", "-t", "los:modules", "-F", "#{pane_active} #{pane_index}"])
                            .output()
                            .ok()
                            .and_then(|o| String::from_utf8(o.stdout).ok())
                            .and_then(|s| {
                                s.lines()
                                    .find(|l| l.starts_with('1'))
                                    .and_then(|l| l.split_whitespace().nth(1))
                                    .and_then(|idx| idx.parse::<i64>().ok())
                            })
                            .unwrap_or(0);
                        
                        let session_state = state::SessionState {
                            meta: state::Meta {
                                name: filename.trim_end_matches(".toml").to_string(),
                                created: now,
                            },
                            tmux: state::TmuxState {
                                active_pane,
                                ..state::TmuxState::default()
                            },
                            windows: vec![state::WindowState {
                                name: "modules".into(),
                                layout: "tiled".into(),
                                panes,
                            }],
                        };
                        
                        if let Ok(toml_str) = state::to_toml_string(&session_state) {
                            let _ = state::write_state_file(&save_path, &toml_str);
                        }
                        needs_refresh = true;
                        
                    }
                    KeyCode::Char('l') if selected < entries.len() => {
                        // Load selected state: write module state files, send SIGUSR2
                        let path = state::states_dir().join(&entries[selected]);
                        
                        // Read the saved state file
                        if let Ok(st) = state::from_toml_file::<state::SessionState>(&path) {
                            // Write module state files from the loaded session
                            for win in &st.windows {
                                for pane in &win.panes {
                                    if let Some(ref inline) = pane.patch_inline {
                                        let filepath = state::module_state_path(&pane.module, pane.instance);
                                        let toml_str = toml::to_string_pretty(inline)
                                            .unwrap_or_default();
                                        let _ = state::write_state_file(&filepath, &toml_str);
                                    }
                                }
                            }
                            
                            // Send SIGUSR2 to all module processes to trigger reload
                            let modules = ["sequencer", "voice", "mixer", "scope"];
                            for mod_name in &modules {
                                if let Some(pid) = state::read_pid_file(mod_name, 0) {
                                    state::send_reload_signal(pid);
                                }
                            }
                            
                            needs_refresh = true;
                        }
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
                    Line::from("  l          Load selected state (creates new session)"),
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
