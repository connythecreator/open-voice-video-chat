#!/usr/bin/env bash
#
# Real cross-machine test for the commune/profile/direct-message work. Machine
# A runs locally; machine B is reached over SSH. Both sides run the actual
# release binaries, not test doubles.
#
# Covers, with asserts on real output:
#   A. `commune` wording end to end and `commune` == `guild` (alias).
#   B. A sees B in its roster after B joins, before B has posted anything.
#   C. Profiles both ways (about, pronouns, links, you: no) and the validation
#      negative cases: a sixth link, a non-http link, and no display-name write
#      when validation fails.
#   D. Live direct messages both ways. B learns the DM from the announcement,
#      reopens it with the same id, and both sides exchange messages while both
#      commune chat sessions stay connected.
#   E. One founder for the deterministic DM id, identical DM history on both
#      replicas, no import/merge error, and both survive a daemon restart.
#   F. The ordinary commune channel still carries messages both ways.
#
# Usage:
#   scripts/e2e-two-machines-profiles-dms.sh           # use the staged B binary
#   scripts/e2e-two-machines-profiles-dms.sh --stage   # copy release binaries to B
#
# Environment overrides:
#   OVVC_BIN   local binary            (default target/release/ovvc)
#   B_HOST     ssh destination         (required, for example user@host)
#   B_BIN      remote binary path      (default ~/ovvc-lan-pd/ovvc)
#   B_DAEMON   remote daemon path      (default <B_BIN dir>/ovvc-daemon)
#   B_DATA_DIR remote data directory   (default /tmp/ovvc-lan-pd)
#   A_IP       A's LAN address         (required, for example 10.0.0.5)
#   PORT       A's listen port         (default 41000)
#   GUILD      commune name            (default profiles-dms-e2e)
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
B_BIN="${B_BIN:-~/ovvc-lan-pd/ovvc}"
B_DAEMON="${B_DAEMON:-${B_BIN%/*}/ovvc-daemon}"
B_DATA_DIR="${B_DATA_DIR:-/tmp/ovvc-lan-pd}"
A_IP="${A_IP:?set A_IP to the LAN address of machine A}"
PORT="${PORT:-41000}"
GUILD="${GUILD:-profiles-dms-e2e}"

STAGE=0
if [[ "${1:-}" == "--stage" ]]; then
    STAGE=1
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/ovvc-profiledms.XXXXXX")"
A_DIR="$WORK/a"
A_IN="$WORK/a.in"
B_IN="$WORK/b.in"
A_LOG="$WORK/a.log"
A2_LOG="$WORK/a2.log"
B_LOG="$WORK/b.log"
B2_LOG="$WORK/b2.log"
A_PID=""
B_PID=""

ssh_b() {
    ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=10 "$B_HOST" "$@"
}

say() {
    printf '%s\n' "$*"
}

show_output() {
    local label=$1 text=$2
    printf -- '--- %s ---\n%s\n' "$label" "$text"
}

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    local f
    for f in "$A_LOG" "$A2_LOG" "$B_LOG" "$B2_LOG"; do
        [[ -f "$f" ]] || continue
        printf -- '--- %s ---\n' "$f" >&2
        sed 's/^/  /' "$f" >&2
    done
    exit 1
}

assert_contains() {
    local text=$1 needle=$2 what=$3
    case "$text" in
        *"$needle"*) ;;
        *) fail "$what: expected to find '$needle' in: $text" ;;
    esac
}

assert_not_contains() {
    local text=$1 needle=$2 what=$3
    case "$text" in
        *"$needle"*) fail "$what: did not expect '$needle' in: $text" ;;
        *) ;;
    esac
}

assert_line() {
    local text=$1 regex=$2 what=$3
    grep -qE -- "$regex" <<<"$text" \
        || fail "$what: no line matching /$regex/ in: $text"
}

# Same test, without failing. Used inside polling loops where a miss is
# expected until the peer's event arrives.
assert_line_quiet() {
    grep -qE -- "$2" <<<"$1"
}

