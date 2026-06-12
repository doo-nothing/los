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
    unsafe {
        libc::kill(pid as i32, libc::SIGUSR1);
    }
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
    unsafe {
        libc::kill(pid as i32, libc::SIGUSR2);
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
        fs::create_dir_all(d).with_context(|| format!("creating directory {}", d.display()))?;
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

fn default_session_name() -> String {
    "los".into()
}
fn default_window_size() -> String {
    "largest".into()
}

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
    #[serde(default)]
    pub macros: Vec<MacroParam>,
    /// Macro lane slots: one bar each, "" = empty, "a"–"z" = fire that macro.
    #[serde(default)]
    pub lane: Vec<String>,
    #[serde(default)]
    pub lane_len: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepParam {
    pub active: bool,
    pub note: u8,
    pub velocity: u8,
    #[serde(default)]
    pub mod_value: f32,
    #[serde(default = "default_prob")]
    pub prob: u8,
    #[serde(default)]
    pub bind: Option<StepBindParam>,
    /// Micro-timing push, interpreted via `delay_unit` (timing pass).
    #[serde(default)]
    pub delay: f32,
    #[serde(default)]
    pub delay_unit: DelayUnit,
    #[serde(default = "default_prob")]
    pub delay_prob: u8,
    #[serde(default = "default_repeats")]
    pub repeats: u8,
    #[serde(default = "default_prob")]
    pub repeat_prob: u8,
}

/// Steps in saves that predate probability always fire.
fn default_prob() -> u8 {
    100
}

/// Steps in saves that predate ratchets play once.
fn default_repeats() -> u8 {
    1
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
    #[serde(default)]
    pub cycle: CycleMode,
    /// Library scale name; `scale_cents` is authoritative when non-empty
    /// (covers `.scl` imports whose name isn't in the library).
    #[serde(default)]
    pub scale: Option<String>,
    #[serde(default)]
    pub scale_cents: Vec<f64>,
    #[serde(default)]
    pub scale_period: Option<f64>,
    /// MIDI root note for the scale (default 60).
    #[serde(default)]
    pub root: Option<u8>,
    /// Active pattern slot 0–7 (`a`–`h`).
    #[serde(default)]
    pub active_slot: usize,
    /// Inactive non-empty pattern slots.
    #[serde(default)]
    pub slots: Vec<SlotParam>,
    /// MPC swing 50–75 (% of an 8th-note pair; 50 = straight).
    #[serde(default = "default_swing")]
    pub swing: u8,
    /// Groove template name from `theory::groove`; `None` = straight.
    #[serde(default)]
    pub groove: Option<String>,
    /// Timing jitter ± ms, re-rolled per cycle (0 = off).
    #[serde(default)]
    pub humanize: f32,
    /// Ratchet velocity shape −100..=100: + = decay, − = crescendo.
    #[serde(default)]
    pub ratchet_decay: i8,
}

/// Tracks in saves that predate swing play straight.
fn default_swing() -> u8 {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MixerParams {
    pub master: Option<f32>,
    #[serde(default)]
    pub tracks: Vec<MixerTrackParam>,
    // master strip console params (v2): EQ + drive + width
    #[serde(default)]
    pub master_drive: f32,
    #[serde(default)]
    pub master_eq_lo: f32,
    #[serde(default)]
    pub master_eq_mid: f32,
    #[serde(default = "default_mid_freq")]
    pub master_eq_freq: f32,
    #[serde(default)]
    pub master_eq_hi: f32,
    #[serde(default = "default_width")]
    pub master_width: f32,
    // fx send taps on the master bus (default 0: the master includes
    // the fx returns, so master sends invite feedback — on purpose only)
    #[serde(default)]
    pub master_send_a: f32,
    #[serde(default)]
    pub master_send_b: f32,
}

fn default_mid_freq() -> f32 {
    0.5
}
fn default_width() -> f32 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MixerTrackParam {
    pub level: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    // console strip (v2): drive -> 3-band EQ; gains in dB, freq 0..1
    #[serde(default)]
    pub drive: f32,
    #[serde(default)]
    pub eq_lo: f32,
    #[serde(default)]
    pub eq_mid: f32,
    #[serde(default = "default_mid_freq")]
    pub eq_freq: f32,
    #[serde(default)]
    pub eq_hi: f32,
    // per-strip fx send levels (post-fader taps into send/0 + send/1)
    #[serde(default)]
    pub send_a: f32,
    #[serde(default)]
    pub send_b: f32,
    // mod-input bindings, one per bindable param ("module/inst/output")
    #[serde(default)]
    pub level_src: Option<String>,
    #[serde(default)]
    pub pan_src: Option<String>,
    #[serde(default)]
    pub drive_src: Option<String>,
    #[serde(default)]
    pub lo_src: Option<String>,
    #[serde(default)]
    pub mid_src: Option<String>,
    #[serde(default)]
    pub freq_src: Option<String>,
    #[serde(default)]
    pub hi_src: Option<String>,
    #[serde(default)]
    pub send_a_src: Option<String>,
    #[serde(default)]
    pub send_b_src: Option<String>,
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

/// The template module's params (modules/template.rs — the worked example
/// for new module authors). The conventions on display: every field is
/// `Option` (or `serde(default)`) so saves from older builds load instead
/// of erroring; bindings serialize as source-address strings
/// ("envelope/0/ch1"); `format` stamps the save so future migrations can
/// tell eras apart.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TemplateParams {
    #[serde(default)]
    pub format: u32,
    pub rate: Option<f32>,
    /// Shape by name ("sine"), not index — saves stay readable and survive
    /// reordering the shape list.
    pub shape: Option<String>,
    pub depth: Option<f32>,
    pub pitch: Option<f32>,
    pub level: Option<f32>,
    pub unipolar: Option<bool>,
    #[serde(default)]
    pub rate_src: Option<String>,
    #[serde(default)]
    pub depth_src: Option<String>,
    #[serde(default)]
    pub pitch_src: Option<String>,
    #[serde(default)]
    pub level_src: Option<String>,
}

/// One DLD channel (modules/dld) — the 4ms-style looping delay.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DldChannelParams {
    pub time: Option<f32>,
    /// Time switch by name: "/8", "=", "+16".
    pub switch: Option<String>,
    pub fdbk: Option<f32>,
    pub feed: Option<f32>,
    pub mix: Option<f32>,
    pub hold: Option<bool>,
    pub rev: Option<bool>,
    pub win: Option<f32>,
    #[serde(default)]
    pub time_src: Option<String>,
    #[serde(default)]
    pub fdbk_src: Option<String>,
    #[serde(default)]
    pub feed_src: Option<String>,
    #[serde(default)]
    pub win_src: Option<String>,
    #[serde(default)]
    pub hold_src: Option<String>,
    #[serde(default)]
    pub rev_src: Option<String>,
}

/// The DLD (modules/dld) — two channels around one Ping.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DldParams {
    #[serde(default)]
    pub format: u32,
    /// 0 = Ping follows the transport beat; >0 = free Ping in ms.
    pub ping_ms: Option<f32>,
    pub mono: Option<bool>,
    #[serde(default)]
    pub input: Option<String>,
    pub a: Option<DldChannelParams>,
    pub b: Option<DldChannelParams>,
}

