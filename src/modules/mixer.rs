use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{enable_raw_mode, EnterAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    style::Style,
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

pub mod dsp;

const SHM_NAME: &str = "/los_mix_in";

/// The bindable params of a strip, in undo-slot kind order.
/// UI rows run console order: drive, hi, mid, freq, lo, pan, level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Param {
    Level,
    Pan,
    Drive,
    Lo,
    Mid,
    Freq,
    Hi,
    /// master strip only (sits where pan does on a channel)
    Width,
}

/// Strip rows top → bottom (channel strips; master swaps Pan for Width).
const STRIP_ROWS: [Param; 7] =
    [Param::Drive, Param::Hi, Param::Mid, Param::Freq, Param::Lo, Param::Pan, Param::Level];

impl Param {
    fn kind(self) -> usize {
        match self {
            Param::Level => 0,
            Param::Pan => 1,
            Param::Drive => 4,
            Param::Lo => 5,
            Param::Mid => 6,
            Param::Freq => 7,
            Param::Hi => 8,
            Param::Width => 1, // master reuses the pan kind slot
        }
    }

    fn label(self) -> &'static str {
        match self {
            Param::Level => "lvl",
            Param::Pan => "pan",
            Param::Drive => "drv",
            Param::Lo => "lo",
            Param::Mid => "mid",
            Param::Freq => "frq",
            Param::Hi => "hi",
            Param::Width => "wid",
        }
    }

    /// Map a modbus value onto this param's range (bound replaces manual).
    fn map_mod(self, v: f32) -> f32 {
        match self {
            Param::Level | Param::Drive | Param::Freq => v.clamp(0.0, 1.0),
            Param::Pan => v.clamp(-1.0, 1.0),
            Param::Lo | Param::Mid | Param::Hi => v.clamp(-1.0, 1.0) * 15.0,
            Param::Width => v.clamp(0.0, 1.0) * 2.0,
        }
    }

    fn default_value(self) -> f32 {
        match self {
            Param::Level => 0.8,
            Param::Width => 1.0,
            Param::Freq => 0.5,
            _ => 0.0,
        }
    }
}

/// srcs/resolved/eff index for a param.
fn src_index(p: Param) -> usize {
    match p {
        Param::Level => 0,
        Param::Pan | Param::Width => 1,
        Param::Drive => 2,
        Param::Lo => 3,
        Param::Mid => 4,
        Param::Freq => 5,
        Param::Hi => 6,
    }
}

/// Manual params + bindings of one strip (channel or master; on the
/// master, `pan` is the WIDTH control).
#[derive(Clone)]
struct Strip {
    level: f32,
    pan: f32,
    drive: f32,
    eq_lo: f32,
    eq_mid: f32,
    eq_freq: f32,
    eq_hi: f32,
    srcs: [Option<SourceAddr>; 7],
    resolved: [Option<usize>; 7],
    /// Live effective values, written by the audio thread (ghost display).
    eff: [f32; 7],
}

impl Strip {
    fn new(level: f32) -> Self {
        Self {
            level,
            pan: 0.0,
            drive: 0.0,
            eq_lo: 0.0,
            eq_mid: 0.0,
            eq_freq: 0.5,
            eq_hi: 0.0,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [level, 0.0, 0.0, 0.0, 0.0, 0.5, 0.0],
        }
    }

    fn get(&self, p: Param) -> f32 {
        match p {
            Param::Level => self.level,
            Param::Pan | Param::Width => self.pan,
            Param::Drive => self.drive,
            Param::Lo => self.eq_lo,
            Param::Mid => self.eq_mid,
            Param::Freq => self.eq_freq,
            Param::Hi => self.eq_hi,
        }
    }

    fn set(&mut self, p: Param, v: f32) {
        match p {
            Param::Level => self.level = v.clamp(0.0, 1.0),
            Param::Pan => self.pan = v.clamp(-1.0, 1.0),
            Param::Width => self.pan = v.clamp(0.0, 2.0),
            Param::Drive => self.drive = v.clamp(0.0, 1.0),
            Param::Lo => self.eq_lo = v.clamp(-15.0, 15.0),
            Param::Mid => self.eq_mid = v.clamp(-15.0, 15.0),
            Param::Freq => self.eq_freq = v.clamp(0.0, 1.0),
            Param::Hi => self.eq_hi = v.clamp(-15.0, 15.0),
        }
    }

    /// Effective value: the live mod source when bound, else the manual.
    fn effective(&self, p: Param, bus: Option<&ModulationBus>) -> f32 {
        match (self.resolved[src_index(p)], bus) {
            (Some(ch), Some(bus)) => p.map_mod(bus.get(ch)),
            _ => self.get(p),
        }
    }
}

#[derive(Clone)]
struct TrackState {
    name: String,
    strip: Strip,
    mute: bool,
    solo: bool,
    meter: f32,
}

struct AudioSource {
    shm_name: String,
    ringbuf: AudioRingbuf,
}

struct MixerInner {
    tracks: Vec<TrackState>,
    audio_sources: Vec<AudioSource>,
    master: Strip,
    master_meter: f32,
    /// Tape out: when armed, the audio callback streams the mixed master
    /// blocks here until the sample budget runs out (sender drop ends the
    /// writer thread, which finalizes the WAV and drops a .done marker).
    tape: Option<(std::sync::mpsc::Sender<Vec<f32>>, u64)>,
    selected: usize,
    /// Selected row within the strip (index into STRIP_ROWS).
    selected_param: usize,
    scope_rb: Option<AudioRingbuf>,
}

impl MixerInner {
    fn strip(&self, idx: usize) -> &Strip {
        if idx < self.tracks.len() {
            &self.tracks[idx].strip
        } else {
            &self.master
        }
    }

    fn strip_mut(&mut self, idx: usize) -> &mut Strip {
        if idx < self.tracks.len() {
            &mut self.tracks[idx].strip
        } else {
            &mut self.master
        }
    }

    /// The param the cursor sits on (master swaps Pan for Width).
    fn current_param(&self) -> Param {
        let p = STRIP_ROWS[self.selected_param.min(STRIP_ROWS.len() - 1)];
        if p == Param::Pan && self.selected >= self.tracks.len() {
            Param::Width
        } else {
            p
        }
    }
}

