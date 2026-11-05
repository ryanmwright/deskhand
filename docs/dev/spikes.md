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
| 9 | Logitech HID++ (M16) | **works**: identity, battery and backlight with events; sleep/wake as link notifications plus an unasked battery report |

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

**Status: done.** Two runs on the KDE host (2026-11-04: Fedora 44, kernel 7.2.13; MX Keys S on a
Logi Bolt receiver, and a G305 on a Lightspeed receiver). The second run used the first probe
binary, so the receiver replug left only indirect evidence (see below). This dev host and the
Plasma VM have no Logitech hardware. The Bolt traffic is saved in
`crates/deskhand-linux/tests/fixtures/hidpp/`, and `tests/hidpp_fixtures.rs` decodes it with
`openlogi-hidpp`.

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
2. Optional: plug in the USB-C cable for 20 s, then unplug it. If that's awkward, skip it; the
   charging events can be captured later, the next time the keyboard is charged anyway (run the
   probe with a longer time, such as `600`, while plugging it in).
3. Change the backlight with its keys (up, down, off).
4. Unplug the receiver for 10 s, then plug it back in.

Send both capture files back. The udev rule for running without `sudo`
(`data/udev/70-deskhand-logitech.rules`, also in the artifact):

```sh
sudo install -m644 data/udev/70-deskhand-logitech.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=hidraw
```

### Second run (sleep and receiver)

One run, with the cable unplugged (the keyboard stays lit and awake on cable power):

```sh
target/release/examples/hidpp_probe 1500 hidpp-capture-2.txt
```
1. Don't touch the keyboard for at least 15 minutes. Use the mouse if you need the machine.
2. Press a key, then wait about 10 s.
3. Unplug the Bolt receiver, wait about 10 s, plug it back in, wait 10 s, then press Ctrl-C.

This shows whether the keyboard drops its link when it sleeps (0x41 offline, then online on the
key press), and what the probe sees for the replug (`CHANNEL … disconnected`,
`NODE removed/added`). If the receiver is still connected at the end, the probe also pings the
keyboard once and logs how long it took to answer.

### Results (first run)

| Question | Answer |
|---|---|
| Driver bound to the Bolt receiver (why UPower shows nothing) | **`hid-generic`** for all four Bolt interfaces (`046d:c548`), so the kernel's Logitech drivers never see it. The HID++ interface was `hidraw10`; `hid-multitouch` holds another one. The G305's Lightspeed receiver (`c53f`) is bound to `logitech-djreceiver`, and the mouse to `logitech-hidpp-device`, so UPower already has the G305. |
| Permissions | Read-write without `sudo`, because Solaar's udev rules are installed. They do what `data/udev/70-deskhand-logitech.rules` does, so `doctor` should accept either. |
| Pairings | One pairing. Slot 1 is a keyboard named `MX KEYS S`, WPID `B378`, unit ID (redacted). Empty slots answer HID++ 1.0 error `0x08`. The connection event says encrypted; `openlogi-hidpp`'s pairing-info decode says not encrypted, so use the event. Receiver unique ID: 16 hex characters (redacted). Notification flags were already wireless + software-present. |
| Identity | Protocol 4.5. 0x0003 v4: model ID `B378`, serial (12 characters, redacted), firmware entities BL1 88.03.B0018 (bootloader), **RBK 81.03.B0018** (application, active), hardware revision 1. Name "MX Keys S" (0x0005) and friendly name "MX KEYS S" (0x0007). 33 features, including 0x1004 v3, 0x1982 v3, 0x1d4b, 0x1814/0x1815, 0x1b04 v5, 0x40a3, 0x4521, 0x4531 and 0x4220 (lock keys). |
| Request latency | The first request (protocol ping) took 1.0–1.4 s, even though the keyboard had just been used to type the command. After that, 14–30 ms per request. A full read (features, identity, battery, backlight) takes about 2 s. |
| 0x1004 | Reports percentage and levels (critical, low, good, full) and is rechargeable. Reading: 100 %, full, discharging. **Events arrive without polling:** USB-C in sent `Charging` (100 %, full, external power 1), and USB-C out sent `Discharging`, each within a second and each together with a backlight event. |
| 0x1982 | 8 levels; level 2 in automatic (ALS) mode. Durations: hands out 3, hands in 6, powered 4 (the info call reports out-of-box values 3/6/60). The units still need checking in M19. **Every backlight key press sent an event** (`TemporaryManual`, levels 0–3). |
| Sleep (link down/up, 0x41) | **Link down after about 10 minutes untouched** (cable unplugged): `10 01 41 10 41 78 b3`, bit 6 set. The G305 on its Lightspeed receiver went offline about a minute later. On the next key press (11.5 minutes later, the lock-screen password): link up (0x41), then a 0x1d4b reconnection broadcast (`SoftwareReconfigurationNeeded`, reason `PowerSwitchActivated`), **a 0x1004 battery report sent unasked**, and a 0x4220 lock-key state, all within 45 ms. Nothing was polled. |
| Request to a sleeping device | Not tested on purpose: Deskhand never asks while the link is down. The first request after start-up took 0.5–1.4 s, even with an awake keyboard. |
| Receiver unplug and replug | Indirect only (first probe binary): after the replug, writing to the old node failed ("restore notification flags: ERROR"). That is the error the hidraw channel maps to "disconnected". The `NODE`/`CHANNEL` logging was not in that binary. The M19 host check covers the replug with the real integration. |
| Other HID++ clients | During start-up, another program (most likely Solaar, software id 7) read the keyboard on the same node. Its replies reach every reader, so request matching by software ID is required. Solaar also turns the receiver's notification flags on. This time they were wireless-only at start (Solaar not running yet). |
| `battery_refresh` needed? | **No (default `off`).** Charge-state changes are events, and every wake (several times a day) sends a fresh battery report. Still unknown: whether a percentage drop while awake sends an event; the `updated` timestamp makes that visible in HA. |

Other findings:
- The kernel's HID++ driver uses software ID 1 (`11 01 00 11 …` on the G305). The probe's 0x0b, and later Deskhand's `Leased` policy, keep clear of it.
- DJ device nodes (`logitech-hidpp-device`) carry the device's HID++ at its slot index (0x01), not 0xff. They also carry raw input (mouse movement). The probe now skips these nodes and only logs HID++ reports (IDs 0x10–0x12). Deskhand must never log non-HID++ reports.
- `openlogi-hidpp` treats the Lightspeed receiver `c53f` as Unifying. Pairing info (`0xb5 0x5n`) fails with error `0x03` there, but device arrival works (mouse, WPID `4074`, online).

## Incidental

- The first kglobalaccel attempt also left a pipewire SIGSEGV in the same second as the KWin
  abort, probably a consequence of the compositor restart. It did not happen again.
