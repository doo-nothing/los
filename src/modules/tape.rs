//! tape — the record window's deck (docs/plans/tape-deck.md).
//!
//! Six tracks on a three-minute tape, somewhere between a Tascam
//! 4-track and OP-1 tape: tracks default to recording **the mix** (the
//! mixer's print bus — everything except this deck's own playback, so
//! overdubs never re-record themselves) and can be re-armed to any
//! audio source via the same input claim the fx modules use. Varispeed
//! repitches playback (the Cortini knob — bindable), loops overdub
//! sound-on-sound, tracks reverse, faders carry recorded automation
//! lanes and `@` bindings, takes bounce and export to `~/Music/los/`,
//! and an optional RAVE helper (tools/los-rave) reprocesses takes
//! through neural models offline.
//!
//! Recording is locked to 1× speed: capture stays sample-locked to the
//! rig; varispeed is a playback instrument.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind},
    execute,
    terminal::{enable_raw_mode, EnterAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use crate::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

/// Used only until the transport answers with the device's real rate.
const FALLBACK_RATE: f32 = 48_000.0;
pub const TRACKS: usize = 6;
/// Tape length, seconds. Finite on purpose — a tape, not a DAW.
const TAPE_SECS: u64 = 180;

const GLOBAL_STRIP: usize = TRACKS;

/// Columns within a track lane (h/l walks them; the rest are keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackCol {
    /// What this track records: the mix (print bus) or a claimed source.
    Input,
    Fader,
    Pan,
}
const TRACK_COLS: [TrackCol; 3] = [TrackCol::Input, TrackCol::Fader, TrackCol::Pan];

// ── state ──────────────────────────────────────────────────────────────────

struct Track {
    /// Interleaved stereo i16, allocated on first use (~31 MB each).
    audio: Option<Vec<i16>>,
    /// Frames written so far (waveform/export bound).
    filled: u64,
    /// None = the mix (print bus); Some("voice/1") = a claimed source.
    input: Option<String>,
    armed: bool,
    /// Pass the armed input through to the deck's output (input
    /// monitoring — hear what you're about to print).
    monitor: bool,
    muted: bool,
    reversed: bool,
    fader: f32,
    pan: f32,
    srcs: [Option<SourceAddr>; 2], // fader, pan
    resolved: [Option<usize>; 2],
    eff: [f32; 2],
    /// The recorded fader lane: (frame, value), sorted by frame.
    auto: Vec<(u64, f32)>,
    write_auto: bool,
    /// Pre-RAVE audio kept for A/B (the `N` key restores it).
    backup: Option<(Vec<i16>, u64)>,
    /// Live waveform overview cache (rebuilt when filled changes).
    wave_cache: Vec<f32>,
    wave_for: (u64, usize),
}

impl Track {
    fn new() -> Self {
        Self {
            audio: None,
            filled: 0,
            input: None,
            armed: false,
            monitor: true,
            muted: false,
            reversed: false,
            fader: 0.8,
            pan: 0.0,
            srcs: Default::default(),
            resolved: Default::default(),
            eff: [0.8, 0.0],
            auto: Vec::new(),
            write_auto: false,
            backup: None,
            wave_cache: Vec::new(),
            wave_for: (u64::MAX, 0),
        }
    }

    /// The automation lane's value at `frame` (last point at or before),
    /// or the manual fader when the lane is empty.
    fn lane_value(&self, frame: u64) -> f32 {
        if self.auto.is_empty() {
            return self.fader;
        }
        match self.auto.binary_search_by_key(&frame, |(f, _)| *f) {
            Ok(i) => self.auto[i].1,
            Err(0) => self.auto[0].1,
            Err(i) => self.auto[i - 1].1,
        }
    }
}

struct TapeState {
    tracks: [Track; TRACKS],
    speed: f32,
    speed_src: Option<SourceAddr>,
    speed_resolved: Option<usize>,
    speed_eff: f32,
    /// Playhead in frames (fractional under varispeed).
    pos: f64,
    recording: bool,
    loop_on: bool,
    loop_in: u64,
    loop_out: u64,
    /// Device rate the engine is running at (frames/sec).
    rate: f32,
    /// Frames on the tape (TAPE_SECS × rate).
    tape_len: u64,
    selected: usize,
    sel_col: usize,
    /// Pending two-press confirm for destructive clears.
    pending_clear: Option<usize>,
    /// RAVE: (track, progress 0..1 or <0 = failed) while running.
    rave: Option<(usize, f32)>,
    status: Option<String>,
}

impl TapeState {
    fn new() -> Self {
        let rate = FALLBACK_RATE;
        Self {
            tracks: std::array::from_fn(|_| Track::new()),
            speed: 1.0,
            speed_src: None,
            speed_resolved: None,
            speed_eff: 1.0,
            pos: 0.0,
            recording: false,
            loop_on: false,
            loop_in: 0,
            loop_out: TAPE_SECS * rate as u64,
            rate,
            tape_len: TAPE_SECS * rate as u64,
            selected: 0,
            sel_col: 1, // the fader — the 90% case
            pending_clear: None,
            rave: None,
            status: None,
        }
    }

    fn current_col(&self) -> TrackCol {
        TRACK_COLS[self.sel_col.min(TRACK_COLS.len() - 1)]
    }

    fn speed_effective(&self, bus: Option<&ModulationBus>) -> f32 {
        match (self.speed_resolved, bus) {
            // 0..1 spans 0.25×–2× exponentially; 0.5 lands on 1×
            (Some(ch), Some(bus)) => 0.25 * 8.0_f32.powf(bus.get(ch).clamp(0.0, 1.0)),
            _ => self.speed,
        }
    }
}

// ── undo ───────────────────────────────────────────────────────────────────
// Slots: track t × 8 + {0 fader, 1 pan, 2 mute, 3 reverse, 4 input,
// 5 fader-src, 6 pan-src}; globals at 1000 + {0 speed, 1 speed-src,
// 2 loop_on}.

const GLOBAL_SLOT: usize = 1000;
const T_STRIDE: usize = 8;

impl crate::undo::ParamUndo for TapeState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if let Some(k) = slot.checked_sub(GLOBAL_SLOT) {
            return match k {
                0 => Some(V::F32(self.speed)),
                1 => Some(V::Src(self.speed_src.as_ref().map(|a| a.to_string()))),
                2 => Some(V::Bool(self.loop_on)),
                _ => None,
            };
        }
        let (t, k) = (slot / T_STRIDE, slot % T_STRIDE);
        let tr = self.tracks.get(t)?;
        match k {
            0 => Some(V::F32(tr.fader)),
            1 => Some(V::F32(tr.pan)),
            2 => Some(V::Bool(tr.muted)),
            3 => Some(V::Bool(tr.reversed)),
            4 => Some(V::Src(tr.input.clone())),
            5 => Some(V::Src(tr.srcs[0].as_ref().map(|a| a.to_string()))),
            6 => Some(V::Src(tr.srcs[1].as_ref().map(|a| a.to_string()))),
            _ => None,
        }
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        if let Some(k) = slot.checked_sub(GLOBAL_SLOT) {
            match (k, value) {
                (0, V::F32(v)) => self.speed = v.clamp(0.25, 2.0),
                (1, V::Src(v)) => {
                    self.speed_src = v.as_deref().and_then(SourceAddr::parse);
                    self.speed_resolved = None;
                }
                (2, V::Bool(v)) => self.loop_on = v,
                _ => {}
            }
            return;
        }
        let (t, k) = (slot / T_STRIDE, slot % T_STRIDE);
        let Some(tr) = self.tracks.get_mut(t) else { return };
        match (k, value) {
            (0, V::F32(v)) => tr.fader = v.clamp(0.0, 1.0),
            (1, V::F32(v)) => tr.pan = v.clamp(-1.0, 1.0),
            (2, V::Bool(v)) => tr.muted = v,
            (3, V::Bool(v)) => tr.reversed = v,
            (4, V::Src(v)) => tr.input = v,
            (5, V::Src(v)) => {
                tr.srcs[0] = v.as_deref().and_then(SourceAddr::parse);
                tr.resolved[0] = None;
            }
            (6, V::Src(v)) => {
                tr.srcs[1] = v.as_deref().and_then(SourceAddr::parse);
                tr.resolved[1] = None;
            }
            _ => {}
        }
    }
}

