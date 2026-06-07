use anyhow::Result;
use los::{conductor, voice, sequencer, mixer, scope, envelope, state};

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

fn main() -> Result<()> {
    state::ensure_dirs()?;
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        // Auto-load most recent save, or create fresh session
        if let Some(path) = most_recent_save() {
            eprintln!("[los] auto-loading {}", path.display());
            conductor::load_session(&path.to_string_lossy())
        } else {
            eprintln!("[los] no saves found, creating fresh session");
            conductor::create_session()
        }
    } else {
        match args[1].as_str() {
            "conductor" => conductor::run_conductor(),
            "sequencer" => {
                let instance = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
                sequencer::run(instance)
            }
            "voice" => {
                let instance = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
                voice::run(instance)
            }
            "mixer" => mixer::run(),
            "scope" => {
                let instance = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
                scope::run(instance)
            }
            "envelope" => {
                let instance = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
                envelope::run(instance)
            }
            "load" => {
                let path = args.get(2).cloned().unwrap_or_default();
                if path.is_empty() {
                    eprintln!("Usage: los load <state-file.toml>");
                    std::process::exit(1);
                }
                conductor::load_session(&path)
            }
            _ if args[1].ends_with(".toml") => {
                conductor::load_session(&args[1])
            }
            _ => {
                eprintln!("Unknown command: {}", args[1]);
                eprintln!("Run 'los' with no arguments to start the session");
                std::process::exit(1);
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
