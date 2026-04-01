mod args;

use anyhow::Context as _;
use clap::Parser;

use crate::args::{Args, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Week 1: config plumbing exists and is exercised on startup.
    let global_path = claude_core::config::global::default_global_config_path()?;
    let _global_cfg = claude_core::config::global::load_global_config(&global_path)
        .with_context(|| format!("loading global config at {global_path:?}"))?;

    if let Some(cmd) = args.command {
        match cmd {
            Command::Auth => {
                eprintln!("auth: not implemented (Week 2+)");
                return Ok(());
            }
            Command::Doctor => {
                eprintln!("doctor: not implemented (Week 2+)");
                return Ok(());
            }
            Command::Mcp => {
                eprintln!("mcp: not implemented (Week 5+)");
                return Ok(());
            }
        }
    }

    if !args.print {
        eprintln!("interactive mode is not implemented in the Rust rewrite. Use -p/--print.");
        std::process::exit(2);
    }

    // Week 1 ends at CLI + config scaffolding.
    eprintln!("headless runner: not implemented yet (Week 3+)");
    Ok(())
}
