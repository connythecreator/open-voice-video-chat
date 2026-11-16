#!/usr/bin/env bash
#
# Prove that one configuration selects the same store and control socket in the
# CLI, the TUI and the GUI.
#
# The CLI prints its answer through `ovvc config show --json`. The TUI and GUI
# have a `--print-paths` flag that runs their real argument, configuration and
# path resolution and exits before a terminal or window is created. The script
# then starts one daemon through the CLI from the same config and checks that
# the identity and socket live in the configured store and nowhere else.
#
# Usage:
#   scripts/frontend-config-check.sh [--no-build]
#
# Environment:
#   CARGO_TARGET_DIR  target directory (default: <repo>/target)

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN_DIR="$CARGO_TARGET_DIR/debug"

BUILD=1
for arg in "$@"; do
    case "$arg" in
        --no-build) BUILD=0 ;;
        *)
            echo "frontend-config-check: unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

if (( BUILD )); then
    cargo build -p ovvc-cli -p ovvc-tui -p ovvc-gui -p ovvc-daemon
fi

for bin in ovvc ovvc-tui ovvc-gui ovvc-daemon; do
    [[ -x "$BIN_DIR/$bin" ]] || {
        echo "frontend-config-check: missing $BIN_DIR/$bin" >&2
        exit 1
    }
done

WORK="$(mktemp -d "${TMPDIR:-/tmp}/ovvc-frontend-config.XXXXXX")"
DAEMON_PID=""

cleanup() {
    if [[ -n "$DAEMON_PID" ]]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

CONFIG_DIR="$WORK/config"
DATA_DIR="$WORK/custom-data"
SOCKET="$WORK/custom.sock"
mkdir -p "$CONFIG_DIR" "$DATA_DIR" "$WORK/runtime" "$WORK/home"
chmod 700 "$WORK/runtime"

cat >"$CONFIG_DIR/config.toml" <<EOF
data_dir = "$DATA_DIR"
socket = "$SOCKET"
EOF

# Keep every platform default inside the work directory, so a frontend that
# ignored the config could not touch the developer's real store.
export OVVC_CONFIG_DIR="$CONFIG_DIR"
export OVVC_SYSTEM_CONFIG="$CONFIG_DIR/system.toml"
export OVVC_DAEMON="$BIN_DIR/ovvc-daemon"
export HOME="$WORK/home"
export XDG_CONFIG_HOME="$WORK/home/.config"
export XDG_DATA_HOME="$WORK/home/.local/share"
export XDG_RUNTIME_DIR="$WORK/runtime"
unset OVVC_DATA_DIR OVVC_SOCKET OVVC_PROFILE OVVC_DATA || true

fail() {
    echo "frontend-config-check: $*" >&2
    exit 1
}

json_field() {
    python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"
}

printed_field() {
    sed -n "s/^$1=//p" <<<"$2"
}

echo "== CLI resolves the config =="
CLI_JSON="$("$BIN_DIR/ovvc" --config "$CONFIG_DIR/config.toml" config show --json)"
CLI_DATA="$(json_field data_dir <<<"$CLI_JSON")"
CLI_SOCKET="$(json_field socket <<<"$CLI_JSON")"
echo "cli  data_dir=$CLI_DATA"
echo "cli  socket=$CLI_SOCKET"

echo
echo "== TUI resolves the same paths =="
TUI_OUT="$("$BIN_DIR/ovvc-tui" --config "$CONFIG_DIR/config.toml" --print-paths)"
TUI_DATA="$(printed_field data_dir "$TUI_OUT")"
TUI_SOCKET="$(printed_field socket "$TUI_OUT")"
echo "tui  data_dir=$TUI_DATA"
echo "tui  socket=$TUI_SOCKET"

echo
echo "== GUI resolves the same paths =="
GUI_OUT="$("$BIN_DIR/ovvc-gui" --config "$CONFIG_DIR/config.toml" --print-paths)"
GUI_DATA="$(printed_field data_dir "$GUI_OUT")"
GUI_SOCKET="$(printed_field socket "$GUI_OUT")"
echo "gui  data_dir=$GUI_DATA"
echo "gui  socket=$GUI_SOCKET"

[[ "$CLI_DATA" == "$DATA_DIR" ]] || fail "CLI ignored data_dir: $CLI_DATA"
[[ "$CLI_SOCKET" == "$SOCKET" ]] || fail "CLI ignored socket: $CLI_SOCKET"
[[ "$TUI_DATA" == "$DATA_DIR" ]] || fail "TUI data_dir differs: $TUI_DATA"
[[ "$TUI_SOCKET" == "$SOCKET" ]] || fail "TUI socket differs: $TUI_SOCKET"
[[ "$GUI_DATA" == "$DATA_DIR" ]] || fail "GUI data_dir differs: $GUI_DATA"
[[ "$GUI_SOCKET" == "$SOCKET" ]] || fail "GUI socket differs: $GUI_SOCKET"

echo
echo "== flags and environment override the file =="
FLAG_DATA="$WORK/flag-data"
FLAG_SOCKET="$WORK/flag.sock"
FLAG_TUI="$("$BIN_DIR/ovvc-tui" --config "$CONFIG_DIR/config.toml" \
    --data-dir "$FLAG_DATA" --socket "$FLAG_SOCKET" --print-paths)"
[[ "$(printed_field data_dir "$FLAG_TUI")" == "$FLAG_DATA" ]] \
    || fail "TUI --data-dir did not override the config"
[[ "$(printed_field socket "$FLAG_TUI")" == "$FLAG_SOCKET" ]] \
    || fail "TUI --socket did not override the config"

ENV_DATA="$WORK/env-data"
ENV_SOCKET="$WORK/env.sock"
ENV_GUI="$(OVVC_DATA_DIR="$ENV_DATA" OVVC_SOCKET="$ENV_SOCKET" \
    "$BIN_DIR/ovvc-gui" --config "$CONFIG_DIR/config.toml" --print-paths)"
[[ "$(printed_field data_dir "$ENV_GUI")" == "$ENV_DATA" ]] \
    || fail "OVVC_DATA_DIR did not override the config"
[[ "$(printed_field socket "$ENV_GUI")" == "$ENV_SOCKET" ]] \
    || fail "OVVC_SOCKET did not override the config"

echo
echo "== one daemon, one identity, one socket =="
"$BIN_DIR/ovvc" --config "$CONFIG_DIR/config.toml" init
[[ -f "$DATA_DIR/identity.key" ]] || fail "identity.key was not written to $DATA_DIR"
[[ -S "$SOCKET" ]] || fail "the daemon did not listen on $SOCKET"
[[ -f "$DATA_DIR/daemon.json" ]] || fail "daemon.json was not written to $DATA_DIR"

read -r DAEMON_PID DAEMON_DATA < <(
    python3 -c 'import json,sys; info=json.load(open(sys.argv[1])); print(info["pid"], info["data_dir"])' \
        "$DATA_DIR/daemon.json"
)
[[ "$DAEMON_DATA" == "$DATA_DIR" ]] \
    || fail "daemon.json points at another data dir: $DAEMON_DATA"
echo "daemon pid=$DAEMON_PID data_dir=$DAEMON_DATA socket=$SOCKET"

CLI_ID="$("$BIN_DIR/ovvc" --config "$CONFIG_DIR/config.toml" id)"
TUI_ID_FILE="$DATA_DIR/identity.key"
[[ -f "$TUI_ID_FILE" ]] || fail "the TUI would not see the CLI identity"
echo "identity=$CLI_ID"

echo
echo "frontend config check passed: one config, one store, one socket"
