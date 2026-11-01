//! Logging setup: `RUST_LOG` overrides `[general] log`. Logs go to stderr so
//! commands like `print-discovery` keep stdout clean. Under systemd
//! (journald) timestamps and colours are left to the journal, and each line
//! gets a `<N>` syslog priority prefix so `journalctl -p warning` works.

use std::fmt;

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

pub fn init(default_filter: &str) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(default_filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let journald = std::env::var_os("JOURNAL_STREAM").is_some();
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false);
    let res = if journald {
        let format = tracing_subscriber::fmt::format()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_level(false);
        builder
            .with_ansi(false)
            .event_format(JournalPriority(format))
            .try_init()
    } else {
        builder.try_init()
    };
    // Already initialised (tests, or a subcommand called twice): keep the first.
    let _ = res;
}

/// Prefixes each event with its syslog priority (`<3>` error ... `<7>` debug).
struct JournalPriority<F>(F);

impl<S, N, F> FormatEvent<S, N> for JournalPriority<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let priority = match *event.metadata().level() {
            Level::ERROR => 3,
            Level::WARN => 4,
            Level::INFO => 6,
            Level::DEBUG | Level::TRACE => 7,
        };
        write!(writer, "<{priority}>")?;
        self.0.format_event(ctx, writer, event)
    }
}