// ── persistence ────────────────────────────────────────────────────────────
// Params in TOML; the audio itself lives as WAVs under
// ~/.config/los/tape/ (written on save, loaded at startup).

fn tape_dir() -> PathBuf {
    state::los_dir().join("tape")
}

fn snapshot_params(s: &TapeState) -> state::TapeParams {
    state::TapeParams {
        format: state::STATE_FORMAT,
        speed: Some(s.speed),
        loop_on: Some(s.loop_on),
        loop_in: Some(s.loop_in),
        loop_out: Some(s.loop_out),
        speed_src: s.speed_src.as_ref().map(|a| a.to_string()),
        tracks: s
            .tracks
            .iter()
            .map(|t| state::TapeTrackParam {
                input: t.input.clone(),
                fader: t.fader,
                pan: t.pan,
                armed: t.armed,
                muted: t.muted,
                reversed: t.reversed,
                monitor: t.monitor,
                fader_src: t.srcs[0].as_ref().map(|a| a.to_string()),
                pan_src: t.srcs[1].as_ref().map(|a| a.to_string()),
                auto: t.auto.iter().map(|(f, v)| (*f, *v)).collect(),
            })
            .collect(),
    }
}

fn apply_params(s: &mut TapeState, p: &state::TapeParams) {
    if let Some(v) = p.speed {
        s.speed = v.clamp(0.25, 2.0);
    }
    if let Some(v) = p.loop_on {
        s.loop_on = v;
    }
    if let Some(v) = p.loop_in {
        s.loop_in = v;
    }
    if let Some(v) = p.loop_out {
        s.loop_out = v;
    }
    s.speed_src = p.speed_src.as_deref().and_then(SourceAddr::parse);
    s.speed_resolved = None;
    let parse = |o: &Option<String>| o.as_deref().and_then(SourceAddr::parse);
    for (i, tp) in p.tracks.iter().enumerate().take(TRACKS) {
        let t = &mut s.tracks[i];
        t.input = tp.input.clone();
        t.fader = tp.fader.clamp(0.0, 1.0);
        t.pan = tp.pan.clamp(-1.0, 1.0);
        t.armed = tp.armed;
        t.muted = tp.muted;
        t.reversed = tp.reversed;
        t.monitor = tp.monitor;
        t.srcs = [parse(&tp.fader_src), parse(&tp.pan_src)];
        t.resolved = Default::default();
        t.auto = tp.auto.clone();
        t.auto.sort_by_key(|(f, _)| *f);
    }
}

