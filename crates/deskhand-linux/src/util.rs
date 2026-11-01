//! Small helpers shared by the Linux integrations.

use deskhand_core::{Commands, IntegrationError};

/// D-Bus errors become integration failures (the supervisor restarts us).
pub(crate) fn dbus_err(e: zbus::Error) -> IntegrationError {
    IntegrationError::other(e)
}

/// Receives from an optional command stream; never resolves for `None`.
pub(crate) async fn recv_opt<T>(commands: Option<&mut Commands<T>>) -> Option<T> {
    match commands {
        Some(c) => c.recv().await,
        None => std::future::pending().await,
    }
}
