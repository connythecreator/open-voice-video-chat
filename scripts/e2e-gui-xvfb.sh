#!/usr/bin/env bash
#
# Run the real eframe/wgpu window under Xvfb and capture two frames:
#
#   * offline onboarding (no daemon)
#   * a populated transcript after seeding a commune through the CLI
#
# This is the only test that exercises main.rs, winit, eframe, wgpu surface
# creation, the screenshot callback and the final PNG. The headless kittest
# render tests cover the widgets; this covers the desktop shell around them.
#
# Usage:
#   scripts/e2e-gui-xvfb.sh
#
# Environment:
#   OVVC_XVFB  path to the Xvfb binary (default: Xvfb on PATH, then
#              $ROOT/.test-tools/xvfb-root/usr/bin/Xvfb)
#
# Installing Xvfb without root:
#   mkdir -p .test-tools && cd .test-tools
#   apt-get download xvfb libxfont2 xkb-data
#   for deb in *.deb; do dpkg-deb -x "$deb" xvfb-root; done
#   export OVVC_XVFB=$PWD/xvfb-root/usr/bin/Xvfb

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN_DIR="$CARGO_TARGET_DIR/debug"

XVFB="${OVVC_XVFB:-}"
if [[ -z "$XVFB" ]]; then
    XVFB="$(command -v Xvfb || true)"
fi
if [[ -z "$XVFB" ]]; then
    XVFB="$ROOT/.test-tools/xvfb-root/usr/bin/Xvfb"
fi
[[ -x "$XVFB" ]] || {
    echo "e2e-gui-xvfb: Xvfb not found; set OVVC_XVFB (see the header of this script)" >&2
    exit 1
}

if [[ ! -x "$BIN_DIR/ovvc-gui" || ! -x "$BIN_DIR/ovvc" || ! -x "$BIN_DIR/ovvc-daemon" ]]; then
    cargo build -p ovvc-gui -p ovvc-cli -p ovvc-daemon
fi
[[ -x "$BIN_DIR/ovvc-gui" ]] || { echo "e2e-gui-xvfb: ovvc-gui was not built" >&2; exit 1; }
[[ -x "$BIN_DIR/ovvc" ]] || { echo "e2e-gui-xvfb: ovvc was not built" >&2; exit 1; }
[[ -x "$BIN_DIR/ovvc-daemon" ]] || { echo "e2e-gui-xvfb: ovvc-daemon was not built" >&2; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/ovvc-gui-xvfb.XXXXXX")"
XVFB_PID=""
DAEMON_PID=""

cleanup() {
    [[ -n "$DAEMON_PID" ]] && kill "$DAEMON_PID" 2>/dev/null || true
    [[ -n "$XVFB_PID" ]] && kill "$XVFB_PID" 2>/dev/null || true
    [[ -n "$DAEMON_PID" ]] && wait "$DAEMON_PID" 2>/dev/null || true
    [[ -n "$XVFB_PID" ]] && wait "$XVFB_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

# Pick a free display. The lock file is what X itself uses.
DISPLAY_NUMBER=""
for number in $(seq 90 130); do
    if [[ ! -e "/tmp/.X${number}-lock" ]]; then
        DISPLAY_NUMBER="$number"
        break
    fi
done
[[ -n "$DISPLAY_NUMBER" ]] || { echo "e2e-gui-xvfb: no free X display" >&2; exit 1; }

mkdir -p "$WORK/runtime"
chmod 700 "$WORK/runtime"
export DISPLAY=":$DISPLAY_NUMBER"
export XDG_RUNTIME_DIR="$WORK/runtime"
# Force the X11 backend: the developer shell may have a stale Wayland socket.
unset WAYLAND_DISPLAY || true
# lavapipe is the software Vulkan device that makes this work without a GPU.
export WGPU_BACKEND=vulkan
export LIBGL_ALWAYS_SOFTWARE=1

"$XVFB" ":$DISPLAY_NUMBER" -screen 0 1280x800x24 -nolisten tcp \
    >"$WORK/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do
    if xdpyinfo -display ":$DISPLAY_NUMBER" >/dev/null 2>&1; then
        break
    fi
    sleep 0.05
done
xdpyinfo -display ":$DISPLAY_NUMBER" >/dev/null 2>&1 || {
    echo "e2e-gui-xvfb: Xvfb did not start" >&2
    cat "$WORK/xvfb.log" >&2
    exit 1
}

screenshot() {
    local data="$1" out="$2"
    env -u WAYLAND_DISPLAY \
        "$BIN_DIR/ovvc-gui" \
        --data-dir "$data" \
        --socket "$data/daemon.sock" \
        --no-spawn-daemon \
        --screenshot "$out"
}

OFFLINE="$WORK/offline"
mkdir -p "$OFFLINE"
screenshot "$OFFLINE" "$OFFLINE/shot.png" 2>"$OFFLINE/err"
[[ -s "$OFFLINE/shot.png" ]] || { cat "$OFFLINE/err" >&2; exit 1; }

ONLINE="$WORK/online"
mkdir -p "$ONLINE"
"$BIN_DIR/ovvc-daemon" --data-dir "$ONLINE" --socket "$ONLINE/daemon.sock" \
    >"$ONLINE/daemon.out" 2>"$ONLINE/daemon.err" &
DAEMON_PID=$!
for _ in $(seq 1 200); do
    [[ -S "$ONLINE/daemon.sock" ]] && break
    sleep 0.05
done
[[ -S "$ONLINE/daemon.sock" ]] || { cat "$ONLINE/daemon.err" >&2; exit 1; }

"$BIN_DIR/ovvc" --data-dir "$ONLINE" guild create "The Guild" >"$ONLINE/create.out" 2>"$ONLINE/create.err"
"$BIN_DIR/ovvc" --data-dir "$ONLINE" send "The Guild" general "hello from the GUI screenshot" \
    >"$ONLINE/send.out" 2>"$ONLINE/send.err"
"$BIN_DIR/ovvc" --data-dir "$ONLINE" send "The Guild" general "second line for the transcript" \
    >"$ONLINE/send2.out" 2>"$ONLINE/send2.err"

screenshot "$ONLINE" "$ONLINE/shot.png" 2>"$ONLINE/err"
[[ -s "$ONLINE/shot.png" ]] || { cat "$ONLINE/err" >&2; exit 1; }

python3 "$ROOT/scripts/check-gui-screenshot.py" offline "$OFFLINE/shot.png"
python3 "$ROOT/scripts/check-gui-screenshot.py" populated "$ONLINE/shot.png" \
    --compare "$OFFLINE/shot.png"

kill "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""
echo "GUI Xvfb e2e: offline and populated frames rendered and validated"
