#!/usr/bin/env bash
#
# Two-machine smoke test for ovvc, driven entirely through the real CLI binary.
#
# Two independent data directories on two loopback IPs (127.0.0.2 and
# 127.0.0.3) talk over real TCP. Covers: create/invite/join, bidirectional
# messages, and restart-with-history/reconnect.
#
# Usage:
#   scripts/e2e-two-machines.sh
#   OVVC_BIN=/path/to/ovvc scripts/e2e-two-machines.sh
#
# Notes:
#  * The redb store takes an exclusive lock while `chat` runs, so every other
#    CLI invocation on the same data directory happens while no chat process is
#    alive. The listen port is learned from a short probe chat first, then
#    rebound for the real session.
#  * This is a human-runnable companion to crates/ovvc-cli/tests/e2e.rs, not a
#    replacement for it.

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN="${OVVC_BIN:-$ROOT/target/debug/ovvc}"
GUILD="e2e-smoke"
A_IP="127.0.0.2"
B_IP="127.0.0.3"

if [[ ! -x "$BIN" ]]; then
    echo "building ovvc-cli..."
    cargo build -p ovvc-cli
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/ovvc-e2e.XXXXXX")"
A_DIR="$WORK/a"
B_DIR="$WORK/b"
PROBE_LOG="$WORK/probe.log"
A_LOG="$WORK/a.log"
B_LOG="$WORK/b.log"
A2_LOG="$WORK/a2.log"
B2_LOG="$WORK/b2.log"
PROBE_IN="$WORK/probe.in"
A_IN="$WORK/a.in"
B_IN="$WORK/b.in"
A2_IN="$WORK/a2.in"
B2_IN="$WORK/b2.in"

A_PID=""
B_PID=""

cleanup() {
    set +e
    if [[ -n "$A_PID" ]]; then
        kill "$A_PID" 2>/dev/null
        wait "$A_PID" 2>/dev/null
    fi
    if [[ -n "$B_PID" ]]; then
        kill "$B_PID" 2>/dev/null
        wait "$B_PID" 2>/dev/null
    fi
    exec 3>&- 4>&- 5>&- 6>&- 7>&- 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    local file
    for file in "$PROBE_LOG" "$A_LOG" "$B_LOG" "$A2_LOG" "$B2_LOG"; do
        [[ -f "$file" ]] || continue
        echo "--- $file ---" >&2
        sed 's/^/  /' "$file" >&2
    done
    exit 1
}

# wait_for_grep FILE PATTERN [TIMEOUT_SECONDS] [PID]
wait_for_grep() {
    local file=$1 pattern=$2 timeout=${3:-20} pid=${4:-}
    local deadline=$((SECONDS + timeout))
    while (( SECONDS < deadline )); do
        if grep -qF -- "$pattern" "$file" 2>/dev/null; then
            return 0
        fi
        if [[ -n "$pid" ]] && ! kill -0 "$pid" 2>/dev/null; then
            fail "process $pid exited while waiting for '$pattern' in $file"
        fi
        sleep 0.1
    done
    fail "timed out waiting for '$pattern' in $file"
}

dial_from_log() {
    sed -n 's/^dial: //p' "$1" | head -n 1
}

echo "working directory: $WORK"
echo "binary:            $BIN"

# ---------------------------------------------------------------------------
# 1. Create both identities, and a guild on A.
# ---------------------------------------------------------------------------
"$BIN" --data-dir "$A_DIR" init >/dev/null
"$BIN" --data-dir "$B_DIR" init >/dev/null
"$BIN" --data-dir "$A_DIR" guild create "$GUILD" >/dev/null

# ---------------------------------------------------------------------------
# 2. Learn a concrete listen address + peer id from a short probe session.
#    The port is then rebound for the real session, so the invite can be
#    created while no process holds A's store lock.
# ---------------------------------------------------------------------------
: >"$PROBE_LOG"
mkfifo "$PROBE_IN"
exec 3<>"$PROBE_IN"
"$BIN" --data-dir "$A_DIR" chat "$GUILD" --listen "/ip4/$A_IP/tcp/0" \
    <"$PROBE_IN" >"$PROBE_LOG" 2>&1 &
A_PID=$!

wait_for_grep "$PROBE_LOG" "dial: " 20 "$A_PID"
A_DIAL="$(dial_from_log "$PROBE_LOG")"
[[ -n "$A_DIAL" ]] || fail "could not parse A's dial address from $PROBE_LOG"
A_LISTEN="${A_DIAL%%/p2p/*}"
echo "A dial address:    $A_DIAL"