# assert_no_dm_errors <label> <log> <dm-id>
# An import/merge failure would name the DM guild (or say "direct message").
assert_no_dm_errors() {
    local label=$1 file=$2 dm_id=$3
    [[ -f "$file" ]] || return 0
    local bad
    bad=$(grep -inE 'error|conflict|merge|import|could not (open|join) direct message' "$file" \
        | grep -iE "$dm_id|${dm_id:0:8}|direct message" || true)
    [[ -z "$bad" ]] || fail "$label chat log has a DM import/merge error: $bad"
}

# poll_contains <timeout> <what> <needle> <command...>
# Runs the command repeatedly and prints its output once the needle appears.
poll_contains() {
    local timeout=$1 what=$2 needle=$3
    shift 3
    local deadline=$((SECONDS + timeout)) out=""
    while (( SECONDS < deadline )); do
        out=$("$@" 2>&1) || true
        case "$out" in
            *"$needle"*) printf '%s\n' "$out"; return 0 ;;
            *) ;;
        esac
        sleep 0.3
    done
    fail "$what: timed out waiting for '$needle'; last output: $out"
}

# expect_fail <what> <needle> <command...>
# Requires a nonzero exit and the needle on the combined stdout/stderr.
expect_fail() {
    local what=$1 needle=$2
    shift 2
    local out rc
    set +e
    out=$("$@" 2>&1)
    rc=$?
    set -e
    (( rc != 0 )) || fail "$what: command succeeded but should have failed; output: $out"
    assert_contains "$out" "$needle" "$what (error text)"
    say "  [expected failure] $what: ${out%%$'\n'*}"
}

daemon_pid_from() {
    sed -n 's/.*"pid": *\([0-9][0-9]*\).*/\1/p' "$1" 2>/dev/null | head -n 1
}

# Kill the daemon recorded in A's own temporary data directory. No process
# outside that directory is touched.
stop_local_daemon() {
    local pid i
    pid=$(daemon_pid_from "$A_DIR/daemon.json")
    [[ -n "$pid" ]] || return 0
    kill "$pid" 2>/dev/null || return 0
    for ((i = 0; i < 100; i++)); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.1
    done
    kill -9 "$pid" 2>/dev/null || true
}

# Same, for B's data directory. The remote script receives the directory as
# its only argument and reads only files inside it.
stop_remote_daemon() {
    ssh_b "bash -s -- '$B_DATA_DIR'" <<'REMOTE_STOP' 2>/dev/null || true
dir=$1
pid=$(sed -n 's/.*"pid": *\([0-9][0-9]*\).*/\1/p' "$dir/daemon.json" 2>/dev/null | head -n 1)
if [ -n "$pid" ]; then
    kill "$pid" 2>/dev/null
    i=0
    while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 100 ]; do
        sleep 0.1
        i=$((i + 1))
    done
    kill -9 "$pid" 2>/dev/null
fi
REMOTE_STOP
}

# Wipe B's data directory and stop only the daemons recorded inside it. Used
# before the run (so --stage cannot race a running binary) and by cleanup.
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
    stop_local_daemon
    ssh_b "bash -s -- '$B_DATA_DIR'" <<'REMOTE_CLEANUP' 2>/dev/null
dir=$1
pid=$(sed -n 's/.*"pid": *\([0-9][0-9]*\).*/\1/p' "$dir/daemon.json" 2>/dev/null | head -n 1)
[ -n "$pid" ] && kill "$pid" 2>/dev/null
pid=$(cat "$dir.pid" 2>/dev/null)
[ -n "$pid" ] && kill "$pid" 2>/dev/null
rm -f "$dir.pid"
rm -rf "$dir"
REMOTE_CLEANUP
    exec 3>&- 4>&- 5>&- 6>&- 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

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

if (( STAGE )); then
    # Clear our own B daemons first, or the copy fails with "Text file busy".
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
echo "A short: $A_SHORT"
echo "B short: $B_SHORT"

