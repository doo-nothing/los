use std::io::{self, BufRead, Write};
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result};

use crate::layout::Layout;
use crate::tmux;

fn los_bin() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "los".into())
}

fn create_session(layout: &Layout, los_bin: &str) -> Result<()> {
    let session = &layout.session_name;

    if tmux::session_exists(session) {
        anyhow::bail!("session '{session}' already exists");
    }

    let total = layout.total_modules();
    if total == 0 {
        anyhow::bail!("no modules defined in layout");
    }

    // Create a detached session with one pane.
    tmux::cmd(["new-session", "-d", "-s", session, "-n", "main"])
        .context("creating tmux session")?;

    // Set an initial size large enough for all splits.
    // When the user attaches, tmux resizes to the client's terminal.
    tmux::cmd(["resize-window", "-t", &format!("{session}:0"), "-x", "80", "-y", "40"])
        .ok();

    // Split vertically: each split creates a pane below the current one.
    for _ in 1..total {
        tmux::cmd(["split-window", "-v", "-t", &format!("{session}:0")])
            .context("splitting window")?;
    }

    // Even vertical layout gives each pane full width and equal height.
    tmux::cmd(["select-layout", "-t", &format!("{session}:0"), "even-vertical"])?;

    // Discover panes sorted by pane_index
    let pane_output = tmux::cmd([
        "list-panes",
        "-t",
        &format!("{session}:0"),
        "-F",
        "#{pane_index} #{pane_id}",
    ])?;

    let mut panes: Vec<(usize, String)> = pane_output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let idx = parts.next()?.parse::<usize>().ok()?;
            let id = parts.next()?.to_string();
            Some((idx, id))
        })
        .collect();

    panes.sort_by_key(|(idx, _)| *idx);

    // Build flat list of module → pane assignments
    let mut assignments: Vec<(String, usize)> = Vec::new();
    for module in &layout.modules {
        for instance in 0..module.count {
            assignments.push((module.kind.clone(), instance));
        }
    }

    // Show pane titles in the pane border so the user can see what's what.
    tmux::cmd(["set-window-option", "-t", &format!("{session}:0"), "pane-border-status", "top"]).ok();

    // Label and spawn each module in its pane
    for (pane_idx, (kind, instance)) in assignments.iter().enumerate() {
        if pane_idx >= panes.len() {
            break;
        }
        let (_, pane_id) = &panes[pane_idx];

        // Set pane title (shown in the top border)
        let label = if *instance > 0 {
            format!("{kind}#{}", *instance + 1)
        } else {
            kind.clone()
        };
        tmux::cmd(["select-pane", "-t", pane_id, "-T", &label]).ok();

        // Custom command from layout, or default
        let custom_cmd = layout.modules.iter().find(|m| m.kind == *kind).and_then(|m| m.command.as_ref());
        let cmd = match custom_cmd {
            Some(custom) => custom.clone(),
            None => format!("clear; {} {} {}", los_bin, kind, instance + 1),
        };

        tmux::cmd(["send-keys", "-t", pane_id, &cmd, "Enter"])
            .with_context(|| format!("spawning {kind} #{instance} in {pane_id}"))?;
    }

    // Resolve module pane IDs by looking up the kind in assignments
    let find_pane = |kind: &str| -> Option<String> {
        assignments.iter().position(|(k, _)| k == kind).and_then(|i| {
            panes.get(i).map(|(_, id)| id.clone())
        })
    };

    // Bind keys: send single-letter commands to specific module panes.
    //   p — toggle sequencer play
    //   s — stop sequencer
    //   r — (future: toggle recording)
    //   q — quit on conductor (which will exit)
    let conductor_target = panes.first().map(|(_, id)| id.clone()).unwrap_or("%0".into());
    let sequencer_target = find_pane("sequencer").unwrap_or_else(|| conductor_target.clone());

    tmux::cmd(["bind-key", "-T", "prefix", "p", "send-keys", "-t", &sequencer_target, "p", "Enter"])
        .context("binding prefix-p")?;
    tmux::cmd(["bind-key", "-T", "prefix", "s", "send-keys", "-t", &sequencer_target, "s", "Enter"])
        .context("binding prefix-s")?;
    tmux::cmd(["bind-key", "-T", "prefix", "q", "send-keys", "-t", &conductor_target, "quit", "Enter"])
        .context("binding prefix-q")?;

    eprintln!("los: created session '{session}' with {total} panes");
    Ok(())
}

pub fn run_create(attach: bool) -> Result<()> {
    tmux::check_available()?;

    let layout = Layout::load()?;
    let session = &layout.session_name;
    let exists = tmux::session_exists(session);

    if !exists {
        let bin = los_bin();
        create_session(&layout, &bin)?;
    }

    if !attach {
        if exists {
            eprintln!("los: session '{session}' already exists");
        } else {
            eprintln!("los: session '{session}' created. Use `tmux attach -t {session}` to connect.");
        }
        return Ok(());
    }

    // Try to put the user in the session.
    // Inside tmux: switch-client (instant, non-blocking).
    // Outside tmux: exec tmux attach-session (takes over the terminal).
    if std::env::var("TMUX").is_ok() {
        if tmux::cmd(["switch-client", "-t", session]).is_ok() {
            eprintln!("los: switched to session '{session}'.");
            return Ok(());
        }
        eprintln!("los: use `tmux switch-client -t {session}` to connect.");
        return Ok(());
    }

    // Outside tmux: exec attach-session. This replaces the process.
    let err = std::process::Command::new("tmux")
        .args(["attach-session", "-t", session])
        .exec();
    // If we get here, exec failed.
    let msg = if exists {
        format!("los: session '{session}' already exists")
    } else {
        format!("los: session '{session}' created")
    };
    eprintln!("{msg}. Could not attach: {err}");

    // macOS fallback: open a new Terminal window
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "tell application \"Terminal\" to do script \"tmux attach -t {session}\""
        );
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .output();
        eprintln!("los: opened a new Terminal window for session '{session}'.");
    }

    #[cfg(not(target_os = "macos"))]
    eprintln!("los: run `tmux attach -t {session}` in a terminal to connect.");

    Ok(())
}

pub fn run_monitor() -> Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();

    // Print banner
    println!("los conductor v0.2 — awaiting commands");

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .context("conductor: reading stdin")?;

        if n == 0 {
            break; // EOF
        }

        let cmd = line.trim();
        match cmd {
            "play" => println!("▶ play"),
            "stop" => println!("■ stop"),
            "record" => println!("● record"),
            "quit" | "q" => {
                println!("los: goodbye");
                break;
            }
            "" => {}
            other => println!("? {other}"),
        }

        io::stdout().flush().ok();
    }

    Ok(())
}
