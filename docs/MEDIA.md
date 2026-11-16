# Media

ovvc voice is a peer-to-peer audio call with end-to-end encryption. The media
plane is deliberately separate from the durable event log, because a voice
frame is latency-sensitive and worthless once stale. It is also separate from
the control plane, which carries the signed signalling that tells a peer where
to send media and which key to use.

This document describes the shipped v0.1 path. Video and screen sharing are
not implemented, even where the packet format or a negotiated codec name has a
place for them.

## Call state and topology

`ovvc-media::VoiceCall` is a transport-free state machine. A daemon feeds it
local actions and remote hints, such as join, leave, state update, heartbeat
and timeout, and turns the resulting `VoiceUpdate` into control frames.

The topology rules are:

* **Two members are always a direct pair.** `topology_for` returns `Mesh` for
  two members under every scope, and the call snapshot leaves the hub unset.
  The election may still name a coordinator, but no media is forwarded.
* **`CallScope::Mesh`** never grows a hub. Every member connects to every
  other member.
* **`CallScope::Auto`** keeps a mesh up to six members, then becomes a flat
  star.
* **`CallScope::Star`** becomes a flat star as soon as a call has three
  members.

A **flat star** has one hub and a set of leaves. A leaf sends its uplink only
to the hub and receives only a downlink from the hub. The hub sends no leaf
another leaf's encrypted frame, and a direct leaf-to-leaf media frame is
dropped before decryption.

The hub policy is part of the signed guild policy. `HubMode` is `Elected`,
`Server` or `None`, and `HubTrust` is `Trusted` or `CiphertextOnly`.
`HubPolicy` carries `allow_mixing`, `allow_video_forwarding` and
`min_trust_tier`. In v0.1 the local daemon advertises trust tier 1, so a
minimum tier above one is rejected, because the alternative is a star with no
eligible hub.

## Hub election

The control plane elects the hub. A candidate advertises a `Priority` with
trust tier, reachability, RTT, upload and download capacity, battery, uptime,
CPU load, memory use, recent failures, core count, hardware encode and decode,
active calls, hub cooldown, worst RTT, loss, unreachable members and the
number of members it measured.

The ranking is deterministic. It compares trust, reachability, unreachable
members, worst RTT, RTT, CPU, hardware encode and decode, upload, download,
cores, active calls, cooldown, loss, battery and uptime, then falls back to the
lower author id. A candidate below an eligibility floor is not ranked, and a
candidate that reports an unreachable member is ineligible because a hub that
some leaves cannot reach cannot serve the call.

The master holds a lease and sends heartbeats. A backup stands by. A bad RTT,
loss or CPU sample starts a health timer, a healthy sample clears it, and only
a breach that lasts the health window replaces the master. Promotion requires
positive liveness after the old lease plus grace, so a claim that was merely
fresh before the master died is not enough. A dead master is removed from the
live candidate list, and an unhealthy master is parked out of both roles for
the health window so it cannot take the role straight back.

The daemon samples CPU, memory, transport throughput, RTT, loss, active calls
and recent failures once per control tick, smooths the noisy fields and feeds
them into `Priority`. On a platform without `/proc`, the sampler degrades to a
conservative value that keeps an idle but healthy machine eligible.

## Packet and codec

Media bypasses the signed event log. A `MediaPacket` carries only the routing
fields a peer needs and a bounded payload.

```text
MediaPacket {
  version: 1,
  kind: Audio | Video | Screen,
  call_id: 16 bytes,
  author: 32 bytes,
  seq: u16,
  timestamp_ms: u64,
  payload: at most 1200 bytes,
}
```

The packet header is 62 bytes. The payload cap is 1200 bytes on both the libp2p
and WebRTC paths. The sequence number is 16 bits on the wire and is extended to
32 bits by the jitter buffer, so it survives the `65535 -> 0` wrap.