echo
echo "== A. commune wording and alias =="
CREATE_OUT=$("$BIN" --data-dir "$A_DIR" commune create "$GUILD")
show_output "commune create" "$CREATE_OUT"
assert_contains "$CREATE_OUT" "created commune " "commune create wording"
GUILD_ID=$(sed -n 's/^created commune //p' <<<"$CREATE_OUT" | head -n 1)
[[ -n "$GUILD_ID" ]] || fail "no commune id from create output"
GUILD_SHORT=${GUILD_ID:0:8}

echo "== start A chat session (stays connected for the whole run) =="
mkfifo "$A_IN"
exec 3<>"$A_IN"
"$BIN" --data-dir "$A_DIR" chat "$GUILD" --listen "/ip4/$A_IP/tcp/$PORT" <"$A_IN" >"$A_LOG" 2>&1 &
A_PID=$!
wait_for_grep "$A_LOG" "listening: " 30 "$A_PID"
wait_for_grep "$A_LOG" "dial: " 30 "$A_PID"
A_DIAL=$(dial_from_log "$A_LOG")
[[ -n "$A_DIAL" ]] || fail "no dial address from A"
echo "A dial: $A_DIAL"

echo "== invite and B join =="
INVITE=$("$BIN" --data-dir "$A_DIR" commune invite "$GUILD" --peer "$A_DIAL")
[[ -n "$INVITE" ]] || fail "empty invite"
JOIN_OUT=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR commune join '$INVITE'" 2>&1) \
    || fail "B join failed: $JOIN_OUT"
show_output "B commune join" "$JOIN_OUT"
assert_contains "$JOIN_OUT" "joined commune" "B join wording"
assert_contains "$JOIN_OUT" "$GUILD" "B join names the commune"

LIST_A=$("$BIN" --data-dir "$A_DIR" commune list)
LIST_B=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR commune list")
show_output "A commune list" "$LIST_A"
show_output "B commune list" "$LIST_B"
assert_contains "$LIST_A" "$GUILD_SHORT" "A commune list contains the commune id"
assert_contains "$LIST_A" "$GUILD" "A commune list contains the commune name"
assert_contains "$LIST_B" "$GUILD_SHORT" "B commune list contains the commune id"

SHOW_A=$("$BIN" --data-dir "$A_DIR" commune show "$GUILD")
SHOW_B=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR commune show '$GUILD'")
SHOW_A_GUILD=$("$BIN" --data-dir "$A_DIR" guild show "$GUILD")
LIST_A_GUILD=$("$BIN" --data-dir "$A_DIR" guild list)
HELP_COMMUNE=$("$BIN" commune --help)
HELP_GUILD=$("$BIN" guild --help)
show_output "A commune show" "$SHOW_A"
show_output "B commune show" "$SHOW_B"
assert_contains "$SHOW_A" "id: $GUILD_ID" "A commune show id"
assert_contains "$SHOW_B" "id: $GUILD_ID" "B commune show id"
assert_contains "$SHOW_A" "founder: $A_SHORT" "A commune show founder"
assert_contains "$SHOW_B" "founder: $A_SHORT" "B commune show founder"
assert_not_contains "$SHOW_A" "guild" "A commune show uses commune wording"
assert_not_contains "$SHOW_B" "guild" "B commune show uses commune wording"
[[ "$SHOW_A" == "$SHOW_A_GUILD" ]] || fail "commune show and guild show differ"
[[ "$LIST_A" == "$LIST_A_GUILD" ]] || fail "commune list and guild list differ"
[[ "$HELP_COMMUNE" == "$HELP_GUILD" ]] || fail "commune --help and guild --help differ"
say "  [ok] commune and guild subcommands produce identical output"

echo
echo "== B. joiner visibility before any message =="
# At this point B has joined and A's chat session has been connected all along.
# B has not posted a message and has not even started its chat session.
MARK=$(stat -c %s "$A_LOG")
printf '/members\n' >&3
deadline=$((SECONDS + 40))
A_MEMBERS_AFTER_JOIN=""
while (( SECONDS < deadline )); do
    A_MEMBERS_AFTER_JOIN=$(tail -c +$((MARK + 1)) "$A_LOG")
    if grep -qF "  $B_SHORT  $B_SHORT" <<<"$A_MEMBERS_AFTER_JOIN"; then
        break
    fi
    sleep 0.3