fn mixer_thread(
    state: Arc<Mutex<MixerInner>>,
    shutdown: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;

    let mut transport = ShmTransport::open()
        .or_else(|_| ShmTransport::create(48000))?;

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No output device"))?;

    let config = device
        .default_output_config()
        .map_err(|e| anyhow::anyhow!("Failed to get output config: {}", e))?;

    let channels = config.channels() as usize;
    let sample_rate = config.sample_rate().0;
    let slot_len = 128; // max 64 frames * 2 channels

    let state_cb = Arc::clone(&state);
    let fs = sample_rate as f32;
    let mut chain: Vec<dsp::ChannelDsp> = Vec::new();
    let mut master_chain = dsp::ChannelDsp::new();
    let mut width_smooth = dsp::Smoother::new(1.0);
    let mut cb_bus: Option<ModulationBus> = ModulationBus::open().ok();

    let stream = device
        .build_output_stream(
            &config.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut inner = state_cb.lock().unwrap();
                if cb_bus.is_none() {
                    cb_bus = ModulationBus::open().ok();
                }
                while chain.len() < inner.audio_sources.len() {
                    chain.push(dsp::ChannelDsp::new());
                }
                let mut peak = 0.0f32;
                let mut written = 0;
                let any_solo = inner.tracks.iter().any(|t| t.solo);

                while written + slot_len <= data.len() {
                    for sample in data[written..written + slot_len].iter_mut() {
                        *sample = 0.0;
                    }

                    let mut voice_buf = [0.0f32; 128];
                    let n = inner.audio_sources.len().min(inner.tracks.len());
                    let mut track_peaks = vec![0.0f32; n];

                    for i in 0..n {
                        // effective params (mod-bound values replace manual)
                        let (eff, silent) = {
                            let t = &inner.tracks[i];
                            let s = &t.strip;
                            let bus = cb_bus.as_ref();
                            let eff = [
                                s.effective(Param::Level, bus),
                                s.effective(Param::Pan, bus),
                                s.effective(Param::Drive, bus),
                                s.effective(Param::Lo, bus),
                                s.effective(Param::Mid, bus),
                                s.effective(Param::Freq, bus),
                                s.effective(Param::Hi, bus),
                            ];
                            (eff, t.mute || (any_solo && !t.solo))
                        };
                        inner.tracks[i].strip.eff = eff;
                        let dspc = &mut chain[i];
                        dspc.tune(fs, eff[3], eff[4], eff[5], eff[6]);
                        dspc.level.target = if silent { 0.0 } else { eff[0] };
                        dspc.pan.target = eff[1];
                        dspc.drive_amt.target = eff[2];

                        let read_ok = inner.audio_sources[i]
                            .ringbuf
                            .read(&mut voice_buf[..slot_len])
                            .unwrap_or(false);
                        if !read_ok {
                            voice_buf[..slot_len].iter_mut().for_each(|v| *v = 0.0);
                        }
                        for j in (0..slot_len).step_by(2) {
                            let drv = dspc.drive_amt.tick();
                            let (l, r) = dspc.process(voice_buf[j], voice_buf[j + 1], drv);
                            let (gl, gr) = dsp::pan_gains(dspc.pan.tick());
                            let lvl = dspc.level.tick();
                            let (l, r) = (l * gl * lvl, r * gr * lvl);
                            data[written + j] += l;
                            data[written + j + 1] += r;
                            track_peaks[i] = track_peaks[i].max(l.abs().max(r.abs()));
                        }
                    }
                    for (i, &p) in track_peaks.iter().enumerate() {
                        if let Some(t) = inner.tracks.get_mut(i) {
                            // peak with ~decay so the meters breathe
                            t.meter = p.max(t.meter * 0.92);
                        }
                    }

                    // master: drive → EQ → width → level
                    let (m_eff, m_lvl) = {
                        let m = &inner.master;
                        let bus = cb_bus.as_ref();
                        (
                            [
                                m.effective(Param::Drive, bus),
                                m.effective(Param::Lo, bus),
                                m.effective(Param::Mid, bus),
                                m.effective(Param::Freq, bus),
                                m.effective(Param::Hi, bus),
                                m.effective(Param::Width, bus),
                            ],
                            m.effective(Param::Level, bus),
                        )
                    };
                    inner.master.eff =
                        [m_lvl, m_eff[5], m_eff[0], m_eff[1], m_eff[2], m_eff[3], m_eff[4]];
                    master_chain.tune(fs, m_eff[1], m_eff[2], m_eff[3], m_eff[4]);
                    master_chain.drive_amt.target = m_eff[0];
                    master_chain.level.target = m_lvl;
                    width_smooth.target = m_eff[5];
                    for j in (written..written + slot_len).step_by(2) {
                        let drv = master_chain.drive_amt.tick();
                        let (l, r) = master_chain.process(data[j], data[j + 1], drv);
                        let (l, r) = dsp::width(l, r, width_smooth.tick());
                        let lvl = master_chain.level.tick();
                        data[j] = l * lvl;
                        data[j + 1] = r * lvl;
                        peak = peak.max(data[j].abs().max(data[j + 1].abs()));
                    }

                    if let Some(ref mut scope_rb) = inner.scope_rb {
                        let _ = scope_rb.write(&data[written..written + slot_len]);
                    }

                    let tape_done = if let Some((tx, remaining)) = inner.tape.as_mut() {
                        let _ = tx.send(data[written..written + slot_len].to_vec());
                        *remaining = remaining.saturating_sub(slot_len as u64);
                        *remaining == 0
                    } else {
                        false
                    };
                    if tape_done {
                        inner.tape = None;
                    }

                    written += slot_len;
                }

                for sample in data[written..].iter_mut() {
                    *sample = 0.0;
                }

                inner.master_meter = peak;
                transport.add_clock_frames((data.len() / channels) as u64);
            },
            move |err| {
                eprintln!("Audio error: {}", err);
            },
            None,
        )
        .map_err(|e| anyhow::anyhow!("Failed to build output stream: {}", e))?;

    stream.play().map_err(|e| anyhow::anyhow!("Failed to play stream: {}", e))?;

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        manifest.reap_dead();
        let entries = manifest.entries();

        let mut inner = state.lock().unwrap();

        // tape-out arming: `los record` drops a request file; we pick it
        // up here (≤500ms later) and start streaming the master mix
        let arm = state::tmp_dir().join("record.arm");
        if inner.tape.is_none() && arm.exists() {
            let req = std::fs::read_to_string(&arm).unwrap_or_default();
            let _ = std::fs::remove_file(&arm);
            if let Some((secs, path)) = parse_arm(&req) {
                let (tx, rx) = std::sync::mpsc::channel::<Vec<f32>>();
                let total = (secs * sample_rate as f32) as u64 * 2; // stereo samples
                std::thread::spawn(move || tape_writer(rx, &path, sample_rate));
                inner.tape = Some((tx, total.max(2)));
            }
        }

        let mut to_remove = Vec::new();
        for (i, source) in inner.audio_sources.iter().enumerate() {
            let still_alive = entries.iter().any(|e| {
                e.audio_shm.as_deref() == Some(&source.shm_name)
            });
            if !still_alive {
                to_remove.push(i);
            }
        }
        for i in to_remove.into_iter().rev() {
            inner.audio_sources.remove(i);
            inner.tracks.remove(i);
            if inner.selected > 0 && inner.selected >= inner.tracks.len() {
                inner.selected = inner.tracks.len().saturating_sub(1);
            }
        }

        for entry in &entries {
            let has_shm = entry.audio_shm.is_some();
            if !has_shm { continue; }
            let shm_name = entry.audio_shm.as_ref().unwrap();

            let already = inner.audio_sources.iter().any(|s| s.shm_name == *shm_name);
            if already { continue; }

            if let Ok(ringbuf) = AudioRingbuf::open(shm_name) {
                let label = format!("{} {}", capitalize(&entry.module_name), entry.instance);
                // The envelope's function out carries raw control
                // transients (zero-rise strikes = impulses). Its fader
                // starts DOWN — like an unpatched cable on the hardware —
                // so those clicks are opt-in, not the default mix.
                let level = if entry.module_name == "envelope" { 0.0 } else { 0.8 };
                inner.audio_sources.push(AudioSource {
                    shm_name: shm_name.clone(),
                    ringbuf,
                });
                inner.tracks.push(TrackState {
                    name: label,
                    strip: Strip::new(level),
                    mute: false,
                    solo: false,
                    meter: 0.0,
                });
            }
        }

        // re-resolve every strip's mod bindings + publish what we listen
        // to (the sequencer's who's-listening markers)
        let mut consumed: u64 = 0;
        for t in inner.tracks.iter_mut() {
            for k in 0..7 {
                t.strip.resolved[k] = t.strip.srcs[k]
                    .as_ref()
                    .and_then(|a| routing::resolve(&entries, a));
                if let Some(ch) = t.strip.resolved[k] {
                    if ch < 64 {
                        consumed |= 1 << ch;
                    }
                }
            }
        }
        for k in 0..7 {
            inner.master.resolved[k] = inner.master.srcs[k]
                .as_ref()
                .and_then(|a| routing::resolve(&entries, a));
            if let Some(ch) = inner.master.resolved[k] {
                if ch < 64 {
                    consumed |= 1 << ch;
                }
            }
        }
        manifest.publish_consumes(consumed, 0);

        if inner.scope_rb.is_none() {
            inner.scope_rb = AudioRingbuf::open(SHM_NAME)
                .or_else(|_| AudioRingbuf::create(SHM_NAME)).ok();
        }

        drop(inner);

        std::thread::sleep(Duration::from_millis(500));
    }

    Ok(())
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

