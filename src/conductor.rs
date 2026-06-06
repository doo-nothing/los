use std::process::Command;
use anyhow::Result;

pub fn create_session() -> Result<()> {
    // Kill existing session if it exists
    let _ = Command::new("tmux").args(&["kill-session", "-t", "los"]).output();

    // Create new session with conductor in first window
    Command::new("tmux")
        .args(&["new-session", "-d", "-s", "los", "-n", "conductor"])
        .output()?;

    // Get the binary path
    let exe = std::env::current_exe()?;
    let exe_str = exe.to_str().unwrap();

    // Start conductor in first pane using its pane_id
    let output = Command::new("tmux")
        .args(&["list-panes", "-t", "los:conductor", "-F", "#{pane_id}"])
        .output()?;
    let conductor_pane = String::from_utf8(output.stdout)?.trim().to_string();
    Command::new("tmux")
        .args(&["respawn-pane", "-k", "-t", &conductor_pane, &format!("{} conductor", exe_str)])
        .output()?;

    // Create modules window and split into 4 panes
    Command::new("tmux")
        .args(&["new-window", "-t", "los", "-n", "modules"])
        .output()?;
    
    // Split into 4 panes (3 splits), then tiled layout arranges as 2x2
    for _ in 0..3 {
        Command::new("tmux")
            .args(&["split-window", "-t", "los:modules"])
            .output()?;
    }
    Command::new("tmux")
        .args(&["select-layout", "-t", "los:modules", "tiled"])
        .output()?;

    // Enable pane borders with labels
    Command::new("tmux")
        .args(&["set-option", "-t", "los:modules", "pane-border-status", "top"])
        .output()?;
    Command::new("tmux")
        .args(&["set-option", "-t", "los:modules", "pane-border-format", " #{pane_title} "])
        .output()?;

    // Discover panes sorted by pane_index
    let output = Command::new("tmux")
        .args(&["list-panes", "-t", "los:modules", "-F", "#{pane_index} #{pane_id}"])
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

    // Assign modules to discovered panes using pane_id (%N format, always works)
    let modules = [("sequencer", "Sequencer"), ("voice", "Voice"), ("mixer", "Mixer"), ("scope", "Scope")];
    
    for (i, (_, pane_id)) in panes.iter().enumerate() {
        if i >= modules.len() {
            break;
        }
        let (module_cmd, module_label) = modules[i];
        
        Command::new("tmux")
            .args(&["select-pane", "-t", pane_id, "-T", module_label])
            .output()?;
        
        let cmd = format!("{} {}", exe_str, module_cmd);
        Command::new("tmux")
            .args(&["respawn-pane", "-k", "-t", pane_id, &cmd])
            .output()?;
    }

    // Select modules window and attach
    Command::new("tmux")
        .args(&["select-window", "-t", "los:modules"])
        .output()?;
    
    Command::new("tmux")
        .args(&["attach-session", "-t", "los"])
        .status()?;

    Ok(())
}

pub fn run_conductor() -> Result<()> {
    println!("LOS Conductor");
    println!("=============");
    println!();
    println!("Session: los");
    println!("Windows:");
    println!("  1. conductor - Session control");
    println!("  2. modules   - Sequencer, Voice, Mixer, Scope");
    println!();
    println!("Press Ctrl+C to stop the session");

    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