The shipped live codec is **48 kHz mono Opus**. The engine accepts 10, 20 or
40 ms frames, and the shipped `voice-standard` profile selects 20 ms at
10 kbps. A decoder is created per remote peer, because each stream has its own
state. A lost packet reaches the decoder as an empty payload, which runs Opus
loss concealment for exactly one frame so the decoder state advances across the
gap.

The voice profile table describes three tiers: `voice-standard` (Opus, 48 kHz,
10 kbps), `voice-low` (Opus, 8 kHz, 6 kbps) and `voice-minimal` (Codec 2,
8 kHz, 2 kbps, push-to-talk only). The v0.1 audio backend implements 48 kHz
mono Opus only. The lower tiers exist in the negotiation types and in the
configuration names, but there is no 8 kHz Opus or Codec 2 encoder, and the
daemon capability report claims only `opus`, so a frontend must not assume
either tier is available. Profile sets are still negotiated deterministically
from the roster, which is what the later tiers will use.

A `Video` or `Screen` packet kind exists in the format, and VP8 is named when a
member asks for video, but there is no capture, track, screen-share operation
or live video codec path in v0.1. The video engine descriptor is a capability
statement only.

## Jitter buffer and speaking detection

Each remote stream has a jitter buffer. The default depth is three packets, the
buffer holds at most 256, and a gap longer than eight packets collapses the
playout point instead of synthesising the whole missing run. A duplicate or a
late packet is counted and dropped, and reordering inside the buffer is hidden
from the decoder.

Speaking detection uses frame RMS with hysteresis. A frame at or above -35 dBFS
starts speaking immediately, a frame below -45 dBFS counts towards ending it,
and ten consecutive quiet frames end the flag. Levels in between leave the
state alone, so a quiet word does not flicker the indicator on every frame.

## Encryption and key topology

Every call member generates an ephemeral X25519 keypair and publishes the
public half in a signed `VoiceKey` control frame. The daemon installs a peer's
key only after the frame signature has been verified, because `ovvc-crypto`
does not verify control frames itself.

From the two public keys, `derive_pairwise` derives two directional keys with
BLAKE3 and the context `ovvc/media/v1`. The direction label is chosen from the
canonically sorted author pair, so both sides derive the same pair and assign
them to opposite directions. Media is sealed with ChaCha20-Poly1305.

```text
epoch u32 BE | counter u64 BE | ciphertext | tag (16 bytes)
```

The 12-byte header is also the AEAD nonce, so the nonce never repeats while the
send counter does not repeat. Each incoming direction has a 64-counter replay
window, which accepts reordering inside the window and rejects duplicates and
counters that fall too far behind. Keys are per epoch, not a full ratchet.
Rotating the keyring gives forward secrecy for the old epoch, but there is no
per-message ratchet.

`KeyTopology` selects the peers a member needs a key with:

* **`AllPairs`** installs one cipher per other member, which is the mesh rule
  and the two-member direct-pair rule.
* **`Star { hub }`** installs one leaf-to-hub key at each leaf and one key per
  leaf at the hub. No leaf holds a key for another leaf.
* **Ciphertext-only hub.** If the hub policy says the hub cannot decrypt, a
  star falls back to `AllPairs`, because a hub that cannot decrypt cannot
  re-seal for each listener and every leaf must seal per recipient.

The hub re-seals each listener's downlink with that listener's pairwise key, so
a leaf can decrypt only its own mix.

## Transports

Two transports carry the same encrypted packet.

**WebRTC.** `ovvc-rtc` uses `webrtc-rs` 0.21.1. Each remote author gets a
`PeerConnection`, the Opus payload is written as a 20 ms RTP sample, and the
remote track is read back into a `MediaPacket` event for the daemon. Every RTP
payload is sealed before it is written and opened before it becomes an event,
so plaintext is never written or emitted. The lower author id creates the
offer, and the higher side creates its connection but emits nothing until the
offer arrives. A failed peer is reported and removed without stopping the rest
of the call. SDP and ICE values travel in signed control frames. The manager
binds an IPv4 wildcard and an IPv4 loopback UDP socket per peer and does not
gather IPv6 candidates.

