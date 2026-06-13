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

use crate::routing::{self, SourceAddr};
use crate::shm::{AudioRingbuf, EventRingbuf, Manifest, ModulationBus, ShmTransport};
use crate::state;

#[derive(Clone)]
struct VoiceState {
    shape: f32,
    sub: f32,
    fm: f32,
    output: u8,
    freq: f32,
    gate: bool,
    level: f32,
    velocity: f32, // 0.0-1.0 from last note_on
    // Receiver-side bindings: each input names its modulation source
    // (docs/keybindings.md, routing.rs). None = use the local param value.
    shape_src: Option<SourceAddr>,
    sub_src: Option<SourceAddr>,
    fm_src: Option<SourceAddr>,
    lpg_src: Option<SourceAddr>,
    level_src: Option<SourceAddr>,
    /// Amplitude control (replaces the old hardwired modbus ch 0).
    /// None = 1.0 (an unpatched voice is audible); bound but unresolvable
    /// = 0.0 (a dead envelope silences the voice instead of droning it).
    amp_src: Option<SourceAddr>,
    /// Which sequencer track's notes this voice plays. None = all tracks.
    notes_src: Option<SourceAddr>,
    /// Low-pass-gate amount (0 = plain VCA, 1 = full vactrol-style LPG:
    /// the amp envelope closes a one-pole filter as it closes the gate, so
    /// amplitude and brightness fall together — the pluck sound).
    lpg: f32,
}

impl VoiceState {
    /// Fresh-session wiring for voice `i`: it plays sequencer track 2i+1
    /// and breathes through maths channel 2i+1 — odd tracks carry the
    /// default melodies and odd envelope channels their plucks, while the
    /// even ones stay free for patching. Instances past the wired range
    /// start unpatched (amp unbound = audible, notes = all tracks).
    fn default_for_instance(instance: usize) -> Self {
        let n = 2 * instance + 1;
        Self {
            amp_src: (n <= 6)
                .then(|| SourceAddr::parse(&format!("envelope/0/ch{n}")))
                .flatten(),
            notes_src: (n <= 8)
                .then(|| SourceAddr::parse(&format!("sequencer/0/t{n}")))
                .flatten(),
            // voice 1 ships as an audibly different instrument (sub-heavy,
            // darker): with identical patches, "which voice is playing?"
            // was unanswerable by ear and made routing look broken
            shape: if instance == 1 { 0.25 } else { 0.5 },
            sub: if instance == 1 { 0.7 } else { 0.0 },
            lpg: if instance == 1 { 0.35 } else { 0.0 },
            ..Default::default()
        }
    }
}

impl Default for VoiceState {
    fn default() -> Self {
        Self {
            shape: 0.5,
            sub: 0.0,
            fm: 0.0,
            output: 0,
            freq: 440.0,
            gate: false,
            level: 0.0,
            velocity: 0.0,
            shape_src: None,
            sub_src: None,
            fm_src: None,
            lpg_src: None,
            level_src: None,
            amp_src: SourceAddr::parse("envelope/0/ch1"),
            notes_src: None,
            lpg: 0.0,
        }
    }
}

/// The amplitude rule: unbound = 1.0 (audible, drone by choice); bound =
/// the resolved modbus value, or 0.0 when the binding is orphaned (source
/// module gone). Full volume is never the failure mode.
fn amp_level(bound: bool, resolved: Option<f32>) -> f32 {
    if bound {
        resolved.unwrap_or(0.0)
    } else {
        1.0
    }
}