/// Write each non-empty track to ~/.config/los/tape/track_N.wav.
fn save_audio(s: &TapeState) {
    let dir = tape_dir();
    let _ = std::fs::create_dir_all(&dir);
    for (i, t) in s.tracks.iter().enumerate() {
        let path = dir.join(format!("track_{}.wav", i));
        let Some(audio) = t.audio.as_ref() else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if t.filled == 0 {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: s.rate as u32,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        if let Ok(mut wr) = hound::WavWriter::create(&path, spec) {
            let n = (t.filled as usize * 2).min(audio.len());
            for v in &audio[..n] {
                let _ = wr.write_sample(*v);
            }
            let _ = wr.finalize();
        }
    }
}

/// Load track WAVs back in (startup).
fn load_audio(s: &mut TapeState) {
    let dir = tape_dir();
    for i in 0..TRACKS {
        let path = dir.join(format!("track_{}.wav", i));
        let Ok(mut rd) = hound::WavReader::open(&path) else { continue };
        let mut buf = vec![0i16; (s.tape_len as usize) * 2];
        let mut n = 0usize;
        for sample in rd.samples::<i16>() {
            if n >= buf.len() {
                break;
            }
            buf[n] = sample.unwrap_or(0);
            n += 1;
        }
        s.tracks[i].filled = (n / 2) as u64;
        s.tracks[i].audio = Some(buf);
    }
}

// ── the engine ─────────────────────────────────────────────────────────────
//
// Clocked by the mixer's print bus (one block in = one block out, the
// device-rate heartbeat); falls back to self-pacing if the mixer is
// gone. Reads every armed claimed source each block, advances the
// playhead by speed × frames while the transport plays, records at 1×,
// plays back with fractional (varispeed) reads, and writes the deck's
// stereo output ring — which the console adopts as the Tape 0 strip.

fn audio_thread(shared: Arc<Mutex<TapeState>>, instance: usize) -> Result<()> {
    let out_name = format!("/los_audio_tape_{}", instance);
    let mut out_rb = AudioRingbuf::create(&out_name).context("creating output ringbuffer")?;
    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    manifest.register("tape", instance, Some(&out_name), 0)?;
    let modbus = ModulationBus::open().or_else(|_| ModulationBus::create()).ok();

    let slot_frames = out_rb.slot_frames() as usize;
    let slot_len = out_rb.slot_len();

    let mut transport = ShmTransport::open().ok();
    let rate_of = |t: &Option<ShmTransport>| {
        t.as_ref().map(|t| t.sample_rate() as f32).filter(|r| *r > 0.0).unwrap_or(FALLBACK_RATE)
    };
    let mut sample_rate = rate_of(&transport);
    {
        let mut s = shared.lock().unwrap();
        s.rate = sample_rate;
        s.tape_len = TAPE_SECS * sample_rate as u64;
        if s.loop_out > s.tape_len || s.loop_out == 0 {
            s.loop_out = s.tape_len;
        }
    }
    let mut slot_duration =
        Duration::from_nanos((slot_frames as f64 / sample_rate as f64 * 1e9) as u64);

    // the print bus (the mix) — also the engine's clock when alive
    let mut print_rb: Option<AudioRingbuf> = None;
    // claimed per-source inputs: (track, ring); each claim needs its own
    // manifest entry, so a handle rides along
    let mut claims: Vec<(usize, AudioRingbuf, Manifest)> = Vec::new();
    let mut claimed_for: [Option<String>; TRACKS] = Default::default();

    let mut print_block = vec![0.0_f32; slot_len];
    let mut src_blocks: Vec<Vec<f32>> = (0..TRACKS).map(|_| vec![0.0; slot_len]).collect();
    let mut out_block = vec![0.0_f32; slot_len];
    let mut scratch = vec![0.0_f32; slot_len];
    let mut speed_smooth = 1.0_f32;
    let mut blocks: u64 = 0;

    loop {
        // ── slow path: rates, bindings, input claims ──────────────────
        if blocks.is_multiple_of(64) {
            if transport.is_none() {
                transport = ShmTransport::open().ok();
            }
            let now_rate = rate_of(&transport);
            if (now_rate - sample_rate).abs() > 0.5 {
                sample_rate = now_rate;
                slot_duration =
                    Duration::from_nanos((slot_frames as f64 / sample_rate as f64 * 1e9) as u64);
                let mut s = shared.lock().unwrap();
                s.rate = sample_rate;
                s.tape_len = TAPE_SECS * sample_rate as u64;
            }
            if print_rb.is_none() {
                print_rb = AudioRingbuf::open("/los_mix_print").ok();
                if let Some(rb) = print_rb.as_mut() {
                    while rb.available() > 1 {
                        let _ = rb.read(&mut scratch);
                    }
                }
            }
            let entries = manifest.entries();
            {
                let mut s = shared.lock().unwrap();
                s.speed_resolved =
                    s.speed_src.as_ref().and_then(|a| routing::resolve(&entries, a));
                for t in s.tracks.iter_mut() {
                    for k in 0..2 {
                        t.resolved[k] =
                            t.srcs[k].as_ref().and_then(|a| routing::resolve(&entries, a));
                    }
                }
                // reconcile claims: armed tracks with a named source own
                // a claim; everything else lets go
                #[allow(clippy::needless_range_loop)] // claimed_for + s.tracks in lockstep
                for i in 0..TRACKS {
                    let want: Option<String> = if s.tracks[i].armed {
                        s.tracks[i].input.as_ref().and_then(|sel| {
                            let (m, inst) = sel.split_once('/')?;
                            let inst: usize = inst.parse().ok()?;
                            entries
                                .iter()
                                .find(|e| e.module_name == m && e.instance == inst)
                                .and_then(|e| e.audio_shm.clone())
                        })
                    } else {
                        None
                    };
                    if want != claimed_for[i] {
                        claims.retain(|(t, _, _)| *t != i);
                        if let Some(shm) = want.as_deref() {
                            if let (Ok(rb), Ok(mut mh)) =
                                (AudioRingbuf::open(shm), Manifest::open())
                            {
                                if mh.register("tapein", i, None, 0).is_ok() {
                                    mh.publish_input(Some(shm));
                                    claims.push((i, rb, mh));
                                }
                            }
                        }
                        claimed_for[i] = want;
                    }
                }
            }
        }

        // ── acquire this block's inputs (the print bus is the clock) ──
        let tick = Instant::now();
        let mut got_print = false;
        if let Some(rb) = print_rb.as_mut() {
            loop {
                if rb.read(&mut print_block).unwrap_or(false) {
                    got_print = true;
                    break;
                }
                if tick.elapsed() > Duration::from_millis(4) {
                    break;
                }
                thread::sleep(Duration::from_micros(200));
            }
        }
        if !got_print {
            print_block.iter_mut().for_each(|v| *v = 0.0);
        }
        for b in src_blocks.iter_mut() {
            b.iter_mut().for_each(|v| *v = 0.0);
        }
        for (track, rb, _) in claims.iter_mut() {
            // drain to the freshest block; sources free-run regardless
            let buf = &mut src_blocks[*track];
            while rb.read(scratch.as_mut_slice()).unwrap_or(false) {
                buf.copy_from_slice(&scratch);
            }
        }

        // ── the tape ───────────────────────────────────────────────────
        out_block.iter_mut().for_each(|v| *v = 0.0);
        {
            let mut s = shared.lock().unwrap();
            let playing = transport.as_ref().map(|t| t.playing()).unwrap_or(false);
            let bus = modbus.as_ref();
            let speed_target = s.speed_effective(bus).clamp(0.25, 2.0);
            s.speed_eff = speed_target;
            let tape_len = s.tape_len;
            let loop_on = s.loop_on;
            let (l_in, l_out) = (s.loop_in.min(tape_len), s.loop_out.min(tape_len).max(1));
            let at_speed_one = (speed_target - 1.0).abs() < 0.01;
            let rec = s.recording && playing && at_speed_one;

            // effective per-track fader/pan once per block
            let mut fader = [0.0f32; TRACKS];
            let mut pan = [0.0f32; TRACKS];
            for (i, t) in s.tracks.iter_mut().enumerate() {
                fader[i] = match (t.resolved[0], bus) {
                    (Some(ch), Some(bus)) => bus.get(ch).clamp(0.0, 1.0),
                    _ => t.fader,
                };
                pan[i] = match (t.resolved[1], bus) {
                    (Some(ch), Some(bus)) => bus.get(ch).clamp(-1.0, 1.0),
                    _ => t.pan,
                };
                t.eff = [fader[i], pan[i]];
            }

            let mut pos = s.pos;
            for f in 0..slot_frames {
                speed_smooth += (speed_target - speed_smooth) * 0.0005;
                let frame = pos as u64;

                for ti in 0..TRACKS {
                    let t = &mut s.tracks[ti];
                    // record: sum the input into the tape (sound-on-
                    // sound — the tape never erases; X clears)
                    if rec && t.armed && frame < tape_len {
                        let input = if t.input.is_some() {
                            (src_blocks[ti][2 * f], src_blocks[ti][2 * f + 1])
                        } else {
                            (print_block[2 * f], print_block[2 * f + 1])
                        };
                        let audio = t.audio.get_or_insert_with(|| {
                            vec![0i16; (tape_len as usize) * 2]
                        });
                        let idx = (frame as usize) * 2;
                        if idx + 1 < audio.len() {
                            let l = audio[idx] as f32 / 32767.0 + input.0;
                            let r = audio[idx + 1] as f32 / 32767.0 + input.1;
                            audio[idx] = (l.clamp(-1.0, 1.0) * 32767.0) as i16;
                            audio[idx + 1] = (r.clamp(-1.0, 1.0) * 32767.0) as i16;
                        }
                        t.filled = t.filled.max(frame + 1);
                    }

                    // playback: fractional read, reverse-aware
                    if !t.muted && t.filled > 0 && playing {
                        if let Some(audio) = t.audio.as_ref() {
                            let span = t.filled.max(1) as f64;
                            let p = if t.reversed { span - 1.0 - pos.min(span - 1.0) } else { pos };
                            if p >= 0.0 && p < span - 1.0 {
                                let i0 = p as usize;
                                let frac = (p - i0 as f64) as f32;
                                let idx = i0 * 2;
                                if idx + 3 < audio.len() {
                                    let l0 = audio[idx] as f32 / 32767.0;
                                    let r0 = audio[idx + 1] as f32 / 32767.0;
                                    let l1 = audio[idx + 2] as f32 / 32767.0;
                                    let r1 = audio[idx + 3] as f32 / 32767.0;
                                    // the lane replaces the fader when present
                                    let g = if t.auto.is_empty() {
                                        fader[ti]
                                    } else {
                                        t.lane_value(frame)
                                    };
                                    let (gl, gr) = crate::delay::dsp::pan_gains(pan[ti]);
                                    out_block[2 * f] += (l0 + (l1 - l0) * frac) * g * gl;
                                    out_block[2 * f + 1] += (r0 + (r1 - r0) * frac) * g * gr;
                                }
                            }
                        }
                    }

                    // input monitoring: armed + monitor passes the source
                    // through the deck (the mix is already audible on the
                    // console, so only claimed sources monitor here)
                    if t.armed && t.monitor && t.input.is_some() {
                        let g = fader[ti];
                        out_block[2 * f] += src_blocks[ti][2 * f] * g;
                        out_block[2 * f + 1] += src_blocks[ti][2 * f + 1] * g;
                    }
                }

                if playing {
                    pos += speed_smooth as f64;
                    if loop_on && pos >= l_out as f64 {
                        pos = l_in as f64;
                    }
                    if pos >= tape_len as f64 {
                        pos = if loop_on { l_in as f64 } else { 0.0 };
                    }
                }
            }
            s.pos = pos;
        }

        while out_rb.write(&out_block).is_err() {
            thread::sleep(Duration::from_micros(500));
        }

        blocks += 1;
        if !got_print {
            let elapsed = tick.elapsed();
            if elapsed < slot_duration {
                thread::sleep(slot_duration - elapsed);
            }
        }
    }
}

// ── offline renders: bounce & export ───────────────────────────────────────

/// Mix the unmuted tracks at 1× into a stereo f32 buffer (automation,
/// pans, reverse applied) over the filled extent.
fn render(s: &TapeState) -> (Vec<f32>, u64) {
    let frames =
        s.tracks.iter().filter(|t| !t.muted).map(|t| t.filled).max().unwrap_or(0);
    let mut out = vec![0.0f32; (frames as usize) * 2];
    for t in s.tracks.iter().filter(|t| !t.muted && t.filled > 0) {
        let Some(audio) = t.audio.as_ref() else { continue };
        let (gl, gr) = crate::delay::dsp::pan_gains(t.pan);
        for f in 0..t.filled.min(frames) {
            let src = if t.reversed { t.filled - 1 - f } else { f } as usize * 2;
            if src + 1 >= audio.len() {
                continue;
            }
            let g = t.lane_value(f);
            let g = if t.auto.is_empty() { t.fader } else { g };
            out[(f as usize) * 2] += audio[src] as f32 / 32767.0 * g * gl;
            out[(f as usize) * 2 + 1] += audio[src + 1] as f32 / 32767.0 * g * gr;
        }
    }
    (out, frames)
}

/// Export the tape to ~/Music/los/tape-<n>.wav. Returns the path.
fn export(s: &TapeState) -> Result<PathBuf> {
    let (mix, frames) = render(s);
    anyhow::ensure!(frames > 0, "the tape is empty");
    let dir = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
        .join("Music")
        .join("los");
    std::fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("tape-{}.wav", stamp));
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: s.rate as u32,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wr = hound::WavWriter::create(&path, spec)?;
    for v in &mix {
        wr.write_sample((v.clamp(-1.0, 1.0) * 32767.0) as i16)?;
    }
    wr.finalize()?;
    Ok(path)
}

