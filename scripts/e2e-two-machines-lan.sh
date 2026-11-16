#!/usr/bin/env bash
#
# Real cross-machine smoke test: machine A runs locally, machine B is reached
# over SSH. Exercises the actual CLI binary on both hosts over the LAN.
#
# Covers: create/invite/join, bidirectional messages, reply/mention propagation
# (including the mention notification on B), edit/delete propagation, restart
# with history, offline catch-up, and a post-restart B -> A message. Checks that
# the listen port does not accumulate connections (the old dial-storm
# regression).
#
# Usage:
#   scripts/e2e-two-machines-lan.sh            # use the current B binary
#   scripts/e2e-two-machines-lan.sh --stage    # copy target/release/ovvc to B first
#
# Environment overrides:
#   OVVC_BIN   local binary            (default target/release/ovvc)
#   B_HOST     ssh destination         (required, for example user@host)
#   B_BIN      remote binary path      (default ~/ovvc-lan/ovvc)
#   B_DAEMON   remote daemon path      (default <B_BIN dir>/ovvc-daemon)
#   B_DATA_DIR remote data directory   (default /tmp/ovvc-lan-b)
#   A_IP       A's LAN address         (required, for example 10.0.0.5)
#   PORT       A's listen port         (default 41000)
#   GUILD      guild name              (default lan-e2e)
#
# Prerequisites: ssh key auth to B, B can reach A on $PORT, and A's firewall
# allows the port. B needs no Rust toolchain: `--stage` copies both binaries,
# because the CLI spawns a sibling `ovvc-daemon` when no daemon is listening.

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${OVVC_BIN:-$ROOT/target/release/ovvc}"
DAEMON_BIN="$(dirname "$BIN")/ovvc-daemon"
B_HOST="${B_HOST:?set B_HOST to the ssh destination for machine B}"
B_BIN="${B_BIN:-~/ovvc-lan/ovvc}"
B_DAEMON="${B_DAEMON:-$(dirname "$B_BIN")/ovvc-daemon}"
B_DATA_DIR="${B_DATA_DIR:-/tmp/ovvc-lan-b}"
A_IP="${A_IP:?set A_IP to the LAN address of machine A}"
PORT="${PORT:-41000}"
GUILD="${GUILD:-lan-e2e}"

STAGE=0
if [[ "${1:-}" == "--stage" ]]; then
    STAGE=1
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/ovvc-lan.XXXXXX")"
A_DIR="$WORK/a"
A_IN="$WORK/a.in"
B_IN="$WORK/b.in"
B2_IN="$WORK/b2.in"
A_LOG="$WORK/a.log"
A2_LOG="$WORK/a2.log"
A3_LOG="$WORK/a3.log"
B_LOG="$WORK/b.log"
B2_LOG="$WORK/b2.log"
B3_LOG="$WORK/b3.log"
A_PID=""
B_PID=""

ssh_b() {
    ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=10 "$B_HOST" "$@"
}

# The CLI starts a sibling daemon and exits, so the daemon does not share the
# chat process's pid. Each data directory records its daemon in daemon.json;
# without that kill, every run leaves a daemon behind holding the binary, and
# the next `--stage` cannot replace it ("Text file busy").
daemon_pid_from() {
    sed -n 's/.*"pid": *\([0-9][0-9]*\).*/\1/p' "$1" 2>/dev/null | head -n 1
}

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
    local a_daemon
    a_daemon=$(daemon_pid_from "$A_DIR/daemon.json")
    [[ -n "$a_daemon" ]] && kill "$a_daemon" 2>/dev/null
    ssh_b "bash -s -- '$B_DATA_DIR'" <<'REMOTE_CLEANUP' 2>/dev/null
dir=$1
pid=$(sed -n 's/.*"pid": *\([0-9][0-9]*\).*/\1/p' "$dir/daemon.json" 2>/dev/null | head -n 1)
[ -n "$pid" ] && kill "$pid" 2>/dev/null
pid=$(cat "$dir.pid" 2>/dev/null)
[ -n "$pid" ] && kill "$pid" 2>/dev/null
rm -f "$dir.pid"
rm -rf "$dir"
REMOTE_CLEANUP
    exec 3>&- 4>&- 5>&- 6>&- 7>&- 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    local f
    for f in "$A_LOG" "$A2_LOG" "$A3_LOG" "$B_LOG" "$B2_LOG" "$B3_LOG"; do
        [[ -f "$f" ]] || continue
        echo "--- $f ---" >&2
        sed 's/^/  /' "$f" >&2
    done
    exit 1
}

