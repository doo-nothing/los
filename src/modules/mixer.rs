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

use crate::shm::{AudioRingbuf, Manifest, ShmTransport};
use crate::state;

const SHM_NAME: &str = "/los_mix_in";

#[derive(Clone)]
struct TrackState {
    name: String,
    level: f32,
    pan: f32,
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
    master: f32,
    master_meter: f32,
    /// Tape out: when armed, the audio callback streams the mixed master
    /// blocks here until the sample budget runs out (sender drop ends the
    /// writer thread, which finalizes the WAV and drops a .done marker).
    tape: Option<(std::sync::mpsc::Sender<Vec<f32>>, u64)>,
    selected: usize,
    scope_rb: Option<AudioRingbuf>,
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

    let stream = device
        .build_output_stream(
            &config.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut inner = state_cb.lock().unwrap();
                let mut peak = 0.0f32;
                let mut written = 0;

                while written + slot_len <= data.len() {
                    for sample in data[written..written + slot_len].iter_mut() {
                        *sample = 0.0;
                    }

                    let mut voice_buf = [0.0f32; 128];
                    let track_info: Vec<(f32, bool)> = inner.tracks.iter()
                        .map(|t| (t.level, t.mute))
                        .collect();

                    let mut track_peaks = vec![0.0f32; inner.audio_sources.len()];
                    for (i, source) in inner.audio_sources.iter_mut().enumerate() {
                        if i >= track_info.len() { break; }
                        if track_info[i].1 { continue; }
                        let track_level = track_info[i].0;
                        if let Ok(true) = source.ringbuf.read(&mut voice_buf[..slot_len]) {
                            for j in 0..slot_len {
                                let s = voice_buf[j] * track_level;
                                data[written + j] += s;
                                track_peaks[i] = track_peaks[i].max(s.abs());
                            }
                        }
                    }
                    for (i, &p) in track_peaks.iter().enumerate() {
                        if let Some(t) = inner.tracks.get_mut(i) {
                            // peak with ~decay so the meters breathe
                            t.meter = p.max(t.meter * 0.92);
                        }
                    }

                    for sample in data[written..written + slot_len].iter_mut() {
                        *sample *= inner.master;
                        peak = peak.max(sample.abs());
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
                inner.audio_sources.push(AudioSource {
                    shm_name: shm_name.clone(),
                    ringbuf,
                });
                inner.tracks.push(TrackState {
                    name: label,
                    level: 0.8,
                    pan: 0.0,
                    mute: false,
                    solo: false,
                    meter: 0.0,
                });
            }
        }

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
    state::MixerParams {
        master: Some(s.master),
        tracks: s.tracks.iter().map(|t| state::MixerTrackParam {
            level: t.level,
            pan: t.pan,
            mute: t.mute,
            solo: t.solo,
        }).collect(),
    }
}

fn apply_params(s: &mut MixerInner, params: &state::MixerParams) {
    if let Some(v) = params.master { s.master = v; }
    for (i, tp) in params.tracks.iter().enumerate().take(s.tracks.len()) {
        s.tracks[i].level = tp.level;
        s.tracks[i].pan = tp.pan;
        s.tracks[i].mute = tp.mute;
        s.tracks[i].solo = tp.solo;
    }
}

/// Undo slots: strip*4 + (0 level, 1 pan, 2 mute, 3 solo); master = MASTER_SLOT.
const MASTER_SLOT: usize = 1_000_000;

impl crate::undo::ParamUndo for MixerInner {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        if slot == MASTER_SLOT {
            return Some(V::F32(self.master));
        }
        let t = self.tracks.get(slot / 4)?;
        match slot % 4 {
            0 => Some(V::F32(t.level)),
            1 => Some(V::F32(t.pan)),
            2 => Some(V::Bool(t.mute)),
            _ => Some(V::Bool(t.solo)),
        }
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        if slot == MASTER_SLOT {
            if let V::F32(v) = value {
                self.master = v;
            }
            return;
        }
        let Some(t) = self.tracks.get_mut(slot / 4) else { return };
        match (slot % 4, value) {
            (0, V::F32(v)) => t.level = v,
            (1, V::F32(v)) => t.pan = v,
            (2, V::Bool(v)) => t.mute = v,
            (3, V::Bool(v)) => t.solo = v,
            _ => {}
        }
    }
}