/// Bounce the unmuted tracks into the first empty track, then mute the
/// sources. Returns the destination track.
fn bounce(s: &mut TapeState) -> Result<usize> {
    let dest = (0..TRACKS)
        .find(|i| s.tracks[*i].filled == 0)
        .ok_or_else(|| anyhow::anyhow!("no empty track to bounce into"))?;
    let (mix, frames) = render(s);
    anyhow::ensure!(frames > 0, "nothing to bounce");
    let mut audio = vec![0i16; (s.tape_len as usize) * 2];
    for (i, v) in mix.iter().enumerate().take(audio.len()) {
        audio[i] = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
    }
    for (i, t) in s.tracks.iter_mut().enumerate() {
        if i != dest && t.filled > 0 && !t.muted {
            t.muted = true;
        }
    }
    let t = &mut s.tracks[dest];
    t.audio = Some(audio);
    t.filled = frames;
    t.auto.clear();
    t.fader = 0.8;
    t.muted = false;
    t.reversed = false;
    Ok(dest)
}

// ── RAVE (optional helper) ─────────────────────────────────────────────────

fn rave_models() -> Vec<PathBuf> {
    let dir = state::los_dir().join("rave");
    let mut v: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "ts"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn rave_helper() -> Option<PathBuf> {
    // PATH first, then the repo's tools/ next to the binary's source
    if let Ok(out) = std::process::Command::new("which").arg("los-rave").output() {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
    }
    let local = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/los-rave");
    local.exists().then_some(local)
}

/// Kick a RAVE render of `track` through `model` (async; progress lands
/// in state.rave, the swapped take in the track on success).
fn rave_process(shared: Arc<Mutex<TapeState>>, track: usize, model: PathBuf) {
    let Some(helper) = rave_helper() else {
        shared.lock().unwrap().status =
            Some(String::from("RAVE: helper not found (tools/los-rave)"));
        return;
    };
    let (wav_in, rate, audio, filled) = {
        let s = shared.lock().unwrap();
        let t = &s.tracks[track];
        let Some(audio) = t.audio.clone() else {
            return;
        };
        (
            std::env::temp_dir().join(format!("los_rave_in_{}.wav", track)),
            s.rate as u32,
            audio,
            t.filled,
        )
    };
    let wav_out = std::env::temp_dir().join(format!("los_rave_out_{}.wav", track));
    // write the take
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    if let Ok(mut wr) = hound::WavWriter::create(&wav_in, spec) {
        for v in &audio[..(filled as usize * 2).min(audio.len())] {
            let _ = wr.write_sample(*v);
        }
        let _ = wr.finalize();
    }
    shared.lock().unwrap().rave = Some((track, 0.0));
    thread::spawn(move || {
        let ok = std::process::Command::new(&helper)
            .arg("process")
            .arg("--model")
            .arg(&model)
            .arg(&wav_in)
            .arg(&wav_out)
            .status()
            .map(|st| st.success())
            .unwrap_or(false);
        let mut s = shared.lock().unwrap();
        if ok {
            if let Ok(mut rd) = hound::WavReader::open(&wav_out) {
                let t = &mut s.tracks[track];
                t.backup = t.audio.take().map(|a| (a, t.filled));
                let mut buf = vec![0i16; (s.tape_len as usize) * 2];
                let mut n = 0usize;
                for sample in rd.samples::<i16>() {
                    if n >= buf.len() {
                        break;
                    }
                    buf[n] = sample.unwrap_or(0);
                    n += 1;
                }
                let t = &mut s.tracks[track];
                t.filled = (n / 2) as u64;
                t.audio = Some(buf);
                s.status = Some(format!("RAVE done → t{} (N restores the original)", track + 1));
            }
        } else {
            s.status = Some(String::from("RAVE failed (see los-rave output)"));
        }
        s.rave = None;
        let _ = std::fs::remove_file(&wav_in);
        let _ = std::fs::remove_file(&wav_out);
    });
}

// ── rendering ──────────────────────────────────────────────────────────────

fn fmt_time(frames: u64, rate: f32) -> String {
    let secs = frames as f32 / rate.max(1.0);
    format!("{}:{:04.1}", (secs / 60.0) as u32, secs % 60.0)
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    s: &mut TapeState,
    instance: usize,
    show_help: bool,
    overlay: Option<&str>,
    picker: Option<(Vec<String>, usize)>,
    entries: &[crate::shm::ManifestEntry],
) -> Result<()> {
    use crate::theme;

    terminal.draw(|f| {
        let area = f.area();
        let w = area.width as usize;
        let h = area.height as usize;
        let mut lines: Vec<Line> = Vec::new();

        // transport header: speed, position, loop, the red light
        let rec = if s.recording { "● REC" } else { "" };
        let ctx = format!(
            "{} · {:.2}× · {} / {}{}",
            instance,
            s.speed_eff,
            fmt_time(s.pos as u64, s.rate),
            fmt_time(s.tape_len, s.rate),
            if s.loop_on {
                format!(" · ⟲ {}–{}", fmt_time(s.loop_in, s.rate), fmt_time(s.loop_out, s.rate))
            } else {
                String::new()
            }
        );
        lines.push(theme::header("TAPE", &ctx, rec, w));

        // track lanes: a header row and a waveform row each
        let wave_w = w.saturating_sub(4).max(8);
        for (i, t) in s.tracks.iter_mut().enumerate() {
            let sel = i == s.selected;
            let mut spans: Vec<Span> = Vec::new();
            let name_style = if sel { theme::selected() } else { theme::chrome_hi() };
            spans.push(Span::styled(format!(" t{} ", i + 1), name_style));
            // arm / monitor / write / mute / reverse badges
            let badge = |on: bool, txt: &str, hot: Style, cold: Style| {
                Span::styled(format!("{} ", txt), if on { hot } else { cold })
            };
            spans.push(badge(t.armed, "●", theme::signal(theme::alert()), theme::dim()));
            spans.push(badge(
                t.monitor && t.input.is_some(),
                "MON",
                theme::signal(theme::cv()),
                theme::dim(),
            ));
            spans.push(badge(t.write_auto, "✎", theme::signal(theme::clock()), theme::dim()));
            spans.push(badge(t.muted, "M", theme::signal(theme::alert()), theme::dim()));
            spans.push(badge(t.reversed, "◀", theme::signal(theme::clock()), theme::dim()));
            // input · fader · pan cells (h/l walks them)
            let col = s.sel_col.min(TRACK_COLS.len() - 1);
            let cell = |on: bool, txt: String, bound: Option<&SourceAddr>| {
                let style = if sel && on {
                    theme::selected()
                } else if let Some(a) = bound {
                    theme::signal(routing::cable_color(entries, a))
                } else {
                    theme::value()
                };
                Span::styled(txt, style)
            };
            spans.push(cell(
                col == 0,
                format!(
                    " in:{} ",
                    t.input.as_deref().map(|x| x.replace('/', " ")).unwrap_or_else(|| "mix".into())
                ),
                None,
            ));
            let shown_f = if t.srcs[0].is_some() { t.eff[0] } else { t.fader };
            spans.push(cell(col == 1, format!(" lvl {:>3.0}% ", shown_f * 100.0), t.srcs[0].as_ref()));
            let shown_p = if t.srcs[1].is_some() { t.eff[1] } else { t.pan };
            let ptxt = if shown_p.abs() < 0.05 {
                String::from("·")
            } else if shown_p < 0.0 {
                format!("‹{:.0}", shown_p.abs() * 100.0)
            } else {
                format!("{:.0}›", shown_p * 100.0)
            };
            spans.push(cell(col == 2, format!(" pan {} ", ptxt), t.srcs[1].as_ref()));
            if !t.auto.is_empty() {
                spans.push(Span::styled(
                    format!(" ✎{} ", t.auto.len()),
                    theme::signal(theme::clock()),
                ));
            }
            if let Some((rt, p)) = s.rave {
                if rt == i {
                    spans.push(Span::styled(
                        format!(" RAVE {:.0}% ", p * 100.0),
                        theme::signal(theme::clock()),
                    ));
                }
            }
            lines.push(Line::from(spans));

            // the waveform lane (cached overview + playhead)
            if t.wave_for != (t.filled, wave_w) {
                t.wave_cache = wave_overview(t, wave_w, s.tape_len);
                t.wave_for = (t.filled, wave_w);
            }
            let head = ((s.pos / s.tape_len.max(1) as f64) * wave_w as f64) as usize;
            let mut wl: Vec<Span> = vec![Span::raw("  ")];
            for (x, &amp) in t.wave_cache.iter().enumerate() {
                if x == head.min(wave_w.saturating_sub(1)) {
                    wl.push(Span::styled(
                        "▮",
                        if s.recording && t.armed {
                            theme::signal(theme::alert())
                        } else {
                            theme::selected()
                        },
                    ));
                } else if amp <= 0.0 {
                    wl.push(Span::styled("·", theme::dim()));
                } else {
                    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
                    let idx = ((theme::meter_frac(amp) * 7.0) as usize).min(7);
                    let style = if t.muted {
                        theme::dim()
                    } else {
                        theme::signal(theme::audio())
                    };
                    wl.push(Span::styled(BARS[idx].to_string(), style));
                }
            }
            lines.push(Line::from(wl));
        }

        // GLOBAL: the speed row
        {
            let sel = s.selected == GLOBAL_STRIP;
            let bound = s.speed_src.is_some();
            let style = if sel {
                theme::selected()
            } else if bound {
                let hue = s
                    .speed_src
                    .as_ref()
                    .map(|a| routing::cable_color(entries, a))
                    .unwrap_or_else(theme::clock);
                theme::signal(hue)
            } else {
                theme::value()
            };
            let shown = if bound { s.speed_eff } else { s.speed };
            lines.push(Line::from(vec![
                Span::styled(" speed ", if sel { theme::selected() } else { theme::chrome() }),
                Span::styled(
                    format!("{}{:.2}×", if bound { theme::BIND } else { ' ' }, shown),
                    style,
                ),
                Span::styled(
                    "   r rec · a arm · i/o loop pts · L loop · w write · E export · B bounce",
                    theme::dim(),
                ),
            ]));
        }

        theme::anchor_bottom(&mut lines, h, 2);
        lines.push(theme::rule(w));
        lines.push(theme::status(
            if s.recording { "RECORD" } else { "NORMAL" },
            overlay.unwrap_or(""),
            "",
            w,
        ));
        f.render_widget(Paragraph::new(lines), area);

        if show_help {
            let help = Paragraph::new(vec![
                Line::from("━━━ TAPE · the record window ━━━"),
                Line::from(""),
                Line::from("  j/k        Select track (then GLOBAL) · h/l column"),
                Line::from("  K/J or =/- Adjust fader/pan/speed 1% (_/+ 5%)"),
                Line::from("  r          ● record (transport must roll at 1×)"),
                Line::from("  a / o      arm track / monitor its input"),
                Line::from("  m / v      mute / reverse track"),
                Line::from("  i / o*     set loop in/out at playhead (*on GLOBAL)"),
                Line::from("  L          loop on/off · gg/G ends"),
                Line::from("  w / W      write fader automation / clear lane"),
                Line::from("  X X        clear track (twice) · n RAVE · N restore"),
                Line::from("  E / B      export ~/Music/los · bounce to empty track"),
                Line::from("  @ / x      bind (input col: pick source) / unbind"),
                Line::from(""),
                Line::from("Tracks record the MIX unless armed to a source."),
                Line::from("Overdubs layer (sound-on-sound); the tape never"),
                Line::from("erases until you X. Speed is the Cortini knob."),
                Line::from(""),
                Line::from("  ? closes help"),
            ])
            .style(Style::default().fg(theme::ink()).bg(theme::bg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" TAPE ", theme::chrome_hi())),
            );
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
                    .title(Span::styled(" patch ", theme::chrome_hi())),
            );
            f.render_widget(list, r);
        }
    })?;
    Ok(())
}

