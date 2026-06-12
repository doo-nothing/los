// Los — a modular groovebox that lives in your terminal
// Copyright (C) 2026 doo-nothing / AU Supply
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version. See LICENSE.

//! Offline analysis of a rendered WAV: `los audit`.
//!
//! The point is letting an agent (or a human without speakers) "hear" a
//! render numerically: a windowed RMS table with a sparkline column, a
//! summary block (peak, crest, dynamic arc), and — given the song file —
//! a per-section dynamics table aligned to the macro lane via
//! [`crate::session::song::timeline`]. Output is plain aligned text,
//! designed to be pasted straight into a model context.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};

use crate::session::song::{self, SongTimeline};
use crate::state::SessionState;

/// Display floor in dBFS: silence prints as −90, never −inf.
pub const DB_FLOOR: f64 = -90.0;

/// Sparkline width in columns for a window at 0 dBFS.
const METER_COLS: f64 = 30.0;

/// dBFS the sparkline bottoms out at (an empty bar).
const METER_FLOOR: f64 = -60.0;

// ── WAV decode ───────────────────────────────────────────────────────────────

/// Decoded audio, mixed to mono for analysis.
#[derive(Debug, Clone)]
pub struct Wav {
    /// Channel-averaged samples in −1..1.
    pub mono: Vec<f64>,
    pub sample_rate: u32,
    /// Sample peak (absolute, linear) across all channels, pre-mix.
    pub peak: f64,
}

impl Wav {
    /// Length in seconds.
    #[must_use]
    pub fn duration_secs(&self) -> f64 {
        self.mono.len() as f64 / f64::from(self.sample_rate)
    }
}

/// Read a WAV and mix it to mono. Supports the formats los itself deals
/// in: 16-bit int (the mixer's tape out) and 32-bit float, mono or stereo
/// (any channel count averages).
pub fn read_wav(path: &Path) -> Result<Wav> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    let samples: Vec<f64> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => reader
            .samples::<f32>()
            .map(|s| s.map(f64::from))
            .collect::<Result<_, _>>()
            .context("decoding f32 samples")?,
        (hound::SampleFormat::Int, 16) => reader
            .samples::<i16>()
            .map(|s| s.map(|v| f64::from(v) / 32768.0))
            .collect::<Result<_, _>>()
            .context("decoding i16 samples")?,
        (format, bits) => bail!(
            "unsupported WAV format: {bits}-bit {format:?} — \
             los audit reads 16-bit int and 32-bit float"
        ),
    };
    ensure!(!samples.is_empty(), "{} holds no samples", path.display());
    let channels = usize::from(spec.channels.max(1));
    let peak = samples.iter().fold(0.0_f64, |acc, s| acc.max(s.abs()));
    let mono = samples
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f64>() / frame.len() as f64)
        .collect();
    Ok(Wav {
        mono,
        sample_rate: spec.sample_rate,
        peak,
    })
}

// ── windowed RMS ─────────────────────────────────────────────────────────────

/// One analysis window's loudness.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowStat {
    pub start_secs: f64,
    pub end_secs: f64,
    /// Bar index when the windows follow a song timeline.
    pub bar: Option<usize>,
    pub rms_db: f64,
}

/// Linear amplitude to dBFS, floored at [`DB_FLOOR`].
#[must_use]
pub fn dbfs(x: f64) -> f64 {
    if x <= 0.0 {
        DB_FLOOR
    } else {
        (20.0 * x.log10()).max(DB_FLOOR)
    }
}

fn rms_db(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return DB_FLOOR;
    }
    let mean_sq = samples.iter().map(|s| s * s).sum::<f64>() / samples.len() as f64;
    dbfs(mean_sq.sqrt())
}

/// Fixed-width windows (`--window` seconds, default 1.0).
#[must_use]
pub fn fixed_windows(wav: &Wav, secs: f64) -> Vec<WindowStat> {
    let rate = f64::from(wav.sample_rate);
    let len = ((secs * rate) as usize).max(1);
    wav.mono
        .chunks(len)
        .enumerate()
        .map(|(i, chunk)| {
            let start = (i * len) as f64 / rate;
            WindowStat {
                start_secs: start,
                end_secs: start + chunk.len() as f64 / rate,
                bar: None,
                rms_db: rms_db(chunk),
            }
        })
        .collect()
}

