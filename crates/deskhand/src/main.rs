//! Deskhand binary: wires the core to the enabled backends.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod logging;
mod run;

#[derive(Debug, Parser)]
#[command(
    name = "deskhand",
    version,
    about = "Expose your desktop to Home Assistant over MQTT"
)]
struct Cli {
    /// Config file [default: ~/.config/deskhand/config.toml]
    #[arg(long, global = true, env = "DESKHAND_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon (default).
    Run,
}

fn default_config_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "deskhand")
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("deskhand.toml"))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(default_config_path);
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(run::run(&config_path))
        }
    }
}
