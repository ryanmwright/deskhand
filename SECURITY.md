# Security

Deskhand connects a desktop computer to an MQTT broker, and through it to Home
Assistant. Whoever can publish to Deskhand's command topics can do what its
entities allow: lock the screen, suspend or shut down the computer, change the
volume, show notifications, press the configured script buttons. Whoever can
read its topics learns what the computer is doing. This page explains what
Deskhand does to limit that, and what you should do.

## Reporting a vulnerability

Please report security problems privately through GitHub: **Security →
Report a vulnerability** on the repository page. Don't open a public issue.
You can expect an answer within a week.

## What Home Assistant can and cannot do

- **No remote commands.** Deskhand never runs a program, URL or command line
  that arrives over MQTT. Home Assistant can only press buttons for things you
  configured:
  - `[[script]]` entries run fixed argv lists from the config file; a press
    carries no arguments.
  - `[integrations.systemd] units` lists the only user units it may start and
    stop.
  - `[integrations.shortcuts] invoke` lists the only KDE shortcuts it may
    trigger.
  - Notification buttons and replies only come back as events.
- **No remote unlock by default.** Home Assistant gets a *Lock screen* button,
  not a lock it can open. `[integrations.lock] allow_remote_unlock = true`
  turns it into a lock that can also unlock: anyone with access to Home
  Assistant (or able to publish to the broker) could then unlock your session.
  Only enable it if that is acceptable.
- **Opt-in for disruptive actions.** Home Assistant gets no power buttons
  unless you list them in `[integrations.power] buttons` (`deskhand setup`
  asks): suspend, hibernate, restart, shut down and log out can all interrupt
  you, and restart, shut down and log out can lose unsaved work. Turning
  outputs off is opt-in too. Locking the screen is always available; it loses
  nothing.
- **Retained commands are ignored**, so a stale "shut down" message left on
  the broker never fires when Deskhand connects.
- Deskhand runs as your user with your permissions; it needs no root.

## Protect the broker

The broker is the security boundary. Use credentials for every client, and
give each computer its own broker user with access only to its own topics.

With Mosquitto (`acl_file`), for a computer with node id `0123456789ab`
(shown by `deskhand doctor`):

```
# The computer: its own topics and its discovery document
user deskhand-office
topic readwrite deskhand/0123456789ab/#
topic readwrite homeassistant/device/deskhand_0123456789ab/config
topic readwrite homeassistant/media_player/deskhand_0123456789ab/config
topic read homeassistant/status

# Home Assistant: everything
user homeassistant
topic readwrite #
```

(The `media_player` line is only needed with `compat_mqtt_media_player`.)

With Home Assistant's Mosquitto add-on, every Home Assistant user can log in
to the broker. Create a dedicated, non-administrator user for Deskhand, and
consider the add-on's `customize` option with an ACL file like the above.

Use TLS (`[mqtt] tls = true`) when the broker is reached over a network you
don't fully trust.

## Protect the credentials

- `deskhand setup` writes the config with mode 0600 in a 0700 directory, and
  the password to a separate 0600 file (`password_file`).
- Prefer `password_file` or `password_command` (e.g. a password manager) to
  `password` in the config.
- With Nix, never put the password in `services.deskhand.settings`: it would
  land in the world-readable Nix store. Use `passwordFile` with agenix or
  sops-nix; the module refuses `settings.mqtt.password`.

## Privacy

Everything Deskhand publishes is visible to anyone who can read its topics or
use Home Assistant, and Home Assistant keeps history. Consider what you want
recorded:

- **Window titles** can contain document names, chat partners and URLs.
  `[integrations.active_window] publish_title = false` keeps them out (the app
  id is still published); `enabled = false` drops the active window entirely.
- **Media** titles and artists, **Wi-Fi network names**, and the **apps**
  using the microphone or camera are published as attributes.
- Turn off any capability with `[integrations.<capability>] enabled = false`;
  its entities are then removed from Home Assistant.
- Exclude noisy entities from Home Assistant's recorder in its
  `configuration.yaml` if you want them live but not stored.
- The tray's **Pause** stops publishing (Home Assistant sees the computer as
  unavailable) until you resume.

## Other measures

- The KWin script that reports the active window calls back over D-Bus;
  Deskhand only accepts these calls from the process that owns KWin's bus
  name, so other programs can't feed it fake windows.
- Commands are checked before anything happens: a switch accepts only
  `ON`/`OFF`, a number must be a finite number (integrations clamp it to their
  range), a select must name one of its options. Anything else is dropped.
- Script processes run without a shell unless the config says `shell = true`,
  and are killed (with their process group) at their timeout.