/// Variable-width windows following a song timeline: one per bar, each
/// the duration of that bar at its bpm. Bars past the end of the WAV are
/// dropped; the last overlapping bar is clipped to the samples present.
#[must_use]
pub fn bar_windows(wav: &Wav, tl: &SongTimeline) -> Vec<WindowStat> {
    let rate = f64::from(wav.sample_rate);
    let total = wav.mono.len();
    tl.bars
        .iter()
        .filter_map(|b| {
            let start = (b.start_secs * rate).round() as usize;
            if start >= total {
                return None;
            }
            let end_secs = b.start_secs + 240.0 / b.bpm;
            let end = ((end_secs * rate).round() as usize).min(total);
            Some(WindowStat {
                start_secs: b.start_secs,
                end_secs: end as f64 / rate,
                bar: Some(b.index),
                rms_db: rms_db(&wav.mono[start..end]),
            })
        })
        .collect()
}

// ── summary ──────────────────────────────────────────────────────────────────

/// The one-glance numbers for a render.
#[derive(Debug, Clone)]
pub struct Summary {
    pub peak_db: f64,
    pub overall_rms_db: f64,
    /// Peak − overall RMS, dB.
    pub crest_db: f64,
    /// 5th-percentile window RMS.
    pub floor_db: f64,
    /// 95th-percentile window RMS.
    pub ceiling_db: f64,
    /// Ceiling − floor, dB.
    pub arc_db: f64,
    pub loudest: WindowStat,
    pub quietest: WindowStat,
}

/// Crunch the summary block. `None` only when `windows` is empty.
#[must_use]
pub fn summarize(wav: &Wav, windows: &[WindowStat]) -> Option<Summary> {
    let loudest = windows
        .iter()
        .max_by(|a, b| a.rms_db.total_cmp(&b.rms_db))?
        .clone();
    let quietest = windows
        .iter()
        .min_by(|a, b| a.rms_db.total_cmp(&b.rms_db))?
        .clone();
    let mut sorted: Vec<f64> = windows.iter().map(|w| w.rms_db).collect();
    sorted.sort_by(f64::total_cmp);
    let floor_db = percentile(&sorted, 0.05);
    let ceiling_db = percentile(&sorted, 0.95);
    let peak_db = dbfs(wav.peak);
    let overall_rms_db = rms_db(&wav.mono);
    Some(Summary {
        peak_db,
        overall_rms_db,
        crest_db: peak_db - overall_rms_db,
        floor_db,
        ceiling_db,
        arc_db: ceiling_db - floor_db,
        loudest,
        quietest,
    })
}

/// Nearest-rank percentile of an already-sorted slice (empty = floor).
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return DB_FLOOR;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

// ── per-section dynamics ─────────────────────────────────────────────────────

/// One macro-lane section's dynamics, for the `--song` table.
#[derive(Debug, Clone)]
pub struct SectionStat {
    pub section: usize,
    /// The lane letter that opened the section (`None` for section 0).
    pub macro_fired: Option<char>,
    pub first_bar: usize,
    pub last_bar: usize,
    pub start_secs: f64,
    pub end_secs: f64,
    /// Power-domain mean of the section's window RMS values, dBFS.
    pub mean_rms_db: f64,
    pub max_rms_db: f64,
}

/// Group bar windows by timeline section. Sections wholly past the end
/// of the WAV (no windows) are dropped.
#[must_use]
pub fn section_stats(tl: &SongTimeline, windows: &[WindowStat]) -> Vec<SectionStat> {
    let sections: Vec<usize> = {
        let mut s: Vec<usize> = tl.bars.iter().map(|b| b.section).collect();
        s.dedup();
        s
    };
    sections
        .into_iter()
        .filter_map(|section| {
            let bars: Vec<&song::BarInfo> =
                tl.bars.iter().filter(|b| b.section == section).collect();
            let first = bars.first()?;
            let last = bars.last()?;
            let ws: Vec<&WindowStat> = windows
                .iter()
                .filter(|w| w.bar.is_some_and(|b| b >= first.index && b <= last.index))
                .collect();
            let max_rms_db = ws.iter().map(|w| w.rms_db).max_by(f64::total_cmp)?;
            let mean_pow = ws
                .iter()
                .map(|w| 10.0_f64.powf(w.rms_db / 10.0))
                .sum::<f64>()
                / ws.len() as f64;
            Some(SectionStat {
                section,
                macro_fired: first.macro_fired,
                first_bar: first.index,
                last_bar: last.index,
                start_secs: first.start_secs,
                end_secs: ws
                    .iter()
                    .map(|w| w.end_secs)
                    .fold(first.start_secs, f64::max),
                mean_rms_db: (10.0 * mean_pow.log10()).max(DB_FLOOR),
                max_rms_db,
            })
        })
        .collect()
}

