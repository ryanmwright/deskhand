//! Logging setup: `RUST_LOG` overrides `[general] log`. Under systemd
//! (journald) timestamps and colours are left to the journal.

use tracing_subscriber::EnvFilter;

pub fn init(default_filter: &str) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(default_filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let journald = std::env::var_os("JOURNAL_STREAM").is_some();
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    if journald {
        builder.without_time().with_ansi(false).init();
    } else {
        builder.init();
    }
}
