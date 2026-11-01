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

# Boot the Plasma 6 test VM (added in M4)
vm:
    nix run .#plasma-vm
