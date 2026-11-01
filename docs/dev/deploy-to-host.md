# Deploying a CI build to a desktop

Until Deskhand is packaged (Nix module in M13, others after V1), you can try it on a real
desktop with the binary CI builds on every push. This is how hardware features (Bluetooth,
camera, DDC brightness, suspend) get tested on the developer's KDE machine.

## 1. Get the binary

The `release-binary` CI job builds on Ubuntu 22.04, so the binary runs on any distribution with
glibc 2.35 or newer. Its only runtime library is `libpulse` (PulseAudio or PipeWire's
`pipewire-pulse`), which every desktop distribution has.

From the GitHub UI: open the latest CI run on `main`, scroll to **Artifacts**, download
`deskhand-main-x86_64-linux-gnu`. Or with `gh`:

```sh
gh run download --repo ryanmwright/deskhand --name deskhand-main-x86_64-linux-gnu
tar xzf deskhand-main-x86_64-linux-gnu.tar.gz
install -Dm755 deskhand-main-x86_64-linux-gnu/deskhand ~/.local/bin/deskhand
```

Tagged builds (`v*`) are also attached to a draft GitHub release, with a `.sha256` file.

## 2. Write a config

`~/.config/deskhand/config.toml` (keep it private: `chmod 600`):

```toml
[mqtt]
host = "homeassistant.local"   # your broker
username = "deskhand"
password_file = "~/.config/deskhand/mqtt-password"

# Optional: a button in HA that runs a command (argv, no shell unless shell = true)
[[script]]
id = "hello"
name = "Say hello"
command = ["notify-send", "Hello from Home Assistant"]
```

Give Deskhand its own broker user. Its ACL needs `deskhand/#` and `homeassistant/#` (read and
write); see `dev/mosquitto/acl` for an example.

## 3. Check it

```sh
deskhand doctor
```

`doctor` checks the config, connects to the broker (and explains a refused connection), and
lists which integrations would run here and why the others can't. `deskhand doctor --json`
gives the same as JSON. `deskhand print-discovery` shows what would be published, without a
broker.

## 4. Run it

In a terminal first:

```sh
deskhand run          # Ctrl-C publishes "offline" and exits
```

The device appears in Home Assistant under **Settings → Devices → MQTT**. To keep it running,
use a systemd user service (the setup wizard in M12 will install this for you):

```ini
# ~/.config/systemd/user/deskhand.service
[Unit]
Description=Deskhand
PartOf=graphical-session.target
After=graphical-session.target

[Service]
Type=notify
ExecStart=%h/.local/bin/deskhand run
Restart=on-failure

[Install]
WantedBy=graphical-session.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now deskhand.service
journalctl --user -u deskhand -f          # logs; -p warning for problems only
```

## 5. Remove it

```sh
systemctl --user disable --now deskhand.service
deskhand uninstall    # deletes everything it published; HA then removes the device
rm -r ~/.local/bin/deskhand ~/.config/deskhand ~/.config/systemd/user/deskhand.service
```

`uninstall` refuses while the daemon is online (it would publish everything again); stop the
service first or pass `--force`. `--dry-run` lists what would be removed.