/// Downsampled |peak| per column over the WHOLE tape (empty regions 0).
fn wave_overview(t: &Track, width: usize, tape_len: u64) -> Vec<f32> {
    let mut out = vec![0.0f32; width];
    let Some(audio) = t.audio.as_ref() else { return out };
    if t.filled == 0 {
        return out;
    }
    let frames_per_col = (tape_len as usize / width.max(1)).max(1);
    for (x, slot) in out.iter_mut().enumerate() {
        let start = x * frames_per_col;
        if start as u64 >= t.filled {
            break;
        }
        let end = ((x + 1) * frames_per_col).min(t.filled as usize);
        let mut peak = 0i16;
        // stride through the column's frames cheaply
        let step = ((end - start) / 64).max(1);
        let mut fidx = start;
        while fidx < end {
            let i = fidx * 2;
            if i + 1 < audio.len() {
                peak = peak.max(audio[i].abs()).max(audio[i + 1].abs());
            }
            fidx += step;
        }
        *slot = peak as f32 / 32767.0;
    }
    out
}

// ── entry point ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Picking {
    ModSource,
    Input,
    RaveModel,
}

pub fn run(instance: usize) -> Result<()> {
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("tape", instance);

    let shared = Arc::new(Mutex::new(TapeState::new()));
    if let Ok(p) = state::load_module_state::<state::TapeParams>("tape", instance) {
        apply_params(&mut shared.lock().unwrap(), &p);
    }
    load_audio(&mut shared.lock().unwrap());

    let audio_state = Arc::clone(&shared);
    let audio_builder =
        thread::Builder::new().name(String::from("tape-audio")).stack_size(4 * 1024 * 1024);
    let _ = audio_builder.spawn(move || {
        while let Err(e) = audio_thread(Arc::clone(&audio_state), instance) {
            let _ = std::fs::write(
                state::tmp_dir().join(format!("tape_{}.err", instance)),
                format!("{}", e),
            );
            eprintln!("[tape {}] audio thread error (retrying): {}", instance, e);
            thread::sleep(Duration::from_millis(500));
        }
    });

    for attempt in 0..20 {
        match enable_raw_mode() {
            Ok(()) => break,
            Err(e) if attempt < 19 => {
                let _ = e;
                thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(anyhow::anyhow!("enabling raw mode: {}", e)),
        }
    }
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let mut show_help = false;
    let mut count = crate::keys::Count::default();
    let mut pending_g = false;
    let mut history = crate::undo::ParamHistory::default();
    let mut ex = crate::excmd::ExLine::default();
    let mut picker = crate::picker::Picker::default();
    let mut picking = Picking::ModSource;
    let mut input_options: Vec<String> = Vec::new();
    let mut model_options: Vec<PathBuf> = Vec::new();
    let mut ex_msg: Option<String> = None;
    let mut patch_name: Option<String> = None;
    let mut should_quit = false;
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();
    let manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let mut ui_entries: Vec<crate::shm::ManifestEntry> = Vec::new();
    let mut ui_entries_at: Option<Instant> = None;
    let mut baseline =
        state::to_toml_string(&snapshot_params(&shared.lock().unwrap())).unwrap_or_default();
    // autosave: a killed pane must never eat a take. Track the filled
    // counts we last wrote; park the tape whenever they've moved and
    // either recording just stopped or 30 s have passed.
    let mut saved_filled = [0u64; TRACKS];
    let mut was_recording = false;
    let mut autosave_at = Instant::now();

    loop {
        if state::check_save_signal() {
            let s = shared.lock().unwrap();
            let _ = state::save_module_state("tape", instance, &snapshot_params(&s));
            save_audio(&s);
        }
        if state::check_reload_signal() {
            if let Ok(p) = state::load_module_state::<state::TapeParams>("tape", instance) {
                apply_params(&mut shared.lock().unwrap(), &p);
            }
        }
        if ui_entries_at.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
            ui_entries = manifest.entries();
            ui_entries_at = Some(Instant::now());
        }
        {
            let s = shared.lock().unwrap();
            let filled: [u64; TRACKS] = std::array::from_fn(|i| s.tracks[i].filled);
            let dirty = filled != saved_filled;
            let stopped_recording = was_recording && !s.recording;
            was_recording = s.recording;
            if dirty
                && !s.recording
                && (stopped_recording || autosave_at.elapsed() > Duration::from_secs(30))
            {
                let _ = state::save_module_state("tape", instance, &snapshot_params(&s));
                save_audio(&s);
                saved_filled = filled;
                autosave_at = Instant::now();
            }
        }

        let overlay = {
            let s = shared.lock().unwrap();
            if ex.is_active() {
                Some(ex.display())
            } else {
                ex_msg.clone().or_else(|| s.status.clone())
            }
        };
        {
            let mut s = shared.lock().unwrap();
            let picker_rows = picker.is_active().then(|| picker.rows());
            draw_ui(
                &mut terminal,
                &mut s,
                instance,
                show_help,
                overlay.as_deref(),
                picker_rows,
                &ui_entries,
            )?;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let ev = event::read()?;

        if let Event::Mouse(m) = ev {
            if picker.is_active() || ex.is_active() {
                continue;
            }
            match m.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let steps = if m.kind == MouseEventKind::ScrollUp { 1 } else { -1 };
                    use crate::undo::ParamUndo;
                    let mut s = shared.lock().unwrap();
                    let slot = slot_at(&s);
                    let old = s.get_param(slot);
                    adjust(&mut s, steps, false);
                    let new = s.get_param(slot);
                    if let (Some(old), Some(new)) = (old, new) {
                        history.record(slot, "Adjust", old, new);
                    }
                }
                MouseEventKind::Down(_) => {
                    // lanes are two rows each starting under the header;
                    // clicking a waveform also seeks the playhead
                    let mut s = shared.lock().unwrap();
                    let row = m.row as usize;
                    if row >= 1 {
                        let lane = (row - 1) / 2;
                        if lane < TRACKS {
                            s.selected = lane;
                            if (row - 1) % 2 == 1 {
                                let wave_w = (terminal.size().map(|r| r.width as usize))
                                    .unwrap_or(80)
                                    .saturating_sub(4)
                                    .max(8);
                                let x = (m.column as usize).saturating_sub(2).min(wave_w - 1);
                                s.pos = (x as f64 / wave_w as f64) * s.tape_len as f64;
                            }
                        } else {
                            s.selected = GLOBAL_STRIP;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let Event::Key(key) = ev else { continue };
        ex_msg = None;
        shared.lock().unwrap().status = None;

        if picker.is_active() {
            match picker.handle_key(key.code) {
                crate::picker::PickerEvent::Chosen(addr) => match picking {
                    Picking::ModSource => {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = shared.lock().unwrap();
                        if let Some(slot) = src_slot_at(&s) {
                            let old = s.get_param(slot);
                            s.set_param(
                                slot,
                                ParamValue::Src(addr.as_ref().map(|a| a.to_string())),
                            );
                            if let Some(old) = old {
                                history.record(
                                    slot,
                                    "Bind",
                                    old,
                                    ParamValue::Src(addr.map(|a| a.to_string())),
                                );
                            }
                        }
                    }
                    Picking::Input => {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = shared.lock().unwrap();
                        let sel = s.selected.min(TRACKS - 1);
                        let slot = sel * T_STRIDE + 4;
                        let old = s.get_param(slot);
                        s.tracks[sel].input = None; // — mix —
                        if let Some(old) = old {
                            history.record(slot, "Input", old, ParamValue::Src(None));
                        }
                    }
                    Picking::RaveModel => {}
                },
                crate::picker::PickerEvent::ChosenSpecial(i) => match picking {
                    Picking::Input => {
                        use crate::undo::{ParamUndo, ParamValue};
                        if let Some(sel_src) = input_options.get(i.saturating_sub(1)).cloned() {
                            let mut s = shared.lock().unwrap();
                            let sel = s.selected.min(TRACKS - 1);
                            let slot = sel * T_STRIDE + 4;
                            let old = s.get_param(slot);
                            s.tracks[sel].input = Some(sel_src.clone());
                            if let Some(old) = old {
                                history.record(slot, "Input", old, ParamValue::Src(Some(sel_src)));
                            }
                        }
                    }
                    Picking::RaveModel => {
                        if let Some(model) = model_options.get(i).cloned() {
                            let track = shared.lock().unwrap().selected.min(TRACKS - 1);
                            rave_process(Arc::clone(&shared), track, model);
                        }
                    }
                    Picking::ModSource => {}
                },
                _ => {}
            }
            continue;
        }
        if ex.is_active() {
            let completer =
                crate::excmd::standard_completer(crate::excmd::patch_names(&state::patches_dir()));
            if let crate::excmd::ExEvent::Submit(cmd) = ex.handle_key(key.code, &completer) {
                use crate::excmd::ExCommand;
                let params = snapshot_params(&shared.lock().unwrap());
                match cmd {
                    ExCommand::Write(name) => {
                        ex_msg = Some(
                            match crate::excmd::ex_write(name, &mut patch_name, &mut baseline, &params) {
                                Ok(m) | Err(m) => m,
                            },
                        );
                    }
                    ExCommand::Edit(name) => match state::load_patch::<state::TapeParams>(&name) {
                        Ok(p) => {
                            apply_params(&mut shared.lock().unwrap(), &p);
                            baseline =
                                state::to_toml_string(&snapshot_params(&shared.lock().unwrap()))
                                    .unwrap_or_default();
                            patch_name = Some(name.clone());
                            ex_msg = Some(format!("Loaded {}", name));
                        }
                        Err(e) => ex_msg = Some(e.to_string()),
                    },
                    ExCommand::Quit { force } => {
                        if !force && crate::excmd::is_dirty(&params, &baseline) {
                            ex_msg = Some(String::from(
                                "Unsaved changes (:q! to discard, :w <name> to save)",
                            ));
                        } else {
                            should_quit = true;
                        }
                    }
                    ExCommand::WriteQuit(name) => {
                        match crate::excmd::ex_write(name, &mut patch_name, &mut baseline, &params)
                        {
                            Ok(_) => should_quit = true,
                            Err(m) => ex_msg = Some(m),
                        }
                    }
                    ExCommand::Set(k, v) => {
                        let mut s = shared.lock().unwrap();
                        ex_msg = Some(ex_set(&mut s, &mut history, &k, &v));
                    }
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
        if !matches!(key.code, KeyCode::Char('X')) {
            shared.lock().unwrap().pending_clear = None;
        }
        if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
            let n = count.take();
            let mut s = shared.lock().unwrap();
            ex_msg = Some(crate::undo::history_status("Redo", n, || history.redo(&mut *s)));
            continue;
        }
        if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
            let s = shared.lock().unwrap();
            let _ = state::save_module_state("tape", instance, &snapshot_params(&s));
            save_audio(&s);
            ex_msg = Some(String::from("Saved (params + tape audio)"));
            continue;
        }

        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
            KeyCode::Char('j') | KeyCode::Down => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, n, TRACKS + 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.selected = crate::keys::cycle(s.selected, -n, TRACKS + 1);
            }
            KeyCode::Char('h') | KeyCode::Left => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.sel_col = crate::keys::cycle(s.sel_col, -n, TRACK_COLS.len());
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let n = count.take() as i32;
                let mut s = shared.lock().unwrap();
                s.sel_col = crate::keys::cycle(s.sel_col, n, TRACK_COLS.len());
            }
            KeyCode::Char(c @ ('-' | '=' | '_' | '+' | 'J' | 'K')) => {
                let n = count.take() as i32;
                let (steps, coarse) = match c {
                    '-' | 'J' => (-n, false),
                    '=' | 'K' => (n, false),
                    '_' => (-n, true),
                    _ => (n, true),
                };
                use crate::undo::ParamUndo;
                let mut s = shared.lock().unwrap();
                let slot = slot_at(&s);
                let old = s.get_param(slot);
                let writing = adjust(&mut s, steps, coarse);
                let new = s.get_param(slot);
                if let (Some(old), Some(new)) = (old, new) {
                    history.record(slot, "Adjust", old, new);
                }
                if writing {
                    ex_msg = Some(String::from("✎ writing"));
                }
            }
            // ● the record toggle — the point of the page
            KeyCode::Char('r') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                if !s.recording && (s.speed_eff - 1.0).abs() > 0.01 {
                    ex_msg = Some(String::from("tape must roll at 1.00× to record"));
                } else if !s.recording && !s.tracks.iter().any(|t| t.armed) {
                    ex_msg = Some(String::from("arm a track first (a)"));
                } else {
                    s.recording = !s.recording;
                }
            }
            KeyCode::Char('a') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                if s.selected < TRACKS {
                    let sel = s.selected;
                    s.tracks[sel].armed = !s.tracks[sel].armed;
                }
            }
            KeyCode::Char('o') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                if s.selected < TRACKS {
                    let sel = s.selected;
                    s.tracks[sel].monitor = !s.tracks[sel].monitor;
                } else {
                    // GLOBAL: set loop out at the playhead
                    s.loop_out = (s.pos as u64).max(s.loop_in + 1).min(s.tape_len);
                    ex_msg = Some(format!("loop out {}", fmt_time(s.loop_out, s.rate)));
                }
            }
            KeyCode::Char('i') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                s.loop_in = (s.pos as u64).min(s.tape_len.saturating_sub(1));
                if s.loop_out <= s.loop_in {
                    s.loop_out = s.tape_len;
                }
                ex_msg = Some(format!("loop in {}", fmt_time(s.loop_in, s.rate)));
            }
            KeyCode::Char('O') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                s.loop_out = (s.pos as u64).max(s.loop_in + 1).min(s.tape_len);
                ex_msg = Some(format!("loop out {}", fmt_time(s.loop_out, s.rate)));
            }
            KeyCode::Char('L') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                let old = s.get_param(GLOBAL_SLOT + 2);
                s.loop_on = !s.loop_on;
                if let Some(old) = old {
                    history.record(GLOBAL_SLOT + 2, "Loop", old, ParamValue::Bool(s.loop_on));
                }
            }
            KeyCode::Char('m') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                if s.selected < TRACKS {
                    let sel = s.selected;
                    let slot = sel * T_STRIDE + 2;
                    let old = s.get_param(slot);
                    s.tracks[sel].muted = !s.tracks[sel].muted;
                    if let Some(old) = old {
                        history.record(slot, "Mute", old, ParamValue::Bool(s.tracks[sel].muted));
                    }
                }
            }
            KeyCode::Char('v') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                if s.selected < TRACKS {
                    let sel = s.selected;
                    let slot = sel * T_STRIDE + 3;
                    let old = s.get_param(slot);
                    s.tracks[sel].reversed = !s.tracks[sel].reversed;
                    if let Some(old) = old {
                        history.record(slot, "Reverse", old, ParamValue::Bool(s.tracks[sel].reversed));
                    }
                }
            }
            KeyCode::Char('w') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                if s.selected < TRACKS {
                    let sel = s.selected;
                    s.tracks[sel].write_auto = !s.tracks[sel].write_auto;
                    ex_msg = Some(
                        if s.tracks[sel].write_auto {
                            String::from("✎ fader writes to the lane while the tape rolls")
                        } else {
                            String::from("write off")
                        },
                    );
                }
            }
            KeyCode::Char('W') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                if s.selected < TRACKS {
                    let sel = s.selected;
                    s.tracks[sel].auto.clear();
                    ex_msg = Some(String::from("lane cleared"));
                }
            }
            KeyCode::Char('X') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                if s.selected < TRACKS {
                    let sel = s.selected;
                    if s.pending_clear == Some(sel) {
                        s.tracks[sel].audio = None;
                        s.tracks[sel].filled = 0;
                        s.tracks[sel].auto.clear();
                        s.tracks[sel].backup = None;
                        s.pending_clear = None;
                        ex_msg = Some(format!("t{} erased", sel + 1));
                    } else {
                        s.pending_clear = Some(sel);
                        ex_msg = Some(format!("X again to erase t{}", sel + 1));
                    }
                }
            }
            // RAVE: process the take through a model; N restores
            KeyCode::Char('n') => {
                count.clear();
                let s = shared.lock().unwrap();
                if s.selected < TRACKS && s.tracks[s.selected].filled > 0 && s.rave.is_none() {
                    drop(s);
                    model_options = rave_models();
                    if model_options.is_empty() {
                        ex_msg = Some(String::from(
                            "no RAVE models in ~/.config/los/rave (see tools/los-rave)",
                        ));
                    } else {
                        let specials: Vec<String> = model_options
                            .iter()
                            .map(|p| {
                                p.file_stem().map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_default()
                            })
                            .collect();
                        picking = Picking::RaveModel;
                        picker.open_with(specials, Vec::new(), None, 0);
                    }
                }
            }
            KeyCode::Char('N') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                if s.selected < TRACKS {
                    let sel = s.selected;
                    if let Some((audio, filled)) = s.tracks[sel].backup.take() {
                        s.tracks[sel].audio = Some(audio);
                        s.tracks[sel].filled = filled;
                        ex_msg = Some(String::from("original take restored"));
                    }
                }
            }
            KeyCode::Char('E') => {
                count.clear();
                let s = shared.lock().unwrap();
                ex_msg = Some(match export(&s) {
                    Ok(p) => format!("exported {}", p.display()),
                    Err(e) => e.to_string(),
                });
            }
            KeyCode::Char('B') => {
                count.clear();
                let mut s = shared.lock().unwrap();
                ex_msg = Some(match bounce(&mut s) {
                    Ok(t) => format!("bounced → t{} (sources muted)", t + 1),
                    Err(e) => e.to_string(),
                });
            }
            KeyCode::Char('@') | KeyCode::Enter => {
                count.clear();
                let s = shared.lock().unwrap();
                let on_input = s.selected < TRACKS && s.current_col() == TrackCol::Input;
                if on_input {
                    let current = s.tracks[s.selected].input.clone();
                    drop(s);
                    let entries = Manifest::open().map(|m| m.entries()).unwrap_or_default();
                    input_options = entries
                        .iter()
                        .filter(|e| e.audio_shm.is_some())
                        .filter(|e| !matches!(e.module_name.as_str(), "tape" | "mix" | "send"))
                        .map(|e| format!("{}/{}", e.module_name, e.instance))
                        .collect();
                    input_options.sort();
                    let mut specials = vec![String::from("— mix —")];
                    specials.extend(input_options.iter().map(|o| o.replace('/', " ")));
                    let cur = current
                        .as_ref()
                        .and_then(|c| input_options.iter().position(|o| o == c))
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    picking = Picking::Input;
                    picker.open_with(specials, Vec::new(), None, cur);
                } else if src_slot_at(&s).is_some() {
                    let current = current_binding(&s);
                    drop(s);
                    let sources = Manifest::open()
                        .map(|m| routing::live_sources(&m.entries()))
                        .unwrap_or_default();
                    picking = Picking::ModSource;
                    picker.open(sources, current.as_ref());
                } else {
                    ex_msg = Some(String::from("not bindable"));
                }
            }
            KeyCode::Char('x') => {
                count.clear();
                use crate::undo::{ParamUndo, ParamValue};
                let mut s = shared.lock().unwrap();
                if let Some(slot) = src_slot_at(&s) {
                    let old = s.get_param(slot);
                    if !matches!(old, Some(ParamValue::Src(None))) {
                        s.set_param(slot, ParamValue::Src(None));
                        if let Some(old) = old {
                            history.record(slot, "Unbind", old, ParamValue::Src(None));
                        }
                    }
                }
            }
            KeyCode::Char('u') => {
                let n = count.take();
                let mut s = shared.lock().unwrap();
                ex_msg = Some(crate::undo::history_status("Undo", n, || history.undo(&mut *s)));
            }
            KeyCode::Char('g') => {
                count.clear();
                if pending_g {
                    pending_g = false;
                    shared.lock().unwrap().selected = 0;
                } else {
                    pending_g = true;
                }
            }
            KeyCode::Char('G') => {
                count.clear();
                shared.lock().unwrap().selected = GLOBAL_STRIP;
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
        if should_quit {
            break;
        }
    }

    // park the tape on the way out
    {
        let s = shared.lock().unwrap();
        let _ = state::save_module_state("tape", instance, &snapshot_params(&s));
        save_audio(&s);
    }
    crossterm::terminal::disable_raw_mode()?;
    execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    Ok(())
}