fn voice_thread(
    state: Arc<Mutex<VoiceState>>,
    shutdown: std::sync::mpsc::Receiver<()>,
    instance: usize,
) -> Result<()> {
    let shm_name = format!("/los_audio_voice_{}", instance);
    let mut ringbuf = AudioRingbuf::open(&shm_name).or_else(|_| AudioRingbuf::create(&shm_name))?;

    let mut manifest = Manifest::open().or_else(|_| Manifest::create())?;
    let _ = manifest.register("voice", instance, Some(&shm_name), 0);

    let mut events = EventRingbuf::open_dynamic().ok();
    let mut modbus = ModulationBus::open()
        .or_else(|_| ModulationBus::create())
        .ok();

    let transport = ShmTransport::open().or_else(|_| ShmTransport::create(48000))?;

    let mut phase = 0.0f64;
    let mut sub_phase = 0.0f64;
    // per-sample smoothed amplitude (kills block-rate zipper on fast
    // envelopes) and the LPG one-pole filter state
    let mut level_smooth = 0.0f32;
    let mut lp_state = 0.0f32;

    // the mixer publishes the device's REAL rate once cpal answers; a
    // 48k assumption on a 44.1k device plays every pitch ~1.5 semitones
    // flat (samples are consumed at the device rate, not ours)
    let mut sample_rate = f64::from(transport.sample_rate()).max(1.0);
    let block_size = 64;

    // Resolved modbus channels for each binding; refreshed periodically so a
    // restarted source module (fresh channel claim) keeps working.
    let mut ch_shape: Option<usize> = None;
    let mut ch_sub: Option<usize> = None;
    let mut ch_fm: Option<usize> = None;
    let mut ch_lpg: Option<usize> = None;
    let mut ch_amp: Option<usize> = None;
    let mut note_filter: Option<u8> = None;
    let mut refresh_in = 0u32;

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        // Reconnect to shared resources if disconnected
        if events.is_none() {
            events = EventRingbuf::open_dynamic().ok();
        }
        if modbus.is_none() {
            modbus = ModulationBus::open()
                .or_else(|_| ModulationBus::create())
                .ok();
        }

        // Re-resolve bindings through the manifest (~every 256 blocks)
        if refresh_in == 0 {
            refresh_in = 256;
            // pick up the device rate (the mixer may publish it after we
            // start; a mixer respawn may change it)
            sample_rate = f64::from(transport.sample_rate()).max(1.0);
            let entries = manifest.entries();
            let s = state.lock().unwrap();
            ch_shape = s
                .shape_src
                .as_ref()
                .and_then(|a| routing::resolve(&entries, a));
            ch_sub = s
                .sub_src
                .as_ref()
                .and_then(|a| routing::resolve(&entries, a));
            ch_fm = s
                .fm_src
                .as_ref()
                .and_then(|a| routing::resolve(&entries, a));
            ch_lpg = s
                .lpg_src
                .as_ref()
                .and_then(|a| routing::resolve(&entries, a));
            ch_amp = s
                .amp_src
                .as_ref()
                .and_then(|a| routing::resolve(&entries, a));
            note_filter = s.notes_src.as_ref().and_then(routing::note_source_track);
            // publish what this voice listens to (the sequencer's
            // who's-listening markers read it back)
            let channels = [ch_shape, ch_sub, ch_fm, ch_lpg, ch_amp]
                .iter()
                .flatten()
                .filter(|&&c| c < 64)
                .fold(0u64, |m, &c| m | (1 << c));
            let notes = note_filter.filter(|&t| t < 8).map_or(0u8, |t| 1 << t);
            manifest.publish_consumes(channels, notes);
        }
        refresh_in -= 1;

        // Read events (note_on sets pitch + velocity, note_off sets gate=false).
        // With a notes binding, only events from that sequencer track apply.
        if let Some(ref mut ev) = events {
            while let Some(event) = ev.read_event() {
                if let Some(t) = note_filter {
                    if event.source != t {
                        continue;
                    }
                }
                let mut s = state.lock().unwrap();
                match event.event_type {
                    0 => {
                        // Note on
                        s.freq = event.value; // frequency from note
                        s.velocity = event.param as f32 / 127.0;
                        s.gate = true;
                    }
                    1 => {
                        // Note off
                        s.gate = false;
                        // velocity stays as last value for release tail
                    }
                    _ => {}
                }
            }
        }

        // Generate audio
        let s = state.lock().unwrap();
        let freq = s.freq as f64;
        let output_mode = s.output;
        // If gate is on but velocity hasn't been set (no note_on received),
        // default to full velocity so the voice produces sound immediately
        // on session load or when the sequencer hasn't started yet.
        let velocity = if s.gate && s.velocity < 0.001 {
            1.0
        } else {
            s.velocity
        };

        let chan_val = |ch: Option<usize>| -> Option<f32> {
            ch.and_then(|c| modbus.as_ref().map(|m| m.get(c)))
        };

        // a plugged cable replaces the knob (los-wide convention);
        // max/min not clamp — NaN from a stale channel must die here
        #[allow(clippy::manual_clamp)]
        let lpg = match chan_val(ch_lpg) {
            Some(v) if s.lpg_src.is_some() => v.max(0.0).min(1.0),
            _ => s.lpg,
        };

        // Amplitude: unbound -> 1.0 (a drone, by explicit choice). Bound ->
        // the source owns the level; if the binding can't resolve (envelope
        // removed or dead) go SILENT, not full-volume — a vanished envelope
        // must never turn the voice into a drone.
        let amp = amp_level(s.amp_src.is_some(), chan_val(ch_amp));

        let shape = chan_val(ch_shape).unwrap_or(s.shape).clamp(0.0, 1.0);
        let sub_mix = chan_val(ch_sub).unwrap_or(s.sub).clamp(0.0, 1.0);
        let fm_amount = chan_val(ch_fm).unwrap_or(s.fm).clamp(0.0, 1.0);

        // Final amplitude: amp source (usually an envelope) × step velocity
        let level = amp * velocity;

        let mut block = vec![0.0f32; block_size * 2];

        for i in 0..block_size {
            // FM
            let fm_mod = (phase * fm_amount as f64 * 2.0 * std::f64::consts::PI).sin() * 0.1;

            // Main oscillator with shape morphing
            let main_phase = (phase + fm_mod).fract();
            let sine = (main_phase * 2.0 * std::f64::consts::PI).sin() as f32;
            let saw = (main_phase * 2.0 - 1.0) as f32;
            let square = if main_phase < 0.5 { 1.0f32 } else { -1.0f32 };

            let main = if shape < 0.5 {
                sine * (1.0 - shape * 2.0) + saw * (shape * 2.0)
            } else {
                saw * (1.0 - (shape - 0.5) * 2.0) + square * ((shape - 0.5) * 2.0)
            };

            // Sub oscillator (square, one octave down)
            let sub = if sub_phase < 0.5 { 1.0f32 } else { -1.0f32 };

            // Mix
            let sample = match output_mode {
                0 => main,
                1 => main * (1.0 - sub_mix) + sub * sub_mix,
                _ => main * (1.0 - sub_mix) + sub * sub_mix * 0.5,
            };

            // ~0.7ms amplitude smoothing: the amp source updates at block
            // rate; without this, fast envelope edges arrive as steps
            level_smooth += (level - level_smooth) * 0.1;

            let output = if lpg > 0.0 {
                // Low-pass gate: cutoff tracks the (smoothed) amp level —
                // 25Hz closed to ~12kHz open — and the VCA leans on the
                // filter (gain ~ sqrt(level)) like a vactrol LPG.
                let fc = lpg_cutoff(level_smooth);
                let g = 1.0 - (-2.0 * std::f32::consts::PI * fc / sample_rate as f32).exp();
                lp_state += (sample - lp_state) * g;
                let plain = sample * level_smooth;
                let gated = lp_state * level_smooth.max(0.0).sqrt();
                (plain + (gated - plain) * lpg) * 0.5
            } else {
                sample * level_smooth * 0.5
            };
            block[i * 2] = output;
            block[i * 2 + 1] = output;

            phase = (phase + freq / sample_rate).fract();
            sub_phase = (sub_phase + freq / (sample_rate * 2.0)).fract();
        }

        drop(s);

        // Update level meter for TUI
        {
            let mut s = state.lock().unwrap();
            s.level = level;
        }

        // Write to ringbuffer — retry when full, don't drop blocks. A
        // full ring holds ~21 ms of audio and the mixer frees a slot
        // every ~1.3 ms, so a short sleep beats burning the core on
        // yield (this loop used to pin a CPU per voice).
        loop {
            match ringbuf.write(&block) {
                Ok(()) => break,
                Err(_) => {
                    std::thread::sleep(Duration::from_micros(500));
                }
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &VoiceState,
    selected: usize,
    show_help: bool,
    overlay: Option<&str>,
    picker: Option<(Vec<String>, usize)>,
    ghosts: &[Option<f32>; 3],
    entries: &[crate::shm::ManifestEntry],
    picker_colors: &[Option<ratatui::style::Color>],
    instance: usize,
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
        lines.push(theme::header("VOICE", &instance.to_string(), "", w));

        let label = |row: usize, name: &str| -> Span<'static> {
            if row == selected {
                Span::styled(format!(" {:<6}", name), theme::selected())
            } else {
                Span::styled(format!(" {:<6}", name), theme::chrome())
            }
        };

        let bar_w = theme::bar_width(w, 24);
        // value gauges with mod ghosts (§5)
        let value_rows = [
            (0usize, "shape", state.shape, &state.shape_src, ghosts[0]),
            (1, "sub", state.sub, &state.sub_src, ghosts[1]),
            (2, "fm", state.fm, &state.fm_src, ghosts[2]),
        ];
        for (row, name, set, src, ghost) in value_rows {
            let mut spans = vec![label(row, name)];
            let hue = match src {
                Some(a) => crate::routing::cable_color(entries, a),
                None => theme::amber(),
            };
            spans.extend(theme::bar(set, ghost, bar_w, hue));
            spans.push(Span::styled(format!(" {:.2}", set), theme::value()));
            if let Some(a) = src {
                spans.push(Span::styled(
                    format!(" {}{}", theme::BIND, a.output),
                    theme::signal(crate::routing::cable_color(entries, a)),
                ));
            }
            lines.push(Line::from(spans));
        }

        // output mode: every option visible, h/l slides the block
        let mut out_spans = vec![label(3, "out")];
        out_spans.extend(theme::segments(
            &["main", "main+sub", "mix"],
            state.output as usize,
        ));
        lines.push(Line::from(out_spans));

        // binding rows (CV hue — these ARE the patch cables)
        lines.push(Line::from(vec![
            label(4, "amp"),
            match &state.amp_src {
                Some(a) if crate::routing::resolve(entries, a).is_none() => Span::styled(
                    format!("{}{} ✗ offline = silent", theme::BIND, a),
                    theme::flash(theme::note()),
                ),
                Some(a) => Span::styled(
                    format!("{}{}", theme::BIND, a),
                    theme::signal(crate::routing::cable_color(entries, a)),
                ),
                None => Span::styled("unbound = 1.0".to_string(), theme::dim()),
            },
        ]));
        lines.push(Line::from(vec![
            label(5, "notes"),
            match &state.notes_src {
                Some(a) => Span::styled(
                    format!("{}{}", theme::BIND, a),
                    theme::signal(crate::routing::cable_color(entries, a)),
                ),
                None => Span::styled("all tracks".to_string(), theme::dim()),
            },
        ]));

        // LPG
        let mut lpg_spans = vec![label(6, "lpg")];
        lpg_spans.extend(theme::bar(state.lpg, None, bar_w, theme::audio()));
        lpg_spans.extend(vec![
            Span::styled(format!(" {:.2}", state.lpg), theme::value()),
            Span::styled(
                if state.lpg > 0.0 { " vactrol" } else { "" }.to_string(),
                theme::signal(theme::audio()),
            ),
        ]);
        lines.push(Line::from(lpg_spans));

        theme::anchor_bottom(&mut lines, area.height as usize, 4);
        lines.push(theme::rule(w));

        // live output line (AUDIO hue)
        let gate = if state.gate {
            theme::GATE_HI
        } else {
            theme::GATE_LO
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", theme::AUDIO_GLYPH),
                theme::signal(theme::audio()),
            ),
            Span::styled(
                (0..8)
                    .map(|i| theme::meter_char((state.level - i as f32 * 0.06).clamp(0.0, 1.0)))
                    .collect::<String>(),
                theme::signal(theme::audio()),
            ),
            Span::styled(
                format!(
                    "  {:.0}Hz {} vel {:.0}%",
                    state.freq,
                    gate,
                    state.velocity * 100.0
                ),
                theme::dim(),
            ),
        ]));

        lines.push(theme::rule(w));
        lines.push(theme::status("NORMAL", overlay.unwrap_or(""), "", w));

        f.render_widget(Paragraph::new(lines), area);

        if show_help {
            let help_text = vec![
                Line::from("━━━ VOICE ━━━"),
                Line::from(""),
                Line::from("  j/k h/l    Row / adjust (H/L ×10, counts)"),
                Line::from("  gg / G     First / last row"),
                Line::from("  @          Bind row to a source"),
                Line::from("  amp row    Amplitude source (env ch1)"),
                Line::from("  notes row  Which seq track to play"),
                Line::from("  lpg row    0=VCA … 1=vactrol low-pass gate"),
                Line::from("  u/^r       Undo / redo (counts)"),
                Line::from("  :w/:e/:q   Patches / quit"),
                Line::from("  space      Play/pause (global)"),
                Line::from(""),
                Line::from("  ? closes help"),
            ];
            let help = Paragraph::new(help_text)
                .style(Style::default().fg(theme::ink()).bg(theme::bg()))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(theme::chrome())
                        .title(Span::styled(" VOICE ", theme::chrome_hi())),
                );
            f.render_widget(help, area);
        }

        if let Some((rows, sel)) = picker {
            let h = (rows.len() as u16 + 2).min(area.height);
            let pw = rows.iter().map(|r| r.len()).max().unwrap_or(10).max(20) as u16 + 4;
            let r = ratatui::layout::Rect::new(
                (area.width.saturating_sub(pw)) / 2,
                (area.height.saturating_sub(h)) / 2,
                pw.min(area.width),
                h,
            );
            f.render_widget(ratatui::widgets::Clear, r);
            let items: Vec<ratatui::widgets::ListItem> = rows
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    let style = if i == sel {
                        theme::selected()
                    } else if let Some(Some(c)) = picker_colors.get(i) {
                        theme::signal(*c)
                    } else {
                        theme::value()
                    };
                    ratatui::widgets::ListItem::new(row.clone()).style(style)
                })
                .collect();
            let list = ratatui::widgets::List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::chrome())
                    .title(Span::styled(" bind source ", theme::chrome_hi())),
            );
            f.render_widget(list, r);
        }
    })?;

    Ok(())
}