// ── rendering ────────────────────────────────────────────────────────────────

/// A horizontal block-character bar, [`METER_FLOOR`]..0 dBFS over
/// [`METER_COLS`] columns, eighth-block resolution.
#[must_use]
pub fn meter(db: f64) -> String {
    const PARTIALS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let frac = ((db - METER_FLOOR) / -METER_FLOOR).clamp(0.0, 1.0);
    let eighths = (frac * METER_COLS * 8.0).round() as usize;
    let mut s = "█".repeat(eighths / 8);
    if !eighths.is_multiple_of(8) {
        s.push(PARTIALS[eighths % 8 - 1]);
    }
    s
}

/// The windowed-RMS table: `time  bar  RMS dBFS  bar-meter`.
#[must_use]
pub fn render_windows(windows: &[WindowStat]) -> String {
    let mut out = String::from("    time  bar   RMS dBFS\n");
    for w in windows {
        let bar = w.bar.map_or_else(|| "  ·".into(), |b| format!("{b:>3}"));
        let _ = writeln!(
            out,
            "{:>7.2}s {} {:>9.1}  {}",
            w.start_secs,
            bar,
            w.rms_db,
            meter(w.rms_db)
        );
    }
    out
}

/// The summary block.
#[must_use]
pub fn render_summary(s: &Summary) -> String {
    format!(
        "summary\n\
         \x20 sample peak   {:>7.1} dBFS\n\
         \x20 overall RMS   {:>7.1} dBFS\n\
         \x20 crest factor  {:>7.1} dB\n\
         \x20 RMS floor     {:>7.1} dBFS  (5th-pct window)\n\
         \x20 RMS ceiling   {:>7.1} dBFS  (95th-pct window)\n\
         \x20 dynamic arc   {:>7.1} dB\n\
         \x20 loudest       {:>7.2}s  ({:.1} dBFS)\n\
         \x20 quietest      {:>7.2}s  ({:.1} dBFS)\n",
        s.peak_db,
        s.overall_rms_db,
        s.crest_db,
        s.floor_db,
        s.ceiling_db,
        s.arc_db,
        s.loudest.start_secs,
        s.loudest.rms_db,
        s.quietest.start_secs,
        s.quietest.rms_db,
    )
}

/// The per-section table (`--song` only).
#[must_use]
pub fn render_sections(stats: &[SectionStat]) -> String {
    let mut out = String::from(
        "sections\n  sec  macro  bars     time              mean RMS   max RMS   Δ mean\n",
    );
    let mut prev_mean: Option<f64> = None;
    for s in stats {
        let letter = s.macro_fired.map_or('·', |c| c);
        let bars = if s.first_bar == s.last_bar {
            format!("{:<8}", s.first_bar)
        } else {
            format!("{:<8}", format!("{}–{}", s.first_bar, s.last_bar))
        };
        let time = format!("{:.2}–{:.2}s", s.start_secs, s.end_secs);
        let delta = prev_mean.map_or_else(
            || "     —".to_string(),
            |p| format!("{:>+6.1}", s.mean_rms_db - p),
        );
        let _ = writeln!(
            out,
            "  {:>3}  {}      {} {:<17} {:>8.1}  {:>8.1}   {}",
            s.section, letter, bars, time, s.mean_rms_db, s.max_rms_db, delta
        );
        prev_mean = Some(s.mean_rms_db);
    }
    out
}

// ── entry point ──────────────────────────────────────────────────────────────

