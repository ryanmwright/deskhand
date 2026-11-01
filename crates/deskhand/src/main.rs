//! Deskhand binary: wires the core to the enabled backends.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "deskhand",
    version,
    about = "Expose your desktop to Home Assistant over MQTT"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon (default).
    Run,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => {
            println!(
                "deskhand {} ({}): not implemented yet, see docs/v1-plan.md",
                deskhand_core::VERSION,
                deskhand_core::APP_ID
            );
        }
    }
    Ok(())
}