/// LPG cutoff tracking: amp level 0..1 → 25Hz (closed) to ~12kHz (open),
/// exponential like a vactrol's resistance curve.
fn lpg_cutoff(level: f32) -> f32 {
    25.0 * (480.0f32).powf(level.clamp(0.0, 1.0))
}

fn snapshot_params(s: &VoiceState) -> state::VoiceParams {
    state::VoiceParams {
        format: state::STATE_FORMAT,
        shape: Some(s.shape),
        sub: Some(s.sub),
        fm: Some(s.fm),
        output: Some(s.output),
        freq: Some(s.freq),
        gate: Some(s.gate),
        level: Some(s.level),
        velocity: Some(s.velocity),
        shape_src: s.shape_src.as_ref().map(|a| a.to_string()),
        sub_src: s.sub_src.as_ref().map(|a| a.to_string()),
        fm_src: s.fm_src.as_ref().map(|a| a.to_string()),
        lpg_src: s.lpg_src.as_ref().map(|a| a.to_string()),
        level_src: s.level_src.as_ref().map(|a| a.to_string()),
        amp_src: s.amp_src.as_ref().map(|a| a.to_string()),
        notes_src: s.notes_src.as_ref().map(|a| a.to_string()),
        lpg: Some(s.lpg),
    }
}

