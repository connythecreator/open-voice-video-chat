#!/usr/bin/env bash
#
# Cross-machine stability test for the daemon-backed stack through Phase 2.5.
# Machine A runs locally; machine B is reached over SSH. Both sides run the real
# release binaries, not test doubles.
#
# Covers, with asserts on real output:
#   A. Attachments both ways: a 2 MiB blob from A and a 512 KiB blob from B,
#      fetched over the peer connection and verified by BLAKE3 on both sides.
#   B. Channel creation from A propagating to B, and two-way messages in it.
#   C. Search parity on both replicas for a unique token.
#   D. B's daemon SIGKILLed mid-session, restarted, and its still-running chat
#      client reconnects without restarting (both directions keep flowing).
#   E. A daemon down while A posts a 300-message burst; B restarts, catches up
#      exactly once and in the same order; a second restart adds no duplicates.
#   F. A's daemon SIGKILLed and restarted; it resumes its listener on the same
#      port and B reconnects while both clients stay up.
#   G. A 30-round ping-pong soak with a connection-count check.
#
# Usage:
#   scripts/stability-two-machines.sh           # use the staged B binaries
#   scripts/stability-two-machines.sh --stage   # copy release binaries to B
#
# Environment overrides:
#   OVVC_BIN   local binary            (default target/release/ovvc)
#   B_HOST     ssh destination         (required, for example user@host)
#   B_BIN      remote binary path      (default ~/ovvc-stability/ovvc)
#   B_DAEMON   remote daemon path      (default <B_BIN dir>/ovvc-daemon)
#   B_DATA_DIR remote data directory   (default /tmp/ovvc-stability-b)
#   A_IP       A's LAN address         (required, for example 10.0.0.5)
#   PORT       A's listen port         (default 41100)
#   GUILD      commune name            (default stability-e2e)
#   BURST      offline burst size      (default 300)
#   ROUNDS     ping-pong rounds        (default 30)
#
# The CLI starts a sibling `ovvc-daemon` when no daemon is listening, so B
# needs both binaries; `--stage` copies them. B's data directory is wiped
# before and after the run, and only daemons recorded in that directory (and
# in A's temporary directory) are stopped.

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${OVVC_BIN:-$ROOT/target/release/ovvc}"
DAEMON_BIN="$(dirname "$BIN")/ovvc-daemon"
B_HOST="${B_HOST:?set B_HOST to the ssh destination for machine B}"
B_BIN="${B_BIN:-~/ovvc-stability/ovvc}"
B_DAEMON="${B_DAEMON:-${B_BIN%/*}/ovvc-daemon}"
B_DATA_DIR="${B_DATA_DIR:-/tmp/ovvc-stability-b}"
A_IP="${A_IP:?set A_IP to the LAN address of machine A}"
PORT="${PORT:-41100}"
GUILD="${GUILD:-stability-e2e}"
BURST="${BURST:-300}"
ROUNDS="${ROUNDS:-30}"

STAGE=0
if [[ "${1:-}" == "--stage" ]]; then
    STAGE=1
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/ovvc-stability.XXXXXX")"
A_DIR="$WORK/a"
A_IN="$WORK/a.in"
B_IN="$WORK/b.in"
A_LOG="$WORK/a.log"
B_LOG="$WORK/b.log"
A_DAEMON_LOG="$WORK/a-daemon.log"
B_DAEMON_LOG="$B_DATA_DIR.daemon.log"
A_PID=""
B_PID=""

ssh_b() {
    ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=10 "$B_HOST" "$@"
}

say() {
    printf '%s\n' "$*"
}

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    local f
    for f in "$A_LOG" "$B_LOG" "$A_DAEMON_LOG"; do
        [[ -f "$f" ]] || continue
        printf -- '--- %s (tail) ---\n' "$f" >&2
        tail -n 40 "$f" | sed 's/^/  /' >&2
    done
    printf -- '--- %s (tail) ---\n' "$B_DAEMON_LOG" >&2
    ssh_b "tail -n 40 $B_DAEMON_LOG 2>/dev/null" | sed 's/^/  /' >&2 || true
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
    fail "timed out after ${timeout}s waiting for '$pattern' in $file"
}

dial_from_log() {
    sed -n 's/^dial: //p' "$1" | head -n 1
}

sha256_of() {
    sha256sum "$1" | awk '{print $1}'
}

