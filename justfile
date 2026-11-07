# Deskhand developer commands. Run inside `nix develop`.

compose := env_var_or_default("COMPOSE", "docker compose")

# List recipes
default:
    @just --list

# Format, lint, test and audit (what CI runs)
ci: fmt-check clippy test deny

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
    cargo test --workspace --all-features

# MQTT integration tests against a mosquitto container
it:
    DESKHAND_IT=1 cargo test --workspace --all-features

deny:
    cargo deny check

# Run the daemon against the dev stack
run *ARGS:
    cargo run -p deskhand -- --config dev/config.toml run {{ARGS}}

# Start the dev Home Assistant + Mosquitto stack and configure HA
dev-up:
    mkdir -p dev/ha/config
    {{compose}} up -d
    ./dev/ha/bootstrap.sh

# Stop the dev stack (keeps HA config)
dev-down:
    {{compose}} down

# Stop the dev stack and wipe HA config
dev-reset:
    {{compose}} down
    rm -rf dev/ha/config

# Watch all MQTT traffic on the dev broker
mqtt-watch:
    mosquitto_sub -h localhost -u homeassistant -P homeassistant-dev -v -t '#'

# Boot the Plasma 6 Wayland test VM (window; state in dev/vm/plasma-vm.qcow2)
vm:
    nix run .#plasma-vm

# Boot the VM without a window (QEMU monitor at /tmp/plasma-vm.mon for screenshots,
# QMP at /tmp/plasma-vm.qmp for dev/vm/click.sh)
vm-headless:
    QEMU_OPTS="-display none -monitor unix:/tmp/plasma-vm.mon,server,nowait -qmp unix:/tmp/plasma-vm.qmp,server,nowait" nix run .#plasma-vm

# ssh into the VM (or run a command there)
vm-ssh *ARGS:
    dev/vm/ssh.sh {{ if ARGS == "" { "" } else { quote(ARGS) } }}

# Rebuild deskhand on the host and restart it in the VM
# The VM sees target/ over 9p and can keep stale pages of a replaced binary
# (new processes then segfault): stop, drop the guest's caches, start.
vm-deploy:
    cargo build -p deskhand
    dev/vm/ssh.sh 'systemctl --user stop deskhand-dev; sudo sh -c "sync; echo 3 > /proc/sys/vm/drop_caches"; systemctl --user start deskhand-dev && systemctl --user --no-pager status deskhand-dev | head -3'

# Save a screenshot of a headless VM to /tmp/plasma-vm.png
vm-screenshot:
    echo "screendump /tmp/plasma-vm.png -f png" | socat - UNIX-CONNECT:/tmp/plasma-vm.mon >/dev/null
    @sleep 1
    @echo /tmp/plasma-vm.png

# Delete the VM disk (the next boot starts fresh)
vm-reset:
    rm -f dev/vm/plasma-vm.qcow2

# Plasma VM end-to-end test of the packaged daemon (needs KVM; nightly in CI)
e2e:
    nix build .#checks.x86_64-linux.kde-e2e --print-build-logs

# Headless VM test: no desktop, lingering user, Deskhand starts at boot (needs KVM)
headless-e2e:
    nix build .#checks.x86_64-linux.headless-e2e --print-build-logs

# Every flake check (package, crane fmt/clippy/test/deny, Home Manager module, e2e)
flake-check:
    nix flake check --print-build-logs