fn snapshot_params(s: &MixerInner) -> state::MixerParams {
    let src = |o: &Option<SourceAddr>| o.as_ref().map(|a| a.to_string());
    state::MixerParams {
        master: Some(s.master.level),
        master_drive: s.master.drive,
        master_eq_lo: s.master.eq_lo,
        master_eq_mid: s.master.eq_mid,
        master_eq_freq: s.master.eq_freq,
        master_eq_hi: s.master.eq_hi,
        master_width: s.master.pan,
        tracks: s.tracks.iter().map(|t| state::MixerTrackParam {
            level: t.strip.level,
            pan: t.strip.pan,
            mute: t.mute,
            solo: t.solo,
            drive: t.strip.drive,
            eq_lo: t.strip.eq_lo,
            eq_mid: t.strip.eq_mid,
            eq_freq: t.strip.eq_freq,
            eq_hi: t.strip.eq_hi,
            level_src: src(&t.strip.srcs[0]),
            pan_src: src(&t.strip.srcs[1]),
            drive_src: src(&t.strip.srcs[2]),
            lo_src: src(&t.strip.srcs[3]),
            mid_src: src(&t.strip.srcs[4]),
            freq_src: src(&t.strip.srcs[5]),
            hi_src: src(&t.strip.srcs[6]),
        }).collect(),
    }
}

fn apply_params(s: &mut MixerInner, params: &state::MixerParams) {
    if let Some(v) = params.master { s.master.level = v; }
    s.master.drive = params.master_drive;
    s.master.eq_lo = params.master_eq_lo;
    s.master.eq_mid = params.master_eq_mid;
    s.master.eq_freq = params.master_eq_freq;
    s.master.eq_hi = params.master_eq_hi;
    s.master.pan = params.master_width;
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    for (i, tp) in params.tracks.iter().enumerate().take(s.tracks.len()) {
        let t = &mut s.tracks[i];
        t.strip.level = tp.level;
        t.strip.pan = tp.pan;
        t.mute = tp.mute;
        t.solo = tp.solo;
        t.strip.drive = tp.drive;
        t.strip.eq_lo = tp.eq_lo;
        t.strip.eq_mid = tp.eq_mid;
        t.strip.eq_freq = tp.eq_freq;
        t.strip.eq_hi = tp.eq_hi;
        t.strip.srcs = [
            parse(&tp.level_src),
            parse(&tp.pan_src),
            parse(&tp.drive_src),
            parse(&tp.lo_src),
            parse(&tp.mid_src),
            parse(&tp.freq_src),
            parse(&tp.hi_src),
        ];
    }
}

/// Undo slots: strip*16 + kind (0 level, 1 pan/width, 2 mute, 3 solo,
/// 4 drive, 5 lo, 6 mid, 7 freq, 8 hi, 9+srcIndex bindings); the master
/// strip lives at MASTER_SLOT + kind.
const MASTER_SLOT: usize = 1_000_000;
const STRIDE: usize = 16;

