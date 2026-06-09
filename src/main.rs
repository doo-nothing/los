use anyhow::Result;
use los::{conductor, voice, sequencer, mixer, scope, envelope, tone, shm, state};

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
    eprintln!("  los ctl [play|stop|toggle|status]  Control the global transport");
    eprintln!("  los add <module> [instance]   Spawn a module in the running session");
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
    eprintln!();
    eprintln!("Aliases:");
    eprintln!("  sto                → voice");
    eprintln!("  maths              → envelope");
}

fn dispatch_module(name: &str, instance: usize) -> Result<()> {
    match name {
        "conductor" => conductor::run_conductor(),
        "sequencer" => sequencer::run(instance),
        "voice" | "sto" => voice::run(instance),
        "mixer" => mixer::run(),
        "scope" => scope::run(instance),
        "envelope" | "maths" => envelope::run(instance),
        "tone" => tone::run(440.0, instance),
        _ => anyhow::bail!("unknown module: {name}"),
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
                conductor::create_session()
            }
            "load" => {
                let path = args.get(2).cloned().unwrap_or_default();
                if path.is_empty() {
                    anyhow::bail!("Usage: los load <state-file.toml>");
                }
                conductor::load_session(&path)
            }
            "ctl" => ctl(args.get(2).map(|s| s.as_str()).unwrap_or("toggle")),
            "ps" => {
                let m = shm::Manifest::open()
                    .map_err(|_| anyhow::anyhow!("no manifest found — is a session running?"))?;
                let entries = m.entries();
                println!("manifest: {} live entries, next free modbus channel: {}", entries.len(), m.next_channel());
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
                        if e.audio_shm.is_some() { "  [audio]" } else { "" },
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
            _ if args[1].ends_with(".toml") => {
                conductor::load_session(&args[1])
            }
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
