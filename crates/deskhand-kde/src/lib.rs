//! KDE Plasma backend for Deskhand.
//!
//! - [`active_window`]: a KWin script reports the active window, fullscreen
//!   and the number of displays.
//! - [`desktops`]: virtual desktop select.
//! - [`night_light`]: KWin night light state and an inhibit switch.
//! - [`dnd`]: Plasma's Do Not Disturb.
//! - [`color_scheme`]: switches light/dark for the portal-based `appearance`
//!   integration in `deskhand-linux`.
//!
//! Every candidate probes as unavailable outside a Plasma session
//! (`XDG_CURRENT_DESKTOP` without `KDE`).

pub mod active_window;
pub mod color_scheme;
pub mod desktops;
pub mod dnd;
pub mod night_light;

use std::sync::Arc;
use std::time::Duration;

use deskhand_core::{Integration, Probe};
use deskhand_linux::LinuxEnv;
use deskhand_linux::appearance::Appearance;

pub const KWIN: &str = "org.kde.KWin";

/// The KDE integrations, in priority order per capability. List them
/// before the generic Linux ones so they win where both provide a
/// capability (`appearance`).
pub fn candidates(env: &Arc<LinuxEnv>) -> Vec<Box<dyn Integration>> {
    vec![
        Box::new(active_window::ActiveWindow::new(env.clone())),
        Box::new(desktops::VirtualDesktops::new(env.clone())),
        Box::new(night_light::NightLight::new(env.clone())),
        Box::new(dnd::Dnd::new(env.clone())),
        Box::new(Appearance::with_writer(
            env.clone(),
            "appearance.kde",
            Arc::new(color_scheme::KdeColorScheme::default()),
        )),
    ]
}

/// The session bus, if this is a Plasma session with KWin on it.
pub(crate) async fn kwin_session(env: &LinuxEnv) -> Result<zbus::Connection, Probe> {
    if !env.desktop_is("KDE") {
        return Err(Probe::unavailable("not a KDE Plasma session"));
    }
    let conn = env
        .session_bus()
        .await
        .map_err(|e| Probe::unavailable(format!("session bus: {e}")))?;
    if !LinuxEnv::has_owner(&conn, KWIN).await {
        return Err(Probe::unavailable("KWin is not on the session bus"));
    }
    Ok(conn)
}

/// Where to look for a tool when `PATH` lacks it. A systemd user unit can
/// have a minimal `PATH` (NixOS `systemd.user.services` sets one), while the
/// desktop's tools live in the system or user profile.
const TOOL_DIRS: &[&str] = &[
    "/run/current-system/sw/bin",
    "/etc/profiles/per-user/$USER/bin",
    "$HOME/.nix-profile/bin",
    "$HOME/.local/state/nix/profile/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
];

/// The full path of `program`: as given if it contains a `/`, else the
/// first match in `PATH`, then in [`TOOL_DIRS`].
pub fn find_tool(program: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let path_dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    let fallback = TOOL_DIRS.iter().map(|d| {
        PathBuf::from(
            d.replace("$USER", &var("USER"))
                .replace("$HOME", &var("HOME")),
        )
    });
    path_dirs
        .into_iter()
        .chain(fallback)
        .map(|d| d.join(program))
        .find(|p| is_executable(p))
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Runs a KDE command-line tool (argv, no shell) with a timeout.
pub(crate) async fn run_tool(program: &str, args: &[&str]) -> Result<(), String> {
    let path = find_tool(program).ok_or_else(|| format!("{program} not found"))?;
    let mut cmd = tokio::process::Command::new(&path);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| format!("{program} timed out"))?
        .map_err(|e| format!("cannot run {}: {e}", path.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_tools_on_path_or_absolute() {
        assert!(find_tool("sh").is_some_and(|p| p.is_absolute()));
        assert_eq!(
            find_tool("/opt/x/tool"),
            Some(std::path::PathBuf::from("/opt/x/tool"))
        );
        assert_eq!(find_tool("deskhand-no-such-tool"), None);
    }
}
