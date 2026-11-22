# controller-idle-inhibit

Inhibit Wayland idle **while a gamepad is in use** — so your screen stops
blanking in the middle of a game that never bothered to implement the
idle-inhibit protocol.

```
 gamepad event ──▶ evdev watcher ──▶ activity? ──▶ zwp_idle_inhibit_manager_v1
 (buttons/sticks)   /dev/input/event*     │            (river)
                                     120 s grace ──▶ inhibitor released
```

Built for **river** (classic 0.3.x and the 1.0 alpha), single ~400 KB static-ish
Rust binary, zero native dependencies beyond glibc, runs as a systemd user
service from login.

---

## Why this exists

On Wayland, the compositor decides when you are "idle". Applications are
supposed to hold a `zwp_idle_inhibit_v1` inhibitor while they are showing
something the user must see (videos, games). Most games never do, so your
idle daemon — `swayidle`, `hypridle`, … — eventually fires its timeout and
switches the outputs off mid-match, because keyboard and mouse look idle
while you play on a controller.

This daemon watches your gamepads directly through evdev and holds an idle
inhibitor on your behalf:

- **Activity based** — a button press or stick/trigger movement starts
  inhibition; it is held for a grace period (default **120 s**) after the
  last input and then released, so the screen still blanks normally when you
  put the pad down.
- **Hotplug aware** — pads can be plugged/unplugged any time; devices are
  re-scanned every few seconds.
- **Non-exclusive** — devices are opened read-only and never grabbed; games
  keep receiving every event.
- **Compositor restart safe** — if river dies and comes back, the daemon
  reconnects and rebuilds its inhibitor.

## How it talks to river

The daemon speaks the Wayland wire protocol directly (hand-rolled client, no
external Wayland crates) and uses:

- `zwp_idle_inhibit_manager_v1` (the idle-inhibit protocol river implements)
  to create/destroy `zwp_idle_inhibitor_v1` objects, and
- `zwlr_layer_shell_v1` to create the surface the inhibitor is attached to.