fn apply_params(s: &mut VoiceState, params: &state::VoiceParams) {
    if let Some(v) = params.shape {
        s.shape = v;
    }
    if let Some(v) = params.sub {
        s.sub = v;
    }
    if let Some(v) = params.fm {
        s.fm = v;
    }
    if let Some(v) = params.output {
        s.output = v;
    }
    if let Some(v) = params.freq {
        s.freq = v;
    }
    if let Some(v) = params.gate {
        s.gate = v;
    }
    if let Some(v) = params.level {
        s.level = v;
    }
    if let Some(v) = params.velocity {
        s.velocity = v;
    }
    if let Some(v) = params.lpg {
        s.lpg = v;
    }
    // Binding fields only exist in format-2 files. An old file simply lacks
    // them — keep the defaults (e.g. amp -> envelope/0/ch1) rather than
    // unbinding everything.
    if params.format >= state::STATE_FORMAT {
        s.shape_src = params.shape_src.as_deref().and_then(SourceAddr::parse);
        s.sub_src = params.sub_src.as_deref().and_then(SourceAddr::parse);
        s.fm_src = params.fm_src.as_deref().and_then(SourceAddr::parse);
    s.lpg_src = params.lpg_src.as_deref().and_then(SourceAddr::parse);
        s.level_src = params.level_src.as_deref().and_then(SourceAddr::parse);
        s.amp_src = params.amp_src.as_deref().and_then(SourceAddr::parse);
        s.notes_src = params.notes_src.as_deref().and_then(SourceAddr::parse);
    }
}

