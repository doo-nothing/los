use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use std::sync::atomic::{AtomicBool, Ordering};

// ── SIGUSR1 save signal ──────────────────────────────────────────────────────

static SAVE_FLAG: AtomicBool = AtomicBool::new(false);

extern "C" fn sigusr1_handler(_: i32) {
    SAVE_FLAG.store(true, Ordering::SeqCst);
}

/// Install the SIGUSR1 handler. Call once at module startup.
pub fn setup_save_signal() {
    // Double-cast through *const () to match macOS sighandler_t (which is usize)
    unsafe {
        let handler = sigusr1_handler as *const () as libc::sighandler_t;
        let prev = libc::signal(libc::SIGUSR1, handler);
        if prev == libc::SIG_ERR {
            // Signal installation failed — fall back to no-op
        }
    }
}

/// Check if a save signal was received. Returns true once per signal.
pub fn check_save_signal() -> bool {
    SAVE_FLAG.swap(false, Ordering::SeqCst)
}

/// Send SIGUSR1 to a process.
pub fn send_save_signal(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGUSR1);
    }
}

// ── directory helpers ───────────────────────────────────────────────────────

pub fn los_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".config").join("los")
}

pub fn states_dir() -> PathBuf {
    los_dir().join("states")
}

pub fn patches_dir() -> PathBuf {
    los_dir().join("patches")
}

pub fn tmp_dir() -> PathBuf {
    los_dir().join("tmp")
}

pub fn ensure_dirs() -> Result<()> {
    for d in &[los_dir(), states_dir(), patches_dir(), tmp_dir()] {
        fs::create_dir_all(d)
            .with_context(|| format!("creating directory {}", d.display()))?;
    }
    Ok(())
}

pub fn module_state_path(module: &str, instance: usize) -> PathBuf {
    let name = format!("{}_{}", module, instance);
    let mut p = tmp_dir().join(&name);
    p.set_extension("state");
    p
}

// ── top-level session state ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub meta: Meta,
    #[serde(default)]
    pub tmux: TmuxState,
    #[serde(default)]
    pub windows: Vec<WindowState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    #[serde(default)]
    pub created: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmuxState {
    #[serde(default = "default_session_name")]
    pub session_name: String,
    #[serde(default)]
    pub active_window: String,
    #[serde(default = "default_window_size")]
    pub window_size: String,
}

fn default_session_name() -> String { "los".into() }
fn default_window_size() -> String { "largest".into() }

impl Default for TmuxState {
    fn default() -> Self {
        Self {
            session_name: default_session_name(),
            active_window: String::new(),
            window_size: default_window_size(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowState {
    pub name: String,
    #[serde(default)]
    pub layout: String,
    #[serde(default)]
    pub panes: Vec<PaneState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneState {
    pub module: String,
    #[serde(default)]
    pub instance: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_inline: Option<toml::Value>,
}

// ── per-module param structs ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VoiceParams {
    pub shape: Option<f32>,
    pub sub: Option<f32>,
    pub fm: Option<f32>,
    pub output: Option<u8>,
    pub freq: Option<f32>,
    pub gate: Option<bool>,
    pub level: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SequencerParams {
    pub bpm: Option<f64>,
    pub playing: Option<bool>,
    pub euclidean_pulses: Option<usize>,
    pub euclidean_length: Option<usize>,
    pub euclidean_rotation: Option<usize>,
    #[serde(default)]
    pub steps: Vec<StepParam>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StepParam {
    pub active: bool,
    pub note: u8,
    pub velocity: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MixerParams {
    pub master: Option<f32>,
    #[serde(default)]
    pub tracks: Vec<MixerTrackParam>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MixerTrackParam {
    pub level: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScopeParams {
    pub mode: Option<usize>,
    pub channel: Option<usize>,
    pub zoom: Option<f32>,
    pub gain: Option<f32>,
}

// ── write helpers ────────────────────────────────────────────────────────────

/// Atomically write a TOML string to a file (write .tmp, rename).
pub fn write_state_file(path: &Path, toml_str: &str) -> Result<()> {
    let tmp_path = path.with_extension("tmp");
    let mut f = fs::File::create(&tmp_path)
        .with_context(|| format!("creating {}", tmp_path.display()))?;
    f.write_all(toml_str.as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))?;
    Ok(())
}

/// Serialize a value to a TOML string.
pub fn to_toml_string<T: Serialize>(val: &T) -> Result<String> {
    toml::to_string_pretty(val).context("serializing to toml")
}

/// Deserialize from a TOML file path.
pub fn from_toml_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))
}

/// Save a module's state to ~/.config/los/tmp/<module>_<instance>.state
pub fn save_module_state<T: Serialize>(module: &str, instance: usize, params: &T) -> Result<()> {
    let path = module_state_path(module, instance);
    let toml_str = to_toml_string(params)?;
    write_state_file(&path, &toml_str)
}

/// Write this process's PID so the conductor can send us signals
pub fn write_pid_file(module: &str, instance: usize) {
    let name = format!("{}_{}", module, instance);
    let mut path = tmp_dir().join(&name);
    path.set_extension("pid");
    let _ = std::fs::write(&path, format!("{}", std::process::id()));
}

/// Read a module's PID file to find the real process
pub fn read_pid_file(module: &str, instance: usize) -> Option<u32> {
    let name = format!("{}_{}", module, instance);
    let mut path = tmp_dir().join(&name);
    path.set_extension("pid");
    std::fs::read_to_string(&path).ok()?.trim().parse().ok()
}

/// Load a module's state from ~/.config/los/tmp/<module>_<instance>.state
pub fn load_module_state<T: for<'de> Deserialize<'de>>(module: &str, instance: usize) -> Result<T> {
    let path = module_state_path(module, instance);
    if path.exists() {
        from_toml_file(&path)
    } else {
        anyhow::bail!("state file not found: {}", path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    
    #[test]
    fn test_signal_handler() {
        setup_save_signal();
        let pid = std::process::id() as u32;
        send_save_signal(pid);
        std::thread::sleep(Duration::from_millis(100));
        assert!(check_save_signal(), "SIGUSR1 signal should have been received");
    }
    
    #[test]
    fn test_cross_process_signal() {
        // Fork a child that installs the signal handler and waits
        let child = std::thread::spawn(|| {
            setup_save_signal();
            let pid = std::process::id() as u32;
            // Send signal to parent
            let parent_pid = std::env::var("TEST_PARENT_PID").ok()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(pid);
            send_save_signal(parent_pid);
            // Wait briefly
            std::thread::sleep(Duration::from_millis(50));
        });
        
        // Parent installs handler
        setup_save_signal();
        std::env::set_var("TEST_PARENT_PID", std::process::id().to_string());
        
        child.join().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        
        assert!(check_save_signal(), "Cross-process SIGUSR1 should work");
    }
}
