#!/usr/bin/env bash
# Install controller-idle-inhibit for the current user:
#   - builds with cargo when available (otherwise falls back to the bundled
#     prebuilt binary in bin/, which needs glibc >= 2.34),
#   - installs the binary to ~/.local/bin,
#   - prints how to start it from your compositor session.
#
# Usage: ./install.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin_dir="$HOME/.local/bin"

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

echo "==> installed: $bin_dir/controller-idle-inhibit"

cat <<'EOF'

Next steps
----------
Start the daemon from inside your Wayland session so it can see
WAYLAND_DISPLAY — e.g. at the end of ~/.config/river/init:

    spawn "sh" "-c" "exec controller-idle-inhibit >/dev/null 2>&1 &"

Any session autostart works the same way (sway `exec`, hyprland
`exec-once`, ...). If the compositor is not up yet, the daemon logs
"compositor not ready" and retries every 2 s until it is.

Logs go to stdout: run it in a terminal first to watch the log lines while
pressing buttons, or redirect to a file to keep a trail.
EOF
