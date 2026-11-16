#!/usr/bin/env bash
#
# Run every opt-in test that needs more than plain `cargo test`:
#
#   * GPU rasterization of the GUI widgets (egui_kittest + wgpu)
#   * the relay server's real listen/stop loop
#   * the TUI in a pseudo terminal
#   * desktop notifications over a private DBus
#   * the StatusNotifierItem tray over a private DBus
#   * the real eframe window under Xvfb
#
# Prerequisites:
#   * Vulkan software rasterizer (mesa-vulkan-drivers / lavapipe)
#   * python3-gi, dbus-run-session, gdbus, notify-send
#   * Xvfb, or OVVC_XVFB pointing at one (see scripts/e2e-gui-xvfb.sh)
#
# Usage:
#   scripts/test-all-infra.sh
#
# The DBus scripts always create their own private session bus; they never
# touch the developer's desktop notifications or tray.

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
cd "$ROOT"

echo "== GUI widget rasterization (wgpu) =="
cargo test -p ovvc-gui --test render raster::

echo "== relay server lifecycle =="
cargo test -p ovvc-server --test run

echo "== TUI pseudo terminal e2e =="
"$ROOT/scripts/e2e-tui-pty.sh"

echo "== desktop notifications (private DBus) =="
"$ROOT/scripts/e2e-dbus-notifications.sh"

echo "== tray StatusNotifierItem (private DBus) =="
"$ROOT/scripts/e2e-dbus-tray.sh"

echo "== GUI window under Xvfb =="
"$ROOT/scripts/e2e-gui-xvfb.sh"

echo "all infrastructure tests passed"
