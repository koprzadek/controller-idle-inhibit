#!/usr/bin/env bash
# Install controller-idle-inhibit for the current user:
#   - builds with cargo when available (otherwise falls back to the bundled
#     prebuilt binary in bin/, which needs glibc >= 2.34),
#   - installs the binary to ~/.local/bin,
#   - installs the systemd --user unit,
#   - prints the two-line river setup.
#
# Usage: ./install.sh            (build + install)
#        ./install.sh --enable   (build + install + enable the unit now)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin_dir="$HOME/.local/bin"
unit_dir="$HOME/.config/systemd/user"

enable=0
[ "${1:-}" = "--enable" ] && enable=1

if ! command -v cargo >/dev/null 2>&1 && [ -x "$HOME/.cargo/bin/cargo" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

if command -v cargo >/dev/null 2>&1; then
    echo "==> building with cargo"
    (cd "$here" && cargo build --release)
    src_bin="$here/target/release/controller-idle-inhibit"
elif [ -x "$here/bin/controller-idle-inhibit" ]; then
    echo "==> cargo not found; using bundled prebuilt binary (needs glibc >= 2.34)"
    src_bin="$here/bin/controller-idle-inhibit"
else
    echo "error: no cargo toolchain and no prebuilt binary in bin/." >&2
    echo "       install Rust first:  https://rustup.rs" >&2
    exit 1
fi

install -Dm755 "$src_bin" "$bin_dir/controller-idle-inhibit"
install -Dm644 "$here/controller-idle-inhibit.service" \
    "$unit_dir/controller-idle-inhibit.service"
systemctl --user daemon-reload

echo "==> binary:  $bin_dir/controller-idle-inhibit"
echo "==> unit:    $unit_dir/controller-idle-inhibit.service"

cat <<'EOF'

Next steps
----------
1. Make your river session publish itself to systemd. Add these two lines
   to your river startup (e.g. the end of ~/.config/river/init):

       systemctl --user import-environment WAYLAND_DISPLAY
       systemctl --user start graphical-session.target

2. Enable the daemon:

       systemctl --user enable --now controller-idle-inhibit.service

3. Plug in a gamepad, press a button, and watch:

       journalctl --user -u controller-idle-inhibit -f

   You should see "gamepad activity — idle inhibited" and your idle daemon
   (swayidle/hypridle/...) should no longer blank the screen while you play.
EOF

if [ "$enable" = 1 ]; then
    echo "==> enabling unit"
    systemctl --user enable --now controller-idle-inhibit.service
fi
