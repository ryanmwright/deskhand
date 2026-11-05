# M5 spike results

Run on 2026-11-01 in the Plasma VM (`just vm`): KWin/Plasma **6.6.6** (Wayland), PipeWire 1.6.6
with the pulse server, systemd user session, nixos-26.05. The probes are examples in
`crates/deskhand-linux/examples/` and `crates/deskhand-kde/examples/`; build them on the host
with `cargo build --examples` and run them in the VM from `/mnt/deskhand/target/debug/examples/`.
Run them as a user service (`systemd-run --user --wait --pipe ...`) to get the same environment
Deskhand has. An ssh session has no `WAYLAND_DISPLAY`, and logind's `auto` session is the ssh
session there.

| # | Area | Result |
|---|---|---|
| 1 | Idle: `ext_idle_notifier_v1` v2 | **works** |
| 2 | KWin script callback + sender check | **works** |
| 3 | kglobalaccel register / press / persist | **works**, but a malformed call crashes KWin (see below) |
| 4 | Plasma Do Not Disturb | **works**, with a better write path than planned |
| 5 | DPMS off / on | **works** (read and set through the Wayland protocol, M11) |
| 6 | libpulse mic filters | **works** (corked streams not exercised) |
| 7 | Camera `/proc` scan + inotify | **works** |
| 8 | logind session from a user service | **works**, but use `User.Display` first |
| 9 | Logitech HID++ (M16) | **pending** the run on the KDE host |

## 1. Idle (`idle_probe`)

- KWin advertises `ext_idle_notifier_v1` **v2** (also the older `org_kde_kwin_idle` v1 and
  `zwp_idle_inhibit_manager_v1`, and `org_kde_kwin_dpms_manager` v1).
- With mpv playing a video in a visible window (`--vo=wlshm`), `get_idle_notification`
  (legacy) never went idle. It went idle 3 s after mpv stopped. `get_input_idle_notification`
  (v2) went idle after 3 s while the video played, which is what `user_active` should do.
- The timeout counts from the **last input**, not from when the notification was created (it
  fired after 2.85 s for a 3 s timeout when the last key was 0.15 s before start). So on `idled`,
  `idle_since = now - timeout` is right.
- Works from a user service (the user manager has `WAYLAND_DISPLAY` imported by Plasma).
- VM notes: QEMU's `mouse_move` is not seen as input with the USB tablet; `sendkey` is. mpv's
  GPU outputs crash without GL; use `--vo=wlshm`.

## 2. KWin script (`kwin_script_probe`)

- `org.kde.kwin.Scripting.loadScript(path, pluginName)` returns an id; `/Scripting/Script<id>`
  `org.kde.kwin.Script.run()` starts only that script (better than `Scripting.start()`).
  `isScriptLoaded` / `unloadScript(pluginName)` work for cleanup and reloads.
- The script calls `callDBus(service, path, interface, "Event", JSON.stringify({...}))`.
  **Pass one JSON string**: JS numbers would arrive as doubles, so typed D-Bus signatures are
  fragile.
- Sender check works: every event's sender was the unique name owning `org.kde.KWin` (looked
  up with `GetNameOwner`). A call from another client was rejected with `AccessDenied`.
- Plasma 6 API: `workspace.windowActivated`, `workspace.activeWindow`, `workspace.screens`,
  `workspace.screensChanged`; window fields `desktopFileName` (e.g. `org.kde.konsole`),
  `resourceClass`, `caption`, `pid`, `fullScreen`, `output.name`. Send an `initial` event when
  the script starts.
- For M10: title and fullscreen changes of the *active* window need `captionChanged` /
  `fullScreenChanged` connections. Activation alone does not report them.

## 3. kglobalaccel (`kglobalaccel_probe`)

- `doRegister([component, action, componentFriendly, actionFriendly])`, then
  `setShortcutKeys(actionId, keys, IsDefault=8)` and `setShortcutKeys(actionId, keys,
  SetPresent=2)`. The second returns the active keys, which are the user's own if they changed
  them. Component `deskhand`, as the plan requires.
- **KWin crash:** KF6 demarshals each `QKeySequence` as a struct of **exactly four ints**. Sending
  `[(combo)]` instead of `[(combo, 0, 0, 0)]` made libdbus abort inside **KWin** (Plasma 6 runs
  kglobalaccel in KWin), which took down the compositor until it restarted. The M11 code must
  pad to four ints and have a unit test for the encoding; never pass user-provided key arrays
  through unchecked.
