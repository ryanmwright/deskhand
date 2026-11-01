{
  description = "Deskhand: expose your desktop to Home Assistant over MQTT";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    in
    {
      packages = forAllSystems (pkgs: {
        deskhand = pkgs.rustPlatform.buildRustPackage {
          pname = "deskhand";
          version = cargoToml.workspace.package.version;
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [
            "--package"
            "deskhand"
          ];
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.libpulseaudio ];
          # Tests need dbus-daemon and containers; CI runs them outside the Nix sandbox.
          doCheck = false;
          meta = {
            description = "Expose your desktop to Home Assistant over MQTT";
            license = with pkgs.lib.licenses; [
              mit
              asl20
            ];
            mainProgram = "deskhand";
            platforms = pkgs.lib.platforms.linux;
          };
        };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.deskhand;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            # Rust toolchain from nixpkgs (no overlay)
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer
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
          ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          shellHook = ''
            # testcontainers talks to the Docker API; point it at rootless podman if that is what we have.
            if [ -z "''${DOCKER_HOST:-}" ] && [ -S "''${XDG_RUNTIME_DIR:-/run/user/$UID}/podman/podman.sock" ]; then
              export DOCKER_HOST="unix://''${XDG_RUNTIME_DIR:-/run/user/$UID}/podman/podman.sock"
              export TESTCONTAINERS_RYUK_DISABLED=true
            fi
          '';
        };
      });

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