/// `los audit <wav> [--song <file.toml>] [--window <secs>]` — print the
/// whole report to stdout.
pub fn run(wav_path: &Path, song_path: Option<&Path>, window_secs: f64) -> Result<()> {
    ensure!(window_secs > 0.0, "--window must be > 0 seconds");
    let wav = read_wav(wav_path)?;
    println!(
        "{}: {:.2}s @ {} Hz\n",
        wav_path.display(),
        wav.duration_secs(),
        wav.sample_rate
    );

    let windows = match song_path {
        Some(sp) => {
            let report = crate::validate::validate_file(sp);
            if !report.is_clean() {
                bail!(
                    "{} fails validation — fix it first (`los check`):\n{}",
                    sp.display(),
                    report.render_errors()
                );
            }
            let st: SessionState = crate::state::from_toml_file(sp)?;
            let Some(seq) = song::sequencer_params(&st) else {
                bail!(
                    "{} declares no sequencer 0 pane with patch_inline — \
                     nothing to build a timeline from",
                    sp.display()
                );
            };
            let tl = song::timeline(&seq);
            let dur = wav.duration_secs();
            if (dur - tl.total_secs).abs() > tl.total_secs * 0.05 {
                println!(
                    "NOTE: the WAV is {dur:.2}s but the song's lane is {:.2}s — the render \
                     may have a tail or be partial; only the overlapping part maps to bars\n",
                    tl.total_secs
                );
            }
            let windows = bar_windows(&wav, &tl);
            print!("{}", render_windows(&windows));
            println!();
            if let Some(summary) = summarize(&wav, &windows) {
                print!("{}", render_summary(&summary));
            }
            println!();
            print!("{}", render_sections(&section_stats(&tl, &windows)));
            return Ok(());
        }
        None => fixed_windows(&wav, window_secs),
    };

    print!("{}", render_windows(&windows));
    println!();
    if let Some(summary) = summarize(&wav, &windows) {
        print!("{}", render_summary(&summary));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{MacroCmd, MacroParam, Quant, SequencerParams};
    use std::path::PathBuf;

    /// A unique temp path; the returned guard deletes it on drop.
    struct TmpWav(PathBuf);

    impl TmpWav {
        fn new(name: &str) -> Self {
            TmpWav(
                std::env::temp_dir()
                    .join(format!("los-audit-test-{}-{name}.wav", std::process::id())),
            )
        }
    }

    impl Drop for TmpWav {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn write_i16(path: &Path, channels: u16, frames: &[Vec<f64>]) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for frame in frames {
            for s in frame {
                w.write_sample((s * 32767.0) as i16).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    fn write_f32(path: &Path, samples: &[f64]) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for s in samples {
            w.write_sample(*s as f32).unwrap();
        }
        w.finalize().unwrap();
    }

    /// A Nyquist square wave (±amp alternating): RMS = amp exactly.
    fn square(amp: f64, secs: f64) -> Vec<f64> {
        (0..(44_100.0 * secs) as usize)
            .map(|i| if i % 2 == 0 { amp } else { -amp })
            .collect()
    }

    #[test]
    fn silence_floors_at_minus_ninety() {
        let tmp = TmpWav::new("silence");
        write_i16(&tmp.0, 1, &[vec![0.0; 44_100]]);
        let wav = read_wav(&tmp.0).unwrap();
        let windows = fixed_windows(&wav, 0.25);
        assert_eq!(windows.len(), 4);
        assert!(windows.iter().all(|w| w.rms_db == DB_FLOOR));
        let s = summarize(&wav, &windows).unwrap();
        assert_eq!(s.peak_db, DB_FLOOR);
        assert_eq!(s.overall_rms_db, DB_FLOOR);
        assert_eq!(s.floor_db, DB_FLOOR);
    }

    #[test]
    fn quiet_to_loud_step_shows_in_windows() {
        let tmp = TmpWav::new("step");
        let mut samples = square(0.05, 2.0);
        samples.extend(square(0.5, 2.0));
        write_f32(&tmp.0, &samples);
        let wav = read_wav(&tmp.0).unwrap();
        let windows = fixed_windows(&wav, 1.0);
        assert_eq!(windows.len(), 4);
        // quiet windows ≈ 20·log10(0.05) ≈ −26.0; loud ≈ −6.0
        assert!((windows[0].rms_db - (-26.02)).abs() < 0.1, "{windows:?}");
        assert!((windows[1].rms_db - (-26.02)).abs() < 0.1, "{windows:?}");
        assert!((windows[2].rms_db - (-6.02)).abs() < 0.1, "{windows:?}");
        let step = windows[2].rms_db - windows[1].rms_db;
        assert!((step - 20.0).abs() < 0.5, "step was {step}");
        let s = summarize(&wav, &windows).unwrap();
        assert!((s.peak_db - 20.0 * 0.5_f64.log10()).abs() < 0.01);
        // both loud windows tie on RMS — either is a correct "loudest"
        assert!(s.loudest.start_secs >= 2.0, "{:?}", s.loudest);
        assert!(s.quietest.start_secs < 2.0, "{:?}", s.quietest);
    }

    #[test]
    fn i16_and_f32_decode_agree() {
        let a = TmpWav::new("fmt-i16");
        let b = TmpWav::new("fmt-f32");
        let samples = square(0.25, 0.1);
        write_i16(&a.0, 1, std::slice::from_ref(&samples));
        write_f32(&b.0, &samples);
        let wa = read_wav(&a.0).unwrap();
        let wb = read_wav(&b.0).unwrap();
        assert_eq!(wa.mono.len(), wb.mono.len());
        assert!((wa.peak - wb.peak).abs() < 0.001);
        let dba = rms_db(&wa.mono);
        let dbb = rms_db(&wb.mono);
        assert!((dba - dbb).abs() < 0.01, "{dba} vs {dbb}");
    }

    #[test]
    fn stereo_mixes_to_mono_and_tracks_premix_peak() {
        let tmp = TmpWav::new("stereo");
        // L = 0.5 constant, R = silent: pre-mix peak 0.5, mono 0.25
        let frames: Vec<Vec<f64>> = (0..4410).map(|_| vec![0.5, 0.0]).collect();
        write_i16(&tmp.0, 2, &frames);
        let wav = read_wav(&tmp.0).unwrap();
        assert_eq!(wav.mono.len(), 4410);
        assert!((wav.peak - 0.5).abs() < 0.001);
        assert!((wav.mono[0] - 0.25).abs() < 0.001);
    }

    #[test]
    fn bar_windows_follow_the_timeline_and_clip_to_the_wav() {
        let seq = SequencerParams {
            bpm: Some(120.0),
            macros: vec![MacroParam {
                id: "b".into(),
                quant: Quant::Bar,
                cmds: vec![MacroCmd::SetBpm { bpm: 60.0 }],
            }],
            lane: vec!["".into(), "".into(), "b".into(), "".into()],
            lane_len: Some(4),
            ..Default::default()
        };
        let tl = song::timeline(&seq);
        assert_eq!(tl.total_secs, 12.0);
        // a 6 s wav covers bars 0,1 (2 s each) and half of bar 2 (4 s)
        let tmp = TmpWav::new("bars");
        write_f32(&tmp.0, &square(0.2, 6.0));
        let wav = read_wav(&tmp.0).unwrap();
        let windows = bar_windows(&wav, &tl);
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].bar, Some(0));
        assert_eq!(windows[2].bar, Some(2));
        assert!((windows[2].end_secs - 6.0).abs() < 0.001);
        // sections: 0 (bars 0–1) and 1 (bars 2–3, clipped to bar 2)
        let stats = section_stats(&tl, &windows);
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].macro_fired, None);
        assert_eq!((stats[0].first_bar, stats[0].last_bar), (0, 1));
        assert_eq!(stats[1].macro_fired, Some('b'));
        assert!((stats[0].mean_rms_db - dbfs(0.2)).abs() < 0.1);
    }

    #[test]
    fn meter_scales_and_clamps() {
        assert_eq!(meter(DB_FLOOR), "");
        assert_eq!(meter(METER_FLOOR), "");
        assert_eq!(meter(0.0).chars().count(), METER_COLS as usize);
        assert_eq!(meter(10.0).chars().count(), METER_COLS as usize);
        let half = meter(-30.0);
        assert_eq!(half.chars().count(), METER_COLS as usize / 2);
    }

    #[test]
    fn unsupported_format_is_refused() {
        let tmp = TmpWav::new("i32");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&tmp.0, spec).unwrap();
        w.write_sample(0_i32).unwrap();
        w.finalize().unwrap();
        let err = read_wav(&tmp.0).unwrap_err().to_string();
        assert!(err.contains("unsupported WAV format"), "{err}");
    }
}
