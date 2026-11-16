#!/usr/bin/env bash
#
# End-to-end TUI run through a real pseudo terminal:
# onboarding -> create commune -> /help -> send a message -> /whoami -> Ctrl+C.
#
# This covers the crossterm event loop, raw mode and alternate screen that the
# ratatui `TestBackend` unit tests cannot reach.
#
# Usage:
#   scripts/e2e-tui-pty.sh
#
# Environment:
#   OVVC_TUI_BIN  binary to drive (default target/debug/ovvc-tui)

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${OVVC_TUI_BIN:-$CARGO_TARGET_DIR/debug/ovvc-tui}"

if [[ ! -x "$BIN" || ! -x "$(dirname "$BIN")/ovvc-daemon" ]]; then
    cargo build -p ovvc-tui -p ovvc-daemon
fi

[[ -x "$BIN" ]] || {
    echo "e2e-tui-pty: $BIN is missing" >&2
    exit 1
}

exec python3 "$ROOT/scripts/pty-tui-e2e.py" "$BIN"