- A real key press (QEMU `sendkey meta_l-shift-f12`) emits `globalShortcutPressed` and
  `globalShortcutReleased` on `/component/deskhand` (`org.kde.kglobalaccel.Component`).
  `invokeShortcut` emits only `pressed`. The timestamp argument was 0 in the VM.
- No consent dialog. The binding is written to `~/.config/kglobalshortcutsrc` under
  `[deskhand]`, and `allMainComponents` lists the component, which is what System Settings
  shows.
- **Persists:** after the registering process exited, a new process that only subscribed to the
  signal (no `doRegister`) still received presses.

## 4. Do Not Disturb (`dnd_probe`)

- `org.freedesktop.Notifications` property `Inhibited` is true for **both** app inhibitions and
  the user's own DnD (toggled with plasmashell's `toggle do not disturb` shortcut, which is
  what the tray button does). It emits `PropertiesChanged`. → **Read `Inhibited`.**
- `Inhibit(desktop_entry, reason, hints)` → cookie; `UnInhibit(cookie)` ends it. The inhibition
  also ends when the calling connection closes (checked by exiting without `UnInhibit`).
  `UnInhibit` cannot end a DnD the *user* set.
- The user's DnD lives in `plasmanotifyrc` `[DoNotDisturb] Until=<KConfig datetime>` (the
  toggle writes now + 1 year, e.g. `2027,11,1,15,11,36.365`). Writing or deleting that key
  **with KConfig change notification** (`kwriteconfig6 --notify`, i.e. the
  `org.kde.kconfig.notify` `ConfigChanged` signal) updates plasmashell immediately: `Inhibited`
  followed. A plain file write is ignored.
- → **Plan change (M10):** the DnD switch writes `Until` with a KConfig notification. On sets a
  far-future date, which persists like the UI toggle and survives Deskhand restarts. Off deletes
  the key, which also ends a DnD the user started. `Inhibit` is the fallback if the config write
  fails; it is released if Deskhand dies.

## 5. DPMS

- `invokeShortcut("Turn Off Screen")` on `/component/org_kde_powerdevil` turned the output off
  (the QEMU framebuffer went black).
- `kscreen-doctor --dpms off` / `--dpms on` both work, but only with the session's Wayland
  environment: under plain ssh `kscreen-doctor` aborts. A user service is fine.
- Any key press also turns the screen back on.
- **Wayland protocol (M11, `dpms_probe`):** `org_kde_kwin_dpms_manager` v1 →
  `get(wl_output)` sends `supported`, `mode` (0 on, 1 standby, 2 suspend, 3 off) and `done` per
  output. `set(3)` turned the output off ("Display output is not active"), `set(0)` turned it
  back on, and the mode stays after the client exits. A key press while off sent `mode 0`, and
  idle blanking by PowerDevil showed as `mode 3`. So M11 has a `screens_on` switch, with the
  shortcut as the fallback.

## 6. Microphone (`mic_probe`)

With a virtual source (`module-null-sink media.class=Audio/Source/Virtual`) and `parecord`
streams:
- a recording from a sink monitor (`speakers.monitor`, `monitor_of_sink` set) → skipped;
- an application on the ignore list (matched on `application.name`, e.g. `pavucontrol`) → skipped;
- other recordings → counted, with `application.name` for the attribute.
- `application.process.binary` was `pacat` for all of them, so `application.name` is the useful
  name. `PULSE_PROP` did not override `application.name` for parecord (it sets its own).
- Corked streams were not exercised (no stock tool corks a recording); the `corked` flag is read
  from the same struct. Check on the real host with a browser call that mutes the mic.

## 7. Camera (`camera_probe`)

- inotify `OPEN`/`CLOSE_*` on `/dev/video*` fires on every open, including short device probes
  (`v4l2-ctl --info`). Waiting 250 ms before scanning `/proc/*/fd` filtered that probe out.
- A process holding `/dev/video0` appeared as `(pid, "sleep", "/dev/video0")` and disappeared on
  close.
