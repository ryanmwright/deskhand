//! Deskhand binary: wires the core to the enabled backends.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod doctor;
mod logging;
mod print_discovery;
mod run;
#[cfg(feature = "setup")]
mod setup;
#[cfg(feature = "tray")]
mod tray;
mod uninstall;
mod wiring;

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
    /// Check the config, the broker connection and which integrations work here.
    Doctor {
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the Home Assistant discovery document without connecting to a broker.
    PrintDiscovery {
        /// Print every topic and payload, including states and availability.
        #[arg(long)]
        all: bool,
    },
    /// Set up Deskhand: find the broker, test the connection, write the config
    /// and install the systemd user service.
    #[cfg(feature = "setup")]
    Setup(setup::Options),
    /// Remove everything this device published from the broker (and so from Home Assistant).
    Uninstall {
        /// Don't ask for confirmation.
        #[arg(long, short)]
        yes: bool,
        /// Proceed even if a Deskhand daemon for this device is online.
        #[arg(long)]
        force: bool,
        /// Only list what would be removed.
        #[arg(long)]
        dry_run: bool,
    },
}

fn default_config_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "deskhand")
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("deskhand.toml"))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let explicit_config = cli.config.is_some();
    let config_path = cli.config.unwrap_or_else(default_config_path);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => runtime.block_on(run::run(&config_path)),
        Command::Doctor { json } => {
            logging::init("error");
            let report = runtime.block_on(doctor::report(&config_path));
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                doctor::print_text(&report);
            }
            // Don't wait for background tasks (a hung probe) on the way out.
            runtime.shutdown_background();
            std::process::exit(if report.ok { 0 } else { 1 });
        }
        #[cfg(feature = "setup")]
        Command::Setup(options) => {
            logging::init("warn");
            runtime.block_on(setup::setup(&config_path, explicit_config, options))
        }
        Command::PrintDiscovery { all } => {
            runtime.block_on(print_discovery::print(&config_path, all))
        }
        Command::Uninstall {
            yes,
            force,
            dry_run,
        } => runtime.block_on(uninstall::uninstall(
            &config_path,
            uninstall::Options {
                yes,
                force,
                dry_run,
            },
        )),
    }
}
