# Changelog

All notable changes to ovvc are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/). The project is pre-1.0, so a minor
version may change a wire format and refuse an older peer at the boundary.

## [0.1.0] - Unreleased

The first coherent release line. No tag or published artifact exists yet.

### Added

* A signed, append-only event log with Ed25519 identity, canonical postcard
  encoding, Lamport clocks, author sequence and previous-hash chains, and
  verify-before-index storage.
* The redb storage backend with per-guild append-only segment files, torn-tail
  recovery and a `(lamport, event_id)` sync cursor.
* Communes, categories, text and voice channels, messages, replies, mentions,
  edits, deletes, reactions, read markers and search.
* Content-addressed attachments up to 25 MiB, with per-message metadata and a
  bounded blob fetch path.
* Member display names, profiles, presence, typing indicators and direct
  messages.
* libp2p networking with TCP, Noise, Yamux, request-response sync, mDNS,
  Kademlia, relay, DCUtR, UPnP and NAT-PMP, plus address ranking, deduplication
  and connection limits.
* Event forwarding after import, sync retry with backoff and a periodic resync.
* A signed ephemeral control plane for presence, typing, voice membership,
  WebRTC signalling, voice keys and the media election.
* A lease-based master/backup election with positive-liveness promotion, a
  health window, capacity sampling and deterministic ranking.
* End-to-end encrypted voice with per-call X25519 key agreement,
  ChaCha20-Poly1305, per-direction counters, a replay window and key epochs.
* A direct pair for two members, a mesh up to six members, and a flat star
  above that with leaf-to-hub keys and per-listener re-sealing.
* A star hub audio mixer that decodes each leaf once, mixes a separate downlink
  per listener and excludes the listener and the hub microphone.
* Opus 48 kHz mono encoding, one decoder per peer, jitter buffering, Opus loss
  concealment, level detection and push-to-talk, mute and deafen.
* WebRTC audio through `webrtc-rs` and a libp2p media fallback, both carrying
  the same encrypted packet.
* A null audio backend for CI and multi-daemon tests, and a PipeWire backend
  behind the `pipewire` feature.
* The `ovvc-daemon` process, its Unix socket IPC protocol, uid check, discovery
  record, single-writer store ownership and runtime capability report.
* The `ovvc` CLI, `ovvc-tui` terminal client and `ovvc-gui` egui client.
* Layered configuration with system and user files, system drop-ins, named
  profiles, environment variables and flags.
* Shared themes and a `theme.toml` palette.
* The optional `ovvc-server` libp2p relay.
* Fuzz targets for announcements, invites, events, media packets, audio
  decode, the audio loopback and the GUI.
* Documentation for the architecture, the protocol, the media plane and the
  public roadmap.

### Changed

* Version 2 events use a tagged event kind, so an unknown kind is preserved and
  ignored instead of making the log undecodable. Version 1 events remain
  readable.
* The control protocol is version 7, the local IPC protocol is 6.0 with flat
  counter 17, the policy format is version 2 and the configuration format is
  version 2.
* The flat star replaced the earlier tree-shaped forwarding topology, so the
  leaf-to-hub key rule can key it directly.

### Security

* Canonical postcard decoding at every trust boundary rejects trailing bytes
  and overlong varints.
* Event validation covers the version, guild and channel relationship,
  per-kind structure, size caps, Lamport range, sequence chain and timestamp
  skew before indexing.
* Control frames are signed, bounded, timestamped and nonce-tagged, with a
  replay guard and a rate limiter.
* The local socket checks the peer's OS uid before accepting a frontend.
* Display names and message bodies are sanitised before they reach a terminal
  or a log line.
* Media is sealed before it is written or emitted, and a peer key is installed
  only after its signed control frame is verified.

### Known limits

* Video and screen sharing are descriptors only, and the frontends hide the
  controls.
* Plugins have no loader or sandbox, and the frontends hide the controls.
* Relay-only mode and hiding your IP are not implemented, and the frontends
  hide the controls.
* Avatar picking and channel deletion are not implemented, and the frontends
  hide the controls.
* Guild policy is a read model and is not enforced.
* Communes replicate the full history and do not shard it.
* Media keys rotate per epoch, not per message.
* Windows and WSL parity are not part of this release line.
* Linux is the supported platform.