impl crate::undo::ParamUndo for MixerInner {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        let (strip, kind, is_master) = if slot >= MASTER_SLOT {
            (None, slot - MASTER_SLOT, true)
        } else {
            (Some(slot / STRIDE), slot % STRIDE, false)
        };
        if !is_master && matches!(kind, 2 | 3) {
            let t = self.tracks.get(strip?)?;
            return Some(V::Bool(if kind == 2 { t.mute } else { t.solo }));
        }
        let s: &Strip = if is_master {
            &self.master
        } else {
            &self.tracks.get(strip?)?.strip
        };
        match kind {
            0 => Some(V::F32(s.level)),
            1 => Some(V::F32(s.pan)),
            4 => Some(V::F32(s.drive)),
            5 => Some(V::F32(s.eq_lo)),
            6 => Some(V::F32(s.eq_mid)),
            7 => Some(V::F32(s.eq_freq)),
            8 => Some(V::F32(s.eq_hi)),
            k if (9..16).contains(&k) => {
                Some(V::Src(s.srcs[k - 9].as_ref().map(|a| a.to_string())))
            }
            _ => None,
        }
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        let (strip, kind, is_master) = if slot >= MASTER_SLOT {
            (None, slot - MASTER_SLOT, true)
        } else {
            (Some(slot / STRIDE), slot % STRIDE, false)
        };
        if !is_master && matches!(kind, 2 | 3) {
            if let (Some(t), V::Bool(v)) = (strip.and_then(|i| self.tracks.get_mut(i)), value) {
                if kind == 2 {
                    t.mute = v;
                } else {
                    t.solo = v;
                }
            }
            return;
        }
        let s: &mut Strip = if is_master {
            &mut self.master
        } else {
            match strip.and_then(|i| self.tracks.get_mut(i)) {
                Some(t) => &mut t.strip,
                None => return,
            }
        };
        match (kind, value) {
            (0, V::F32(v)) => s.level = v,
            (1, V::F32(v)) => s.pan = v,
            (4, V::F32(v)) => s.drive = v,
            (5, V::F32(v)) => s.eq_lo = v,
            (6, V::F32(v)) => s.eq_mid = v,
            (7, V::F32(v)) => s.eq_freq = v,
            (8, V::F32(v)) => s.eq_hi = v,
            (k, V::Src(v)) if (9..16).contains(&k) => {
                s.srcs[k - 9] = v.as_deref().and_then(SourceAddr::parse);
            }
            _ => {}
        }
    }
}

/// The undo slot for the selected strip + kind.
fn slot_for(s: &MixerInner, kind: usize) -> usize {
    if s.selected < s.tracks.len() {
        s.selected * STRIDE + kind
    } else {
        MASTER_SLOT + kind
    }
}

/// Adjust the selected param by doctrine steps; per-param granularity.
fn adjust_param(s: &mut MixerInner, steps: i32, coarse: bool) {
    use crate::keys::step_f32;
    let p = s.current_param();
    let sel = s.selected;
    let strip = s.strip_mut(sel);
    let v = strip.get(p);
    let new = match p {
        Param::Level | Param::Drive => step_f32(v, steps, 0.05, coarse, 0.0, 1.0),
        Param::Pan => step_f32(v, steps, 0.05, coarse, -1.0, 1.0),
        Param::Width => step_f32(v, steps, 0.05, coarse, 0.0, 2.0),
        Param::Freq => step_f32(v, steps, 0.02, coarse, 0.0, 1.0),
        Param::Lo | Param::Mid | Param::Hi => step_f32(v, steps, 0.5, coarse, -15.0, 15.0),
    };
    strip.set(p, new);
}

/// Move the strip selection (wraps; the slot after the last track is master).
fn select_strip(s: &mut MixerInner, delta: i32) {
    let n = s.tracks.len() + 1;
    s.selected = crate::keys::cycle(s.selected, delta, n);
}

/// Parse a record.arm request: line 1 = seconds, line 2 = output path
/// (newline-separated so paths may contain spaces).
fn parse_arm(req: &str) -> Option<(f32, String)> {
    let mut lines = req.lines();
    let secs: f32 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    (secs > 0.0 && !path.is_empty()).then(|| (secs, path.to_string()))
}

/// Drains mixed blocks into a 16-bit stereo WAV; finalizes and drops a
/// `<path>.done` marker when the sender side closes.
fn tape_writer(rx: std::sync::mpsc::Receiver<Vec<f32>>, path: &str, sample_rate: u32) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let Ok(mut wr) = hound::WavWriter::create(path, spec) else {
        return;
    };
    for block in rx {
        for s in block {
            let _ = wr.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
        }
    }
    let _ = wr.finalize();
    let _ = std::fs::write(format!("{path}.done"), "ok");
}

/// Value text for a param row ("+3.0", "1.2k", "‹40", "80%").
fn param_text(p: Param, v: f32) -> String {
    match p {
        Param::Level | Param::Drive => format!("{:.0}%", v * 100.0),
        Param::Width => format!("{:.2}", v),
        Param::Freq => {
            let hz = dsp::mid_freq_hz(v);
            if hz >= 1000.0 {
                format!("{:.1}k", hz / 1000.0)
            } else {
                format!("{:.0}", hz)
            }
        }
        Param::Pan => {
            if v.abs() < 0.05 {
                String::from("·")
            } else if v < 0.0 {
                format!("‹{:.0}", v.abs() * 100.0)
            } else {
                format!("{:.0}›", v * 100.0)
            }
        }
        Param::Lo | Param::Mid | Param::Hi => {
            if v.abs() < 0.05 {
                String::from("0")
            } else {
                format!("{:+.1}", v)
            }
        }
    }
}