/// The binding slot for each param row (rows 0-2 modulate values; 4-5 are
/// binding-only rows).
fn row_binding(s: &VoiceState, row: usize) -> Option<&Option<SourceAddr>> {
    match row {
        0 => Some(&s.shape_src),
        1 => Some(&s.sub_src),
        2 => Some(&s.fm_src),
        6 => Some(&s.lpg_src),
        4 => Some(&s.amp_src),
        5 => Some(&s.notes_src),
        _ => None, // output row has no binding
    }
}

fn set_row_binding(s: &mut VoiceState, row: usize, addr: Option<SourceAddr>) {
    match row {
        0 => s.shape_src = addr,
        1 => s.sub_src = addr,
        2 => s.fm_src = addr,
        6 => s.lpg_src = addr,
        4 => s.amp_src = addr,
        5 => s.notes_src = addr,
        _ => {}
    }
}

/// Undo slots: 0–3 = row values (shape/sub/fm/output); 10+row = bindings.
const BIND_SLOT: usize = 10;

impl crate::undo::ParamUndo for VoiceState {
    fn get_param(&self, slot: usize) -> Option<crate::undo::ParamValue> {
        use crate::undo::ParamValue as V;
        match slot {
            0 => Some(V::F32(self.shape)),
            1 => Some(V::F32(self.sub)),
            2 => Some(V::F32(self.fm)),
            3 => Some(V::U8(self.output)),
            6 => Some(V::F32(self.lpg)),
            s if s >= BIND_SLOT => {
                row_binding(self, s - BIND_SLOT).map(|b| V::Src(b.as_ref().map(|a| a.to_string())))
            }
            _ => None,
        }
    }

    fn set_param(&mut self, slot: usize, value: crate::undo::ParamValue) {
        use crate::undo::ParamValue as V;
        match (slot, value) {
            (0, V::F32(v)) => self.shape = v,
            (1, V::F32(v)) => self.sub = v,
            (2, V::F32(v)) => self.fm = v,
            (3, V::U8(v)) => self.output = v,
            (6, V::F32(v)) => self.lpg = v,
            (s, V::Src(a)) if s >= BIND_SLOT => {
                set_row_binding(
                    self,
                    s - BIND_SLOT,
                    a.as_deref().and_then(SourceAddr::parse),
                );
            }
            _ => {}
        }
    }
}

const NUM_ROWS: usize = 7; // shape, sub, fm, output, amp, notes, lpg

/// Adjust a param row by `steps` (doctrine: h/l fine, H/L coarse ×10).
fn adjust(s: &mut VoiceState, row: usize, steps: i32, coarse: bool) {
    use crate::keys::{cycle, step_f32};
    match row {
        0 => s.shape = step_f32(s.shape, steps, 0.05, coarse, 0.0, 1.0),
        1 => s.sub = step_f32(s.sub, steps, 0.05, coarse, 0.0, 1.0),
        2 => s.fm = step_f32(s.fm, steps, 0.05, coarse, 0.0, 1.0),
        3 => s.output = cycle(s.output as usize, steps, 3) as u8,
        6 => s.lpg = step_f32(s.lpg, steps, 0.05, coarse, 0.0, 1.0),
        _ => {}
    }
}