done
show_output "A /members after B joined, before B posted" "$A_MEMBERS_AFTER_JOIN"
grep -qF "  $B_SHORT  $B_SHORT" <<<"$A_MEMBERS_AFTER_JOIN" \
    || fail "A /members does not show B ($B_SHORT) before B posted"
MEMBER_LOOKUP=""
deadline=$((SECONDS + 20))
while :; do
    MEMBER_LOOKUP=$("$BIN" --data-dir "$A_DIR" member "$GUILD" "$B_SHORT" 2>&1) || true
    if assert_line_quiet "$MEMBER_LOOKUP" "^author:[[:space:]]+$B_SHORT$"; then
        break
    fi
    (( SECONDS < deadline )) || fail "A member lookup by B short id never resolved: $MEMBER_LOOKUP"
    sleep 0.3
done
show_output "A member lookup by B short id" "$MEMBER_LOOKUP"
assert_line "$MEMBER_LOOKUP" "^author:[[:space:]]+$B_SHORT$" "A resolves B by short id"
say "  [ok] B is visible to A from the join event alone"

echo
echo "== start B chat session =="
mkfifo "$B_IN"
exec 4<>"$B_IN"
: >"$B_LOG"
ssh_b "bash -c 'echo \$\$ > $B_DATA_DIR.pid; exec $B_BIN --data-dir $B_DATA_DIR chat \"$GUILD\" --dial \"$A_DIAL\"'" <&4 >"$B_LOG" 2>&1 &
B_PID=$!
wait_for_grep "$B_LOG" "synced with" 40 "$B_PID"
wait_for_grep "$A_LOG" "synced with" 40 "$A_PID"
say "  both sessions synced on the commune"

echo
echo "== C. profiles both ways =="
A_PROFILE_OUT=$("$BIN" --data-dir "$A_DIR" profile "$GUILD" ada \
    --about "Ada writes the LAN prover." \
    --pronouns "she/her" \
    --link "https://example.com" \
    --link "https://second.example")
show_output "A profile" "$A_PROFILE_OUT"
assert_contains "$A_PROFILE_OUT" "display name set to ada" "A profile name"
assert_contains "$A_PROFILE_OUT" "pronouns she/her" "A profile pronouns"
assert_contains "$A_PROFILE_OUT" "2 link(s)" "A profile links"

B_PROFILE_OUT=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR profile '$GUILD' bob \
    --about 'Bob checks the other side.' \
    --pronouns 'they/them' \
    --link 'https://bob.example' \
    --link 'https://bob2.example'" 2>&1) \
    || fail "B profile failed: $B_PROFILE_OUT"
show_output "B profile" "$B_PROFILE_OUT"
assert_contains "$B_PROFILE_OUT" "display name set to bob" "B profile name"
assert_contains "$B_PROFILE_OUT" "pronouns they/them" "B profile pronouns"
assert_contains "$B_PROFILE_OUT" "2 link(s)" "B profile links"

A_VIEW_B=$(poll_contains 40 "A reads B's profile" "Bob checks the other side." \
    "$BIN" --data-dir "$A_DIR" member "$GUILD" bob)
show_output "A reads B (member bob)" "$A_VIEW_B"
assert_line "$A_VIEW_B" '^name:[[:space:]]+bob$' "A sees B's name"
assert_line "$A_VIEW_B" '^pronouns:[[:space:]]+they/them$' "A sees B's pronouns"
assert_contains "$A_VIEW_B" "Bob checks the other side." "A sees B's about"
assert_contains "$A_VIEW_B" "https://bob.example" "A sees B's first link"
assert_contains "$A_VIEW_B" "https://bob2.example" "A sees B's second link"
assert_line "$A_VIEW_B" '^you:[[:space:]]+no$' "A reading B says you: no"

B_VIEW_A=$(poll_contains 40 "B reads A's profile" "Ada writes the LAN prover." \
    ssh_b "$B_BIN --data-dir $B_DATA_DIR member '$GUILD' ada")