wait_for_grep() {
    local file=$1 pattern=$2 timeout=${3:-30} pid=${4:-}
    local deadline=$((SECONDS + timeout))
    while (( SECONDS < deadline )); do
        if grep -qF -- "$pattern" "$file" 2>/dev/null; then
            return 0
        fi
        if [[ -n "$pid" ]] && ! kill -0 "$pid" 2>/dev/null; then
            fail "process $pid exited while waiting for '$pattern' in $file"
        fi
        sleep 0.2
    done
    fail "timed out waiting for '$pattern' in $file"
}

dial_from_log() {
    sed -n 's/^dial: //p' "$1" | head -n 1
}

[[ -x "$BIN" ]] || { echo "missing $BIN (run: cargo build --release)" >&2; exit 1; }
[[ -x "$DAEMON_BIN" ]] || { echo "missing $DAEMON_BIN (run: cargo build --release)" >&2; exit 1; }
ssh_b "test -x $B_BIN" || { echo "B binary missing: $B_BIN (run with --stage)" >&2; exit 1; }

if (( STAGE )); then
    echo "== staging $BIN to $B_HOST:$B_BIN =="
    ssh_b "mkdir -p \$(dirname $B_BIN)"
    scp -q -o BatchMode=yes -o ConnectTimeout=10 "$BIN" "$B_HOST:$B_BIN"
    echo "== staging $DAEMON_BIN to $B_HOST:$B_DAEMON =="
    scp -q -o BatchMode=yes -o ConnectTimeout=10 "$DAEMON_BIN" "$B_HOST:$B_DAEMON"
    ssh_b "chmod +x $B_BIN $B_DAEMON"
fi

ssh_b "test -x $B_DAEMON" || { echo "B daemon missing: $B_DAEMON (run with --stage)" >&2; exit 1; }

echo "== preparing =="
rm -rf "$A_DIR"
ssh_b "rm -rf $B_DATA_DIR && mkdir -p $B_DATA_DIR"

"$BIN" --data-dir "$A_DIR" init >/dev/null
"$BIN" --data-dir "$A_DIR" guild create "$GUILD" >/dev/null
ssh_b "$B_BIN --data-dir $B_DATA_DIR init" >/dev/null

echo "== probe A listen address =="
mkfifo "$A_IN"
exec 3<>"$A_IN"
"$BIN" --data-dir "$A_DIR" chat "$GUILD" --listen "/ip4/$A_IP/tcp/$PORT" <"$A_IN" >"$WORK/probe.log" 2>&1 &
A_PID=$!
wait_for_grep "$WORK/probe.log" "dial: " 30 "$A_PID"
A_DIAL=$(dial_from_log "$WORK/probe.log")
[[ -n "$A_DIAL" ]] || fail "no dial address from probe"
echo "A dial: $A_DIAL"
printf '/quit\n' >&3
wait "$A_PID" || fail "probe chat exited nonzero"
A_PID=""
exec 3>&-

echo "== invite =="
INVITE=$("$BIN" --data-dir "$A_DIR" guild invite "$GUILD" --peer "$A_DIAL")
[[ -n "$INVITE" ]] || fail "empty invite"
echo "invite length: ${#INVITE}"

# Sent offline so its event id is available to the chat commands below. B
# pulls it during the initial sync.
echo "== seed an editable message =="
EDIT_ID=$("$BIN" --data-dir "$A_DIR" send "$GUILD" general "editable from A" | awk '{print $2}')
[[ -n "$EDIT_ID" ]] || fail "no event id from the seed message"
echo "editable message: $EDIT_ID"

echo "== start A =="
: >"$A_LOG"
exec 4<>"$A_IN"
"$BIN" --data-dir "$A_DIR" chat "$GUILD" --listen "/ip4/$A_IP/tcp/$PORT" <"$A_IN" >"$A_LOG" 2>&1 &
A_PID=$!
wait_for_grep "$A_LOG" "listening: " 30 "$A_PID"
wait_for_grep "$A_LOG" "dial: $A_DIAL" 30 "$A_PID"

echo "== B join =="
JOIN_OUT=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR guild join '$INVITE'" 2>&1) \
    || fail "B join failed: $JOIN_OUT"