/// One sampler slot (modules/sampler) — the loaded reel + designer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SamplerSlotParams {
    /// Cache path of the loaded sample.
    pub sample: Option<String>,
    /// Mode by name: "oneshot", "loop", "gated", "hold".
    pub mode: Option<String>,
    pub start: Option<f32>,
    pub len: Option<f32>,
    pub pitch: Option<f32>,
    pub speed: Option<f32>,
    pub gene: Option<f32>,
    pub slide: Option<f32>,
    pub atk: Option<f32>,
    pub dec: Option<f32>,
    pub level: Option<f32>,
}

/// The sampler (modules/sampler): eight reels, a kit, a CV bank.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SamplerParams {
    #[serde(default)]
    pub format: u32,
    pub kit: Option<bool>,
    pub edit: Option<usize>,
    #[serde(default)]
    pub notes_src: Option<String>,
    #[serde(default)]
    pub amp_src: Option<String>,
    #[serde(default)]
    pub pitch_src: Option<String>,
    #[serde(default)]
    pub speed_src: Option<String>,
    #[serde(default)]
    pub gene_src: Option<String>,
    #[serde(default)]
    pub slide_src: Option<String>,
    #[serde(default)]
    pub level_src: Option<String>,
    #[serde(default)]
    pub slots: Vec<SamplerSlotParams>,
}

