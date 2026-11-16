#!/usr/bin/env bash
#
# Exercise the daemon's real desktop-notification path:
#
#   daemon notifier task -> notify-rust -> org.freedesktop.Notifications
#
# A mock service records each call. The Rust test sends a mention and a reply,
# checks the summary/body formatting, then disables the preference and checks
# that no further call reaches DBus.
#
# Usage:
#   scripts/e2e-dbus-notifications.sh
#
# The script re-executes itself under `dbus-run-session`, so it never touches
# the developer's real session bus or notification daemon.

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

# Always use a private bus. The developer shell normally has a real session
# bus with a live notification daemon; reusing it would put test popups on the
# desktop and would test the wrong service.
if [[ "${OVVC_DBUS_UNDER_TEST:-}" != "1" ]]; then
    exec dbus-run-session -- env OVVC_DBUS_UNDER_TEST=1 "$0" "$@"
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/ovvc-notify.XXXXXX")"
PROBE_PID=""

cleanup() {
    if [[ -n "$PROBE_PID" ]]; then
        kill "$PROBE_PID" 2>/dev/null || true
        wait "$PROBE_PID" 2>/dev/null || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

python3 -c 'import gi' >/dev/null 2>&1 || {
    echo "e2e-dbus-notifications: python3-gi is required" >&2
    exit 1
}

export OVVC_NOTIFY_LOG="$WORK/notifications.jsonl"
export OVVC_NOTIFY_READY="$WORK/ready"
python3 "$ROOT/scripts/dbus-notify-probe.py" >"$WORK/probe.out" 2>"$WORK/probe.err" &
PROBE_PID=$!

for _ in $(seq 1 100); do
    [[ -f "$OVVC_NOTIFY_READY" ]] && break
    sleep 0.05
done
[[ -f "$OVVC_NOTIFY_READY" ]] || {
    echo "e2e-dbus-notifications: probe did not take the name" >&2
    cat "$WORK/probe.err" >&2
    exit 1
}

cd "$ROOT"
cargo test -p ovvc-daemon --bin ovvc-daemon \
    notifications::tests::desktop_notifications_reach_the_dbus_service \
    -- --nocapture --test-threads=1