case "$JOIN_OUT" in
    *"$GUILD"*) ;;
    *) fail "join output does not mention guild '$GUILD': $JOIN_OUT" ;;
esac
echo "B joined: ${JOIN_OUT%%$'\n'*}"

echo "== start B =="
: >"$B_LOG"
mkfifo "$B_IN"
exec 5<>"$B_IN"
ssh_b "bash -c 'echo \$\$ > $B_DATA_DIR.pid; exec $B_BIN --data-dir $B_DATA_DIR chat \"$GUILD\" --dial \"$A_DIAL\"'" <&5 >"$B_LOG" 2>&1 &
B_PID=$!
wait_for_grep "$B_LOG" "synced with" 40 "$B_PID"
wait_for_grep "$A_LOG" "synced with" 40 "$A_PID"
echo "peers synced"

echo "== bidirectional messages =="
printf 'hello from A\n' >&4
wait_for_grep "$B_LOG" "hello from A" 30 "$B_PID"
printf 'hello from B\n' >&5
wait_for_grep "$A_LOG" "hello from B" 30 "$A_PID"
echo "A -> B and B -> A delivered"

wait_for_grep "$B_LOG" "editable from A" 30 "$B_PID"

echo "== reply and mention propagate =="
# B needs a roster name before A can resolve @bob; A waits until the rename has
# synced, because mention resolution happens against A's local roster.
printf '/name bob\n' >&5
wait_for_grep "$B_LOG" "display name set to bob" 30 "$B_PID"
printf '/members\n' >&4
wait_for_grep "$A_LOG" "bob" 30 "$A_PID"
printf '/reply %s reply from A @bob\n' "$EDIT_ID" >&4
wait_for_grep "$B_LOG" "reply from A" 30 "$B_PID"
wait_for_grep "$B_LOG" " (+1 mentioned)" 30 "$B_PID"
wait_for_grep "$B_LOG" "mentioned you" 30 "$B_PID"
echo "reply and mention notification reached B"

echo "== edit and delete propagate =="
printf '/edit %s edited from A\n' "$EDIT_ID" >&4
wait_for_grep "$B_LOG" "edited a message" 30 "$B_PID"
printf '/delete %s\n' "$EDIT_ID" >&4
wait_for_grep "$B_LOG" "deleted a message" 30 "$B_PID"
echo "edit and delete reached B"

CONNS=$(ss -tn state established "( sport = :$PORT )" 2>/dev/null | tail -n +2 | wc -l)
echo "established connections on A port $PORT: $CONNS"
(( CONNS <= 10 )) || fail "possible dial storm: $CONNS connections"

echo "== restart both =="
printf '/quit\n' >&4
printf '/quit\n' >&5
wait "$A_PID" || fail "A quit nonzero"
A_PID=""
wait "$B_PID" || fail "B quit nonzero"
B_PID=""
exec 4>&- 5>&-

: >"$A2_LOG"
exec 6<>"$A_IN"
"$BIN" --data-dir "$A_DIR" chat "$GUILD" --listen "/ip4/$A_IP/tcp/$PORT" <"$A_IN" >"$A2_LOG" 2>&1 &
A_PID=$!
wait_for_grep "$A2_LOG" "dial: " 30 "$A_PID"
A2_DIAL=$(dial_from_log "$A2_LOG")
[[ -n "$A2_DIAL" ]] || fail "no restarted dial address"
wait_for_grep "$A2_LOG" "hello from A" 20 "$A_PID"
wait_for_grep "$A2_LOG" "hello from B" 20 "$A_PID"
echo "A history intact"

echo "== offline catch-up =="
printf 'offline from A\n' >&6
wait_for_grep "$A2_LOG" "offline from A" 20 "$A_PID"

: >"$B2_LOG"
mkfifo "$B2_IN"
exec 7<>"$B2_IN"
ssh_b "bash -c 'echo \$\$ > $B_DATA_DIR.pid; exec $B_BIN --data-dir $B_DATA_DIR chat \"$GUILD\" --dial \"$A2_DIAL\"'" <&7 >"$B2_LOG" 2>&1 &
B_PID=$!
wait_for_grep "$B2_LOG" "hello from A" 40 "$B_PID"
wait_for_grep "$B2_LOG" "hello from B" 40 "$B_PID"
wait_for_grep "$B2_LOG" "offline from A" 40 "$B_PID"
wait_for_grep "$B2_LOG" "synced with" 40 "$B_PID"
wait_for_grep "$A2_LOG" "synced with" 40 "$A_PID"
echo "B history intact; offline catch-up delivered"