/// The Wasp filter (modules/wasp.rs) — A-124-style dirty multimode SVF.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WaspParams {
    #[serde(default)]
    pub format: u32,
    pub freq: Option<f32>,
    pub res: Option<f32>,
    /// LP→notch→HP blend.
    pub mix: Option<f32>,
    /// The CMOS rasp drive.
    pub dirt: Option<f32>,
    /// Bandpass-output blend.
    pub bp: Option<f32>,
    pub dry: Option<f32>,
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub freq_src: Option<String>,
    #[serde(default)]
    pub res_src: Option<String>,
    #[serde(default)]
    pub mix_src: Option<String>,
    #[serde(default)]
    pub dirt_src: Option<String>,
}

/// The DPO (modules/dpo) — Make Noise-style complex oscillator.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DpoParamsState {
    #[serde(default)]
    pub format: u32,
    pub ratio: Option<f32>,
    pub follow: Option<f32>,
    pub index: Option<f32>,
    pub fm_a: Option<f32>,
    pub fm_b: Option<f32>,
    /// VCO A mode by name: "free", "lock", "sync", "lfo".
    pub mode: Option<String>,
    pub shape: Option<f32>,
    pub angle: Option<f32>,
    pub fold: Option<f32>,
    pub mod_index: Option<f32>,
    pub mix: Option<f32>,
    pub level: Option<f32>,
    pub freq: Option<f32>,
    pub gate: Option<bool>,
    #[serde(default)]
    pub ratio_src: Option<String>,
    #[serde(default)]
    pub index_src: Option<String>,
    #[serde(default)]
    pub shape_src: Option<String>,
    #[serde(default)]
    pub angle_src: Option<String>,
    #[serde(default)]
    pub fold_src: Option<String>,
    #[serde(default)]
    pub mod_src: Option<String>,
    #[serde(default)]
    pub follow_src: Option<String>,
    #[serde(default)]
    pub strike_src: Option<String>,
    #[serde(default)]
    pub amp_src: Option<String>,
    #[serde(default)]
    pub notes_src: Option<String>,
}

/// One LFO channel (modules/lfo.rs).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LfoChannelParams {
    pub freq: Option<f32>,
    /// Shape by name: "sine", "tri", "saw", "sqr", "s&h".
    pub shape: Option<String>,
    pub phase: Option<f32>,
    #[serde(default)]
    pub freq_src: Option<String>,
}

/// The quad LFO (modules/lfo.rs) — Batumi-style bank.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LfoParams {
    #[serde(default)]
    pub format: u32,
    /// Mode by name: "free", "quad", "phase", "div".
    pub mode: Option<String>,
    #[serde(default)]
    pub rst_src: Option<String>,
    #[serde(default)]
    pub channels: Vec<LfoChannelParams>,
}

/// Elements (modules/elements) — the Mutable Instruments modal voice.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ElementsParams {
    #[serde(default)]
    pub format: u32,
    pub contour: Option<f32>,
    pub bow: Option<f32>,
    pub bow_timbre: Option<f32>,
    pub blow: Option<f32>,
    pub blow_meta: Option<f32>,
    pub blow_timbre: Option<f32>,
    pub strike: Option<f32>,
    pub strike_meta: Option<f32>,
    pub strike_timbre: Option<f32>,
    pub geometry: Option<f32>,
    pub brightness: Option<f32>,
    pub damping: Option<f32>,
    pub position: Option<f32>,
    pub space: Option<f32>,
    pub level: Option<f32>,
    pub freq: Option<f32>,
    pub gate: Option<bool>,
    #[serde(default)]
    pub geometry_src: Option<String>,
    #[serde(default)]
    pub brightness_src: Option<String>,
    #[serde(default)]
    pub damping_src: Option<String>,
    #[serde(default)]
    pub position_src: Option<String>,
    #[serde(default)]
    pub space_src: Option<String>,
    #[serde(default)]
    pub contour_src: Option<String>,
    #[serde(default)]
    pub bow_src: Option<String>,
    #[serde(default)]
    pub bow_timbre_src: Option<String>,
    #[serde(default)]
    pub blow_src: Option<String>,
    #[serde(default)]
    pub blow_meta_src: Option<String>,
    #[serde(default)]
    pub blow_timbre_src: Option<String>,
    #[serde(default)]
    pub strike_src: Option<String>,
    #[serde(default)]
    pub strike_meta_src: Option<String>,
    #[serde(default)]
    pub strike_timbre_src: Option<String>,
    #[serde(default)]
    pub level_src: Option<String>,
    #[serde(default)]
    pub amp_src: Option<String>,
    #[serde(default)]
    pub notes_src: Option<String>,
}