printf '/quit\n' >&3
wait "$A_PID" || fail "probe chat exited nonzero"
A_PID=""
exec 3>&-

INVITE="$("$BIN" --data-dir "$A_DIR" guild invite "$GUILD" --peer "$A_DIAL")"
[[ -n "$INVITE" ]] || fail "invite command produced no output"

# ---------------------------------------------------------------------------
# 3. A comes online on the port pinned into the invite, then B joins.
# ---------------------------------------------------------------------------
: >"$A_LOG"
mkfifo "$A_IN"
exec 4<>"$A_IN"
"$BIN" --data-dir "$A_DIR" chat "$GUILD" --listen "$A_LISTEN" \
    <"$A_IN" >"$A_LOG" 2>&1 &
A_PID=$!

wait_for_grep "$A_LOG" "listening: " 20 "$A_PID"
wait_for_grep "$A_LOG" "dial: $A_DIAL" 20 "$A_PID"

JOIN_OUT="$("$BIN" --data-dir "$B_DIR" guild join "$INVITE" 2>&1)" \
    || fail "B failed to join: $JOIN_OUT"
case "$JOIN_OUT" in
    *"$GUILD"*) ;;
    *) fail "join output does not mention guild '$GUILD': $JOIN_OUT" ;;
esac
echo "B joined:          ${JOIN_OUT%%$'\n'*}"

# ---------------------------------------------------------------------------
# 4. B starts chatting and dials A. Both must finish a sync.
# ---------------------------------------------------------------------------
: >"$B_LOG"
mkfifo "$B_IN"
exec 5<>"$B_IN"
"$BIN" --data-dir "$B_DIR" chat "$GUILD" --dial "$A_DIAL" \
    <"$B_IN" >"$B_LOG" 2>&1 &
B_PID=$!

wait_for_grep "$B_LOG" "synced with" 20 "$B_PID"
wait_for_grep "$A_LOG" "synced with" 20 "$A_PID"
echo "peers synced"

# ---------------------------------------------------------------------------
# 5. Bidirectional messages.
# ---------------------------------------------------------------------------
printf 'hello from A\n' >&4
wait_for_grep "$B_LOG" "hello from A" 20 "$B_PID"
echo "A -> B delivered"

printf 'hello from B\n' >&5
wait_for_grep "$A_LOG" "hello from B" 20 "$A_PID"
echo "B -> A delivered"

# ---------------------------------------------------------------------------
# 6. Restart both: history must survive and they must reconnect.
# ---------------------------------------------------------------------------
printf '/quit\n' >&4
printf '/quit\n' >&5
wait "$A_PID" || fail "A chat exited nonzero"
wait "$B_PID" || fail "B chat exited nonzero"
A_PID=""
B_PID=""
exec 4>&- 5>&-

: >"$A2_LOG"
mkfifo "$A2_IN"
exec 6<>"$A2_IN"
"$BIN" --data-dir "$A_DIR" chat "$GUILD" --listen "/ip4/$A_IP/tcp/0" \
    <"$A2_IN" >"$A2_LOG" 2>&1 &
A_PID=$!

wait_for_grep "$A2_LOG" "dial: " 20 "$A_PID"
A2_DIAL="$(dial_from_log "$A2_LOG")"
[[ -n "$A2_DIAL" ]] || fail "could not parse restarted A's dial address"

# A's startup history is local; no peer exists yet.
wait_for_grep "$A2_LOG" "hello from A" 20 "$A_PID"
wait_for_grep "$A2_LOG" "hello from B" 20 "$A_PID"
echo "A startup history intact"

: >"$B2_LOG"
mkfifo "$B2_IN"
exec 7<>"$B2_IN"
"$BIN" --data-dir "$B_DIR" chat "$GUILD" --dial "$A2_DIAL" \
    <"$B2_IN" >"$B2_LOG" 2>&1 &
B_PID=$!

wait_for_grep "$B2_LOG" "hello from A" 20 "$B_PID"
wait_for_grep "$B2_LOG" "hello from B" 20 "$B_PID"
wait_for_grep "$B2_LOG" "synced with" 20 "$B_PID"
wait_for_grep "$A2_LOG" "synced with" 20 "$A_PID"
echo "B startup history intact and peers reconnected"

printf '/quit\n' >&6
printf '/quit\n' >&7
wait "$A_PID" || fail "restarted A chat exited nonzero"
wait "$B_PID" || fail "restarted B chat exited nonzero"
A_PID=""
B_PID=""

echo "PASS: two-machine create/join/send/restart smoke test"
