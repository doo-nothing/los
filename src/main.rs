use anyhow::Result;
use los::{conductor, voice, sequencer, mixer, scope, envelope, state};

fn main() -> Result<()> {
    state::ensure_dirs()?;
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        conductor::create_session()
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