show_output "B reads A (member ada)" "$B_VIEW_A"
assert_line "$B_VIEW_A" '^name:[[:space:]]+ada$' "B sees A's name"
assert_line "$B_VIEW_A" '^pronouns:[[:space:]]+she/her$' "B sees A's pronouns"
assert_contains "$B_VIEW_A" "Ada writes the LAN prover." "B sees A's about"
assert_contains "$B_VIEW_A" "https://example.com" "B sees A's first link"
assert_contains "$B_VIEW_A" "https://second.example" "B sees A's second link"
assert_line "$B_VIEW_A" '^you:[[:space:]]+no$' "B reading A says you: no"
say "  [ok] profiles crossed both ways with you: no"

echo "== C negatives: validation happens before the display-name write =="
expect_fail "six links" "at most 5" \
    "$BIN" --data-dir "$A_DIR" profile "$GUILD" wrong-six \
    --link "https://one.example" \
    --link "https://two.example" \
    --link "https://three.example" \
    --link "https://four.example" \
    --link "https://five.example" \
    --link "https://six.example"
expect_fail "non-http link" "not an http(s) URL" \
    "$BIN" --data-dir "$A_DIR" profile "$GUILD" wrong-scheme \
    --link "ftp://example.com/file"
A_VIEW_A=$("$BIN" --data-dir "$A_DIR" member "$GUILD" ada)
show_output "A profile after the failed writes" "$A_VIEW_A"
assert_line "$A_VIEW_A" '^name:[[:space:]]+ada$' "bad profile write left the display name alone"
assert_not_contains "$A_VIEW_A" "wrong-six" "rejected name wrong-six was not written"
assert_not_contains "$A_VIEW_A" "wrong-scheme" "rejected name wrong-scheme was not written"
say "  [ok] failed profile writes changed nothing"

echo
echo "== D. direct messages both ways, live =="
DM_OPEN=$("$BIN" --data-dir "$A_DIR" dm "$GUILD" bob)
show_output "A dm bob" "$DM_OPEN"
assert_contains "$DM_OPEN" "opened direct message with bob" "A opens the DM"
DM_ID=$(sed -n 's/^id: //p' <<<"$DM_OPEN" | head -n 1)
[[ -n "$DM_ID" ]] || fail "no direct message id from A's dm output"
DM_SHORT=${DM_ID:0:8}
say "DM id: $DM_ID (short $DM_SHORT)"

B_DMS=$(poll_contains 40 "B learns the DM from the announcement" "$DM_SHORT" \
    ssh_b "$B_BIN --data-dir $B_DATA_DIR dms")
show_output "B dms before reopening" "$B_DMS"
assert_contains "$B_DMS" "dm:ada" "B's DM list names A"

DM_REOPEN=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR dm '$GUILD' ada" 2>&1) \
    || fail "B reopening the DM failed: $DM_REOPEN"
show_output "B dm ada (reopen)" "$DM_REOPEN"
assert_contains "$DM_REOPEN" "already exists" "B reuses the conversation"
assert_contains "$DM_REOPEN" "id: $DM_ID" "B derives the same DM id"
assert_not_contains "$DM_REOPEN" "opened direct message" "B did not found a second log"
say "  [ok] B learned the announcement and reused the identical id"

# `dm` is allowed to answer from the announcement before the opener's log has
# synced (that is what the placeholder is for). Wait until B actually holds
# the DM guild before posting to it; a failure here means the two DM nodes
# never met.
echo "== wait for the DM log to reach B =="
deadline=$((SECONDS + 60))
B_DM_EARLY=""
while :; do
    if B_DM_EARLY=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR history '$DM_ID' general" 2>&1); then
        break
    fi
    (( SECONDS < deadline )) || fail "B never imported the DM log '$DM_ID': $B_DM_EARLY"
    sleep 0.5
done
show_output "B DM history before sending" "$B_DM_EARLY"
say "  [ok] the opener's DM log reached B"