fn slot_at(s: &TapeState) -> usize {
    if s.selected == GLOBAL_STRIP {
        GLOBAL_SLOT
    } else {
        s.selected * T_STRIDE
            + match s.current_col() {
                TrackCol::Input => 4,
                TrackCol::Fader => 0,
                TrackCol::Pan => 1,
            }
    }
}

fn src_slot_at(s: &TapeState) -> Option<usize> {
    if s.selected == GLOBAL_STRIP {
        return Some(GLOBAL_SLOT + 1);
    }
    match s.current_col() {
        TrackCol::Fader => Some(s.selected * T_STRIDE + 5),
        TrackCol::Pan => Some(s.selected * T_STRIDE + 6),
        TrackCol::Input => None,
    }
}

fn current_binding(s: &TapeState) -> Option<SourceAddr> {
    if s.selected == GLOBAL_STRIP {
        return s.speed_src.clone();
    }
    match s.current_col() {
        TrackCol::Fader => s.tracks[s.selected].srcs[0].clone(),
        TrackCol::Pan => s.tracks[s.selected].srcs[1].clone(),
        TrackCol::Input => None,
    }
}

/// Returns true when the adjustment also wrote an automation point.
fn adjust(s: &mut TapeState, steps: i32, coarse: bool) -> bool {
    use crate::keys::step_f32;
    let u = |fine: f32, coarse_u: f32| if coarse { coarse_u } else { fine };
    if s.selected == GLOBAL_STRIP {
        s.speed = step_f32(s.speed, steps, u(0.01, 0.05), false, 0.25, 2.0);
        return false;
    }
    let pos = s.pos as u64;
    let sel = s.selected;
    let col = s.current_col();
    let t = &mut s.tracks[sel];
    match col {
        TrackCol::Fader => {
            t.fader = step_f32(t.fader, steps, u(0.01, 0.05), false, 0.0, 1.0);
            if t.write_auto {
                // sorted insert/replace at this frame
                match t.auto.binary_search_by_key(&pos, |(f, _)| *f) {
                    Ok(i) => t.auto[i].1 = t.fader,
                    Err(i) => t.auto.insert(i, (pos, t.fader)),
                }
                return true;
            }
        }
        TrackCol::Pan => t.pan = step_f32(t.pan, steps, u(0.02, 0.10), false, -1.0, 1.0),
        TrackCol::Input => {}
    }
    false
}

