#!/usr/bin/env bash
# Focused verification for the star audio media hub.
#
# The star media tests cover the leaf-to-hub media path, the per-listener mix,
# hub takeover and leaf rejoin. The direct-call tests are the regression that a
# two-member call stays a direct pair.
#
# The default target is an isolated subdirectory so this run cannot collide
# with a normal workspace build. Override with OVVC_VERIFY_TARGET or set
# CARGO_TARGET_DIR before running the script.
set -uo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="${OVVC_VERIFY_TARGET:-${CARGO_TARGET_DIR:-$ROOT/target/media-verify}}"
export CARGO_TARGET_DIR="$TARGET"

mode="${1:-star}"
fail=0

step() { printf '\n== %s ==\n' "$*"; }
pass() { printf 'ok: %s\n' "$*"; }
bad() { printf 'FAIL: %s\n' "$*" >&2; fail=1; }

run() {
    local label="$1"
    shift
    step "$label"
    if "$@"; then
        pass "$label"
    else
        bad "$label"
    fi
}

star() {
    run "leaf-to-hub key rule" \
        cargo test -p ovvc-crypto --test star_keyring
    run "three-daemon star media" \
        cargo test -p ovvc-daemon --test star_media_e2e -- --test-threads=1
}

direct() {
    run "two-daemon libp2p direct call" \
        cargo test -p ovvc-daemon --test audio_e2e -- --test-threads=1
    run "two-daemon WebRTC direct call" \
        cargo test -p ovvc-daemon --test rtc_audio_e2e -- --test-threads=1
}

all() {
    star
    direct
    run "control-plane star and takeover" \
        cargo test -p ovvc-daemon --test star_hub_e2e -- --test-threads=1
    run "packet loss and reordering" \
        cargo test -p ovvc-media jitter -- --test-threads=1
}

case "$mode" in
    --direct|direct) direct ;;
    --all|all) all ;;
    --star|star) star ;;
    -h|--help|help)
        cat <<'EOF'
Usage: scripts/star-media-verify.sh [star|direct|all]

  star    (default) the star key rule and the three-daemon star media tests
  direct  the two-person libp2p and WebRTC regressions
  all     everything above plus control takeover and jitter loss/reordering
EOF
        exit 0
        ;;
    *)
        echo "unknown mode: $mode" >&2
        exit 2
        ;;
esac

step "result"
if [ "$fail" -ne 0 ]; then
    echo "star media verification failed" >&2
    exit 1
fi
echo "star media verification passed"
