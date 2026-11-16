# Roadmap

This is the public roadmap for ovvc. It describes the direction and the next
pieces of work without promising dates. The project is pre-1.0, so a minor
version may still change a wire format and refuse an older peer at the
boundary.

## v0.1, the first coherent release

The v0.1 line is feature-complete enough to use for a small commune on Linux.

* Signed, replicated communes, channels, categories, messages, replies,
  mentions, edits, deletes, reactions, read markers, search and attachments.
* Member profiles, presence, typing and direct messages.
* End-to-end encrypted voice with Opus, a direct pair for two members, a mesh
  up to six members, and a flat star with an elected hub and per-listener
  mixing above that.
* WebRTC and libp2p media transports, a libp2p relay server, and a null audio
  backend for tests.
* CLI, TUI and GUI frontends over one daemon and one local protocol, with the
  first-run setup flow and the network map.
* Layered configuration, profiles, settings and themes.
* Fuzz targets for announcements, invites, events, media packets, audio
  decode, the audio loopback and the GUI.

Before v0.1 is tagged, the remaining work is mostly validation and release
engineering, not new product surface:

* A physical three-machine star call, with one machine as the hub, a takeover
  and a leaf rejoin, on the same build that CI tests.
* A repeatable two-machine setup guide and a first-run check on the supported
  Linux distributions.
* A release build and packaging pass for the CLI, TUI, GUI and daemon, with the
  system-library requirements written down.
* A final pass over the public documentation and the repository history, so no
  local note, personal path or test artifact is published.

## Next

### Media

* The lower voice profiles and Codec 2, so a weak link can fall back below
  48 kHz Opus instead of dropping the call.
* IPv6 ICE candidates, a cheaper per-peer stats path, and clearer recovery when
  a hub changes mid-call.
* Key confirmation and a per-message ratchet for media keys.

### Communes and policy

* Signed policy events and actual enforcement of the guild policy, so the
  existing read model becomes a rule the daemon applies.
* A clearer member and role model, with moderation actions that are themselves
  signed events.
* Attachment retention and cleanup controls, and a cold-storage path behind the
  existing `Storage` trait.

### Scale

* Sharding a commune across members and erasure-coded cold storage, so a very
  large commune does not require every member to hold every event.
* An optional self-hosted server for communes that outgrow direct replication,
  likely past roughly 15 active members. The server stays optional forever, and
  a commune must remain usable without it.
* Server-assisted and server-authoritative communes, which are already named in
  the policy types but are not implemented.

### Platform

* macOS and Windows builds, with the same event model and media path, once the
  Linux and star media work has settled. The Windows work lives in a separate
  fork today.
* A web or mobile frontend built on the `ovvc-client` facade, with no change to
  the daemon protocol beyond what the version rules already allow.

### Developer experience

* Package builds for the common distributions and a signed release channel.
* More real-process tests for the two-machine and three-machine paths, and a
  smaller CI matrix for the long-running media checks.

## Not planned for v0.1

* A mandatory central server, a central account or a hosted message store.
* Telemetry.
* A mobile or web frontend.
* Windows and WSL parity.
* Plugin loading or a plugin sandbox.
* Video or screen sharing.
* Relay-only mode or hiding your IP.
