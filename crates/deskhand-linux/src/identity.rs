//! Linux facts for [`PlatformIdentity`], read from files (no D-Bus needed).

use std::path::Path;

use deskhand_core::PlatformIdentity;

/// Gathers machine id, user, hostnames, OS description and primary MAC.
pub fn platform_identity() -> PlatformIdentity {
    platform_identity_at(Path::new("/"))
}

/// Same as [`platform_identity`] with a different filesystem root (tests).
pub fn platform_identity_at(root: &Path) -> PlatformIdentity {
    let read = |p: &str| {
        std::fs::read_to_string(root.join(p.trim_start_matches('/')))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    let machine_id = read("/etc/machine-id")
        .or_else(|| read("/var/lib/dbus/machine-id"))
        .unwrap_or_default();
    let hostname = read("/proc/sys/kernel/hostname")
        .or_else(|| read("/etc/hostname"))
        .unwrap_or_else(|| "localhost".to_owned());
    let pretty_hostname = read("/etc/machine-info").and_then(|t| shell_var(&t, "PRETTY_HOSTNAME"));
    let os = read("/etc/os-release")
        .and_then(|t| shell_var(&t, "PRETTY_NAME"))
        .unwrap_or_else(|| "Linux".to_owned());
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .and_then(|d| d.split(':').next().map(str::to_owned))
        .filter(|d| !d.is_empty());
    let model = match desktop {
        Some(d) => format!("{d} on {os}"),
        None => os,
    };
    let hw_version = read("/proc/sys/kernel/osrelease").map(|r| format!("Linux {r}"));
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "user".to_owned());
    PlatformIdentity {
        machine_id,
        user,
        hostname,
        pretty_hostname,
        model,
        hw_version,
        mac: primary_mac(root),
    }
}

/// Value of `KEY=value` / `KEY="value"` in an os-release style file.
fn shell_var(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix(key)?.strip_prefix('='))
        .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_owned())
        .find(|v| !v.is_empty())
}

/// MAC of the interface carrying the default route if it is physical,
/// otherwise of the first physical interface by name.
fn primary_mac(root: &Path) -> Option<String> {
    let net = root.join("sys/class/net");
    let mac_of = |iface: &str| -> Option<String> {
        let dir = net.join(iface);
        // Physical NICs have a backing device; bridges, veths, tunnels don't.
        if !dir.join("device").exists() {
            return None;
        }
        let mac = std::fs::read_to_string(dir.join("address")).ok()?;
        let mac = mac.trim().to_ascii_lowercase();
        (mac.len() == 17 && mac != "00:00:00:00:00:00").then_some(mac)
    };
    let default_iface = std::fs::read_to_string(root.join("proc/net/route"))
        .ok()
        .and_then(|t| {
            t.lines().skip(1).find_map(|l| {
                let mut cols = l.split_whitespace();
                let iface = cols.next()?;
                (cols.next()? == "00000000").then(|| iface.to_owned())
            })
        });
    if let Some(mac) = default_iface.as_deref().and_then(mac_of) {
        return Some(mac);
    }
    let mut names: Vec<String> = std::fs::read_dir(&net)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    names.iter().find_map(|n| mac_of(n))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write(root: &Path, path: &str, content: &str) {
        let p = root.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    #[test]
    fn reads_identity_from_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "etc/machine-id", "abcdef0123456789abcdef0123456789\n");
        write(root, "proc/sys/kernel/hostname", "desk\n");
        write(root, "proc/sys/kernel/osrelease", "6.18.57\n");
        write(
            root,
            "etc/machine-info",
            "PRETTY_HOSTNAME=\"Ryan's Desk\"\n",
        );
        write(
            root,
            "etc/os-release",
            "NAME=NixOS\nPRETTY_NAME=\"NixOS 26.05 (Yarara)\"\n",
        );
        // eth0 is physical, docker0 is a bridge, wlan0 is physical and default route.
        write(root, "sys/class/net/docker0/address", "02:42:ac:11:00:01\n");
        write(root, "sys/class/net/eth0/address", "AA:BB:CC:00:00:01\n");
        write(root, "sys/class/net/eth0/device/x", "");
        write(root, "sys/class/net/wlan0/address", "aa:bb:cc:00:00:02\n");
        write(root, "sys/class/net/wlan0/device/x", "");
        write(
            root,
            "proc/net/route",
            "Iface\tDestination\tGateway\nwlan0\t00000000\t0101A8C0\neth0\t0001A8C0\t00000000\n",
        );

        let id = platform_identity_at(root);
        assert_eq!(id.machine_id, "abcdef0123456789abcdef0123456789");
        assert_eq!(id.hostname, "desk");
        assert_eq!(id.pretty_hostname.as_deref(), Some("Ryan's Desk"));
        assert!(id.model.ends_with("NixOS 26.05 (Yarara)"), "{}", id.model);
        assert_eq!(id.hw_version.as_deref(), Some("Linux 6.18.57"));
        assert_eq!(id.mac.as_deref(), Some("aa:bb:cc:00:00:02"));

        fs::remove_file(root.join("proc/net/route")).unwrap();
        assert_eq!(primary_mac(root).as_deref(), Some("aa:bb:cc:00:00:01"));
    }
}
