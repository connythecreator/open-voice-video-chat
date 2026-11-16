#!/usr/bin/env bash
# Exercise the two-person voice pipeline end to end.
#
# The automated scenarios start two real ovvc-daemon processes with separate
# data directories and sockets, connect them over a real libp2p link, join the
# same voice channel, and carry a synthetic tone through the media path. The
# WebRTC scenario runs with the null audio backend and E2EE, so it is safe on a
# single machine. The libp2p scenario exercises the configured fallback path.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "== two-person WebRTC call (null backend, E2EE, synthetic tone) =="
cargo test -p ovvc-daemon --test rtc_audio_e2e -- --test-threads=1 --nocapture

echo
echo "== two-person libp2p media fallback (null backend, E2EE) =="
cargo test -p ovvc-daemon --test audio_e2e -- --test-threads=1 --nocapture

cat <<'EOF'

Automated pipeline passed. For a manual two-machine call:

1. On both machines, build the audio backend:
     source scripts/audio-dev-env.sh
     cargo build -p ovvc-daemon -p ovvc-tui --features pipewire

2. Machine A: start the TUI, create a commune, create a voice channel with
     /newchannel voice standup
   then create an invite with /invite and pass the code to machine B.

3. Machine B: /join <invite>, select #standup, then /voice join.
   Machine A: select #standup, then /voice join.

4. Both sides should show E2EE e1, the peer, the transport and the audio
   counters. A two-person call is a direct WebRTC connection with one
   pairwise key, so there is no hub forwarding at N=2.

Two clients on one machine share the microphone and speaker, so use the
automated scenario above for a repeatable test, or use two machines with
separate audio devices for a live conversation.
EOF