/// Full-console minimum height: header + name + 6 param rows + 3 fader
/// rows + % + MS + rule + status.
const CONSOLE_MIN_H: usize = 15;
const STRIP_W: usize = 9;

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    inner: &MixerInner,
    show_help: bool,
    overlay: Option<&str>,
    picker: Option<(Vec<String>, usize)>,
    entries: &[crate::shm::ManifestEntry],
) -> Result<()> {
    use crate::theme;
    use ratatui::text::Span;

    terminal.draw(|f| {
        let area = f.area();
        let w = area.width as usize;
        let h = area.height as usize;
        let mut lines: Vec<Line> = Vec::new();
        let tracks = &inner.tracks;

        lines.push(theme::header("MIX", &format!("{}ch", tracks.len()), "", w));

        // (name, strip, mute, solo, meter, selected, is_master)
        let strips: Vec<(&str, &Strip, bool, bool, f32, bool, bool)> = tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                (t.name.as_str(), &t.strip, t.mute, t.solo, t.meter, i == inner.selected, false)
            })
            .chain(std::iter::once((
                "MASTER",
                &inner.master,
                false,
                false,
                inner.master_meter,
                inner.selected >= tracks.len(),
                true,
            )))
            .collect();

        let console = h >= CONSOLE_MIN_H && w >= strips.len() * STRIP_W && !strips.is_empty();
        if console {
            // ── the console: vertical strips ────────────────────────────
            let fader_rows = (h - 12).clamp(3, 8);
            let mut name_spans: Vec<Span> = Vec::new();
            for (name, _, _, _, _, sel, _) in &strips {
                let mut nm = format!(" {}", name);
                nm.truncate(STRIP_W - 1);
                while nm.chars().count() < STRIP_W {
                    nm.push(' ');
                }
                name_spans.push(Span::styled(
                    nm,
                    if *sel { theme::selected() } else { theme::chrome_hi() },
                ));
            }
            lines.push(Line::from(name_spans));
            // param rows in console order (level renders as fader below)
            for (row, p0) in STRIP_ROWS.iter().enumerate() {
                if *p0 == Param::Level {
                    continue;
                }
                let mut spans: Vec<Span> = Vec::new();
                for (_, strip, mute, _, _, sel, is_master) in &strips {
                    let p = if *p0 == Param::Pan && *is_master { Param::Width } else { *p0 };
                    let bound = strip.srcs[src_index(p)].is_some();
                    let shown = if bound { strip.eff[src_index(p)] } else { strip.get(p) };
                    let mark = if bound { '▸' } else { ' ' };
                    let mut txt = format!(" {:<3}{}{}", p.label(), mark, param_text(p, shown));
                    txt.truncate(STRIP_W);
                    while txt.chars().count() < STRIP_W {
                        txt.push(' ');
                    }
                    let cursor = *sel && row == inner.selected_param.min(STRIP_ROWS.len() - 1);
                    let style = if cursor {
                        theme::selected()
                    } else if bound {
                        let cable = strip.srcs[src_index(p)]
                            .as_ref()
                            .map(|a| routing::cable_color(entries, a))
                            .unwrap_or_else(theme::clock);
                        theme::signal(cable)
                    } else if *mute {
                        theme::dim()
                    } else if shown.abs() > 0.049 || matches!(p, Param::Freq | Param::Width) {
                        theme::value()
                    } else {
                        theme::dim()
                    };
                    spans.push(Span::styled(txt, style));
                }
                lines.push(Line::from(spans));
            }
            // fader rows: meter fill, level tick, ghost at the live level
            for fr in 0..fader_rows {
                let frac_hi = 1.0 - fr as f32 / fader_rows as f32;
                let half = 0.5 / fader_rows as f32;
                let mut spans: Vec<Span> = Vec::new();
                for (_, strip, mute, _, meter, sel, _) in &strips {
                    let lvl = strip.get(Param::Level);
                    let live = strip.eff[0];
                    let bound = strip.srcs[0].is_some();
                    let tick = (lvl - frac_hi).abs() < half || (frac_hi <= lvl && fr == 0);
                    let ghost = bound && (live - frac_hi).abs() < half;
                    let meter_fill = !mute && *meter >= frac_hi;
                    let (ch, style) = if ghost {
                        (theme::GHOST, theme::signal(theme::clock()))
                    } else if tick {
                        ('█', if *sel { theme::selected() } else { theme::value() })
                    } else if meter_fill {
                        ('▓', theme::signal(theme::audio()))
                    } else {
                        ('·', theme::dim())
                    };
                    spans.push(Span::styled(format!("   {}     ", ch), style));
                }
                lines.push(Line::from(spans));
            }
            // % + M S rows
            let mut pct: Vec<Span> = Vec::new();
            let mut ms: Vec<Span> = Vec::new();
            for (_, strip, mute, solo, _, sel, is_master) in &strips {
                let bound = strip.srcs[0].is_some();
                let shown = if bound { strip.eff[0] } else { strip.get(Param::Level) };
                let cursor =
                    *sel && STRIP_ROWS[inner.selected_param.min(6)] == Param::Level;
                let mut txt = format!("  {:>4}", param_text(Param::Level, shown));
                while txt.chars().count() < STRIP_W {
                    txt.push(' ');
                }
                pct.push(Span::styled(
                    txt,
                    if cursor {
                        theme::selected()
                    } else if bound {
                        theme::signal(theme::clock())
                    } else {
                        theme::value()
                    },
                ));
                if *is_master {
                    ms.push(Span::styled(
                        format!("   {}     ", theme::AUDIO_GLYPH),
                        theme::signal(theme::audio()),
                    ));
                } else {
                    ms.push(Span::styled(
                        "   M ",
                        if *mute { theme::signal(theme::alert()) } else { theme::dim() },
                    ));
                    ms.push(Span::styled(
                        "S   ",
                        if *solo { theme::signal(theme::clock()) } else { theme::dim() },
                    ));
                }
            }
            lines.push(Line::from(pct));
            lines.push(Line::from(ms));
        } else {
            // ── compact: dense rows + selected-strip console detail ─────
            let bar_w = theme::bar_width(w, 28);
            for (name, strip, mute, solo, meter, sel, _) in &strips {
                let name_style = if *sel { theme::selected() } else { theme::chrome() };
                let mut spans: Vec<Span> =
                    vec![Span::styled(format!(" {:<9}", name), name_style)];
                let m = if *mute { 0.0 } else { *meter };
                spans.push(Span::styled(
                    format!("{} ", theme::meter_char(m)),
                    theme::signal(theme::audio()),
                ));
                if *mute {
                    spans.push(Span::styled(
                        theme::bar_str(strip.level, None, bar_w),
                        theme::dim(),
                    ));
                } else {
                    spans.extend(theme::bar(strip.level, None, bar_w, theme::audio()));
                }
                spans.push(Span::styled(
                    format!(" {:>3.0}%", strip.level * 100.0),
                    theme::value(),
                ));
                if *mute {
                    spans.push(Span::styled(" M", theme::signal(theme::alert())));
                }
                if *solo {
                    spans.push(Span::styled(" S", theme::signal(theme::clock())));
                }
                lines.push(Line::from(spans));
            }
            // the selected strip's console params on one detail line
            if let Some((_, strip, _, _, _, _, is_master)) =
                strips.get(inner.selected.min(strips.len() - 1))
            {
                let mut detail: Vec<Span> = vec![Span::styled(" › ", theme::chrome_hi())];
                for (row, p0) in STRIP_ROWS.iter().enumerate() {
                    let p = if *p0 == Param::Pan && *is_master { Param::Width } else { *p0 };
                    let bound = strip.srcs[src_index(p)].is_some();
                    let shown = if bound { strip.eff[src_index(p)] } else { strip.get(p) };
                    let style = if row == inner.selected_param.min(6) {
                        theme::selected()
                    } else if bound {
                        theme::signal(theme::clock())
                    } else {
                        theme::value()
                    };
                    detail.push(Span::styled(
                        format!("{} {}  ", p.label(), param_text(p, shown)),
                        style,
                    ));
                }
                lines.push(Line::from(detail));
            }
        }

        theme::anchor_bottom(&mut lines, h, 2);
        lines.push(theme::rule(w));
        lines.push(theme::status("NORMAL", overlay.unwrap_or(""), "", w));

        f.render_widget(Paragraph::new(lines), area);

        if show_help {
            let help_text = vec![
                Line::from("━━━ MIX · the console ━━━"),
                Line::from(""),
                Line::from("  h/l        Select strip (channels, then MASTER)"),
                Line::from("  j/k        Select param (drv hi mid frq lo pan lvl)"),
                Line::from("  - / =      Adjust selected param (_/+ coarse, counts)"),
                Line::from("  0          Reset param to default"),
                Line::from("  @          Bind a mod source to the param · x unbinds"),
                Line::from("  m / s      Mute / solo strip"),
                Line::from("  gg / G     First strip / master"),
                Line::from("  u/^r       Undo / redo"),
                Line::from("  :w/:e/:q   Patches / quit · space transport"),
                Line::from(""),
                Line::from("Chain: drive → EQ (lo/mid/hi) → pan → fader."),
                Line::from("Master adds width. Bound params show the live"),
                Line::from("value in the source's cable color; ▴ on the"),
                Line::from("fader is the live modulated level."),
                Line::from(""),
                Line::from("  ? closes help"),
            ];
            let help = Paragraph::new(help_text)
                .style(Style::default().fg(theme::ink()).bg(theme::bg()))
                .block(Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" MIX ", theme::chrome_hi())));
            f.render_widget(help, area);
        }

        if let Some((rows, sel)) = picker {
            let ph = (rows.len() as u16 + 2).min(area.height);
            let pw = rows.iter().map(|r| r.len()).max().unwrap_or(10).max(20) as u16 + 4;
            let r = ratatui::layout::Rect::new(
                (area.width.saturating_sub(pw)) / 2,
                (area.height.saturating_sub(ph)) / 2,
                pw.min(area.width),
                ph,
            );
            f.render_widget(ratatui::widgets::Clear, r);
            let items: Vec<ratatui::widgets::ListItem> = rows
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    let style = if i == sel { theme::selected() } else { theme::value() };
                    ratatui::widgets::ListItem::new(row.clone()).style(style)
                })
                .collect();
            let list = ratatui::widgets::List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" bind param ", theme::chrome_hi())),
            );
            f.render_widget(list, r);
        }
    })?;

    Ok(())
}

