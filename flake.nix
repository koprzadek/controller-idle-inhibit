{
  description = ''
    controller-idle-inhibit — hold a Wayland idle inhibitor while a gamepad
    is in use, so idle daemons (swayidle, hypridle, ...) do not blank your
    screen mid-game. Targets river; Linux only (evdev + Wayland).
  '';

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # The daemon uses evdev and the Wayland wire protocol: Linux only.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system: f nixpkgs.legacyPackages.${system}
        );
    in
    {
      packages = forAllSystems (pkgs: rec {
        controller-idle-inhibit = pkgs.callPackage ./nix/package.nix { };
        default = controller-idle-inhibit;
      });

      overlays.default = final: _prev: {
        controller-idle-inhibit = final.callPackage ./nix/package.nix { };
      };

      nixosModules.default = import ./modules/nixos.nix;
      homeManagerModules.default = import ./modules/home-manager.nix;

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            python3 # runs tests/mock_compositor.py
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);

      checks = forAllSystems (pkgs: {
        controller-idle-inhibit =
          self.packages.${pkgs.system}.controller-idle-inhibit;
      });
    };
}