printf 'hello again from B\n' >&7
wait_for_grep "$A2_LOG" "hello again from B" 30 "$A_PID"
echo "B -> A after restart delivered"

printf '/quit\n' >&6
printf '/quit\n' >&7
wait "$A_PID" || fail "restarted A quit nonzero"
A_PID=""
wait "$B_PID" || fail "restarted B quit nonzero"
B_PID=""

echo "== B folds the delete after restart =="
B_HISTORY=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR history '$GUILD' general")
case "$B_HISTORY" in
    *"<message deleted>"*) ;;
    *) fail "B history does not show the delete tombstone: $B_HISTORY" ;;
esac
case "$B_HISTORY" in
    *"edited from A"*) fail "B history still shows the deleted body: $B_HISTORY" ;;
    *) ;;
esac
case "$B_HISTORY" in
    *"replying to"*"<message deleted>"*) ;;
    *) fail "B history lost the reply's tombstone context: $B_HISTORY" ;;
esac
case "$B_HISTORY" in
    *"reply from A @bob (+1 mentioned)"*) ;;
    *) fail "B history lost the reply body or mentions: $B_HISTORY" ;;
esac
echo "B history is the folded view, with the reply context intact"

# Both stores are synced and both chats are stopped, so one post from each side
# is written at (or near) the same guild Lamport value. This is the offline tie
# the automated loopback test pins down exactly; here the point is that two
# independent replicas still fold it into the same order and neither duplicates
# the other side's event.
echo "== offline posts from both sides =="
"$BIN" --data-dir "$A_DIR" send "$GUILD" general "tie from A" >/dev/null
TIE_B=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR send '$GUILD' general 'tie from B'")
[[ -n "$TIE_B" ]] || fail "B could not post offline"
echo "both offline posts written"

: >"$A3_LOG"
exec 4<>"$A_IN"
"$BIN" --data-dir "$A_DIR" chat "$GUILD" --listen "/ip4/$A_IP/tcp/$PORT" <"$A_IN" >"$A3_LOG" 2>&1 &
A_PID=$!
wait_for_grep "$A3_LOG" "dial: " 30 "$A_PID"
A3_DIAL=$(dial_from_log "$A3_LOG")
[[ -n "$A3_DIAL" ]] || fail "no dial address after the tie posts"

: >"$B3_LOG"
exec 5<>"$B_IN"
ssh_b "bash -c 'echo \$\$ > $B_DATA_DIR.pid; exec $B_BIN --data-dir $B_DATA_DIR chat \"$GUILD\" --dial \"$A3_DIAL\"'" <&5 >"$B3_LOG" 2>&1 &
B_PID=$!
wait_for_grep "$A3_LOG" "tie from B" 40 "$A_PID"
wait_for_grep "$B3_LOG" "tie from A" 40 "$B_PID"
wait_for_grep "$A3_LOG" "synced with" 40 "$A_PID"
wait_for_grep "$B3_LOG" "synced with" 40 "$B_PID"

A_ORDER=$("$BIN" --data-dir "$A_DIR" history "$GUILD" general | grep -F "tie from" || true)
B_ORDER=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR history '$GUILD' general" | grep -F "tie from" || true)
[[ "$(printf '%s\n' "$A_ORDER" | grep -c 'tie from')" -eq 2 ]] \
    || fail "A does not hold both tie posts exactly once: $A_ORDER"
[[ "$(printf '%s\n' "$B_ORDER" | grep -c 'tie from')" -eq 2 ]] \
    || fail "B does not hold both tie posts exactly once: $B_ORDER"
[[ "$A_ORDER" == "$B_ORDER" ]] \
    || fail "replicas disagree on tie order: A=[$A_ORDER] B=[$B_ORDER]"
echo "both replicas hold the tie once and in the same order"

printf '/quit\n' >&4
printf '/quit\n' >&5
wait "$A_PID" || fail "tie A quit nonzero"
A_PID=""
wait "$B_PID" || fail "tie B quit nonzero"
B_PID=""
exec 4>&- 5>&-

echo "PASS: LAN two-machine create/join/send/reply/mention/edit/delete/restart/offline-catchup/tie-convergence"
