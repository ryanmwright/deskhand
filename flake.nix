{
  description = "Deskhand: expose your desktop to Home Assistant over MQTT";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    # Rust toolchain only: openlogi-hidpp needs Rust 1.99, nixos-26.05 has 1.95.
    # Drop this input (and `rustFor`) once the stable channel has 1.99 (nixos-26.11).
    nixpkgs-rust.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs-rust,
      crane,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      # rustc, cargo, clippy and rustfmt from nixpkgs-rust; everything else from nixpkgs.
      rustFor = pkgs: {
        inherit (nixpkgs-rust.legacyPackages.${pkgs.stdenv.hostPlatform.system})
          rustc
          cargo
          clippy
          rustfmt
          rust-analyzer
          ;
        inherit (nixpkgs-rust.legacyPackages.${pkgs.stdenv.hostPlatform.system}.rustPlatform) rustLibSrc;
      };

      # Shared by the package and the checks. The dependencies are built once
      # (`cargoArtifacts`); a change to Deskhand only rebuilds its own crates.
      commonFor =
        pkgs:
        let
          rust = rustFor pkgs;
          craneLib = (crane.mkLib pkgs).overrideScope (
            _final: _prev: {
              inherit (rust)
                rustc
                cargo
                clippy
                rustfmt
                ;
            }
          );
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./crates
              ./data
              ./deny.toml
              ./rustfmt.toml
            ];
          };
          args = {
            inherit src;
            pname = "deskhand";
            version = cargoToml.workspace.package.version;
            strictDeps = true;
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.libpulseaudio ];
          };
        in
        {
          inherit craneLib src args;
          cargoArtifacts = craneLib.buildDepsOnly args;
        };
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          common = commonFor pkgs;
        in
        {
          deskhand = pkgs.callPackage ./nix/package.nix {
            inherit common;
            inherit (common) craneLib;
          };
          default = self.packages.${pkgs.stdenv.hostPlatform.system}.deskhand;
          # For servers and VMs (`services.deskhand.headless`): no KDE, tray
          # or libpulse.
          deskhand-headless = self.packages.${pkgs.stdenv.hostPlatform.system}.deskhand.override {
            features = [
              "linux"
              "setup"
              "herdr"
            ];
          };
        }
      );

      # Home Manager module (nix/hm-module.nix). `homeModules` is the newer name.
      homeManagerModules.default = import ./nix/hm-module.nix self;
      homeModules.default = self.homeManagerModules.default;

      # Evaluates the module against a stub of the Home Manager options it uses
      # (so this flake needs no home-manager input) and checks the result.
      checks = forAllSystems (
        pkgs:
        let
          common = commonFor pkgs;
          inherit (common) craneLib args cargoArtifacts;
        in
        {
          inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) deskhand;
          hm-module = import ./nix/hm-module-check.nix {
            inherit pkgs;
            module = self.homeManagerModules.default;
          };
          cargo-fmt = craneLib.cargoFmt { inherit (common) src; };
          cargo-clippy = craneLib.cargoClippy (
            args
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- -D warnings";
            }
          );
          # Unit and D-Bus tests (private dbus-daemon buses). The broker tests
          # need containers and only run with DESKHAND_IT=1 (`just it`).
          cargo-test = craneLib.cargoTest (
            args
            // {
              inherit cargoArtifacts;
              nativeCheckInputs = [ pkgs.dbus ];
              # require_dbus!() fails instead of skipping.
              CI = "1";
            }
          );
          # Advisories need the network; CI's cargo-deny job checks them.
          cargo-deny = craneLib.cargoDeny {
            inherit (common) src;
            cargoDenyChecks = "bans licenses sources";
          };
        }
        # VM end-to-end tests; need KVM. Plasma runs nightly in CI.
        // pkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          kde-e2e = import ./nix/kde-e2e.nix {
            inherit pkgs;
            inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) deskhand;
          };
          headless-e2e = import ./nix/headless-e2e.nix {
            inherit pkgs;
            deskhand = self.packages.${pkgs.stdenv.hostPlatform.system}.deskhand-headless;
          };
        }
      );

      devShells = forAllSystems (
        pkgs:
        let
          rust = rustFor pkgs;
        in
        {
          default = pkgs.mkShell {
            packages = [
              # Rust toolchain from nixpkgs-rust (no overlay)
              rust.rustc
              rust.cargo
              rust.clippy
              rust.rustfmt
              rust.rust-analyzer
            ]
            ++ (with pkgs; [
              # Native deps
              pkg-config
              libpulseaudio
              # Test and dev tools
              dbus # dbus-daemon for private test buses
              mosquitto # mosquitto_sub / mosquitto_pub
              cargo-deny
              cargo-insta
              just
              jq
              curl
              nixfmt
              socat # VM monitor (just vm-screenshot)
            ]);
            RUST_SRC_PATH = "${rust.rustLibSrc}";
            shellHook = ''
              # testcontainers talks to the Docker API; point it at rootless podman if that is what we have.
              if [ -z "''${DOCKER_HOST:-}" ] && [ -S "''${XDG_RUNTIME_DIR:-/run/user/$UID}/podman/podman.sock" ]; then
                export DOCKER_HOST="unix://''${XDG_RUNTIME_DIR:-/run/user/$UID}/podman/podman.sock"
                export TESTCONTAINERS_RYUK_DISABLED=true
              fi
            '';
          };
        }
      );

      # Plasma 6 Wayland test VM (nix/plasma-vm.nix). x86_64 only: it needs KVM.
      nixosConfigurations.plasma-vm = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [ ./nix/plasma-vm.nix ];
      };

      apps.x86_64-linux.plasma-vm =
        let
          pkgs = nixpkgs.legacyPackages.x86_64-linux;
          vm = self.nixosConfigurations.plasma-vm.config.system.build.vm;
          launcher = pkgs.writeShellApplication {
            name = "plasma-vm";
            runtimeInputs = [
              pkgs.openssh
              pkgs.coreutils
              pkgs.gnugrep
            ];
            text = ''
              ws="''${DESKHAND_WORKSPACE:-$PWD}"
              if ! grep -qs deskhand-core "$ws/Cargo.toml"; then
                echo "plasma-vm: run this from the Deskhand repository (or set DESKHAND_WORKSPACE)" >&2
                exit 1
              fi
              DESKHAND_WORKSPACE="$(realpath "$ws")"
              export DESKHAND_WORKSPACE
              state="$DESKHAND_WORKSPACE/dev/vm"
              mkdir -p "$state/ssh"
              if [ ! -f "$state/ssh/id_ed25519" ]; then
                ssh-keygen -q -t ed25519 -N "" -C deskhand-plasma-vm -f "$state/ssh/id_ed25519"
              fi
              # Nix-built QEMU links everything it needs; a host LD_LIBRARY_PATH
              # (for example a newer alsa-lib) breaks it.
              unset LD_LIBRARY_PATH
              export NIX_DISK_IMAGE="''${NIX_DISK_IMAGE:-$state/plasma-vm.qcow2}"
              exec ${vm}/bin/run-plasma-vm-vm "$@"
            '';
          };
        in
        {
          type = "app";
          program = "${launcher}/bin/plasma-vm";
          meta.description = "Plasma 6 Wayland VM with the workspace shared at /mnt/deskhand";
        };

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