pub fn run(instance: usize) -> Result<()> {
    // Initialize terminal with retry logic (handles tmux PTY race)
    state::setup_save_signal();
    state::setup_reload_signal();
    state::write_pid_file("voice", instance);
    for attempt in 0..20 {
        match enable_raw_mode() {
            Ok(()) => break,
            Err(e) => {
                if attempt < 19 {
                    std::thread::sleep(Duration::from_millis(200));
                } else {
                    return Err(anyhow::anyhow!(
                        "Failed to enable raw mode after 20 attempts: {}",
                        e
                    ));
                }
            }
        }
    }
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let state = Arc::new(Mutex::new(VoiceState::default_for_instance(instance)));

    // Load saved state if available
    if let Ok(params) = state::load_module_state::<state::VoiceParams>("voice", instance) {
        apply_params(&mut state.lock().unwrap(), &params);
    }

    let state_clone = Arc::clone(&state);

    let (_tx, rx) = std::sync::mpsc::channel();

    let _voice_handle = std::thread::spawn(move || {
        if let Err(e) = voice_thread(state_clone, rx, instance) {
            eprintln!("Voice thread error: {}", e);
        }
    });

    let mut selected = 0usize;
    let mut show_help = false;
    let mut count = crate::keys::Count::default();
    // Global transport handle for Space = play/pause (lazily reopened)
    let mut transport_ui: Option<ShmTransport> = ShmTransport::open().ok();
    let mut picker = crate::picker::Picker::default();
    let mut history = crate::undo::ParamHistory::default();
    let mut pending_g = false;
    let mut ex = crate::excmd::ExLine::default();
    let mut ex_msg: Option<String> = None;
    let mut patch_name: Option<String> = None;
    let mut baseline =
        state::to_toml_string(&snapshot_params(&state.lock().unwrap())).unwrap_or_default();
    let mut should_quit = false;
    // Live modbus reads for gauge ghosts (§5)
    let mut ui_modbus = ModulationBus::open().ok();
    let mut ui_entries: Vec<crate::shm::ManifestEntry> = Vec::new();
    let mut ui_refresh = 0u32;

    loop {
        // Check for save-on-signal
        if state::check_save_signal() {
            let params = snapshot_params(&state.lock().unwrap());
            let _ = state::save_module_state("voice", instance, &params);
        }

        // Check for reload-on-signal
        if state::check_reload_signal() {
            if let Ok(params) = state::load_module_state::<state::VoiceParams>("voice", instance) {
                apply_params(&mut state.lock().unwrap(), &params);
            }
        }

        let current_state = state.lock().unwrap().clone();
        if ui_refresh == 0 {
            ui_refresh = 40;
            ui_entries = Manifest::open().map(|m| m.entries()).unwrap_or_default();
            if ui_modbus.is_none() {
                ui_modbus = ModulationBus::open().ok();
            }
        }
        ui_refresh -= 1;
        let live = |src: &Option<SourceAddr>| -> Option<f32> {
            src.as_ref()
                .and_then(|a| crate::routing::resolve(&ui_entries, a))
                .and_then(|ch| ui_modbus.as_ref().map(|m| m.get(ch)))
        };
        let ghosts = [
            live(&current_state.shape_src),
            live(&current_state.sub_src),
            live(&current_state.fm_src),
        ];
        let (bpm, playing) = transport_ui
            .as_ref()
            .map(|t| (t.bpm(), t.playing()))
            .unwrap_or((120.0, false));
        let overlay = if ex.is_active() {
            Some(ex.display())
        } else {
            ex_msg.clone()
        };
        let picker_rows = if picker.is_active() {
            Some(picker.rows())
        } else {
            None
        };
        let picker_colors: Vec<Option<ratatui::style::Color>> = if picker.is_active() {
            picker
                .row_sources()
                .iter()
                .map(|s| s.map(|a| crate::routing::cable_color(&ui_entries, a)))
                .collect()
        } else {
            Vec::new()
        };
        draw_ui(
            &mut terminal,
            &current_state,
            selected,
            show_help,
            overlay.as_deref(),
            picker_rows,
            &ghosts,
            &ui_entries,
            &picker_colors,
            instance,
            bpm,
            playing,
        )?;

        if event::poll(Duration::from_millis(50))? {
            let ev = event::read()?;
            if let Event::Mouse(m) = ev {
                use crate::undo::{ParamUndo, ParamValue};
                use crossterm::event::{MouseButton, MouseEventKind};
                // rows 1..=7 are shape/sub/fm/out/amp/notes/lpg
                let row_at =
                    |y: u16| -> Option<usize> { (1..=7).contains(&y).then(|| y as usize - 1) };
                match m.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let steps = if m.kind == MouseEventKind::ScrollUp {
                            1
                        } else {
                            -1
                        };
                        let mut s = state.lock().unwrap();
                        let old = s.get_param(selected);
                        adjust(&mut s, selected, steps, false);
                        let new = s.get_param(selected);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(selected, "Adjust", old, new);
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(row) = row_at(m.row) {
                            selected = row;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        // drag the bar of a value row: bar starts after the
                        // 7-char label
                        if let Some(row) = row_at(m.row) {
                            if matches!(row, 0 | 1 | 2 | 6) {
                                selected = row;
                                let w = terminal.size().map(|r| r.width as usize).unwrap_or(60);
                                let bar_w = crate::theme::bar_width(w, 24);
                                let x = (m.column as usize).saturating_sub(7);
                                let v = (x as f32 / bar_w.saturating_sub(1).max(1) as f32)
                                    .clamp(0.0, 1.0);
                                let mut s = state.lock().unwrap();
                                let old = s.get_param(row);
                                s.set_param(row, ParamValue::F32(v));
                                if let Some(old) = old {
                                    history.record(row, "Slide", old, ParamValue::F32(v));
                                }
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if let Event::Key(key) = ev {
                ex_msg = None;
                if picker.is_active() {
                    if let crate::picker::PickerEvent::Chosen(addr) = picker.handle_key(key.code) {
                        use crate::undo::{ParamUndo, ParamValue};
                        let mut s = state.lock().unwrap();
                        let slot = BIND_SLOT + selected;
                        let old = s.get_param(slot);
                        set_row_binding(&mut s, selected, addr.clone());
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
                    let completer = crate::excmd::standard_completer(crate::excmd::patch_names(
                        &state::patches_dir(),
                    ));
                    if let crate::excmd::ExEvent::Submit(cmd) = ex.handle_key(key.code, &completer)
                    {
                        use crate::excmd::ExCommand;
                        let params = snapshot_params(&state.lock().unwrap());
                        match cmd {
                            ExCommand::Write(name) => {
                                ex_msg = Some(
                                    match crate::excmd::ex_write(
                                        name,
                                        &mut patch_name,
                                        &mut baseline,
                                        &params,
                                    ) {
                                        Ok(m) | Err(m) => m,
                                    },
                                );
                            }
                            ExCommand::Edit(name) => {
                                match state::load_patch::<state::VoiceParams>(&name) {
                                    Ok(p) => {
                                        apply_params(&mut state.lock().unwrap(), &p);
                                        baseline = state::to_toml_string(&snapshot_params(
                                            &state.lock().unwrap(),
                                        ))
                                        .unwrap_or_default();
                                        patch_name = Some(name.clone());
                                        ex_msg = Some(format!("Loaded {}", name));
                                    }
                                    Err(e) => ex_msg = Some(e.to_string()),
                                }
                            }
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
                                match crate::excmd::ex_write(
                                    name,
                                    &mut patch_name,
                                    &mut baseline,
                                    &params,
                                ) {
                                    Ok(_) => should_quit = true,
                                    Err(m) => ex_msg = Some(m),
                                }
                            }
                            ExCommand::Set(k, _) => {
                                ex_msg = Some(format!("Unknown setting: {}", k))
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
                if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
                    let n = count.take();
                    let mut s = state.lock().unwrap();
                    ex_msg = Some(crate::undo::history_status("Redo", n, || {
                        history.redo(&mut *s)
                    }));
                    continue;
                }
                // Ctrl-s: save module state
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    let params = snapshot_params(&state.lock().unwrap());
                    let _ = state::save_module_state("voice", instance, &params);
                    continue;
                }
                match key.code {
                    KeyCode::Char(c) if c.is_ascii_digit() && count.push(c) => {}
                    KeyCode::Char('j') | KeyCode::Down => {
                        selected = crate::keys::cycle(selected, count.take() as i32, NUM_ROWS);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        selected = crate::keys::cycle(selected, -(count.take() as i32), NUM_ROWS);
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
                        let mut s = state.lock().unwrap();
                        let old = s.get_param(selected);
                        adjust(&mut s, selected, steps, coarse);
                        let new = s.get_param(selected);
                        if let (Some(old), Some(new)) = (old, new) {
                            history.record(selected, "Adjust", old, new);
                        }
                    }
                    KeyCode::Char('g') => {
                        count.clear();
                        if pending_g {
                            pending_g = false;
                            selected = 0;
                        } else {
                            pending_g = true;
                        }
                    }
                    KeyCode::Char('G') => {
                        count.clear();
                        selected = NUM_ROWS - 1;
                    }
                    KeyCode::Char('u') => {
                        let n = count.take();
                        let mut s = state.lock().unwrap();
                        ex_msg = Some(crate::undo::history_status("Undo", n, || {
                            history.undo(&mut *s)
                        }));
                    }
                    KeyCode::Char('@') | KeyCode::Enter => {
                        count.clear();
                        let sources = Manifest::open()
                            .map(|m| crate::routing::live_sources(&m.entries()))
                            .unwrap_or_default();
                        let s = state.lock().unwrap();
                        let current = row_binding(&s, selected).cloned().flatten();
                        drop(s);
                        if row_binding(&VoiceState::default(), selected).is_some() {
                            picker.open(sources, current.as_ref());
                        }
                    }
                    KeyCode::Char('x') => {
                        // unbind the selected row's modulation source (consistent
                        // with every other panel module)
                        use crate::undo::{ParamUndo, ParamValue};
                        count.clear();
                        let mut s = state.lock().unwrap();
                        if row_binding(&s, selected).map(|b| b.is_some()).unwrap_or(false) {
                            let slot = BIND_SLOT + selected;
                            let old = s.get_param(slot);
                            set_row_binding(&mut s, selected, None);
                            if let Some(old) = old {
                                history.record(slot, "Unbind", old, ParamValue::Src(None));
                            }
                        }
                    }
                    KeyCode::Char('0') => {
                        // reset the selected value param to its default
                        use crate::undo::ParamUndo;
                        count.clear();
                        let mut s = state.lock().unwrap();
                        let old = s.get_param(selected);
                        if let Some(def) = VoiceState::default().get_param(selected) {
                            s.set_param(selected, def.clone());
                            if let Some(old) = old {
                                history.record(selected, "Reset", old, def);
                            }
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
                        show_help = !show_help;
                    }
                    _ => {
                        count.clear();
                        pending_g = false;
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

    #[test]
    fn adjust_steps_and_clamps_params() {
        let mut s = VoiceState::default();
        let shape0 = s.shape;
        adjust(&mut s, 0, 1, false);
        assert!((s.shape - (shape0 + 0.05)).abs() < 1e-6);
        adjust(&mut s, 0, -100, false);
        assert_eq!(s.shape, 0.0, "shape clamps at 0");
        adjust(&mut s, 1, 100, true);
        assert_eq!(s.sub, 1.0, "sub clamps at 1");
    }

    #[test]
    fn adjust_output_cycles() {
        let mut s = VoiceState::default();
        assert_eq!(s.output, 0);
        adjust(&mut s, 3, 1, false);
        assert_eq!(s.output, 1);
        adjust(&mut s, 3, 2, false);
        assert_eq!(s.output, 0, "output wraps");
        adjust(&mut s, 3, -1, false);
        assert_eq!(s.output, 2, "output wraps backward");
    }

    #[test]
    fn coarse_adjust_is_ten_times() {
        let mut s = VoiceState::default();
        adjust(&mut s, 2, 1, true);
        assert!((s.fm - 0.5).abs() < 1e-6);
    }

    #[test]
    fn old_format_state_keeps_default_bindings() {
        let mut s = VoiceState::default();
        assert!(s.amp_src.is_some(), "default amp binding present");
        // simulate a pre-v2 state file: format 0, no binding fields
        let old = state::VoiceParams {
            shape: Some(0.7),
            ..Default::default()
        };
        apply_params(&mut s, &old);
        assert_eq!(s.shape, 0.7, "values still apply");
        assert!(s.amp_src.is_some(), "old file must not unbind amp");
    }

    #[test]
    fn v2_state_unbind_is_honored() {
        let mut s = VoiceState::default();
        let p = state::VoiceParams {
            format: state::STATE_FORMAT,
            ..Default::default()
        };
        apply_params(&mut s, &p);
        assert!(
            s.amp_src.is_none(),
            "format-2 file with no amp_src = unbound"
        );
    }

    #[test]
    fn lpg_cutoff_tracks_level_exponentially() {
        assert!((lpg_cutoff(0.0) - 25.0).abs() < 0.1, "closed = 25Hz");
        assert!(lpg_cutoff(1.0) > 11_000.0, "open = ~12kHz");
        assert!(
            lpg_cutoff(0.5) > 400.0 && lpg_cutoff(0.5) < 700.0,
            "midpoint is midrange"
        );
        assert!(lpg_cutoff(0.6) > lpg_cutoff(0.4), "monotonic");
    }

    #[test]
    fn lpg_row_adjusts_and_clamps() {
        let mut s = VoiceState::default();
        assert_eq!(s.lpg, 0.0, "LPG defaults off");
        adjust(&mut s, 6, 4, false);
        assert!((s.lpg - 0.2).abs() < 1e-6);
        adjust(&mut s, 6, 100, true);
        assert_eq!(s.lpg, 1.0);
        // persists through params
        let snap = snapshot_params(&s);
        let mut back = VoiceState::default();
        apply_params(&mut back, &snap);
        assert_eq!(back.lpg, 1.0);
    }

    #[test]
    fn amp_rule_never_fails_loud() {
        // unbound: audible drone by choice
        assert_eq!(amp_level(false, None), 1.0);
        assert_eq!(amp_level(false, Some(0.3)), 1.0);
        // bound + live: the source owns the level
        assert_eq!(amp_level(true, Some(0.42)), 0.42);
        assert_eq!(amp_level(true, Some(0.0)), 0.0);
        // bound + orphaned: SILENT — a dead envelope is not a drone
        assert_eq!(amp_level(true, None), 0.0);
    }

    #[test]
    fn per_instance_default_wiring() {
        let v0 = VoiceState::default_for_instance(0);
        assert_eq!(
            v0.amp_src.as_ref().map(|a| a.to_string()),
            Some("envelope/0/ch1".into())
        );
        assert_eq!(
            v0.notes_src.as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t1".into())
        );
        let v1 = VoiceState::default_for_instance(1);
        assert_eq!(
            v1.amp_src.as_ref().map(|a| a.to_string()),
            Some("envelope/0/ch3".into())
        );
        assert_eq!(
            v1.notes_src.as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t3".into())
        );
        // beyond the wired range: unpatched, never panics
        let v3 = VoiceState::default_for_instance(3);
        assert!(v3.amp_src.is_none(), "ch7 does not exist — amp unbound");
        assert_eq!(
            v3.notes_src.as_ref().map(|a| a.to_string()),
            Some("sequencer/0/t7".into())
        );
    }
}
