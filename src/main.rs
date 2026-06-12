// Los — a modular groovebox that lives in your terminal
// Copyright (C) 2026 doo-nothing / AU Supply
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version. See LICENSE.

use anyhow::Result;
use los::{
    badge, conductor, delay, dld, envelope, filterbank, mixer, sampler, scope, sequencer, shm,
    state,
    swarm,
    tape, template, tone, voice,
};

/// `los ctl <action>` — control the global transport from any shell
/// (used by the tmux prefix bindings; also handy for scripting).
fn ctl(action: &str) -> Result<()> {
    let mut t = shm::ShmTransport::open()
        .map_err(|_| anyhow::anyhow!("no los transport found — is a session running?"))?;
    match action {
        "play" => t.set_playing(true),
        "stop" => t.set_playing(false),
        "toggle" => {
            t.toggle_playing();
        }
        "status" => {}
        _ => anyhow::bail!("Usage: los ctl [play|stop|toggle|status]"),
    }
    println!("{}", if t.playing() { "playing" } else { "stopped" });
    Ok(())
}

fn most_recent_save() -> Option<std::path::PathBuf> {
    let states_dir = state::states_dir();
    let mut entries: Vec<_> = std::fs::read_dir(&states_dir)
        .ok()?
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|s| s.ends_with(".toml"))
                .unwrap_or(false)
        })
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let modified = meta.modified().ok()?;
            Some((modified, e.path()))
        })
        .collect();
    entries.sort_by_key(|(m, _)| *m);
    entries.last().map(|(_, p)| p.clone())
}

fn usage() {
    eprintln!("los — Live Operating System (modular synth workstation in tmux)");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  los                           Auto-load last save or create fresh session");
    eprintln!("  los new                       Fresh session, default params (ignores saves)");
    eprintln!("  los <module> [instance]       Run a module directly (independent pane)");
    eprintln!("  los load <state-file.toml>    Load a saved session state");
    eprintln!("  los <state-file.toml>         Same as 'load'");
    eprintln!("  los check <state-file.toml>   Validate a state file (no session needed)");
    eprintln!(
        "  los render <song> <out.wav>   One-shot: spawn the song detached, record, tear down"
    );
    eprintln!("  los audit <wav> [--song <file>]   Analyze a render: RMS arc, peaks, per-section dynamics");
    eprintln!("  los ctl [play|stop|toggle|status]  Control the global transport");
    eprintln!("  los add <module> [instance]   Spawn a module in the running session");
    eprintln!("  los ps                        Inspect the live session (manifest, ring, clock)");
    eprintln!(
        "  los relayout                  Re-apply the house layout sizes (runs on client resize)"
    );
    eprintln!(
        "  los samples pull <q> [--raw]  Prefetch a-u.supply samples into the cache (ls lists)"
    );
    eprintln!(
        "  los record <secs> <out.wav>   Tape out: record the master mix of the running session"
    );
    eprintln!("  los --help                    Show this help");
    eprintln!();
    eprintln!("Modules:");
    eprintln!("  conductor          Session orchestrator (TUI with save/load)");
    eprintln!("  sequencer          Step sequencer (N tracks, Euclidean rhythms)");
    eprintln!("  voice              STO-style synth voice (shape, sub, FM)");
    eprintln!("  mixer              Audio mixer (cpal output, dynamic channels)");
    eprintln!("  scope              ASCII oscilloscope (reads master mix)");
    eprintln!("  envelope           Envelope generator (Maths-inspired, 4 channels)");
    eprintln!("  tone [freq]        Test tone generator (for testing)");
    eprintln!("  template           Worked example module (LFO + drone) — read the source!");
    eprintln!("  delay              8-tap time domain processor (fx: patch a source into it)");
    eprintln!("  filterbank         16-band spectral processor (fx, 296e-style)");
    eprintln!("  tape               6-track tape deck (record window; Tascam x OP-1)");
    eprintln!("  swarm              CS-80-ish brass voice: 7 detuned saws, ladder, chords");
    eprintln!("  dld                Dual looping delay (4ms DLD): clean, clock-locked, holds");
    eprintln!("  sampler            Reels from a-u.supply + Morphagene-ish designer, kit mode");
    eprintln!("  badge              Los faceplate (beat-synced animation, session info)");
    eprintln!();
    eprintln!("Aliases:");
    eprintln!("  sto                → voice");
    eprintln!("  maths              → envelope");
}