B_DM_SEND=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR send '$DM_ID' general 'dm from B'" 2>&1) \
    || fail "B could not send in the DM: $B_DM_SEND"
show_output "B sends in the DM" "$B_DM_SEND"
B_DM_EVENT=$(awk '{print $2}' <<<"$B_DM_SEND")
[[ -n "$B_DM_EVENT" ]] || fail "no event id from B's DM send: $B_DM_SEND"

A_DM_HIST=$(poll_contains 60 "A sees B's DM message" "dm from B" \
    "$BIN" --data-dir "$A_DIR" history "$DM_ID" general)
show_output "A DM history after B's message" "$A_DM_HIST"

A_DM_REPLY=$("$BIN" --data-dir "$A_DIR" reply "$DM_ID" general "$B_DM_EVENT" "dm reply from A")
show_output "A replies in the DM" "$A_DM_REPLY"
assert_contains "$A_DM_REPLY" "replied " "A replied in the DM"

B_DM_HIST=$(poll_contains 60 "B sees A's DM reply" "dm reply from A" \
    ssh_b "$B_BIN --data-dir $B_DATA_DIR history '$DM_ID' general")
show_output "B DM history after A's reply" "$B_DM_HIST"
assert_contains "$B_DM_HIST" "dm from B" "B's own DM message is in its history"
say "  [ok] DM messages crossed both ways while both commune chats stayed up"

echo
echo "== E. single founder, identical DM history, restart persistence =="
deadline=$((SECONDS + 60))
A_DM_HIST=""
B_DM_HIST=""
while :; do
    A_DM_HIST=$("$BIN" --data-dir "$A_DIR" history "$DM_ID" general 2>&1) || true
    B_DM_HIST=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR history '$DM_ID' general" 2>&1) || true
    if [[ -n "$A_DM_HIST" \
        && "$A_DM_HIST" == "$B_DM_HIST" \
        && "$A_DM_HIST" == *"dm from B"* \
        && "$A_DM_HIST" == *"dm reply from A"* ]]; then
        break
    fi
    (( SECONDS < deadline )) || fail "DM histories did not converge: A=[$A_DM_HIST] B=[$B_DM_HIST]"
    sleep 0.5
done
show_output "identical DM history" "$A_DM_HIST"

A_DM_SHOW=$("$BIN" --data-dir "$A_DIR" commune show "$DM_ID")
B_DM_SHOW=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR commune show '$DM_ID'")
show_output "A commune show DM" "$A_DM_SHOW"
show_output "B commune show DM" "$B_DM_SHOW"
A_DM_FOUNDER=$(sed -n 's/^founder: //p' <<<"$A_DM_SHOW" | head -n 1)
B_DM_FOUNDER=$(sed -n 's/^founder: //p' <<<"$B_DM_SHOW" | head -n 1)
[[ "$A_DM_FOUNDER" == "$A_SHORT" ]] \
    || fail "A's DM founder is $A_DM_FOUNDER, expected opener $A_SHORT"
[[ "$B_DM_FOUNDER" == "$A_SHORT" ]] \
    || fail "B's DM founder is $B_DM_FOUNDER, expected opener $A_SHORT"
[[ "$A_DM_FOUNDER" == "$B_DM_FOUNDER" ]] || fail "DM founders differ"
say "  [ok] both replicas name $A_SHORT as the single DM founder"

for pair in "A:$A_LOG" "B:$B_LOG"; do
    assert_no_dm_errors "${pair%%:*}" "${pair#*:}" "$DM_ID"
done
say "  [ok] no DM import/merge error in either chat log"

echo "== restart both daemons =="
printf '/quit\n' >&3
printf '/quit\n' >&4
wait "$A_PID" || fail "A quit nonzero"
A_PID=""
wait "$B_PID" || fail "B quit nonzero"
B_PID=""
exec 3>&- 4>&-

stop_local_daemon
stop_remote_daemon
say "  daemons stopped"

"$BIN" --data-dir "$A_DIR" id >/dev/null
ssh_b "$B_BIN --data-dir $B_DATA_DIR id" >/dev/null
say "  daemons restarted"