/// The swarm voice (modules/swarm.rs) — the CS-80-flavored brass pad:
/// chord by name ("min7"), the five knobs, glide, and the three kinds
/// of binding (knob sources, amp, notes).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SwarmParams {
    #[serde(default)]
    pub format: u32,
    /// Chord spread by name ("oct"), not index — saves stay readable
    /// and survive reordering the chord table.
    pub chord: Option<String>,
    pub detune: Option<f32>,
    pub cutoff: Option<f32>,
    pub res: Option<f32>,
    pub swell: Option<f32>,
    pub glide: Option<f32>,
    pub level: Option<f32>,
    pub freq: Option<f32>,
    pub gate: Option<bool>,
    #[serde(default)]
    pub detune_src: Option<String>,
    #[serde(default)]
    pub cutoff_src: Option<String>,
    #[serde(default)]
    pub res_src: Option<String>,
    #[serde(default)]
    pub swell_src: Option<String>,
    #[serde(default)]
    pub level_src: Option<String>,
    #[serde(default)]
    pub amp_src: Option<String>,
    #[serde(default)]
    pub notes_src: Option<String>,
}

/// The tape deck (modules/tape.rs — docs/plans/tape-deck.md). Audio
/// lives as WAVs under ~/.config/los/tape/, not in TOML.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TapeParams {
    #[serde(default)]
    pub format: u32,
    pub speed: Option<f32>,
    pub loop_on: Option<bool>,
    pub loop_in: Option<u64>,
    pub loop_out: Option<u64>,
    #[serde(default)]
    pub speed_src: Option<String>,
    #[serde(default)]
    pub tracks: Vec<TapeTrackParam>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TapeTrackParam {
    /// None = the mix (print bus); Some("voice/1") = a claimed source.
    #[serde(default)]
    pub input: Option<String>,
    pub fader: f32,
    pub pan: f32,
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub reversed: bool,
    #[serde(default = "default_true")]
    pub monitor: bool,
    #[serde(default)]
    pub fader_src: Option<String>,
    #[serde(default)]
    pub pan_src: Option<String>,
    /// The recorded fader lane: (frame, value) points.
    #[serde(default)]
    pub auto: Vec<(u64, f32)>,
}

fn default_true() -> bool {
    true
}

/// The filterbank module (modules/filterbank.rs —
/// docs/plans/filterbank-296e.md).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FilterbankParams {
    #[serde(default)]
    pub format: u32,
    /// The two stored spectra, 16 faders each.
    #[serde(default)]
    pub bank_a: Vec<f32>,
    #[serde(default)]
    pub bank_b: Vec<f32>,
    pub morph: Option<f32>,
    /// Transfer mode by name: "off" | "o→e" | "e→o" | "both".
    pub xfer: Option<String>,
    pub freeze: Option<bool>,
    pub wcent: Option<f32>,
    pub wwidth: Option<f32>,
    pub spread: Option<f32>,
    pub split: Option<f32>,
    pub dry: Option<f32>,
    pub decay: Option<f32>,
    /// Consumed audio source as "module/instance" (e.g. "send/1").
    #[serde(default)]
    pub input: Option<String>,
    /// Per-band CV-in bindings, 16 entries; "" = unbound (TOML arrays
    /// cannot hold nulls).
    #[serde(default)]
    pub band_srcs: Vec<String>,
    #[serde(default)]
    pub morph_src: Option<String>,
    #[serde(default)]
    pub freeze_src: Option<String>,
    #[serde(default)]
    pub wcent_src: Option<String>,
    #[serde(default)]
    pub wwidth_src: Option<String>,
    #[serde(default)]
    pub spread_src: Option<String>,
    #[serde(default)]
    pub split_src: Option<String>,
    #[serde(default)]
    pub dry_src: Option<String>,
    #[serde(default)]
    pub decay_src: Option<String>,
}