# --- daemon pids and restarts -------------------------------------------------

local_daemon_pid() {
    sed -n 's/.*"pid": *\([0-9][0-9]*\).*/\1/p' "$A_DIR/daemon.json" 2>/dev/null | head -n 1
}

remote_daemon_pid() {
    ssh_b "sed -n 's/.*\"pid\": *\\([0-9][0-9]*\\).*/\\1/p' $B_DATA_DIR/daemon.json 2>/dev/null | head -n 1"
}

wait_remote_daemon() {
    local old=${1:-}
    local deadline=$((SECONDS + 60))
    while (( SECONDS < deadline )); do
        local pid
        pid=$(remote_daemon_pid)
        if [[ -n "$pid" && "$pid" != "$old" ]] && ssh_b "kill -0 $pid" 2>/dev/null; then
            return 0
        fi
        sleep 0.3
    done
    fail "remote daemon did not come back"
}

kill_remote_daemon() {
    local pid
    pid=$(remote_daemon_pid)
    [[ -n "$pid" ]] || fail "no remote daemon pid in $B_DATA_DIR/daemon.json"
    ssh_b "kill -9 $pid" || fail "could not SIGKILL remote daemon $pid"
    local deadline=$((SECONDS + 20))
    while (( SECONDS < deadline )); do
        if ! ssh_b "kill -0 $pid" 2>/dev/null; then
            return 0
        fi
        sleep 0.2
    done
    fail "remote daemon $pid did not die"
}

start_remote_daemon() {
    : >"$B_DAEMON_LOG"
    ssh_b "setsid nohup $B_DAEMON --data-dir $B_DATA_DIR --online >$B_DAEMON_LOG 2>&1 < /dev/null & echo started" >/dev/null
}

kill_local_daemon() {
    local pid
    pid=$(local_daemon_pid)
    [[ -n "$pid" ]] || fail "no local daemon pid in $A_DIR/daemon.json"
    kill -9 "$pid" 2>/dev/null || fail "could not SIGKILL local daemon $pid"
    local deadline=$((SECONDS + 20))
    while (( SECONDS < deadline )); do
        if ! kill -0 "$pid" 2>/dev/null; then
            return 0
        fi
        sleep 0.1
    done
    fail "local daemon $pid did not die"
}

wait_local_daemon() {
    local old=${1:-}
    local deadline=$((SECONDS + 60))
    while (( SECONDS < deadline )); do
        local pid
        pid=$(local_daemon_pid)
        if [[ -n "$pid" && "$pid" != "$old" ]] && kill -0 "$pid" 2>/dev/null; then
            return 0
        fi
        sleep 0.2
    done
    fail "local daemon did not come back"
}

start_local_daemon() {
    : >"$A_DAEMON_LOG"
    setsid nohup "$DAEMON_BIN" --data-dir "$A_DIR" --online >"$A_DAEMON_LOG" 2>&1 </dev/null &
}

# Send one probe direction until the far side logs it, tolerating the
# "daemon is not reachable" errors the client prints while it reconnects.
probe_until() {
    local from_fd=$1 to_log=$2 pattern=$3 timeout=${4:-60}
    local deadline=$((SECONDS + timeout))
    local i=0
    while (( SECONDS < deadline )); do
        i=$((i + 1))
        printf '%s-%d\n' "$pattern" "$i" >&"$from_fd"
        local until=$((SECONDS + 2))
        while (( SECONDS < until )); do
            if grep -qF -- "$pattern-" "$to_log" 2>/dev/null; then
                return 0
            fi
            sleep 0.2
        done
    done
    fail "no '$pattern-*' crossed after ${timeout}s"
}

remote_marker_list() {
    local prefix=$1
    ssh_b "$B_BIN --data-dir $B_DATA_DIR history '$GUILD' general -l 2000" \
        | grep -oE "${prefix}-[0-9]+" || true
}

local_marker_list() {
    local prefix=$1
    "$BIN" --data-dir "$A_DIR" history "$GUILD" general -l 2000 \
        | grep -oE "${prefix}-[0-9]+" || true
}