- `/proc/<pid>/fd` of other users' processes is unreadable (118 entries in the VM). That is
  fine for the user's own apps, but an app using the camera *through PipeWire* (portal camera
  access) would show up as `pipewire`/`wireplumber`. In the VM, WirePlumber did not keep the
  v4l2loopback device open. **Check on the real host in M9**, and consider PipeWire node
  information for the app name.

## 8. logind (`logind_probe`)

| Called from | `GetSession("auto")` | `GetSessionByPID(self)` | `User.Display` | `Can*` |
|---|---|---|---|---|
| user service (`systemd-run --user`) | `_31` (Wayland, KDE) | **error** `NoSessionForPID` | `_31` | `yes` |
| ssh login | `_34` (the tty) | `_34` | `_31` | `challenge` |

- → **Plan change (M6):** resolve the session with `User.Display` (`GetUserByPID(self)` or
  `/org/freedesktop/login1/user/self`), falling back to `GetSession("auto")`. Never use
  `GetSessionByPID` for a user service.
- `loginctl lock-session` emits `Lock`, KDE's locker locks and sets `LockedHint=true`;
  `unlock-session` emits `Unlock`, the locker really unlocks and `LockedHint` goes false.
  `PropertiesChanged` arrives for `LockedHint`. So `lock` (Lock/Unlock) and `screen_locked`
  (LockedHint) work as planned.
- `CanSuspend` etc. are `yes` from the user service of the active local session; from a remote
  session polkit says `challenge`.

## 9. Logitech HID++ (`hidpp_probe`, M16)

**Status: waiting for the run on the KDE host** (MX Keys S on a Logi Bolt receiver). This dev
host and the Plasma VM have no Logitech hardware.

The probe (`crates/deskhand-linux/examples/hidpp_probe.rs`) talks HID++ through
`openlogi-hidpp` over Deskhand's own hidraw channel (`deskhand_linux::hidraw`). It prints and
captures:
- every hidraw node with its bus, vendor:product, bound driver, and whether it opens read-write;
- per Logitech receiver: unique ID, notification flags, pairings and codenames, and device
  arrival;
- per device: the protocol version, the feature table, identity (0x0003/0x0005/0x0007), battery
  (0x1004/0x1000/0x1001) and backlight (0x1982);
- then, for 120 s, connection events and decoded battery, backlight and wireless-status events.

Every report in both directions goes to `hidpp-capture.txt`, timestamped and in hex. The probe
enables the receiver's wireless and software-present notification flags and restores them on
exit; it changes nothing else.

### Running it on the host

```sh
gh run download --repo ryanmwright/deskhand --name hidpp-probe-x86_64-linux-gnu
chmod +x target/release/examples/hidpp_probe
# 1. As yourself: shows the node permissions (expect "rw FAILED: Permission denied").
target/release/examples/hidpp_probe 1 /tmp/hidpp-perms.txt
# 2. With access, for the full run (or install the udev rule below and re-plug the receiver).
sudo target/release/examples/hidpp_probe 120 hidpp-capture.txt
```

During the 120 s, with the time of each step noted:
1. Leave the keyboard untouched until it sleeps (backlight off), then press a key.
2. Plug in the USB-C cable for 20 s, then unplug it.
3. Change the backlight with its keys (up, down, off).
4. Unplug the receiver for 10 s, then plug it back in.

Send both capture files back. The udev rule for running without `sudo`
(`data/udev/70-deskhand-logitech.rules`, also in the artifact):

```sh
sudo install -m644 data/udev/70-deskhand-logitech.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=hidraw
```

### Questions the run answers

| Question | Answer |
|---|---|
| Driver bound to the Bolt receiver (why UPower shows nothing) | |
| Pairings: slot, WPID, unit ID, codename of the MX Keys S | |
| A request to a sleeping device: error, timeout, or answered? (time) | |
| Link down/up notifications (0x41) on sleep and wake? | |
| 0x1004: percentage, levels, charging states; an event per % change? | |
| 0x0003: model IDs, firmware entities, serial number | |
| 0x1982: backlight config; events for key changes? | |
| Receiver unplug and replug: what the channel sees | |
| `battery_refresh` needed? | |

## Incidental

- The first kglobalaccel attempt also left a pipewire SIGSEGV in the same second as the KWin
  abort, probably a consequence of the compositor restart. It did not happen again.
