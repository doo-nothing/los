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

pub fn setup_save_signal() {
    unsafe {
        let handler = sigusr1_handler as *const () as libc::sighandler_t;
        let _ = libc::signal(libc::SIGUSR1, handler);
    }
}

pub fn check_save_signal() -> bool {
    SAVE_FLAG.swap(false, Ordering::SeqCst)
}

pub fn send_save_signal(pid: u32) {
    unsafe { libc::kill(pid as i32, libc::SIGUSR1); }
}

// ── SIGUSR2 reload signal ────────────────────────────────────────────────────

static RELOAD_FLAG: AtomicBool = AtomicBool::new(false);

extern "C" fn sigusr2_handler(_: i32) {
    RELOAD_FLAG.store(true, Ordering::SeqCst);
}

pub fn setup_reload_signal() {
    unsafe {
        let handler = sigusr2_handler as *const () as libc::sighandler_t;
        let _ = libc::signal(libc::SIGUSR2, handler);
    }
}

pub fn check_reload_signal() -> bool {
    RELOAD_FLAG.swap(false, Ordering::SeqCst)
}

pub fn send_reload_signal(pid: u32) {
    unsafe { libc::kill(pid as i32, libc::SIGUSR2); }
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

/// Current session state-file format. v2 (routing source addresses) is a
/// clean break: v1 files are refused at load.
pub const STATE_FORMAT: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub format: u32,
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
    pub active_pane: usize,
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
    /// Params format: files older than STATE_FORMAT keep the module's
    /// default bindings instead of clobbering them with absent fields.
    #[serde(default)]
    pub format: u32,
    pub shape: Option<f32>,
    pub sub: Option<f32>,
    pub fm: Option<f32>,
    pub output: Option<u8>,
    pub freq: Option<f32>,
    pub gate: Option<bool>,
    pub level: Option<f32>,
    pub velocity: Option<f32>,
    // Source-address bindings (state format v2): "module/instance/output"
    #[serde(default)]
    pub shape_src: Option<String>,
    #[serde(default)]
    pub sub_src: Option<String>,
    #[serde(default)]
    pub fm_src: Option<String>,
    #[serde(default)]
    pub level_src: Option<String>,
    #[serde(default)]
    pub amp_src: Option<String>,
    #[serde(default)]
    pub notes_src: Option<String>,
    #[serde(default)]
    pub lpg: Option<f32>,
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
    #[serde(default)]
    pub tracks: Vec<TrackParam>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StepParam {
    pub active: bool,
    pub note: u8,
    pub velocity: u8,
    #[serde(default)]
    pub mod_value: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackParam {
    #[serde(default)]
    pub steps: Vec<StepParam>,
    pub length: Option<usize>,
    pub pulses: Option<usize>,
    pub rotation: Option<usize>,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub mode: TrackMode,
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
    #[serde(default)]
    pub source: Option<usize>,
    #[serde(default)]
    pub modbus_channel: Option<usize>,
    #[serde(default)]
    pub trigger_level: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnvelopeParams {
    #[serde(default)]
    pub format: u32,
    #[serde(default)]
    pub channels: Vec<EnvelopeChannelParams>,
    #[serde(default)]
    pub logic_outputs: LogicOutputConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeChannelParams {
    pub rise: f32,
    pub fall: f32,
    pub shape: f32,
    pub loop_mode: bool,
    pub attenuverter: f32,
    #[serde(default)]
    pub offset: f32,
    #[serde(default)]
    pub pluck: f32,
    // Source-address bindings (state format v2)
    #[serde(default)]
    pub signal_src: Option<String>,
    #[serde(default)]
    pub trigger_src: Option<String>,
    #[serde(default)]
    pub rise_src: Option<String>,
    #[serde(default)]
    pub fall_src: Option<String>,
    #[serde(default)]
    pub shape_src: Option<String>,
    #[serde(default)]
    pub atten_src: Option<String>,
}

impl Default for EnvelopeChannelParams {
    fn default() -> Self {
        Self {
            rise: 0.5,
            fall: 0.5,
            shape: 0.5,
            loop_mode: false,
            attenuverter: 1.0,
            offset: 0.0,
            pluck: 0.0,
            signal_src: None,
            trigger_src: None,
            rise_src: None,
            fall_src: None,
            shape_src: None,
            atten_src: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LogicOutputConfig {
    pub sum_enabled: bool,
    pub or_enabled: bool,
    pub and_enabled: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum TrackMode {
    #[default]
    #[serde(rename = "note")]
    Note,
    #[serde(rename = "modulation")]
    Modulation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModDest {
    pub target_module: String,
    pub target_instance: usize,
    pub target_param: String,
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

/// Save a module patch to ~/.config/los/patches/<name>.toml (`:w name`).
pub fn save_patch<T: Serialize>(name: &str, params: &T) -> Result<()> {
    let path = patches_dir().join(format!("{}.toml", name));
    let toml_str = to_toml_string(params)?;
    write_state_file(&path, &toml_str)
}

/// Load a module patch from ~/.config/los/patches/<name>.toml (`:e name`).
pub fn load_patch<T: for<'de> Deserialize<'de>>(name: &str) -> Result<T> {
    let path = patches_dir().join(format!("{}.toml", name));
    if path.exists() {
        from_toml_file(&path)
    } else {
        anyhow::bail!("patch not found: {}", path.display())
    }
}

#[cfg(test)]
mod reload_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_reload_signal() {
        setup_reload_signal();
        let pid = std::process::id();
        send_reload_signal(pid);
        std::thread::sleep(Duration::from_millis(100));
        assert!(check_reload_signal(), "SIGUSR2 should have been received");
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

    #[test]
    fn session_state_roundtrip_with_layout_and_active_pane() {
        let original = SessionState {
            meta: Meta {
                name: "test-session".into(),
                created: "1234567890".into(),
                format: STATE_FORMAT,
            },
            tmux: TmuxState {
                session_name: "los".into(),
                active_window: "modules".into(),
                window_size: "largest".into(),
            },
            windows: vec![
                WindowState {
                    name: "modules".into(),
                    layout: "1f88,159x79,0,0[159x25,0,0{79x25,0,0,1176,79x25,80,0,1177},159x25,0,26{79x25,0,26,1175,79x25,80,26,1178},159x27,0,52,1174]".into(),
                    active_pane: 3,
                    panes: vec![
                        PaneState {
                            module: "sequencer".into(),
                            instance: 0,
                            patch: None,
                            patch_inline: None,
                        },
                        PaneState {
                            module: "voice".into(),
                            instance: 0,
                            patch: None,
                            patch_inline: None,
                        },
                        PaneState {
                            module: "envelope".into(),
                            instance: 0,
                            patch: None,
                            patch_inline: None,
                        },
                    ],
                },
            ],
        };

        let toml_str = to_toml_string(&original).expect("serialize");
        let loaded: SessionState = from_toml_file_str(&toml_str).expect("deserialize");

        assert_eq!(loaded.meta.name, "test-session");
        assert_eq!(loaded.windows.len(), 1);
        assert_eq!(loaded.windows[0].name, "modules");
        assert_eq!(loaded.windows[0].layout, original.windows[0].layout);
        assert_eq!(loaded.windows[0].active_pane, 3);
        assert_eq!(loaded.windows[0].panes.len(), 3);
        assert_eq!(loaded.windows[0].panes[0].module, "sequencer");
        assert_eq!(loaded.windows[0].panes[1].module, "voice");
        assert_eq!(loaded.windows[0].panes[2].module, "envelope");
    }

    #[test]
    fn session_state_roundtrip_defaults() {
        let original = SessionState {
            meta: Meta {
                name: "minimal".into(),
                created: "".into(),
                format: STATE_FORMAT,
            },
            tmux: TmuxState::default(),
            windows: vec![WindowState {
                name: "modules".into(),
                layout: "".into(),
                active_pane: 0,
                panes: vec![],
            }],
        };

        let toml_str = to_toml_string(&original).expect("serialize");
        let loaded: SessionState = from_toml_file_str(&toml_str).expect("deserialize");

        assert_eq!(loaded.windows[0].layout, "");
        assert_eq!(loaded.windows[0].active_pane, 0);
        assert!(loaded.windows[0].panes.is_empty());
        assert_eq!(loaded.tmux.session_name, "los");
    }

    #[test]
    fn window_state_multiple_panes() {
        let win = WindowState {
            name: "test".into(),
            layout: "tiled".into(),
            active_pane: 2,
            panes: vec![
                PaneState { module: "a".into(), instance: 0, patch: None, patch_inline: None },
                PaneState { module: "b".into(), instance: 1, patch: None, patch_inline: None },
                PaneState { module: "c".into(), instance: 0, patch: None, patch_inline: None },
            ],
        };

        let toml = to_toml_string(&win).expect("serialize");
        let loaded: WindowState = from_toml_file_str(&toml).expect("deserialize");

        assert_eq!(loaded.active_pane, 2);
        assert_eq!(loaded.panes.len(), 3);
        assert_eq!(loaded.panes[1].instance, 1);
    }

    #[test]
    fn pane_state_with_inline_patch() {
        use toml::Value;
        let pane = PaneState {
            module: "voice".into(),
            instance: 0,
            patch: None,
            patch_inline: Some(Value::String("test".into())),
        };

        let toml = to_toml_string(&pane).expect("serialize");
        let loaded: PaneState = from_toml_file_str(&toml).expect("deserialize");

        assert_eq!(loaded.module, "voice");
        assert!(loaded.patch_inline.is_some());
    }

    fn from_toml_file_str<T: for<'de> Deserialize<'de>>(s: &str) -> Result<T> {
        let val: toml::Value = toml::from_str(s).context("parse toml")?;
        let t = T::deserialize(val).context("deserialize")?;
        Ok(t)
    }
}

#[cfg(test)]
mod patch_tests {
    use super::*;

    #[test]
    fn patch_roundtrip() {
        let _ = ensure_dirs();
        let name = "test-patch-roundtrip";
        let params = ScopeParams {
            mode: Some(2),
            channel: Some(1),
            zoom: Some(3.5),
            gain: Some(0.7),
            ..Default::default()
        };
        save_patch(name, &params).expect("save patch");
        let loaded: ScopeParams = load_patch(name).expect("load patch");
        assert_eq!(loaded.mode, Some(2));
        assert_eq!(loaded.channel, Some(1));
        assert_eq!(loaded.zoom, Some(3.5));
        assert_eq!(loaded.gain, Some(0.7));
        let _ = std::fs::remove_file(patches_dir().join(format!("{}.toml", name)));
    }

    #[test]
    fn load_missing_patch_errors() {
        let _ = ensure_dirs();
        assert!(load_patch::<ScopeParams>("definitely-not-a-real-patch").is_err());
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn voice_params_src_roundtrip() {
        let p = VoiceParams {
            shape: Some(0.5),
            amp_src: Some("envelope/0/ch1".into()),
            notes_src: Some("sequencer/0/t3".into()),
            ..Default::default()
        };
        let s = to_toml_string(&p).unwrap();
        let back: VoiceParams = toml::from_str(&s).unwrap();
        assert_eq!(back.amp_src.as_deref(), Some("envelope/0/ch1"));
        assert_eq!(back.notes_src.as_deref(), Some("sequencer/0/t3"));
    }

    #[test]
    fn v1_state_files_parse_but_carry_format_zero() {
        // Old (v1) files lack meta.format — serde default gives 0, which the
        // loader refuses. New saves stamp STATE_FORMAT.
        let toml_str = r#"
[meta]
name = "old"
created = "2026-01-01"
"#;
        let st: SessionState = toml::from_str(toml_str).unwrap();
        assert_eq!(st.meta.format, 0);
        assert!(st.meta.format < STATE_FORMAT);
    }

    #[test]
    fn old_track_binding_fields_are_ignored() {
        // v1 patches carried shape_track = 3 etc.; v2 ignores unknown fields
        // and leaves the binding unset rather than failing the load.
        let toml_str = r#"
shape = 0.7
shape_track = 3
"#;
        let p: VoiceParams = toml::from_str(toml_str).unwrap();
        assert_eq!(p.shape, Some(0.7));
        assert!(p.shape_src.is_none());
    }
}
