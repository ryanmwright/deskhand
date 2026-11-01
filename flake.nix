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

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