/// The delay module (modules/delay.rs — docs/plans/delay-288.md).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DelayParams {
    #[serde(default)]
    pub format: u32,
    /// Per-stage time, seconds.
    pub time: Option<f32>,
    pub regen: Option<f32>,
    pub shim: Option<f32>,
    pub wash: Option<f32>,
    pub dry: Option<f32>,
    /// Active tap count 1–8.
    pub taps: Option<usize>,
    /// Consumed audio source as "module/instance" (e.g. "voice/0") —
    /// resolved to a live ring through the manifest at runtime.
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub time_src: Option<String>,
    #[serde(default)]
    pub regen_src: Option<String>,
    #[serde(default)]
    pub shim_src: Option<String>,
    #[serde(default)]
    pub wash_src: Option<String>,
    #[serde(default)]
    pub dry_src: Option<String>,
    #[serde(default)]
    pub tap: Vec<DelayTapParam>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelayTapParam {
    pub level: f32,
    pub pan: f32,
    /// Phase select by glyph: "+" normal, "·" off, "−" inverted.
    #[serde(default = "default_phase")]
    pub phase: String,
    #[serde(default)]
    pub pan_src: Option<String>,
    #[serde(default)]
    pub level_src: Option<String>,
}

fn default_phase() -> String {
    "+".into()
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
    /// false = trig (full AD per note), true = gate (sustain until note off)
    #[serde(default)]
    pub gate_mode: bool,
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
            gate_mode: false,
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

/// Per-track playhead direction (docs/plans/sequencer-v2.md §3). Lives here
/// so the runtime and the save file share one type, like [`TrackMode`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CycleMode {
    #[default]
    Forward,
    Reverse,
    PingPong,
    Random,
    Drunk,
    EveryOther,
    Spiral,
    PrimeJump,
}

impl CycleMode {
    /// Every mode, in `gc` cycling order.
    pub const ALL: [CycleMode; 8] = [
        CycleMode::Forward,
        CycleMode::Reverse,
        CycleMode::PingPong,
        CycleMode::Random,
        CycleMode::Drunk,
        CycleMode::EveryOther,
        CycleMode::Spiral,
        CycleMode::PrimeJump,
    ];

    /// The `:set cycle <name>` spelling (also what saves serialize).
    pub fn name(&self) -> &'static str {
        match self {
            CycleMode::Forward => "forward",
            CycleMode::Reverse => "reverse",
            CycleMode::PingPong => "pingpong",
            CycleMode::Random => "random",
            CycleMode::Drunk => "drunk",
            CycleMode::EveryOther => "everyother",
            CycleMode::Spiral => "spiral",
            CycleMode::PrimeJump => "primejump",
        }
    }

    pub fn parse(s: &str) -> Option<CycleMode> {
        CycleMode::ALL
            .iter()
            .copied()
            .find(|m| m.name() == s.to_lowercase())
    }
}

/// Which step parameter a value layer or a per-step mod binding targets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindTarget {
    #[default]
    Note,
    Velocity,
    Prob,
    Mod,
    /// Micro-timing push (docs/plans/sequencer-timing.md).
    Delay,
    /// Chance the delay applies as a random amount up to the set value.
    DelayProb,
    /// Ratchet count 1–8.
    Repeats,
    /// Coin flip per repeat beyond the first.
    RepeatProb,
}

/// How a step's delay value is interpreted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DelayUnit {
    /// Literal milliseconds — tempo changes the feel (quirk is a feature).
    #[default]
    Ms,
    /// Percent of the step window — the groove survives tempo changes.
    Pct,
}

/// A per-step mod-in binding: `source`'s modbus value offsets `target`
/// by up to `amount` of the parameter's range at trigger time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepBindParam {
    pub target: BindTarget,
    pub source: String,
    pub amount: f32,
}

/// When a fired macro takes effect (docs/plans/sequencer-v2.md §7).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quant {
    Now,
    Beat,
    #[default]
    Bar,
    PatternEnd,
}

impl Quant {
    pub fn name(&self) -> &'static str {
        match self {
            Quant::Now => "now",
            Quant::Beat => "beat",
            Quant::Bar => "bar",
            Quant::PatternEnd => "end",
        }
    }

    pub fn parse(s: &str) -> Option<Quant> {
        match s.to_lowercase().as_str() {
            "now" | "immediate" => Some(Quant::Now),
            "beat" => Some(Quant::Beat),
            "bar" => Some(Quant::Bar),
            "end" | "patternend" | "pattern-end" => Some(Quant::PatternEnd),
            _ => None,
        }
    }
}