cleanup() {
    set +e
    if [[ -n "$A_PID" ]]; then
        kill "$A_PID" 2>/dev/null
        wait "$A_PID" 2>/dev/null
    fi
    if [[ -n "$B_PID" ]]; then
        printf '/quit\n' >&5 2>/dev/null
        wait "$B_PID" 2>/dev/null
    fi
    local pid
    pid=$(local_daemon_pid)
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null
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

reset_remote_data_dir() {
    ssh_b "bash -s -- '$B_DATA_DIR'" <<'REMOTE_RESET' 2>/dev/null || true
dir=$1
pid=$(sed -n 's/.*"pid": *\([0-9][0-9]*\).*/\1/p' "$dir/daemon.json" 2>/dev/null | head -n 1)
[ -n "$pid" ] && kill "$pid" 2>/dev/null
pid=$(cat "$dir.pid" 2>/dev/null)
[ -n "$pid" ] && kill "$pid" 2>/dev/null
rm -f "$dir.pid"
rm -rf "$dir"
mkdir -p "$dir"
REMOTE_RESET
}

# --- preconditions and staging ------------------------------------------------

[[ -x "$BIN" ]] || { echo "missing $BIN (run: cargo build --release)" >&2; exit 1; }
[[ -x "$DAEMON_BIN" ]] || { echo "missing $DAEMON_BIN (run: cargo build --release)" >&2; exit 1; }

if (( STAGE )); then
    echo "== clearing $B_HOST:$B_DATA_DIR before staging =="
    reset_remote_data_dir
    echo "== staging $BIN to $B_HOST:$B_BIN =="
    ssh_b "mkdir -p \$(dirname $B_BIN)"
    scp -q -o BatchMode=yes -o ConnectTimeout=10 "$BIN" "$B_HOST:$B_BIN"
    echo "== staging $DAEMON_BIN to $B_HOST:$B_DAEMON =="
    scp -q -o BatchMode=yes -o ConnectTimeout=10 "$DAEMON_BIN" "$B_HOST:$B_DAEMON"
    ssh_b "chmod +x $B_BIN $B_DAEMON"
fi

ssh_b "test -x $B_BIN" || { echo "B binary missing: $B_BIN (run with --stage)" >&2; exit 1; }
ssh_b "test -x $B_DAEMON" || { echo "B daemon missing: $B_DAEMON (run with --stage)" >&2; exit 1; }

# --- setup --------------------------------------------------------------------

echo "== clean slate =="
rm -rf "$A_DIR"
reset_remote_data_dir
"$BIN" --data-dir "$A_DIR" init >/dev/null
ssh_b "$B_BIN --data-dir $B_DATA_DIR init" >/dev/null

A_ID=$("$BIN" --data-dir "$A_DIR" id)
B_ID=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR id")
A_SHORT=$(sed -n 's/^short: //p' <<<"$A_ID")
B_SHORT=$(sed -n 's/^short: //p' <<<"$B_ID")
[[ -n "$A_SHORT" && -n "$B_SHORT" ]] || fail "missing short ids (A=[$A_SHORT] B=[$B_SHORT])"
say "A short: $A_SHORT"
say "B short: $B_SHORT"

CREATE_OUT=$("$BIN" --data-dir "$A_DIR" commune create "$GUILD")
GUILD_ID=$(sed -n 's/^created commune //p' <<<"$CREATE_OUT" | head -n 1)
[[ -n "$GUILD_ID" ]] || fail "no commune id from create output: $CREATE_OUT"
say "commune: $GUILD_ID"

echo "== probe A listen address =="
mkfifo "$A_IN"
exec 3<>"$A_IN"
"$BIN" --data-dir "$A_DIR" chat "$GUILD" --listen "/ip4/$A_IP/tcp/$PORT" <"$A_IN" >"$WORK/probe.log" 2>&1 &
A_PID=$!
wait_for_grep "$WORK/probe.log" "dial: " 30 "$A_PID"
A_DIAL=$(dial_from_log "$WORK/probe.log")
[[ -n "$A_DIAL" ]] || fail "no dial address from probe"
say "A dial: $A_DIAL"
printf '/quit\n' >&3
wait "$A_PID" || fail "probe chat exited nonzero"
A_PID=""
exec 3>&-

INVITE=$("$BIN" --data-dir "$A_DIR" guild invite "$GUILD" --peer "$A_DIAL")
[[ -n "$INVITE" ]] || fail "empty invite"

echo "== start A chat =="
: >"$A_LOG"
exec 4<>"$A_IN"
"$BIN" --data-dir "$A_DIR" chat "$GUILD" --listen "/ip4/$A_IP/tcp/$PORT" <"$A_IN" >"$A_LOG" 2>&1 &
A_PID=$!
wait_for_grep "$A_LOG" "listening: " 30 "$A_PID"
wait_for_grep "$A_LOG" "dial: $A_DIAL" 30 "$A_PID"

echo "== B joins with A listening =="
JOIN_OUT=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR commune join '$INVITE'") \
    || fail "B join failed: $JOIN_OUT"
say "B joined: ${JOIN_OUT%%$'\n'*}"

echo "== start B chat =="
: >"$B_LOG"
mkfifo "$B_IN"
exec 5<>"$B_IN"
ssh_b "bash -c 'exec $B_BIN --data-dir $B_DATA_DIR chat \"$GUILD\" --dial \"$A_DIAL\"'" <&5 >"$B_LOG" 2>&1 &
B_PID=$!
wait_for_grep "$B_LOG" "synced with" 60 "$B_PID"
wait_for_grep "$A_LOG" "synced with" 60 "$A_PID"
echo "peers synced"

# --- A. attachments both ways -------------------------------------------------

echo
echo "== A. attachment from A to B (2 MiB) =="
head -c 2097152 /dev/urandom >"$WORK/from-a.bin"
A_SUM=$(sha256_of "$WORK/from-a.bin")
ATTACH_OUT=$("$BIN" --data-dir "$A_DIR" attach "$GUILD" general "$WORK/from-a.bin" "blob from A")
# The first line carries the event id; the second line is
# "<size>  <mime>  <blake3 hash>", and the blob id is the hash.
A_ATTACH=$(awk 'NR==2{print $NF}' <<<"$ATTACH_OUT")
[[ "$A_ATTACH" =~ ^[0-9a-f]{64}$ ]] || fail "no attachment hash from A: $ATTACH_OUT"
say "attachment id: $A_ATTACH"
B_HISTORY=""
for _ in $(seq 1 60); do
    B_HISTORY=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR history '$GUILD' general -l 100")
    grep -q "blob from A" <<<"$B_HISTORY" && break
    sleep 0.5
done
grep -q "blob from A" <<<"$B_HISTORY" || fail "B never received the attachment message"
ssh_b "$B_BIN --data-dir $B_DATA_DIR download '$GUILD' '$A_ATTACH' --out '$B_DATA_DIR.download-a.bin'" >/dev/null \
    || fail "B download of A's blob failed"
B_SUM=$(ssh_b "sha256sum $B_DATA_DIR.download-a.bin | awk '{print \$1}'")
[[ "$A_SUM" == "$B_SUM" ]] || fail "B downloaded $B_SUM, A hashed $A_SUM"
ssh_b "rm -f $B_DATA_DIR.download-a.bin"
echo "A -> B blob verified ($A_SUM)"

echo "== B. attachment from B to A (512 KiB) =="
head -c 524288 /dev/urandom >"$WORK/from-b.bin"
B_SUM=$(sha256_of "$WORK/from-b.bin")
scp -q -o BatchMode=yes "$WORK/from-b.bin" "$B_HOST:$B_DATA_DIR.upload-b.bin"
ATTACH_OUT=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR attach '$GUILD' general '$B_DATA_DIR.upload-b.bin' 'blob from B'")
B_ATTACH=$(awk 'NR==2{print $NF}' <<<"$ATTACH_OUT")
[[ "$B_ATTACH" =~ ^[0-9a-f]{64}$ ]] || fail "no attachment hash from B: $ATTACH_OUT"
for _ in $(seq 1 60); do
    if "$BIN" --data-dir "$A_DIR" history "$GUILD" general | grep -q "blob from B"; then
        break
    fi
    sleep 0.5
done
"$BIN" --data-dir "$A_DIR" history "$GUILD" general | grep -q "blob from B" \
    || fail "A never received B's attachment message"
"$BIN" --data-dir "$A_DIR" download "$GUILD" "$B_ATTACH" --out "$WORK/download-b.bin" >/dev/null \
    || fail "A download of B's blob failed"
A_SUM=$(sha256_of "$WORK/download-b.bin")
[[ "$A_SUM" == "$B_SUM" ]] || fail "A downloaded $A_SUM, B hashed $B_SUM"
ssh_b "rm -f $B_DATA_DIR.upload-b.bin"
echo "B -> A blob verified ($A_SUM)"

# --- C. channel creation and two-way messages ---------------------------------

echo
echo "== C. channel creation propagates and carries messages =="
CH_OUT=$("$BIN" --data-dir "$A_DIR" channel create "$GUILD" ops)
CH_ID=$(grep -oE '[0-9a-f]{32}' <<<"$CH_OUT" | head -n 1)
[[ -n "$CH_ID" ]] || fail "no channel id from create: $CH_OUT"
for _ in $(seq 1 60); do
    if ssh_b "$B_BIN --data-dir $B_DATA_DIR channel list '$GUILD'" | grep -q "#ops"; then
        break
    fi
    sleep 0.5
done
ssh_b "$B_BIN --data-dir $B_DATA_DIR channel list '$GUILD'" | grep -q "#ops" \
    || fail "B never saw #ops"
printf '/channel ops\n' >&4
printf '/channel ops\n' >&5
wait_for_grep "$A_LOG" "switched to #ops" 20 "$A_PID"
wait_for_grep "$B_LOG" "switched to #ops" 20 "$B_PID"
printf 'ops message from A\n' >&4
wait_for_grep "$B_LOG" "ops message from A" 30 "$B_PID"
printf 'ops message from B\n' >&5
wait_for_grep "$A_LOG" "ops message from B" 30 "$A_PID"
printf '/channel general\n' >&4
printf '/channel general\n' >&5
wait_for_grep "$A_LOG" "switched to #general" 20 "$A_PID"
wait_for_grep "$B_LOG" "switched to #general" 20 "$B_PID"
echo "#ops created and messaging both ways"

# --- D. search parity ---------------------------------------------------------

echo
echo "== D. search parity on both replicas =="
NEEDLE="searchneedle-$RANDOM$RANDOM"
printf 'the %s is here\n' "$NEEDLE" >&4
wait_for_grep "$B_LOG" "$NEEDLE" 30 "$B_PID"
for _ in $(seq 1 40); do
    if ssh_b "$B_BIN --data-dir $B_DATA_DIR search '$GUILD' '$NEEDLE'" | grep -q "$NEEDLE"; then
        break
    fi
    sleep 0.5
done
ssh_b "$B_BIN --data-dir $B_DATA_DIR search '$GUILD' '$NEEDLE'" | grep -q "$NEEDLE" \
    || fail "B search did not find the token"
"$BIN" --data-dir "$A_DIR" search "$GUILD" "$NEEDLE" | grep -q "$NEEDLE" \
    || fail "A search did not find the token"
echo "both replicas find $NEEDLE"

# --- E. B daemon SIGKILL and automatic client reconnect -----------------------

echo
echo "== E. B daemon SIGKILL and reconnect (client stays up) =="
OLD_B_PID=$(remote_daemon_pid)
kill_remote_daemon
start_remote_daemon
wait_remote_daemon "$OLD_B_PID"
echo "B daemon restarted"
probe_until 4 "$B_LOG" "reconnect-a" 90
probe_until 5 "$A_LOG" "reconnect-b" 90
printf 'after B daemon restart, A to B\n' >&4
wait_for_grep "$B_LOG" "after B daemon restart, A to B" 30 "$B_PID"
printf 'after B daemon restart, B to A\n' >&5
wait_for_grep "$A_LOG" "after B daemon restart, B to A" 30 "$A_PID"
echo "both directions live after B's daemon restart"

# --- F. offline burst, catch-up, ordering, idempotent re-sync -----------------

echo
echo "== F. $BURST-message offline burst and catch-up =="
OLD_B_PID=$(remote_daemon_pid)
kill_remote_daemon
echo "B daemon down; sending $BURST messages from A"
for i in $(seq -w 0 $((BURST - 1))); do
    printf 'burst-%s payload\n' "$i" >&4
done
wait_for_grep "$A_LOG" "burst-$(printf '%03d' $((BURST - 1)))" 180 "$A_PID"
echo "A appended the whole burst"

start_remote_daemon
wait_remote_daemon "$OLD_B_PID"
echo "B daemon back; waiting for catch-up"
deadline=$((SECONDS + 180))
while (( SECONDS < deadline )); do
    COUNT=$(remote_marker_list burst | sort -u | wc -l)
    (( COUNT >= BURST )) && break
    sleep 1
done
(( COUNT == BURST )) || fail "B caught up $COUNT of $BURST burst messages"

LOCAL_LIST=$(local_marker_list burst)
REMOTE_LIST=$(remote_marker_list burst)
[[ "$LOCAL_LIST" == "$REMOTE_LIST" ]] || fail "the burst order differs between A and B"
UNIQ=$(sort -u <<<"$LOCAL_LIST" | wc -l)
[[ "$UNIQ" == "$BURST" ]] || fail "A has $UNIQ unique burst markers, expected $BURST"
echo "B caught up exactly once, in the same order"

echo "== second B restart must not duplicate imports =="
OLD_B_PID=$(remote_daemon_pid)
kill_remote_daemon
start_remote_daemon
wait_remote_daemon "$OLD_B_PID"
sleep 3
COUNT=$(remote_marker_list burst | sort -u | wc -l)
[[ "$COUNT" == "$BURST" ]] || fail "after re-sync B has $COUNT unique burst markers, expected $BURST"
DUPES=$(remote_marker_list burst | sort | uniq -d | wc -l)
[[ "$DUPES" == 0 ]] || fail "re-sync produced $DUPES duplicated markers"
echo "re-sync added no duplicates"

# --- G. A daemon SIGKILL: listener recovery and reconnect ---------------------

echo
echo "== G. A daemon SIGKILL, listener recovery, B reconnects =="
OLD_A_PID=$(local_daemon_pid)
kill_local_daemon
start_local_daemon
wait_local_daemon "$OLD_A_PID"
echo "A daemon restarted"
for _ in $(seq 1 60); do
    if ss -ltn 2>/dev/null | grep -q ":$PORT "; then
        break
    fi
    sleep 0.5
done
ss -ltn 2>/dev/null | grep -q ":$PORT " || fail "A did not resume its listener on $PORT"
echo "A listener resumed on $PORT"
probe_until 4 "$B_LOG" "recover-a" 90
probe_until 5 "$A_LOG" "recover-b" 90
printf 'after A daemon restart, A to B\n' >&4
wait_for_grep "$B_LOG" "after A daemon restart, A to B" 30 "$B_PID"
printf 'after A daemon restart, B to A\n' >&5
wait_for_grep "$A_LOG" "after A daemon restart, B to A" 30 "$A_PID"
echo "both directions live after A's daemon restart"

# --- H. ping-pong soak --------------------------------------------------------

echo
echo "== H. $ROUNDS-round ping-pong soak =="
START=$SECONDS
for i in $(seq -w 0 $((ROUNDS - 1))); do
    printf 'soak-a-%s\n' "$i" >&4
    wait_for_grep "$B_LOG" "soak-a-$i" 30 "$B_PID"
    printf 'soak-b-%s\n' "$i" >&5
    wait_for_grep "$A_LOG" "soak-b-$i" 30 "$A_PID"
done
ELAPSED=$((SECONDS - START))
echo "$ROUNDS rounds in ${ELAPSED}s"

CONNS=$(ss -tn state established "( sport = :$PORT )" 2>/dev/null | tail -n +2 | wc -l)
echo "established connections on A port $PORT: $CONNS"
(( CONNS <= 10 )) || fail "possible dial storm: $CONNS connections"

for i in $(seq -w 0 $((ROUNDS - 1))); do
    A_COUNT=$(grep -cF "soak-a-$i" "$B_LOG" 2>/dev/null || true)
    B_COUNT=$(grep -cF "soak-b-$i" "$A_LOG" 2>/dev/null || true)
    (( A_COUNT >= 1 )) || fail "B missed soak-a-$i"
    (( B_COUNT >= 1 )) || fail "A missed soak-b-$i"
done
echo "soak clean, no dial storm"

# --- summary ------------------------------------------------------------------

echo
echo "PASS: stability two-machine"
echo "  attachments both ways (BLAKE3 verified)"
echo "  channel creation + two-way messaging"
echo "  search parity"
echo "  B daemon SIGKILL + client reconnect"
echo "  $BURST-message offline catch-up, order parity, idempotent re-sync"
echo "  A daemon SIGKILL + listener recovery + reconnect"
echo "  $ROUNDS-round soak in ${ELAPSED}s"
