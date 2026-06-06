use std::io::{self, BufRead, Write};

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

    // Create a detached session with one pane
    tmux::cmd(["new-session", "-d", "-s", session, "-n", "main"])
        .context("creating tmux session")?;

    // Enlarge the window so all splits have room (400 cols × 100 rows)
    tmux::cmd(["resize-window", "-t", &format!("{session}:0"), "-x", "400", "-y", "100"])
        .ok();

    // Split into the required number of panes.
    // Split is horizontal and always targets the first pane.
    // Each split halves the first pane again; after all splits, `tiled`
    // rearranges everything evenly.
    for _ in 1..total {
        tmux::cmd(["split-window", "-h", "-t", &format!("{session}:0")])
            .context("splitting window")?;
    }

    // Rearrange into an even tiled layout
    tmux::cmd(["select-layout", "-t", &format!("{session}:0"), "tiled"])?;

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

    // Spawn each module in its pane (first pane = conductor)
    for (pane_idx, (kind, instance)) in assignments.iter().enumerate() {
        if pane_idx >= panes.len() {
            break;
        }
        let (_, pane_id) = &panes[pane_idx];

        // Check if this module type has a custom command in the layout
        let custom_cmd = layout.modules.iter().find(|m| m.kind == *kind).and_then(|m| m.command.as_ref());
        let cmd = match custom_cmd {
            Some(custom) => custom.clone(),
            None => format!("{} {} {}", los_bin, kind, instance + 1),
        };

        tmux::cmd(["send-keys", "-t", pane_id, &cmd, "Enter"])
            .with_context(|| format!("spawning {kind} #{instance} in {pane_id}"))?;
    }

    // Bind global keys targeting the conductor pane (first pane)
    let conductor_target = match panes.first() {
        Some((_, id)) => id.clone(),
        None => "%0".into(),
    };

    let binds = [
        ("p", "play"),
        ("s", "stop"),
        ("r", "record"),
        ("q", "quit"),
    ];

    for (key, cmd_text) in &binds {
        tmux::cmd([
            "bind-key",
            "-T",
            "prefix",
            key,
            "send-keys",
            "-t",
            &conductor_target,
            cmd_text,
            "Enter",
        ])
        .with_context(|| format!("binding prefix-{key}"))?;
    }

    eprintln!("los: created session '{session}' with {total} panes");
    Ok(())
}

pub fn run_create(attach: bool) -> Result<()> {
    tmux::check_available()?;

    let layout = Layout::load()?;
    let session = &layout.session_name;

    if tmux::session_exists(session) {
        if attach && unsafe { libc::isatty(libc::STDOUT_FILENO) != 0 } {
            eprintln!("los: session '{session}' already exists, attaching...");
            let _ = tmux::cmd(["attach-session", "-t", session]);
            eprintln!("los: session '{session}' detached. Use `tmux attach -t {session}` to reconnect.");
        } else {
            eprintln!("los: session '{session}' already exists");
        }
        return Ok(());
    }

    let bin = los_bin();
    create_session(&layout, &bin)?;

    if attach && unsafe { libc::isatty(libc::STDOUT_FILENO) != 0 } {
        let _ = tmux::cmd(["attach-session", "-t", session]);
        eprintln!(
            "los: session '{session}' detached. Reattach with `tmux attach -t {session}`, \
             or clean up with `tmux kill-session -t {session}`."
        );
    } else {
        eprintln!("los: use `tmux attach -t {session}` to connect");
    }

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
