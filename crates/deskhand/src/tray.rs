//! Tray icon (StatusNotifierItem through ksni): connection status, Pause,
//! open the config, quit.
//!
//! Only shown when `[general] tray = true` (the default) and a
//! StatusNotifierWatcher is running; otherwise the daemon runs without it.

use std::path::PathBuf;

use deskhand_core::{HubHandle, HubStatus};
use ksni::TrayMethods;
use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};
use tokio_util::sync::CancellationToken;

struct DeskhandTray {
    status: HubStatus,
    /// Our own icon is installed (packages): use it with a status overlay.
    app_icon: bool,
    device: String,
    config_path: PathBuf,
    hub: HubHandle,
    quit: CancellationToken,
}

/// Status text, a theme icon for it, and an overlay for our own icon.
fn describe(status: HubStatus) -> (&'static str, &'static str, &'static str) {
    match (status.paused, status.connected) {
        (true, _) => ("Paused", "user-away", "media-playback-paused"),
        (false, true) => ("Connected", "user-online", ""),
        (false, false) => (
            "Not connected to the broker",
            "user-offline",
            "network-disconnect",
        ),
    }
}

/// Whether the package installed `<APP_ID>.svg` into an icon theme dir.
fn app_icon_installed() -> bool {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(std::env::split_paths(&dirs))
        .any(|d| {
            d.join(format!(
                "icons/hicolor/scalable/apps/{}.svg",
                deskhand_core::APP_ID
            ))
            .is_file()
        })
}

impl ksni::Tray for DeskhandTray {
    fn id(&self) -> String {
        "deskhand".into()
    }

    fn title(&self) -> String {
        "Deskhand".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::SystemServices
    }

    fn icon_name(&self) -> String {
        if self.app_icon {
            deskhand_core::APP_ID.into()
        } else {
            describe(self.status).1.into()
        }
    }

    fn overlay_icon_name(&self) -> String {
        if self.app_icon {
            describe(self.status).2.into()
        } else {
            String::new()
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: format!("Deskhand: {}", self.device),
            description: describe(self.status).0.into(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: format!("{}: {}", self.device, describe(self.status).0),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Pause".into(),
                checked: self.status.paused,
                activate: Box::new(|t: &mut Self| {
                    t.status.paused = !t.status.paused;
                    t.hub.set_paused(t.status.paused);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open config file".into(),
                icon_name: "document-edit".into(),
                activate: Box::new(|t: &mut Self| {
                    if let Err(e) = std::process::Command::new("xdg-open")
                        .arg(&t.config_path)
                        .spawn()
                    {
                        tracing::warn!(error = %e, "cannot run xdg-open");
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| t.quit.cancel()),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Shows the tray and keeps it in sync with the hub until `quit` fires.
/// Logs and returns if there is no tray host.
pub async fn run(hub: HubHandle, device: String, config_path: PathBuf, quit: CancellationToken) {
    let mut status = hub.status();
    let tray = DeskhandTray {
        status: *status.borrow(),
        app_icon: app_icon_installed(),
        device,
        config_path,
        hub,
        quit: quit.clone(),
    };
    let handle = match tray.spawn().await {
        Ok(h) => h,
        Err(e) => {
            tracing::info!(error = %e, "no tray icon (no StatusNotifierWatcher?)");
            return;
        }
    };
    loop {
        tokio::select! {
            () = quit.cancelled() => break,
            r = status.changed() => {
                if r.is_err() {
                    break;
                }
                let s = *status.borrow_and_update();
                handle.update(|t| t.status = s).await;
            }
        }
    }
    handle.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_follow_the_status() {
        let s = |connected, paused| describe(HubStatus { connected, paused });
        assert_eq!(s(true, false).1, "user-online");
        assert_eq!(s(false, false).1, "user-offline");
        assert_eq!(s(true, true).1, "user-away");
        assert_eq!(s(true, false).2, "");
        assert_eq!(s(false, false).2, "network-disconnect");
        assert_eq!(s(false, true).0, "Paused");
    }
}