/// `:set speed 1.25 · :set loop on|off`.
fn ex_set(
    s: &mut TapeState,
    history: &mut crate::undo::ParamHistory,
    key: &str,
    value: &str,
) -> String {
    use crate::undo::ParamUndo;
    match key {
        "speed" => {
            let Ok(v) = value.trim_end_matches('x').trim_end_matches('×').parse::<f32>() else {
                return format!("speed: not a number: {}", value);
            };
            let old = s.get_param(GLOBAL_SLOT);
            s.speed = v.clamp(0.25, 2.0);
            if let (Some(old), Some(new)) = (old, s.get_param(GLOBAL_SLOT)) {
                history.record(GLOBAL_SLOT, "Set", old, new);
            }
            format!("speed = {:.2}×", s.speed)
        }
        "loop" => {
            s.loop_on = matches!(value, "on" | "true" | "1");
            format!("loop = {}", if s.loop_on { "on" } else { "off" })
        }
        _ => format!("Unknown setting: {} (speed loop)", key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_holds_last_value() {
        let mut t = Track::new();
        assert_eq!(t.lane_value(100), t.fader, "empty lane = the fader");
        t.auto = vec![(100, 0.2), (200, 0.9)];
        assert_eq!(t.lane_value(0), 0.2, "before the first point");
        assert_eq!(t.lane_value(100), 0.2);
        assert_eq!(t.lane_value(150), 0.2, "held between points");
        assert_eq!(t.lane_value(200), 0.9);
        assert_eq!(t.lane_value(5000), 0.9);
    }

    #[test]
    fn render_applies_fader_reverse_and_automation() {
        let mut s = TapeState::new();
        // t1: a 4-frame ramp at full fader
        let mut a = vec![0i16; 16];
        for f in 0..4 {
            a[f * 2] = (f as i16 + 1) * 1000;
            a[f * 2 + 1] = (f as i16 + 1) * 1000;
        }
        s.tracks[0].audio = Some(a.clone());
        s.tracks[0].filled = 4;
        s.tracks[0].fader = 1.0;
        s.tracks[0].pan = 0.0;
        let (mix, frames) = render(&s);
        assert_eq!(frames, 4);
        assert!(mix[0] > 0.0 && mix[6] > mix[0], "forward ramp");

        s.tracks[0].reversed = true;
        let (rev, _) = render(&s);
        assert!(rev[0] > rev[6], "reversed ramp descends");

        s.tracks[0].reversed = false;
        s.tracks[0].auto = vec![(0, 0.0), (2, 1.0)];
        let (auto, _) = render(&s);
        assert!(auto[0].abs() < 1e-6, "lane gates the first frames");
        assert!(auto[4] > 0.0, "lane opens at frame 2");

        // a muted track contributes nothing
        s.tracks[0].auto.clear();
        s.tracks[0].muted = true;
        let (m, frames) = render(&s);
        assert_eq!(frames, 0, "all muted = empty render");
        assert!(m.is_empty());
    }

    #[test]
    fn bounce_lands_in_first_empty_track_and_mutes_sources() {
        let mut s = TapeState::new();
        s.tape_len = 1000;
        let mut a = vec![0i16; 2000];
        a[0] = 10_000;
        a[1] = 10_000;
        s.tracks[0].audio = Some(a);
        s.tracks[0].filled = 500;
        let dest = bounce(&mut s).expect("bounce");
        assert_eq!(dest, 1, "first empty track");
        assert!(s.tracks[0].muted, "source muted");
        assert!(!s.tracks[1].muted);
        assert_eq!(s.tracks[1].filled, 500);
        assert!(s.tracks[1].audio.as_ref().unwrap()[0] > 0);
    }

    #[test]
    fn params_round_trip_through_toml() {
        let mut s = TapeState::new();
        s.speed = 0.5;
        s.loop_on = true;
        s.loop_in = 1234;
        s.loop_out = 9999;
        s.speed_src = SourceAddr::parse("envelope/3/ch1");
        s.tracks[2].input = Some("voice/1".into());
        s.tracks[2].fader = 0.4;
        s.tracks[2].reversed = true;
        s.tracks[2].auto = vec![(10, 0.1), (20, 0.9)];
        let toml = state::to_toml_string(&snapshot_params(&s)).expect("serializes");
        let back: state::TapeParams = toml::from_str(&toml).expect("parses");
        let mut s2 = TapeState::new();
        apply_params(&mut s2, &back);
        assert_eq!(s2.speed, 0.5);
        assert!(s2.loop_on);
        assert_eq!((s2.loop_in, s2.loop_out), (1234, 9999));
        assert_eq!(
            s2.speed_src.as_ref().map(|a| a.to_string()),
            Some("envelope/3/ch1".into())
        );
        assert_eq!(s2.tracks[2].input.as_deref(), Some("voice/1"));
        assert!(s2.tracks[2].reversed);
        assert_eq!(s2.tracks[2].auto, vec![(10, 0.1), (20, 0.9)]);
    }

    #[test]
    fn wave_overview_maps_filled_region() {
        let mut t = Track::new();
        let tape_len = 1000u64;
        let mut a = vec![0i16; 2000];
        // loud at the very start only
        a[0] = 30_000;
        a[1] = 30_000;
        t.audio = Some(a);
        t.filled = 500; // half the tape
        let w = wave_overview(&t, 10, tape_len);
        assert!(w[0] > 0.5, "first column carries the hit");
        assert_eq!(w[9], 0.0, "unfilled region is silent");
    }

    #[test]
    fn ex_set_and_adjust() {
        let mut s = TapeState::new();
        let mut h = crate::undo::ParamHistory::default();
        assert_eq!(ex_set(&mut s, &mut h, "speed", "0.5"), "speed = 0.50×");
        assert_eq!(ex_set(&mut s, &mut h, "loop", "on"), "loop = on");
        assert!(ex_set(&mut s, &mut h, "wow", "1").contains("Unknown"));
        // automation write on fader adjust
        s.selected = 0;
        s.sel_col = 1;
        s.tracks[0].write_auto = true;
        s.pos = 480.0;
        assert!(adjust(&mut s, -3, false));
        assert_eq!(s.tracks[0].auto.len(), 1);
        assert_eq!(s.tracks[0].auto[0].0, 480);
    }
}
