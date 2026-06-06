use anyhow::Result;
use los::{conductor, voice, sequencer, mixer, scope};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        // No arguments - start the tmux session
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
            _ => {
                eprintln!("Unknown command: {}", args[1]);
                eprintln!("Run 'los' with no arguments to start the session");
                std::process::exit(1);
            }
        }
    }
}