**libp2p fallback.** The same `MediaPacket` can travel directly over the libp2p
request-response path. The payload is sealed with the same keyring, so the
transport choice does not change the encryption boundary.

## Star hub mixing

`ovvc-audio::HubMixer` is the media-plane half of a star call. Every member,
including the hub, is a source. Each source offers encoded Opus frames into its
own jitter buffer. On each pump the mixer pops at most one frame from every
source, decodes it once, and uses Opus PLC for a gap.

It then builds two things:

* **Local playback for the hub.** This sums every source except the hub, so the
  hub hears the leaves and never its own microphone.
* **One encoded mix per listener.** This sums every source except that
  listener, so a leaf hears the other leaves and the hub but not itself. Each
  listener needs a separate `OpusEncoder`, because the mixes differ and encoder
  state cannot be shared.

The hub does not feed leaf packets into the mesh audio engine a second time, or
the hub would play every leaf twice. The engine still owns the hub microphone,
mute, deafen and push-to-talk, and its encoded microphone packet is the hub's
own source.

`VoiceAudioStats.peers` reports the mixer's leaf source count on the hub and
the current topology peer count on a leaf, which lets a frontend show the live
star instead of a historical decoder count.

## Failure and recovery

A hub takeover rotates the keyring past the announced epoch and publishes a new
`VoiceKey`. A leaf rejoin rotates and republishes as well. Members that rotated
independently can take a bounded number of remote advances per roster rotation,
so they meet on the highest epoch instead of stopping at an intermediate one. A
received epoch far ahead of an aligned local keyring is treated as stale or
hostile, while a keyring that has not yet aligned may adopt the first
authenticated epoch it sees so a joiner is not locked out.

A media frame for the wrong call, the wrong kind, or over the payload limit is
dropped and counted as a bad packet. A decode error plays a concealed frame
instead of aborting the audio pass. One failing WebRTC peer never stops the
others.

## Audio backends and pipelines

`ovvc-audio` joins the transport-free media types to a real codec and a
capture/playback device. The engine talks to an `AudioBackend`, never to a
device API directly.

* **`NullBackend`** drives the synthetic source and null sink from
  `ovvc-media`. It runs the whole encode, packet, jitter and decode path
  without a sound card, and it is what CI and the multi-daemon tests use.
* **`PipeWireBackend`** is compiled only on Linux with the `pipewire` feature.
  It implements the same trait, and because it is not `Send` the service builds
  and drives it inside a dedicated OS thread. `scripts/audio-dev-env.sh` stages
  the development headers under `target/` without root.

The engine configuration fixes 48 kHz, mono, 10, 20 or 40 ms frames and a
positive bitrate. The capture and playback audio graphs are declarative TOML
pipelines with bounded node and input counts, and both are validated before a
call starts. Mute, deafen and push-to-talk are applied on the capture or
playback side without tearing down audio state.

## Limits

* No video capture, video track, screen share or live video codec path. Video
  is a descriptor and a negotiated name only.
* No live Codec 2 or 8 kHz path. The lower voice profiles are types for future
  work, not a shipped backend.
* No per-message ratchet and no explicit key confirmation. An install of the
  wrong key fails later at authentication.
* No IPv6 ICE candidates from the WebRTC manager. It binds IPv4 wildcard and
  loopback sockets per peer.
* No simulcast and no multi-resolution video, because there is no video path.
* One audio device per daemon. Two clients on one machine share the microphone
  and speaker, so a repeatable call test uses the null backend and separate
  processes.
* A hub is a single point of failure for the duration of a takeover. The
  election promotes a backup and the key epoch rotates, but there is a short
  media gap while the new hub reconnects the leaves.