fn dispatch_module(name: &str, instance: usize) -> Result<()> {
    // canonical_module also accepts display titles (SEQ, MIX, los) so
    // saves made from pane titles always load
    let canon = conductor::canonical_module(name)
        .ok_or_else(|| anyhow::anyhow!("unknown module: {name}"))?;
    match canon {
        "conductor" => conductor::run_conductor(),
        "sequencer" => sequencer::run(instance),
        "voice" => voice::run(instance),
        "mixer" => mixer::run(),
        "scope" => scope::run(instance),
        "envelope" => envelope::run(instance),
        "tone" => tone::run(440.0, instance),
        "template" => template::run(instance),
        "delay" => delay::run(instance),
        "filterbank" => filterbank::run(instance),
        "tape" => tape::run(instance),
        "swarm" => swarm::run(instance),
        "dld" => dld::run(instance),
        "sampler" => sampler::run(instance),
        "badge" => badge::run(instance),
        other => anyhow::bail!("unknown module: {other}"),
    }
}

fn main() -> Result<()> {
    state::ensure_dirs()?;
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        if let Some(path) = most_recent_save() {
            eprintln!("[los] auto-loading {}", path.display());
            conductor::load_session(&path.to_string_lossy())
        } else {
            eprintln!("[los] no saves found, creating fresh session");
            conductor::create_session()
        }
    } else {
        match args[1].as_str() {
            "--help" | "-h" => {
                usage();
                Ok(())
            }
            "new" | "fresh" => {
                // Fresh session: ignore saved sessions AND clear leftover
                // per-module tmp state so modules start from defaults.
                if let Ok(rd) = std::fs::read_dir(state::tmp_dir()) {
                    for e in rd.flatten() {
                        if e.path().extension().is_some_and(|x| x == "state") {
                            let _ = std::fs::remove_file(e.path());
                        }
                    }
                }
                eprintln!("[los] fresh session (saved states untouched)");
                // a SIGHUP'd previous session leaves stale control-plane
                // SHM (manifest/modbus/events/transport) — start clean
                shm::unlink_control_plane();
                conductor::create_session()
            }
            "load" => {
                let path = args.get(2).cloned().unwrap_or_default();
                if path.is_empty() {
                    anyhow::bail!("Usage: los load <state-file.toml>");
                }
                conductor::load_session(&path)
            }
            // Headless one-shot render: spawn the song detached, record
            // the master mix from bar 0, tear the session down.
            "render" => {
                let mut song: Option<String> = None;
                let mut out: Option<String> = None;
                let mut secs: Option<f32> = None;
                let mut tail: f32 = 3.0;
                let mut rest = args[2..].iter();
                while let Some(arg) = rest.next() {
                    match arg.as_str() {
                        "--secs" => {
                            let v = rest.next().ok_or_else(|| {
                                anyhow::anyhow!("--secs needs a value (seconds)")
                            })?;
                            secs = Some(v.parse().map_err(|_| {
                                anyhow::anyhow!("--secs {v} is not a number")
                            })?);
                        }
                        "--tail" => {
                            let v = rest.next().ok_or_else(|| {
                                anyhow::anyhow!("--tail needs a value (seconds)")
                            })?;
                            tail = v.parse().map_err(|_| {
                                anyhow::anyhow!("--tail {v} is not a number")
                            })?;
                        }
                        other if song.is_none() => song = Some(other.to_string()),
                        other if out.is_none() => out = Some(other.to_string()),
                        other => anyhow::bail!("unexpected argument: {other}"),
                    }
                }
                let usage = "Usage: los render <song.toml> <out.wav> [--secs N] [--tail S]";
                let song = song.ok_or_else(|| anyhow::anyhow!(usage))?;
                let out = out.ok_or_else(|| anyhow::anyhow!(usage))?;
                conductor::render(&song, &out, secs, tail)
            }
            // Offline validation: every problem in the file at once, no
            // session needed. Exit 0 = loadable (warnings allowed).
            "check" => {
                let path = args.get(2).cloned().unwrap_or_default();
                if path.is_empty() {
                    anyhow::bail!("Usage: los check <state-file.toml>");
                }
                let report = los::validate::validate_file(std::path::Path::new(&path));
                for issue in &report.errors {
                    println!("error: {issue}");
                }
                for issue in &report.warnings {
                    println!("warning: {issue}");
                }
                println!(
                    "{path}: {} error{}, {} warning{}",
                    report.errors.len(),
                    if report.errors.len() == 1 { "" } else { "s" },
                    report.warnings.len(),
                    if report.warnings.len() == 1 { "" } else { "s" },
                );
                if !report.is_clean() {
                    std::process::exit(1);
                }
                Ok(())
            }
            // Offline render analysis: windowed RMS + summary, and the
            // per-section dynamics table when the song file is given.
            // No session needed — it reads the WAV (and TOML) from disk.
            "audit" => {
                const USAGE: &str = "Usage: los audit <wav> [--song <file.toml>] [--window <secs>]";
                let mut wav: Option<String> = None;
                let mut song: Option<String> = None;
                let mut window: f64 = 1.0;
                let mut i = 2;
                while i < args.len() {
                    match args[i].as_str() {
                        "--song" => {
                            song =
                                Some(args.get(i + 1).cloned().ok_or_else(|| {
                                    anyhow::anyhow!("--song needs a file\n{USAGE}")
                                })?);
                            i += 2;
                        }
                        "--window" => {
                            window =
                                args.get(i + 1)
                                    .and_then(|s| s.parse().ok())
                                    .ok_or_else(|| {
                                        anyhow::anyhow!("--window needs seconds\n{USAGE}")
                                    })?;
                            i += 2;
                        }
                        arg if wav.is_none() && !arg.starts_with("--") => {
                            wav = Some(arg.to_string());
                            i += 1;
                        }
                        arg => anyhow::bail!("unexpected argument '{arg}'\n{USAGE}"),
                    }
                }
                let Some(wav) = wav else {
                    anyhow::bail!("{USAGE}");
                };
                los::audit::run(
                    std::path::Path::new(&wav),
                    song.as_deref().map(std::path::Path::new),
                    window,
                )
            }
            "ctl" => ctl(args.get(2).map(|s| s.as_str()).unwrap_or("toggle")),
            // Sample cache tooling: `los samples pull <query> [--raw] [--n N]`
            // prefetches from a-u.supply; `los samples ls` lists the cache.
            "samples" => {
                let sub = args.get(2).map(|s| s.as_str()).unwrap_or("ls");
                match sub {
                    "pull" => {
                        let raw = args.iter().any(|a| a == "--raw");
                        let n: usize = args
                            .iter()
                            .position(|a| a == "--n")
                            .and_then(|i| args.get(i + 1))
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(8);
                        let query = args
                            .iter()
                            .skip(3)
                            .filter(|a| !a.starts_with("--"))
                            .filter(|a| {
                                args.iter()
                                    .position(|x| x == *a)
                                    .map(|i| args.get(i.wrapping_sub(1)).map(|p| p != "--n").unwrap_or(true))
                                    .unwrap_or(true)
                            })
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(" ");
                        let hits = sampler::fetch::search(&query, raw, n)?;
                        anyhow::ensure!(!hits.is_empty(), "no hits for {query:?}");
                        for h in &hits {
                            match sampler::fetch::fetch(h) {
                                Ok(p) => println!("  ✓ {}  →  {}", h.filename, p.display()),
                                Err(e) => println!("  ✗ {}: {}", h.filename, e),
                            }
                        }
                        Ok(())
                    }
                    _ => {
                        let items = sampler::fetch::list_cache();
                        println!("{} cached in {}", items.len(), sampler::fetch::cache_dir().display());
                        for (p, name) in items {
                            println!("  {}  ({})", name, p.file_name().unwrap_or_default().to_string_lossy());
                        }
                        Ok(())
                    }
                }
            }
            // Debug tap: watch live note events (with their source bytes)
            // and the sequencer's modbus channels for a few seconds —
            // `los tap [secs]`. Read-only; safe against a running session.
            "tap" => {
                let secs: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4.0);
                let m = shm::Manifest::open()
                    .map_err(|_| anyhow::anyhow!("no manifest — is a session running?"))?;
                let seq_base = m
                    .entries()
                    .iter()
                    .find(|e| e.module_name == "sequencer")
                    .and_then(|e| e.mod_base);
                let mut events = shm::EventRingbuf::open(shm::consumer_id("tap", 0))?;
                let bus = shm::ModulationBus::open().ok();
                // skip the backlog; we only want what happens from now on
                while events.read_event().is_some() {}
                let start = std::time::Instant::now();
                let deadline = start + std::time::Duration::from_secs_f32(secs);
                println!("tapping {secs}s… (sequencer modbus base: {seq_base:?})");
                let mut last_bus = String::new();
                while std::time::Instant::now() < deadline {
                    while let Some(ev) = events.read_event() {
                        let kind = match ev.event_type {
                            0 => "note_on ",
                            1 => "note_off",
                            t => {
                                let _ = t;
                                continue;
                            }
                        };
                        println!(
                            "{:9.1} {} src={} value={:.2}Hz vel/note={} step={}",
                            start.elapsed().as_secs_f64() * 1000.0,
                            kind,
                            ev.source,
                            ev.value,
                            ev.param,
                            ev.step
                        );
                    }
                    if let Some(ref bus) = bus {
                        // every claimed module range, labeled
                        let mut parts: Vec<String> = Vec::new();
                        for e in m.entries() {
                            if let Some(base) = e.mod_base {
                                for c in 0..e.mod_count.min(16) {
                                    parts.push(format!(
                                        "{}{}={:+.2}",
                                        &e.module_name[..3.min(e.module_name.len())],
                                        c + 1,
                                        bus.get(base + c)
                                    ));
                                }
                            }
                        }
                        let row = parts.join(" ");
                        if row != last_bus {
                            println!("modbus: {row}");
                            last_bus = row;
                        }
                    }
                    // tight poll: the timestamps above are only as good
                    // as this granularity (ratchets land ~15ms apart)
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Ok(())
            }
            "relayout" => conductor::relayout(),
            "record" => {
                let secs: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10.0);
                let path = args
                    .get(3)
                    .cloned()
                    .unwrap_or_else(|| "los-tape.wav".into());
                let abs = if std::path::Path::new(&path).is_absolute() {
                    std::path::PathBuf::from(&path)
                } else {
                    std::env::current_dir()?.join(&path)
                };
                let abs = abs.to_string_lossy().to_string();
                let done = format!("{abs}.done");
                let _ = std::fs::remove_file(&done);
                std::fs::write(
                    state::tmp_dir().join("record.arm"),
                    format!("{secs}\n{abs}"),
                )?;
                eprintln!("[los] tape armed: {secs}s -> {abs} (starts when the mixer sees it)");
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs_f32(secs + 20.0);
                loop {
                    if std::path::Path::new(&done).exists() {
                        let _ = std::fs::remove_file(&done);
                        eprintln!("[los] tape done: {abs}");
                        return Ok(());
                    }
                    anyhow::ensure!(
                        std::time::Instant::now() < deadline,
                        "tape never finished — is a session (mixer) running?"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
            "ps" => {
                let m = shm::Manifest::open()
                    .map_err(|_| anyhow::anyhow!("no manifest found — is a session running?"))?;
                let entries = m.entries();
                println!(
                    "manifest: {} live entries, next free modbus channel: {}",
                    entries.len(),
                    m.next_channel()
                );
                for e in &entries {
                    let alive = unsafe { libc::kill(e.pid as i32, 0) == 0 };
                    let outputs = match e.mod_base {
                        Some(b) => format!("  ch{}-{}", b, b + e.mod_count - 1),
                        None => String::new(),
                    };
                    println!(
                        "  {} {}  pid {}{}{}{}",
                        e.module_name,
                        e.instance,
                        e.pid,
                        if alive { "" } else { "  [DEAD]" },
                        if e.audio_shm.is_some() {
                            "  [audio]"
                        } else {
                            ""
                        },
                        outputs
                    );
                }
                if let Ok(t) = shm::ShmTransport::open() {
                    println!("transport: clock={} playing={}", t.clock(), t.playing());
                }
                if let Ok(ev) = shm::EventRingbuf::open_producer() {
                    println!("events: {}", ev.debug_status());
                }
                Ok(())
            }
            "add" => {
                let module = args.get(2).map(|s| s.as_str()).unwrap_or("");
                if module.is_empty() {
                    anyhow::bail!(
                        "Usage: los add <module> [instance] (addable: {})",
                        conductor::ADDABLE_MODULES.join(", ")
                    );
                }
                let instance = args.get(3).and_then(|s| s.parse().ok());
                conductor::add_module(module, instance)
            }
            _ if args[1].ends_with(".toml") => conductor::load_session(&args[1]),
            _ => {
                let instance = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
                dispatch_module(&args[1], instance)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_most_recent_save() {
        let dir = state::tmp_dir().join("test_states");
        let _ = fs::create_dir_all(&dir);

        // Create two save files with different timestamps
        let old = dir.join("old.toml");
        let recent = dir.join("recent.toml");
        fs::write(&old, "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&recent, "").unwrap();

        // Temporarily override the states_dir for testing
        // Note: most_recent_save uses state::states_dir() directly,
        // so this test would need a refactor to be fully unit-testable.
        // For now we just verify the logic with manual paths.
        let mut entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_str().unwrap().ends_with(".toml"))
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                let modified = meta.modified().ok()?;
                Some((modified, e.path()))
            })
            .collect();
        entries.sort_by_key(|(m, _)| *m);
        let most_recent = entries.last().map(|(_, p)| p.clone());

        assert!(most_recent.is_some());
        assert_eq!(most_recent.unwrap().file_name().unwrap(), "recent.toml");

        let _ = fs::remove_dir_all(&dir);
    }
}