**The interesting part:** river only *honors* an inhibitor whose surface it
can associate with something on screen. From river's
`river/IdleInhibitManager.zig` (`checkActive()`): a bare `wl_surface` with no
role finds no scene node and is skipped — which is exactly why several
"simple idle inhibitor" clients silently do nothing on river 0.3.x (see
river issue #1079). A **mapped layer surface**, however, always counts:

```zig
.layer_surface => |layer_surface| {
    if (layer_surface.wlr_layer_surface.surface.mapped) {
        inhibited = true;
        break;
    }
},
```

So the daemon maps a **1×1, fully transparent, background-layer surface**
that is effectively invisible and inert:

- 1 pixel, alpha 0 (a single transparent `ARGB8888` shm pixel),
- anchored to the top-left corner of the **background** layer — below every
  window,
- `set_input_region(empty)` — the layer-shell spec's documented way to never
  receive pointer/touch input,
- `exclusive_zone = -1` — reserves no screen space,
- keyboard interactivity defaults to `none` — never takes focus.

The mapping sequence follows the spec exactly: `get_layer_surface` → initial
commit *without* a buffer → wait for `configure` → `ack_configure` → attach
buffer → commit. If the output the surface lives on disappears (monitor
hotplug), river sends `closed`; the daemon tears down and rebuilds the
surface on another output automatically.

This works on river classic (0.3.x, which checks `surface.mapped`) and on the
river 1.0 alpha (whose `checkActive` counts any layer surface). On other
compositors your mileage may vary — sway, for example, only counts
inhibitors whose surface belongs to a *visible view*.

## Building & installing

### Nix / flake

Linux only (evdev + Wayland). Try it without installing:

```sh
nix run github:koprzadek/controller-idle-inhibit
```

Install into your profile:

```sh
nix profile install github:koprzadek/controller-idle-inhibit
```

#### Plain binary outputs

Both package outputs are plain, unwrapped executables — no systemd unit,
no wrapper, nothing NixOS-specific. `nix build` drops a `result` symlink
with the binary at `result/bin/controller-idle-inhibit`:

```sh
nix build github:koprzadek/controller-idle-inhibit          # glibc build, for Nix/NixOS use
nix build github:koprzadek/controller-idle-inhibit#static   # fully static musl build
```

The `static` output is a single self-contained executable that runs on
*any* Linux distribution — no /nix/store, no shared libraries. Ideal for
copying to a machine without Nix, or attaching to a GitHub release:

```sh
nix build github:koprzadek/controller-idle-inhibit#static
install -Dm755 result/bin/controller-idle-inhibit ~/.local/bin/
file result/bin/controller-idle-inhibit   # "statically linked"
```

Or point your own flake at the repo and use the overlay and/or modules:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    controller-idle-inhibit.url = "github:koprzadek/controller-idle-inhibit";
    # ... plus whatever else you already have (home-manager, ...)
  };

  outputs = { self, nixpkgs, controller-idle-inhibit, ... }: {
    # Optional: expose pkgs.controller-idle-inhibit to consumers of your flake.
    overlays.default = controller-idle-inhibit.overlays.default;

    nixosConfigurations.my-machine = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ({ ... }: {
          nixpkgs.overlays = [ controller-idle-inhibit.overlays.default ];

          # Installs the package and the systemd --user unit:
          services.controller-idle-inhibit = {
            enable = true;
            grace = 120;          # seconds after the last input
            # buttonsOnly = true; # pads that stream gyro noise
          };
        })
      ];
    };

    # or with Home Manager:
    homeConfigurations.my-user = home-manager.lib.homeManagerConfiguration {
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
      modules = [
        controller-idle-inhibit.homeManagerModules.default
        ({ ... }: {
          services.controller-idle-inhibit = {
            enable = true;
            grace = 120;
          };
        })
      ];
    };
  };
}
```

Both modules define a systemd `--user` unit equivalent to the shipped
`controller-idle-inhibit.service`: bound to `graphical-session.target`,
restarted on failure, logging captured by the journal. Available options:
`enable`, `package`, `grace`, `buttonsOnly`, `verbose`, `extraArgs`.

river still has to start `graphical-session.target` itself — see
[Hooking into your river session](#hooking-into-your-river-session).

### From source

Needs Rust 1.70+ (`rustup` or your distro's `cargo`):

```sh
./install.sh            # build + install binary and systemd --user unit
./install.sh --enable   # same, but also enable the unit right away
```

or manually:

```sh
cargo build --release
install -Dm755 target/release/controller-idle-inhibit ~/.local/bin/
install -Dm644 controller-idle-inhibit.service \
    ~/.config/systemd/user/controller-idle-inhibit.service
systemctl --user daemon-reload
```

## Hooking into your river session

river does not start `graphical-session.target` by itself, so add this to
your river startup — e.g. at the end of `~/.config/river/init`:

```sh
systemctl --user import-environment WAYLAND_DISPLAY
systemctl --user start graphical-session.target
```

Then enable the service:

```sh
systemctl --user enable --now controller-idle-inhibit.service
```

Prefer not to involve systemd? You can simply spawn it from river directly:

```sh
# ~/.config/river/init
spawn "sh" "-c" "exec controller-idle-inhibit >/dev/null 2>&1 &"
```

(The systemd route is recommended: journald logs, automatic restart, clean
session lifecycle.)

## Usage

```
controller-idle-inhibit [FLAGS]

  --grace SECS      Hold inhibition this long after the last gamepad input
                    [default: 120]
  --buttons-only    Ignore stick/trigger movement; only button presses count
                    (useful for pads that stream gyro/accelerometer noise
                    while idle, e.g. Steam Controller lizard mode)
  --poll-ms MS      Gamepad/state check interval          [default: 100]
  --rescan-ms MS    Hotplug rescan interval for /dev/input [default: 5000]
  --verbose         Trace every Wayland message and state change
  -h, --help        Show help
  -V, --version     Show version
