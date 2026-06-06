use std::process::Command;

use anyhow::{Context, Result};

pub fn cmd<T: AsRef<str>>(args: impl AsRef<[T]>) -> Result<String> {
    let args: Vec<&str> = args.as_ref().iter().map(|s| s.as_ref()).collect();
    let output = Command::new("tmux")
        .args(&args)
        .output()
        .context("running tmux command — is tmux installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("tmux {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn check_available() -> Result<()> {
    cmd(["start-server"]).context("tmux not found or not working")?;
    Ok(())
}

pub fn session_exists(name: &str) -> bool {
    cmd(["has-session", "-t", name]).is_ok()
}

pub fn pane_count(session: &str, window: &str) -> Result<usize> {
    let out = cmd([
        "list-panes",
        "-t",
        &format!("{}:{}", session, window),
        "-F",
        "#{pane_id}",
    ])?;
    Ok(out.lines().count())
}