: >"$A2_LOG"
exec 5<>"$A_IN"
"$BIN" --data-dir "$A_DIR" chat "$GUILD" --listen "/ip4/$A_IP/tcp/$PORT" <"$A_IN" >"$A2_LOG" 2>&1 &
A_PID=$!
wait_for_grep "$A2_LOG" "dial: " 30 "$A_PID"
A2_DIAL=$(dial_from_log "$A2_LOG")
[[ -n "$A2_DIAL" ]] || fail "no dial address after restart"

: >"$B2_LOG"
exec 6<>"$B_IN"
ssh_b "bash -c 'echo \$\$ > $B_DATA_DIR.pid; exec $B_BIN --data-dir $B_DATA_DIR chat \"$GUILD\" --dial \"$A2_DIAL\"'" <&6 >"$B2_LOG" 2>&1 &
B_PID=$!
wait_for_grep "$B2_LOG" "synced with" 40 "$B_PID"
wait_for_grep "$A2_LOG" "synced with" 40 "$A_PID"

A_DMS_AFTER=$(poll_contains 40 "A dms persists across restart" "$DM_SHORT" \
    "$BIN" --data-dir "$A_DIR" dms)
B_DMS_AFTER=$(poll_contains 40 "B dms persists across restart" "$DM_SHORT" \
    ssh_b "$B_BIN --data-dir $B_DATA_DIR dms")
show_output "A dms after restart" "$A_DMS_AFTER"
show_output "B dms after restart" "$B_DMS_AFTER"
assert_contains "$A_DMS_AFTER" "dm:bob" "A's dms still names B"
assert_contains "$B_DMS_AFTER" "dm:ada" "B's dms still names A"

A_DM_HIST2=$("$BIN" --data-dir "$A_DIR" history "$DM_ID" general)
B_DM_HIST2=$(ssh_b "$B_BIN --data-dir $B_DATA_DIR history '$DM_ID' general")
show_output "A DM history after restart" "$A_DM_HIST2"
show_output "B DM history after restart" "$B_DM_HIST2"
[[ "$A_DM_HIST2" == "$B_DM_HIST2" ]] || fail "DM history changed or diverged after restart"
assert_contains "$A_DM_HIST2" "dm from B" "B's DM message survived the restart"
assert_contains "$A_DM_HIST2" "dm reply from A" "A's DM reply survived the restart"
A_DM_FOUNDER2=$(sed -n 's/^founder: //p' <<<"$("$BIN" --data-dir "$A_DIR" commune show "$DM_ID")" | head -n 1)
B_DM_FOUNDER2=$(sed -n 's/^founder: //p' <<<"$(ssh_b "$B_BIN --data-dir $B_DATA_DIR commune show '$DM_ID'")" | head -n 1)
[[ "$A_DM_FOUNDER2" == "$A_SHORT" && "$B_DM_FOUNDER2" == "$A_SHORT" ]] \
    || fail "DM founder changed after restart (A=$A_DM_FOUNDER2 B=$B_DM_FOUNDER2)"
for pair in "A2:$A2_LOG" "B2:$B2_LOG"; do
    assert_no_dm_errors "${pair%%:*}" "${pair#*:}" "$DM_ID"
done
say "  [ok] DM history and dms persisted, founder unchanged"

echo
echo "== F. no regression bleed on the normal channel =="
printf 'commune regression from A\n' >&5
wait_for_grep "$B2_LOG" "commune regression from A" 30 "$B_PID"
printf 'commune regression from B\n' >&6
wait_for_grep "$A2_LOG" "commune regression from B" 30 "$A_PID"
say "  [ok] ordinary commune messages still flow both ways"

printf '/quit\n' >&5
printf '/quit\n' >&6
wait "$A_PID" || fail "final A quit nonzero"
A_PID=""
wait "$B_PID" || fail "final B quit nonzero"
B_PID=""
exec 5>&- 6>&-

echo
echo "PASS: commune wording+alias, join visibility, profiles+validation, live DMs with one founder, restart persistence, commune regression"