```

Example: quick release, buttons only:

```sh
controller-idle-inhibit --grace 30 --buttons-only
```

Change the service defaults by editing the `ExecStart=` line in the unit
(or drop an override with `systemctl --user edit controller-idle-inhibit`).

## Verifying it works

1. Watch the journal while pressing gamepad buttons:

   ```sh
   journalctl --user -u controller-idle-inhibit -f
   ```

   Expected lines:

   ```
   [info] gamepad connected: Microsoft Xbox Series X Controller (/dev/input/event25)
   [info] invisible layer surface mapped — idle inhibition active path ready
   [info] gamepad activity — idle inhibited
   [info] grace period elapsed — idle inhibition released
   ```

2. Confirm the protocol is there:

   ```sh
   wayland-info | grep -E "zwp_idle_inhibit|zwlr_layer_shell"
   ```

3. Run the test suite — it includes an integration test that speaks the real
   protocol over a unix socket against a fake compositor
   (`tests/mock_compositor.py`), including the output-unplug/rebuild path:

   ```sh
   cargo test
   ```

4. Manual smoke test without river (uses the fake compositor):

   ```sh
   python3 tests/mock_compositor.py /tmp/wl-test.sock /tmp/wl-test.log &
   XDG_RUNTIME_DIR=/tmp WAYLAND_DISPLAY=wl-test.sock ./target/release/controller-idle-inhibit --verbose
   cat /tmp/wl-test.log
   ```

## Troubleshooting

- **"gamepad connected" never appears.**
  The daemon needs read access to `/dev/input/event*`. On a normal seat the
  udev `uaccess` tag grants the logged-in user an ACL automatically. Check:

  ```sh
  getfacl /dev/input/event* | grep -i user
  ```

  If nothing shows up (e.g. nested/custom session), either add yourself to
  the `input` group (`sudo usermod -aG input $USER`, re-login) or the device
  may not be joystick-classified (`ls /dev/input/js*` empty → try
  `sudo modprobe joydev` and re-plug the pad).

- **The screen still blanks while playing.**
  Protocol inhibition only stops idle daemons that *ask the compositor* for
  idle state — `swayidle`, `hypridle`, anything on `ext-idle-notify-v1` (and
  wlroots' older `wlr_idle`) all do. If you blank the screen with a shell
  timer/loop or a custom script, no inhibitor can stop that; switch that
  script to use `swayidle`.

- **Screen never blanks anymore / inhibition never releases.**
  Some pads stream gyro/accelerometer events while idle. Those are axis
  events, so they count as activity. Run with `--buttons-only`, and/or check
  with `--verbose` which device keeps firing (`libinput debug-events` helps
  too).

- **Daemon exits with "compositor does not support …".**
  You are probably running it outside the river session (no
  `WAYLAND_DISPLAY`), or against a compositor without idle-inhibit. The
  systemd unit retries every 2 s, so it also survives starting before river.

## Project layout

```
├── Cargo.toml / Cargo.lock
├── flake.nix                        Nix package, overlay, NixOS + HM modules
├── nix/package.nix                  buildRustPackage expression
├── modules/                         module implementations (nixos, home-manager)
├── install.sh
├── controller-idle-inhibit.service   systemd --user unit
├── src/
│   ├── main.rs      CLI, signal handling, activity/grace state machine
│   ├── gamepad.rs   evdev watcher: gamepad detection, activity filter, hotplug
│   ├── session.rs   invisible layer surface + inhibitor lifecycle
│   ├── wayland.rs   minimal Wayland client (registry, binds, event pump)
│   ├── wire.rs      Wayland wire framing/serialization primitives
│   └── journal.rs   stdout logging (captured by the journal)
└── tests/
    ├── session.rs           integration test against the fake compositor
    └── mock_compositor.py   minimal protocol-faithful fake compositor
```

## Prior art

- [`swayidle`](https://github.com/swaywm/swayidle) — the idle daemon most
  river setups run; this tool only *inhibits*, it never blanks anything.
- [`idleinhibit`](https://github.com/0x61nas/idleinhibit) — inhibits based on
  focused/fullscreen windows or an arbitrary command.
- [`wayland-pipewire-idle-inhibit`](https://github.com/dripeas/wayland-pipewire-idle-inhibit)
  and `SwayAudioIdleInhibit` — inhibit while media plays.
- [`wayland-idle-inhibitor.py`](https://github.com/eay/wayland-idle-inhibitor.py)
  — manual tray toggle.

The niche here: **controller input** as the inhibition source, with no
window/audio heuristics needed.

## License

MIT — see `LICENSE`. The Wayland protocol knowledge encoded here comes from
the public `wayland.xml`, `wlr-layer-shell-unstable-v1.xml`,
`idle-inhibit-unstable-v1.xml` and river's GPL-licensed sources (used as
specification reference only, no code copied).
