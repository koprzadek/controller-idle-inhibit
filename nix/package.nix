{
  lib,
  rustPlatform,
}:

let
  version = (lib.importTOML ../Cargo.toml).package.version;
in
rustPlatform.buildRustPackage {
  pname = "controller-idle-inhibit";
  inherit version;

  src = lib.cleanSourceWith {
    src = ../.;
    filter =
      path: type:
      let
        name = baseNameOf (toString path);
      in
      !(
        (
          type == "directory"
          && (name == "target" || name == ".git" || name == ".github" || name == "bin")
        )
        || lib.hasPrefix "result" name
      );
  };

  cargoLock.lockFile = ../Cargo.lock;

  # The integration tests drive a Python mock compositor over a unix socket
  # (tests/mock_compositor.py); they are designed for `cargo test` in the
  # source tree / devShell and are kept out of the hermetic Nix build.
  doCheck = false;

  meta = {
    description =
      "Inhibit Wayland idle while a gamepad is in use (targets river)";
    longDescription = ''
      Watches gamepads directly through evdev and holds a
      zwp_idle_inhibitor_v1 on their behalf, so compositor-driven idle
      daemons (swayidle, hypridle, ...) do not switch the outputs off while
      you play. The inhibitor is attached to a 1x1 transparent background
      layer-shell surface, which is the kind of surface river actually
      honors. Activity based: inhibition is held for a grace period
      (default 120 s) after the last input, then released. Hotplug aware,
      non-exclusive (devices are never grabbed), compositor restart safe.
    '';
    license = lib.licenses.mit;
    mainProgram = "controller-idle-inhibit";
    platforms = lib.platforms.linux;
  };
}
