use anyhow::Result;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn usage() {
    eprintln!("los v{VERSION} — Live Operating System");
    eprintln!();
    eprintln!("usage: los [subcommand] [options]");
    eprintln!();
    eprintln!("subcommands:");
    eprintln!("  (none)       Create tmux session and attach");
    eprintln!("  conductor    Run conductor monitor (inside conductor pane)");
    eprintln!("  voice  [freq] [N]  Synth voice (osc + ADSR + filter)");
    eprintln!("  mixer        Run mixer module (cpal audio output)");
    eprintln!("  tone   [freq] [N]  Test tone generator (Phase 1 testing)");
    eprintln!("  sequencer    Run sequencer module (Phase 3)");
    eprintln!("  scope        Run scope module (Phase 4)");
    eprintln!();
    eprintln!("options:");
    eprintln!("  --create-only  Create session but don't attach");
    eprintln!("  --help, -h     Show this help");
}

fn has_help_flag(args: &[String]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if has_help_flag(&args) {
        usage();
        return Ok(());
    }

    // Consume positional args (skip binary name and flags)
    let positional: Vec<&str> = args.iter().skip(1).filter(|a| !a.starts_with("--")).map(|a| a.as_str()).collect();

    match positional.first().copied() {
        Some("conductor") => los::conductor::run_monitor(),
        Some("voice") => {
            let freq = positional.get(1).and_then(|s| s.parse::<f32>().ok()).unwrap_or(220.0);
            let instance = positional.get(2).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
            los::voice::run(freq, instance)
        }
        Some("mixer") => los::mixer::run(),
        Some("sequencer") => {
            let instance = positional.get(1).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
            los::sequencer::run(instance)
        }
        Some("tone") => {
            let freq = positional.get(1).and_then(|s| s.parse::<f32>().ok()).unwrap_or(440.0);
            let instance = positional.get(2).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
            los::tone::run(freq, instance)
        }
        Some("scope") => {
            let instance = positional.get(1).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
            los::scope::run(instance)
        }
        Some(other) => {
            eprintln!("los: unknown subcommand '{other}'");
            eprintln!("usage: los --help");
            std::process::exit(1);
        }
        None => {
            los::conductor::run_create(!args.iter().any(|a| a == "--create-only"))
        }
    }
}