/// The undo slot for the selected strip's param (`kind`: 0 level, 1 pan...).
fn strip_slot(s: &MixerInner, kind: usize) -> usize {
    if s.selected < s.tracks.len() {
        s.selected * 4 + kind
    } else {
        MASTER_SLOT
    }
}

/// Adjust the level of the selected strip (track or master, doctrine steps).
fn adjust_level(s: &mut MixerInner, steps: i32, coarse: bool) {
    use crate::keys::step_f32;
    let sel = s.selected;
    if sel < s.tracks.len() {
        s.tracks[sel].level = step_f32(s.tracks[sel].level, steps, 0.05, coarse, 0.0, 1.0);
    } else {
        s.master = step_f32(s.master, steps, 0.05, coarse, 0.0, 1.0);
    }
}

/// Pan the selected track (no-op on the master strip).
fn adjust_pan(s: &mut MixerInner, steps: i32) {
    let sel = s.selected;
    if sel < s.tracks.len() {
        s.tracks[sel].pan = crate::keys::step_f32(s.tracks[sel].pan, steps, 0.1, false, -1.0, 1.0);
    }
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

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    tracks: &[TrackState],
    master: f32,
    master_meter: f32,
    selected: usize,
    show_help: bool,
    overlay: Option<&str>,
    bpm: f32,
    playing: bool,
) -> Result<()> {
    use crate::theme;
    use ratatui::text::Span;

    terminal.draw(|f| {
        let area = f.area();
        let w = area.width as usize;
        let mut lines: Vec<Line> = Vec::new();

        let _ = (bpm, playing);
        lines.push(theme::header("MIX", &format!("{}ch", tracks.len()), "", w));

        let bar_w = theme::bar_width(w, 28);
        // channel strips as dense rows: name · meter · level gauge · pan · M/S
        for (i, t) in tracks.iter().enumerate() {
            let sel = i == selected;
            let name_style = if sel { theme::selected() } else { theme::chrome() };
            let mut spans: Vec<Span> = vec![Span::styled(format!(" {:<9}", t.name), name_style)];
            // live meter (AUDIO hue), drained when muted
            let m = if t.mute { 0.0 } else { t.meter };
            spans.push(Span::styled(
                format!("{} ", theme::meter_char(m)),
                theme::signal(theme::audio()),
            ));
            if t.mute {
                spans.push(Span::styled(theme::bar_str(t.level, None, bar_w), theme::dim()));
            } else {
                spans.extend(theme::bar(t.level, None, bar_w, theme::audio()));
            }
            spans.push(Span::styled(format!(" {:>3.0}%", t.level * 100.0), theme::value()));
            let pan = if t.pan.abs() < 0.05 {
                String::from(" ·")
            } else if t.pan < 0.0 {
                format!(" ‹{:.0}", t.pan.abs() * 100.0)
            } else {
                format!(" {:.0}›", t.pan * 100.0)
            };
            spans.push(Span::styled(pan, theme::dim()));
            if t.mute {
                spans.push(Span::styled(" M", theme::signal(theme::alert())));
            }
            if t.solo {
                spans.push(Span::styled(" S", theme::signal(theme::clock())));
            }
            lines.push(Line::from(spans));
        }

        lines.push(theme::rule(w));

        // master strip
        let sel = selected >= tracks.len();
        let name_style = if sel { theme::selected() } else { theme::chrome_hi() };
        let mut mspans = vec![
            Span::styled(format!(" {:<9}", "MASTER"), name_style),
            Span::styled(
                format!("{} ", theme::meter_char(master_meter)),
                theme::signal(if master_meter > 0.95 { theme::alert() } else { theme::audio() }),
            ),
        ];
        mspans.extend(theme::bar(master, None, bar_w, theme::audio()));
        mspans.extend(vec![
            Span::styled(format!(" {:>3.0}%", master * 100.0), theme::value()),
            Span::styled(
                format!("  {}", theme::AUDIO_GLYPH),
                theme::signal(theme::audio()),
            ),
        ]);
        lines.push(Line::from(mspans));

        theme::anchor_bottom(&mut lines, area.height as usize, 2);
        lines.push(theme::rule(w));
        lines.push(theme::status("NORMAL", overlay.unwrap_or(""), "", w));

        f.render_widget(Paragraph::new(lines), area);

        if show_help {
            let help_text = vec![
                Line::from("━━━ MIX ━━━"),
                Line::from(""),
                Line::from("  j/k       Select strip (counts, wraps)"),
                Line::from("  gg / G    First strip / master"),
                Line::from("  h/l       Level down/up (H/L ×10)"),
                Line::from("  < / >     Pan left/right"),
                Line::from("  m / s     Mute / solo"),
                Line::from("  u/^r      Undo / redo"),
                Line::from("  :w/:e/:q  Patches / quit"),
                Line::from("  space     Play/pause (global)"),
                Line::from(""),
                Line::from("Channels appear as modules register"),
                Line::from("audio in the manifest."),
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
        master: 0.8,
        master_meter: 0.0,
            tape: None,
        selected: 0,
        scope_rb: None,
    }));

    if let Ok(params) = state::load_module_state::<state::MixerParams>("mixer", 0) {
        let mut s = inner.lock().unwrap();
        if let Some(v) = params.master { s.master = v; }
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
    let mut ex_msg: Option<String> = None;
    let mut patch_name: Option<String> = None;
    let mut baseline = state::to_toml_string(&snapshot_params(&inner.lock().unwrap())).unwrap_or_default();
    let mut should_quit = false;
    // Global transport handle for Space = play/pause (lazily reopened)
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();

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

        let snapshot = {
            let s = inner.lock().unwrap();
            (s.tracks.clone(), s.master, s.master_meter, s.selected)
        };

        let overlay = if ex.is_active() {
            Some(ex.display())
        } else {
            ex_msg.clone()
        };
        let (bpm, playing) = transport_ui
            .as_ref()
            .map(|t| (t.bpm(), t.playing()))
            .unwrap_or((120.0, false));
        draw_ui(
            &mut terminal,
            &snapshot.0,
            snapshot.1,
            snapshot.2,
            snapshot.3,
            show_help,
            overlay.as_deref(),
            bpm,
            playing,
        )?;

        if event::poll(Duration::from_millis(50))? {
            let ev = event::read()?;
            if let Event::Mouse(m) = ev {
                use crossterm::event::{MouseButton, MouseEventKind};
                use crate::undo::{ParamUndo, ParamValue};
                let n = inner.lock().unwrap().tracks.len();
                // y: 1..=n strips; n+2 master
                let strip_at = |y: u16| -> Option<usize> {
                    let y = y as usize;
                    if (1..=n).contains(&y) {
                        Some(y - 1)
                    } else if y == n + 2 {
                        Some(n)
                    } else {
                        None
                    }
                };
                match m.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let steps = if m.kind == MouseEventKind::ScrollUp { 1 } else { -1 };
                        let mut s = inner.lock().unwrap();
                        let slot = strip_slot(&s, 0);
                        let old = s.get_param(slot);
                        adjust_level(&mut s, steps, false);
                        let new = s.get_param(slot);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(slot, "Level", old, new);
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(strip) = strip_at(m.row) {
                            inner.lock().unwrap().selected = strip;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(strip) = strip_at(m.row) {
                            let w = terminal.size().map(|r| r.width as usize).unwrap_or(60);
                            let bar_w = crate::theme::bar_width(w, 28);
                            let x = (m.column as usize).saturating_sub(12);
                            let v = (x as f32 / bar_w.saturating_sub(1).max(1) as f32).clamp(0.0, 1.0);
                            let mut s = inner.lock().unwrap();
                            s.selected = strip;
                            let slot = strip_slot(&s, 0);
                            let old = s.get_param(slot);
                            s.set_param(slot, ParamValue::F32(v));
                            if let Some(old) = old {
                                history.record(slot, "Slide", old, ParamValue::F32(v));
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if let Event::Key(key) = ev {
                ex_msg = None;
                if ex.is_active() {
                    let candidates = crate::excmd::patch_names(&state::patches_dir());
                    if let crate::excmd::ExEvent::Submit(cmd) = ex.handle_key(key.code, &candidates) {
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
                    KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
                    KeyCode::Char('j') | KeyCode::Down => {
                        select_strip(&mut inner.lock().unwrap(), count.take() as i32);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        select_strip(&mut inner.lock().unwrap(), -(count.take() as i32));
                    }
                    KeyCode::Char('h' | 'l' | 'H' | 'L') | KeyCode::Left | KeyCode::Right => {
                        let c = match key.code {
                            KeyCode::Char(c) => c,
                            KeyCode::Left => 'h',
                            _ => 'l',
                        };
                        let n = count.take() as i32;
                        let (steps, coarse) = match c {
                            'h' => (-n, false),
                            'l' => (n, false),
                            'H' => (-n, true),
                            _ => (n, true),
                        };
                        use crate::undo::ParamUndo;
                        let mut s = inner.lock().unwrap();
                        let slot = strip_slot(&s, 0);
                        let old = s.get_param(slot);
                        adjust_level(&mut s, steps, coarse);
                        let new = s.get_param(slot);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(slot, "Level", old, new);
                        }
                    }
                    KeyCode::Char('<' | ',' | '>' | '.') => {
                        let n = count.take() as i32;
                        let steps = if matches!(key.code, KeyCode::Char('<' | ',')) { -n } else { n };
                        use crate::undo::ParamUndo;
                        let mut s = inner.lock().unwrap();
                        let slot = strip_slot(&s, 1);
                        let old = s.get_param(slot);
                        adjust_pan(&mut s, steps);
                        let new = s.get_param(slot);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(slot, "Pan", old, new);
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
                            history.record(sel * 4 + kind, desc, ParamValue::Bool(was), ParamValue::Bool(!was));
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
                    level: 0.8,
                    pan: 0.0,
                    mute: false,
                    solo: false,
                    meter: 0.0,
                })
                .collect(),
            audio_sources: vec![],
            master: 0.8,
            master_meter: 0.0,
            tape: None,
            selected: 0,
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
    fn level_adjust_targets_selected_or_master() {
        let mut s = mixer_with_tracks(2);
        adjust_level(&mut s, -2, false);
        assert!((s.tracks[0].level - 0.7).abs() < 1e-6);
        s.selected = 2; // master
        adjust_level(&mut s, 1, true);
        assert_eq!(s.master, 1.0, "coarse step clamps at 1.0");
        assert!((s.tracks[1].level - 0.8).abs() < 1e-6, "tracks untouched");
    }

    #[test]
    fn pan_clamps_and_skips_master() {
        let mut s = mixer_with_tracks(1);
        adjust_pan(&mut s, -100);
        assert_eq!(s.tracks[0].pan, -1.0);
        s.selected = 1; // master has no pan
        adjust_pan(&mut s, 5);
        assert_eq!(s.tracks[0].pan, -1.0, "master pan is a no-op");
    }

    #[test]
    fn arm_request_round_trip() {
        assert_eq!(parse_arm("16\n/tmp/out tape.wav"), Some((16.0, "/tmp/out tape.wav".into())));
        assert_eq!(parse_arm("0\n/tmp/x.wav"), None, "zero seconds refused");
        assert_eq!(parse_arm("abc\n/tmp/x.wav"), None);
        assert_eq!(parse_arm("5"), None, "missing path refused");
    }
}