pub fn run() -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("mixer", 0);
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    // Never die over a registration problem — the mixer owns the audio
    // output and advances the transport clock for everyone.
    if let Err(e) = manifest.register("mixer", 0, None, 0) {
        eprintln!("[mixer] manifest registration failed (continuing): {}", e);
    }

    for attempt in 0..20 {
        match enable_raw_mode() {
            Ok(()) => break,
            Err(e) => {
                if attempt < 19 {
                    std::thread::sleep(Duration::from_millis(200));
                } else {
                    return Err(anyhow::anyhow!("Failed to enable raw mode after 20 attempts: {}", e));
                }
            }
        }
    }
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let inner = Arc::new(Mutex::new(MixerInner {
        tracks: Vec::new(),
        audio_sources: Vec::new(),
        master: Strip { pan: 1.0, ..Strip::new(0.8) }, // pan = width on master
        master_meter: 0.0,
        tape: None,
        selected: 0,
        selected_param: 6, // the fader — the 90% case
        scope_rb: None,
    }));

    if let Ok(params) = state::load_module_state::<state::MixerParams>("mixer", 0) {
        apply_params(&mut inner.lock().unwrap(), &params);
    }

    let inner_clone = Arc::clone(&inner);
    let (_tx, rx) = std::sync::mpsc::channel();

    let _mixer_handle = std::thread::spawn(move || {
        if let Err(e) = mixer_thread(inner_clone, rx) {
            eprintln!("Mixer thread error: {}", e);
        }
    });

    let mut show_help = false;
    let mut count = crate::keys::Count::default();
    let mut pending_g = false;
    let mut history = crate::undo::ParamHistory::default();
    let mut ex = crate::excmd::ExLine::default();
    let mut picker = crate::picker::Picker::default();
    let mut ex_msg: Option<String> = None;
    let mut patch_name: Option<String> = None;
    let mut baseline = state::to_toml_string(&snapshot_params(&inner.lock().unwrap())).unwrap_or_default();
    let mut should_quit = false;
    // Global transport handle for Space = play/pause (lazily reopened)
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();
    // manifest entries for cable colors (refreshed ~1s)
    let mut ui_entries: Vec<crate::shm::ManifestEntry> = Vec::new();
    let mut ui_entries_at: Option<std::time::Instant> = None;

    loop {
        if state::check_save_signal() {
            let params = snapshot_params(&inner.lock().unwrap());
            let _ = state::save_module_state("mixer", 0, &params);
        }

        if state::check_reload_signal() {
            if let Ok(params) = state::load_module_state::<state::MixerParams>("mixer", 0) {
                apply_params(&mut inner.lock().unwrap(), &params);
            }
        }

        if ui_entries_at.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
            ui_entries = manifest.entries();
            ui_entries_at = Some(std::time::Instant::now());
        }

        let overlay = if ex.is_active() {
            Some(ex.display())
        } else {
            ex_msg.clone()
        };
        {
            let s = inner.lock().unwrap();
            let picker_rows = picker.is_active().then(|| picker.rows());
            draw_ui(&mut terminal, &s, show_help, overlay.as_deref(), picker_rows, &ui_entries)?;
        }

        if event::poll(Duration::from_millis(50))? {
            let ev = event::read()?;
            if let Event::Mouse(m) = ev {
                use crossterm::event::MouseEventKind;
                if picker.is_active() || ex.is_active() {
                    continue;
                }
                match m.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let steps = if m.kind == MouseEventKind::ScrollUp { 1 } else { -1 };
                        use crate::undo::ParamUndo;
                        let mut s = inner.lock().unwrap();
                        let slot = slot_for(&s, s.current_param().kind());
                        let old = s.get_param(slot);
                        adjust_param(&mut s, steps, false);
                        let new = s.get_param(slot);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(slot, "Adjust", old, new);
                        }
                    }
                    MouseEventKind::Down(_) => {
                        // console mode: the column picks the strip
                        let mut s = inner.lock().unwrap();
                        let strip = (m.column as usize) / STRIP_W;
                        if strip <= s.tracks.len() {
                            s.selected = strip;
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if let Event::Key(key) = ev {
                ex_msg = None;
                if picker.is_active() {
                    if let crate::picker::PickerEvent::Chosen(addr) = picker.handle_key(key.code)
                    {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = inner.lock().unwrap();
                        let p = s.current_param();
                        let slot = slot_for(&s, 9 + src_index(p));
                        let old = s.get_param(slot);
                        let sel = s.selected;
                        s.strip_mut(sel).srcs[src_index(p)] = addr.clone();
                        if let Some(old) = old {
                            history.record(
                                slot,
                                "Bind",
                                old,
                                ParamValue::Src(addr.map(|a| a.to_string())),
                            );
                        }
                    }
                    continue;
                }
                if ex.is_active() {
                    let completer = crate::excmd::standard_completer(
                        crate::excmd::patch_names(&state::patches_dir()),
                    );
                    if let crate::excmd::ExEvent::Submit(cmd) = ex.handle_key(key.code, &completer) {
                        use crate::excmd::ExCommand;
                        let params = snapshot_params(&inner.lock().unwrap());
                        match cmd {
                            ExCommand::Write(name) => {
                                ex_msg = Some(match crate::excmd::ex_write(name, &mut patch_name, &mut baseline, &params) {
                                    Ok(m) | Err(m) => m,
                                });
                            }
                            ExCommand::Edit(name) => match state::load_patch::<state::MixerParams>(&name) {
                                Ok(p) => {
                                    apply_params(&mut inner.lock().unwrap(), &p);
                                    baseline = state::to_toml_string(&snapshot_params(&inner.lock().unwrap())).unwrap_or_default();
                                    patch_name = Some(name.clone());
                                    ex_msg = Some(format!("Loaded {}", name));
                                }
                                Err(e) => ex_msg = Some(e.to_string()),
                            },
                            ExCommand::Quit { force } => {
                                if !force && crate::excmd::is_dirty(&params, &baseline) {
                                    ex_msg = Some(String::from("Unsaved changes (:q! to discard, :w <name> to save)"));
                                } else {
                                    should_quit = true;
                                }
                            }
                            ExCommand::WriteQuit(name) => {
                                match crate::excmd::ex_write(name, &mut patch_name, &mut baseline, &params) {
                                    Ok(_) => should_quit = true,
                                    Err(m) => ex_msg = Some(m),
                                }
                            }
                            ExCommand::Set(k, _) => ex_msg = Some(format!("Unknown setting: {}", k)),
                            ExCommand::Unknown(c) => ex_msg = Some(format!("Not a command: {}", c)),
                        }
                    }
                    if should_quit {
                        break;
                    }
                    continue;
                }
                if !matches!(key.code, KeyCode::Char('g')) {
                    pending_g = false;
                }
                if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
                    let n = count.take();
                    let mut s = inner.lock().unwrap();
                    ex_msg = Some(crate::undo::history_status("Redo", n, || history.redo(&mut *s)));
                    continue;
                }
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let params = snapshot_params(&inner.lock().unwrap());
                    let _ = state::save_module_state("mixer", 0, &params);
                    continue;
                }
                match key.code {
                    KeyCode::Char(c) if c.is_ascii_digit() && c != '0' && count.push(c) => {}
                    // the console grid: h/l strips · j/k params
                    KeyCode::Char('h') | KeyCode::Left => {
                        select_strip(&mut inner.lock().unwrap(), -(count.take() as i32));
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        select_strip(&mut inner.lock().unwrap(), count.take() as i32);
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        let n = count.take() as i32;
                        let mut s = inner.lock().unwrap();
                        s.selected_param = crate::keys::cycle(s.selected_param, n, STRIP_ROWS.len());
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        let n = count.take() as i32;
                        let mut s = inner.lock().unwrap();
                        s.selected_param = crate::keys::cycle(s.selected_param, -n, STRIP_ROWS.len());
                    }
                    // -/= adjust the selected param; _/+ (or H/L) coarse
                    KeyCode::Char(c @ ('-' | '=' | '_' | '+' | 'H' | 'L')) => {
                        let n = count.take() as i32;
                        let (steps, coarse) = match c {
                            '-' => (-n, false),
                            '=' => (n, false),
                            '_' | 'H' => (-n, true),
                            _ => (n, true),
                        };
                        use crate::undo::ParamUndo;
                        let mut s = inner.lock().unwrap();
                        let slot = slot_for(&s, s.current_param().kind());
                        let old = s.get_param(slot);
                        adjust_param(&mut s, steps, coarse);
                        let new = s.get_param(slot);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(slot, "Adjust", old, new);
                        }
                    }
                    KeyCode::Char('0') => {
                        count.clear();
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = inner.lock().unwrap();
                        let p = s.current_param();
                        let slot = slot_for(&s, p.kind());
                        let old = s.get_param(slot);
                        let sel = s.selected;
                        s.strip_mut(sel).set(p, p.default_value());
                        if let Some(old) = old {
                            history.record(slot, "Reset", old, ParamValue::F32(p.default_value()));
                        }
                    }
                    // @ binds a mod source to the selected param; x unbinds
                    KeyCode::Char('@') => {
                        count.clear();
                        let sources = Manifest::open()
                            .map(|m| routing::live_sources(&m.entries()))
                            .unwrap_or_default();
                        let s = inner.lock().unwrap();
                        let p = s.current_param();
                        let current = s.strip(s.selected).srcs[src_index(p)].clone();
                        drop(s);
                        picker.open(sources, current.as_ref());
                    }
                    KeyCode::Char('x') => {
                        count.clear();
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = inner.lock().unwrap();
                        let p = s.current_param();
                        let slot = slot_for(&s, 9 + src_index(p));
                        let old = s.get_param(slot);
                        let sel = s.selected;
                        if s.strip(sel).srcs[src_index(p)].is_some() {
                            s.strip_mut(sel).srcs[src_index(p)] = None;
                            s.strip_mut(sel).resolved[src_index(p)] = None;
                            if let Some(old) = old {
                                history.record(slot, "Unbind", old, ParamValue::Src(None));
                            }
                        }
                    }
                    KeyCode::Char('u') => {
                        let n = count.take();
                        let mut s = inner.lock().unwrap();
                        ex_msg = Some(crate::undo::history_status("Undo", n, || history.undo(&mut *s)));
                    }
                    KeyCode::Char('g') => {
                        count.clear();
                        if pending_g {
                            pending_g = false;
                            inner.lock().unwrap().selected = 0;
                        } else {
                            pending_g = true;
                        }
                    }
                    KeyCode::Char('G') => {
                        count.clear();
                        let mut s = inner.lock().unwrap();
                        s.selected = s.tracks.len(); // master strip
                    }
                    KeyCode::Char(c @ ('m' | 's')) => {
                        count.clear();
                        use crate::undo::ParamValue;
                        let mut s = inner.lock().unwrap();
                        let sel = s.selected;
                        if sel < s.tracks.len() {
                            let (kind, desc) = if c == 'm' { (2, "Mute") } else { (3, "Solo") };
                            let was = if c == 'm' { s.tracks[sel].mute } else { s.tracks[sel].solo };
                            if c == 'm' {
                                s.tracks[sel].mute = !was;
                            } else {
                                s.tracks[sel].solo = !was;
                            }
                            history.record(sel * STRIDE + kind, desc, ParamValue::Bool(was), ParamValue::Bool(!was));
                        }
                    }
                    KeyCode::Char(' ') => {
                        if transport_ui.is_none() {
                            transport_ui = ShmTransport::open().ok();
                        }
                        if let Some(ref mut t) = transport_ui {
                            t.toggle_playing();
                        }
                    }
                    KeyCode::Char(':') => {
                        count.clear();
                        ex.open();
                    }
                    KeyCode::Char('?') => {
                        count.clear();
                        show_help = !show_help;
                    }
                    _ => {
                        count.clear();
                    }
                }
            }
        }
        if should_quit {
            break;
        }
    }

    crossterm::terminal::disable_raw_mode()?;
    execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mixer_with_tracks(n: usize) -> MixerInner {
        MixerInner {
            tracks: (0..n)
                .map(|i| TrackState {
                    name: format!("T{}", i),
                    strip: Strip::new(0.8),
                    mute: false,
                    solo: false,
                    meter: 0.0,
                })
                .collect(),
            audio_sources: vec![],
            master: Strip { pan: 1.0, ..Strip::new(0.8) },
            master_meter: 0.0,
            tape: None,
            selected: 0,
            selected_param: 6,
            scope_rb: None,
        }
    }

    #[test]
    fn select_wraps_through_master() {
        let mut s = mixer_with_tracks(2);
        select_strip(&mut s, 1);
        assert_eq!(s.selected, 1);
        select_strip(&mut s, 1);
        assert_eq!(s.selected, 2, "slot after last track is master");
        select_strip(&mut s, 1);
        assert_eq!(s.selected, 0, "wraps to first track");
        select_strip(&mut s, -1);
        assert_eq!(s.selected, 2, "wraps backward to master");
    }

    #[test]
    fn adjust_targets_the_selected_param() {
        let mut s = mixer_with_tracks(2);
        // fader selected by default
        adjust_param(&mut s, -2, false);
        assert!((s.tracks[0].strip.level - 0.7).abs() < 1e-6);
        // walk to the mid band and boost
        s.selected_param = 2; // STRIP_ROWS[2] = Mid
        adjust_param(&mut s, 4, false);
        assert!((s.tracks[0].strip.eq_mid - 2.0).abs() < 1e-6, "0.5 dB steps");
        adjust_param(&mut s, 100, true);
        assert_eq!(s.tracks[0].strip.eq_mid, 15.0, "clamps at +15");
        // master strip: the pan row reads as width
        s.selected = 2;
        s.selected_param = 5; // Pan position
        assert_eq!(s.current_param(), Param::Width);
        adjust_param(&mut s, 100, true);
        assert_eq!(s.master.pan, 2.0, "width clamps at 2.0");
    }

    #[test]
    fn reset_and_defaults() {
        let mut s = mixer_with_tracks(1);
        s.selected_param = 0; // Drive
        adjust_param(&mut s, 10, false);
        assert!(s.tracks[0].strip.drive > 0.0);
        let p = s.current_param();
        s.strip_mut(0).set(p, p.default_value());
        assert_eq!(s.tracks[0].strip.drive, 0.0);
        assert_eq!(Param::Level.default_value(), 0.8);
        assert_eq!(Param::Width.default_value(), 1.0);
        assert_eq!(Param::Freq.default_value(), 0.5);
    }

    #[test]
    fn undo_slots_round_trip_every_param() {
        use crate::undo::{ParamUndo, ParamValue as V};
        let mut s = mixer_with_tracks(1);
        for (kind, v) in
            [(0usize, 0.5f32), (1, -0.3), (4, 0.4), (5, 3.0), (6, -6.0), (7, 0.8), (8, 12.0)]
        {
            s.set_param(kind, V::F32(v));
            assert_eq!(s.get_param(kind), Some(V::F32(v)), "kind {kind}");
        }
        s.set_param(9, V::Src(Some("envelope/0/ch2".into())));
        assert_eq!(s.get_param(9), Some(V::Src(Some("envelope/0/ch2".into()))));
        s.set_param(2, V::Bool(true));
        assert_eq!(s.get_param(2), Some(V::Bool(true)), "mute");
        // master slots
        s.set_param(MASTER_SLOT + 8, V::F32(-9.0));
        assert_eq!(s.master.eq_hi, -9.0);
    }

    #[test]
    fn persistence_round_trips_the_console() {
        let mut s = mixer_with_tracks(2);
        s.tracks[0].strip.drive = 0.3;
        s.tracks[0].strip.eq_lo = 6.0;
        s.tracks[0].strip.eq_freq = 0.7;
        s.tracks[0].strip.srcs[0] = SourceAddr::parse("envelope/0/ch1");
        s.master.eq_hi = -3.0;
        s.master.pan = 1.5; // width
        s.master.drive = 0.2;
        let params = snapshot_params(&s);
        let toml = state::to_toml_string(&params).expect("serializes");
        let back: state::MixerParams = toml::from_str(&toml).expect("parses");
        let mut s2 = mixer_with_tracks(2);
        apply_params(&mut s2, &back);
        assert_eq!(s2.tracks[0].strip.drive, 0.3);
        assert_eq!(s2.tracks[0].strip.eq_lo, 6.0);
        assert_eq!(s2.tracks[0].strip.eq_freq, 0.7);
        assert_eq!(
            s2.tracks[0].strip.srcs[0].as_ref().map(|a| a.to_string()),
            Some("envelope/0/ch1".into())
        );
        assert_eq!(s2.master.eq_hi, -3.0);
        assert_eq!(s2.master.pan, 1.5);
        assert_eq!(s2.master.drive, 0.2);
        // a legacy save (no console fields) loads with sane defaults
        let legacy: state::MixerParams = toml::from_str(
            "master = 0.8\n[[tracks]]\nlevel = 0.5\npan = 0.0\nmute = false\nsolo = false\n",
        )
        .expect("legacy parses");
        let mut s3 = mixer_with_tracks(1);
        apply_params(&mut s3, &legacy);
        assert_eq!(s3.tracks[0].strip.level, 0.5);
        assert_eq!(s3.tracks[0].strip.eq_freq, 0.5, "freq defaults to center");
        assert_eq!(s3.master.pan, 1.0, "width defaults to 1");
    }

    #[test]
    fn effective_uses_manual_when_unbound() {
        let s = Strip::new(0.6);
        assert_eq!(s.effective(Param::Level, None), 0.6);
        // bound mapping ranges
        assert_eq!(Param::Lo.map_mod(1.0), 15.0);
        assert_eq!(Param::Lo.map_mod(-1.0), -15.0);
        assert_eq!(Param::Width.map_mod(0.5), 1.0);
        assert_eq!(Param::Pan.map_mod(2.0), 1.0, "clamped");
    }

    #[test]
    fn arm_request_round_trip() {
        assert_eq!(parse_arm("16\n/tmp/out tape.wav"), Some((16.0, "/tmp/out tape.wav".into())));
        assert_eq!(parse_arm("0\n/tmp/x.wav"), None, "zero seconds refused");
        assert_eq!(parse_arm("abc\n/tmp/x.wav"), None);
        assert_eq!(parse_arm("5"), None, "missing path refused");
    }
}
