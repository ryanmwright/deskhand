# The deskhand package, built with crane. Called with `callPackage`, so
#   deskhand.override { features = [ "linux" "tray" ]; }
# builds a variant (without "audio-pulse" there is no libpulse dependency).
{
  lib,
  stdenv,
  craneLib,
  common,
  installShellFiles,
  features ? [
    "kde"
    "tray"
    "setup"
    "audio-pulse"
    "hidpp"
  ],
}:
craneLib.buildPackage (
  common.args
  // {
    inherit (common) cargoArtifacts;
    cargoExtraArgs = "--locked --package deskhand --no-default-features --features ${lib.concatStringsSep "," features}";
    buildInputs = lib.optionals (lib.elem "audio-pulse" features) common.args.buildInputs;
    nativeBuildInputs = common.args.nativeBuildInputs ++ [ installShellFiles ];
    # Tests run as flake checks (checks.cargo-test), not here.
    doCheck = false;

    postInstall = ''
      install -Dm644 data/io.github.ryanmwright.Deskhand.desktop -t $out/share/applications
      install -Dm644 data/io.github.ryanmwright.Deskhand.svg -t $out/share/icons/hicolor/scalable/apps
      mkdir -p $out/share/systemd/user
      substitute data/deskhand.service.in $out/share/systemd/user/deskhand.service \
        --subst-var-by bindir $out/bin
      substitute data/deskhand-headless.service.in $out/share/systemd/user/deskhand-headless.service \
        --subst-var-by bindir $out/bin
    ''
    # NixOS: services.udev.packages = [ deskhand ];
    + lib.optionalString (lib.elem "hidpp" features) ''
      install -Dm644 data/udev/70-deskhand-logitech.rules -t $out/lib/udev/rules.d
    ''
    + lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
      installShellCompletion --cmd deskhand \
        --bash <($out/bin/deskhand completions bash) \
        --zsh <($out/bin/deskhand completions zsh) \
        --fish <($out/bin/deskhand completions fish)
      $out/bin/deskhand man > deskhand.1
      installManPage deskhand.1
    '';

    passthru = { inherit features; };

    meta = {
      description = "Expose your desktop to Home Assistant over MQTT";
      homepage = "https://github.com/ryanmwright/deskhand";
      license = with lib.licenses; [
        mit
        asl20
      ];
      mainProgram = "deskhand";
      platforms = lib.platforms.linux;
    };
  }
)