/// Auto-fill generator families (`:fill`, macro Fill commands).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FillKind {
    Mutate,
    Density,
    Markov,
    Cantor,
    ThueMorse,
    Fibonacci,
    Sierpinski,
}

impl FillKind {
    pub fn name(&self) -> &'static str {
        match self {
            FillKind::Mutate => "mutate",
            FillKind::Density => "density",
            FillKind::Markov => "markov",
            FillKind::Cantor => "cantor",
            FillKind::ThueMorse => "thuemorse",
            FillKind::Fibonacci => "fibonacci",
            FillKind::Sierpinski => "sierpinski",
        }
    }

    pub fn parse(s: &str) -> Option<FillKind> {
        match s.to_lowercase().as_str() {
            "mutate" | "evolve" => Some(FillKind::Mutate),
            "density" => Some(FillKind::Density),
            "markov" => Some(FillKind::Markov),
            "cantor" => Some(FillKind::Cantor),
            "thuemorse" | "thue-morse" | "tm" => Some(FillKind::ThueMorse),
            "fibonacci" | "fib" => Some(FillKind::Fibonacci),
            "sierpinski" | "sierp" => Some(FillKind::Sierpinski),
            _ => None,
        }
    }
}

/// One semantic command inside a macro — the unit `q{a-z}` records and
/// `@{a-z}` replays. Absolute (not toggling) so replays are predictable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacroCmd {
    SwitchPattern {
        track: usize,
        slot: usize,
    },
    SetMute {
        track: usize,
        muted: bool,
    },
    SetCycle {
        track: usize,
        mode: CycleMode,
    },
    TransposeTrack {
        track: usize,
        by: i32,
    },
    RotateTrack {
        track: usize,
        by: i32,
    },
    /// Empty string = scale off.
    SetScale {
        track: usize,
        scale: String,
    },
    Fill {
        track: usize,
        kind: FillKind,
        arg: f32,
    },
    SetBpm {
        bpm: f64,
    },
    /// Absolute step rewrite — how recorded edits replay exactly.
    SetSteps {
        track: usize,
        start: usize,
        steps: Vec<StepParam>,
    },
    /// A single trigger set absolutely (recorded step toggles).
    SetActive {
        track: usize,
        step: usize,
        active: bool,
    },
    /// Euclidean params, re-applied on replay.
    SetEuclid {
        track: usize,
        pulses: usize,
        length: usize,
        rotation: usize,
    },
    SetMode {
        track: usize,
        mode: TrackMode,
    },
    /// Track timing knobs, absolute (empty groove = straight).
    SetTiming {
        track: usize,
        swing: u8,
        groove: String,
        humanize: f32,
        decay: i8,
    },
}

/// A saved macro: single-letter id, quantize, command list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MacroParam {
    pub id: String,
    #[serde(default)]
    pub quant: Quant,
    #[serde(default)]
    pub cmds: Vec<MacroCmd>,
}

/// An inactive pattern slot's saved contents (active slot data lives
/// inline in [`TrackParam`]). Only non-empty slots are saved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotParam {
    /// Slot index 0–7 (`a`–`h`).
    pub slot: usize,
    #[serde(default)]
    pub steps: Vec<StepParam>,
    pub length: Option<usize>,
    pub pulses: Option<usize>,
    pub rotation: Option<usize>,
}

// ── write helpers ────────────────────────────────────────────────────────────

/// Atomically write a TOML string to a file (write .tmp, rename).
pub fn write_state_file(path: &Path, toml_str: &str) -> Result<()> {
    let tmp_path = path.with_extension("tmp");
    let mut f =
        fs::File::create(&tmp_path).with_context(|| format!("creating {}", tmp_path.display()))?;
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
    let content =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
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
                PaneState {
                    module: "a".into(),
                    instance: 0,
                    patch: None,
                    patch_inline: None,
                },
                PaneState {
                    module: "b".into(),
                    instance: 1,
                    patch: None,
                    patch_inline: None,
                },
                PaneState {
                    module: "c".into(),
                    instance: 0,
                    patch: None,
                    patch_inline: None,
                },
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
