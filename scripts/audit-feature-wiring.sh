#!/usr/bin/env bash
# Audit that claimed features are reachable from the shipped binaries.
#
# This is deliberately not a unit-test run. It builds the documented feature
# combinations, starts real processes, runs the user-facing commands and the
# real device probe, and fails when a feature is only reachable from a library
# test or a stub backend.
set -euo pipefail

cd "$(dirname "$0")/.."

fail=0
step() { printf '\n== %s ==\n' "$*"; }
ok() { printf 'ok: %s\n' "$*"; }
bad() { printf 'FAIL: %s\n' "$*" >&2; fail=1; }

# Start a real daemon with a config and read its runtime capability report
# through `ovvc doctor --json`, which asks the daemon for Op::Capabilities.
# The report must name the expected backend and must call video a descriptor.
runtime_report() {
    local label="$1"
    local backend="$2"
    local want="$3"
    local root
    root=$(mktemp -d)
    mkdir -p "$root/data" "$root/run" "$root/home"
    local config="$root/config.toml"
    local socket="$root/run/daemon.sock"
    printf 'data_dir = "%s"\nsocket = "%s"\n[audio]\nbackend = "%s"\n[media]\ntransport = "webrtc"\nice_servers = []\n' \
        "$root/data" "$socket" "$backend" > "$config"

    HOME="$root/home" XDG_RUNTIME_DIR="$root/run" \
        target/debug/ovvc-daemon --config "$config" >"$root/daemon.log" 2>&1 &
    local pid=$!
    local bound=0
    for _ in $(seq 1 300); do
        if [ -S "$socket" ]; then bound=1; break; fi
        if ! kill -0 "$pid" 2>/dev/null; then break; fi
        sleep 0.05
    done

    local report=""
    if [ "$bound" -eq 1 ]; then
        report=$(HOME="$root/home" XDG_RUNTIME_DIR="$root/run" \
            target/debug/ovvc --config "$config" doctor --json 2>/dev/null || true)
    fi
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true

    if [ "$bound" -ne 1 ]; then
        echo "daemon failed to start for $label:" >&2
        cat "$root/daemon.log" >&2
        rm -rf "$root"
        bad "runtime capability report ($label): daemon did not start"
        return
    fi
    rm -rf "$root"

    if ! printf '%s' "$report" | grep -q '"audio_backends_compiled"'; then
        bad "runtime capability report ($label): no compiled backend field"
    elif ! printf '%s' "$report" | grep -q '"video_descriptor_only": true'; then
        bad "runtime capability report ($label): video was not reported as a descriptor"
    elif ! printf '%s' "$report" | grep -q "\"$want\""; then
        bad "runtime capability report ($label): expected $want in the report"
    else
        ok "runtime capability report ($label)"
    fi
}

step "default workspace build"
if cargo check --workspace --all-targets; then ok "default workspace"; else bad "default workspace"; fi

step "lean daemon build"
if cargo check -p ovvc-daemon --no-default-features; then ok "lean daemon"; else bad "lean daemon"; fi

step "audio probe, null backend"
if cargo run -q -p ovvc-audio --bin ovvc-audio-probe -- --backend null --seconds 1; then
    ok "null audio probe"
else
    bad "null audio probe"
fi

step "media and E2EE scenarios"
if cargo test -p ovvc-daemon --test rtc_audio_e2e -- --test-threads=1; then
    ok "two-daemon WebRTC + E2EE"
else
    bad "two-daemon WebRTC + E2EE"
fi
if cargo test -p ovvc-daemon --test audio_e2e -- --test-threads=1; then
    ok "two-daemon libp2p fallback + E2EE"
else
    bad "two-daemon libp2p fallback + E2EE"
fi

step "runtime capability report, default build"
if cargo build -q -p ovvc-cli -p ovvc-daemon; then
    runtime_report "default" "auto" "null"
else
    bad "default binary build for the capability report"
fi

step "live daemon feature and device probe"
if command -v pw-cli >/dev/null 2>&1; then
    # shellcheck disable=SC1091
    source scripts/audio-dev-env.sh
    if cargo build -p ovvc-daemon --features pipewire; then
        ok "daemon builds with pipewire"
        runtime_report "pipewire" "pipewire" "pipewire"
    else
        bad "daemon pipewire feature build"
    fi
    if cargo run -q -p ovvc-audio --features pipewire --bin ovvc-audio-probe -- \
        --backend pipewire --seconds 1 --play
    then
        ok "live PipeWire probe"
    else
        bad "live PipeWire probe"
    fi
else
    bad "no PipeWire session; live audio cannot be verified on this machine"
fi

step "config path selects the documented backend"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp"
printf '[audio]\nbackend = "pipewire"\n[media]\ntransport = "webrtc"\n' > "$tmp/config.toml"
if OVVC_CONFIG_DIR="$tmp" OVVC_SYSTEM_CONFIG="$tmp/system.toml" \
    cargo run -q -p ovvc-cli -- config validate
then
    ok "config accepts the live backend and WebRTC transport"
else
    bad "config validation for the live path"
fi

step "result"
if [ "$fail" -ne 0 ]; then
    echo "feature wiring audit failed" >&2
    exit 1
fi
echo "feature wiring audit passed"
