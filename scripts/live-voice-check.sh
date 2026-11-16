#!/usr/bin/env bash
# Verify the live audio path and print the two-machine call steps.
#
# This checks the local PipeWire source and sink, builds the daemon with the
# live backend, and runs a one-second capture/decode probe. A real two-person
# live call needs two machines, or two independent sets of audio devices,
# because two daemons on one machine share the default source and sink.
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v pw-cli >/dev/null 2>&1; then
    echo "PipeWire tools are missing; install PipeWire and a session manager." >&2
    exit 1
fi

echo "== PipeWire status =="
wpctl status 2>/dev/null | sed -n '1,35p' || true

echo
echo "== staging a build environment if needed =="
if ! pkg-config --exists libpipewire-0.3 2>/dev/null; then
    # shellcheck disable=SC1091
    source scripts/audio-dev-env.sh
else
    echo "system PipeWire development headers are present"
fi

echo
echo "== building the daemon with live audio =="
cargo build -p ovvc-daemon --features pipewire

echo
echo "== live capture and decode probe =="
cargo run -q -p ovvc-audio --features pipewire --bin ovvc-audio-probe -- \
    --backend pipewire --seconds 1 --play

cat <<'EOF'

Live path is working on this machine. For a two-person call:

1. On both machines, from the repository root:
     source scripts/audio-dev-env.sh
     cargo build -p ovvc-daemon --features pipewire

2. Create a config on both machines, for example ~/.config/ovvc/config.toml:
     [audio]
     backend = "pipewire"
     [media]
     transport = "webrtc"

3. Start the TUI on both machines with that config, create a commune and a
   voice channel on A with /newchannel voice standup, invite B, join from B,
   then /voice join on both.

4. Both should show E2EE e1, transport webrtc and non-zero capture/playback
   counters. A two-person call is direct WebRTC with one pairwise key.

If the daemon was built without the pipewire feature, the call is silent even
though it joins, because the null backend is selected.
EOF
