#!/usr/bin/env bash
#
# Exercise the real StatusNotifierItem: the daemon's tray task registers with a
# private org.kde.StatusNotifierWatcher, publishes its title/icon/status and a
# dbusmenu with Open/status/Quit entries. The test clicks "Open ovvc" with a
# deliberately missing GUI binary (the error path) and then "Quit" to prove the
# menu callback shuts the daemon down.
#
# Usage:
#   scripts/e2e-dbus-tray.sh
#
# Always runs on a private session bus created by `dbus-run-session`.

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "${OVVC_DBUS_UNDER_TEST:-}" != "1" ]]; then
    exec dbus-run-session -- env \
        OVVC_DBUS_UNDER_TEST=1 \
        OVVC_RUN_DBUS_TESTS=1 \
        "$0" "$@"
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

python3 -c 'import gi' >/dev/null 2>&1 || {
    echo "e2e-dbus-tray: python3-gi is required" >&2
    exit 1
}
command -v gdbus >/dev/null || {
    echo "e2e-dbus-tray: gdbus is required" >&2
    exit 1
}

cd "$ROOT"
cargo test -p ovvc-daemon --test dbus_services -- --nocapture --test-threads=1
