# Architecture

ovvc is a local-first group chat and voice system. A **commune** is a guild, a
named group with channels, categories, members and a policy. Every commune is a
signed append-only event log that every member stores and verifies locally.
There is no required server, no central account and no central message store.
Members find each other with libp2p, sync the log directly, and carry voice
media over WebRTC or a libp2p fallback.

The design goal is that a commune keeps working when no other member is
reachable. Networking is an optimisation for convergence and presence, not a
precondition for reading or writing.

## Process model

One **daemon** per user and data directory owns the identity, the redb store
and every libp2p node. The daemon exposes a Unix domain socket, checks the
peer's OS uid, and is the only process that opens the store. That keeps redb
single-writer and lets the CLI, TUI and GUI run side by side against one
identity.

Frontends never open the event store. They send requests and receive events
over the local protocol in `ovvc-ipc`, and `ovvc-remote` wraps that protocol in
the same facade shape as the in-process `ovvc-client` API. The GUI starts a
daemon when none is listening, while the CLI and TUI attach to an existing one
or start a sibling daemon.

```
+---------+   +---------+   +---------+
|  ovvc   |   | ovvc-tui|   | ovvc-gui|
+----+----+   +----+----+   +----+----+
     |             |             |
     +-------------+-------------+
                   |  Unix socket, length-prefixed postcard
             +-----+------+
             | ovvc-daemon|
             +-----+------+
                   |
       +-----------+-----------+
       |                       |
  +----+----+            +-----+-----+
  | redb +  |            | libp2p    |
  | event   |            | transport |
  | log     |            +-----+-----+
  +---------+                  |
                         other members
```

## Crate map

The workspace is split so that a frontend, a storage backend and a transport
can each be replaced without dragging the others along.

| Crate | Responsibility |
| --- | --- |
| `ovvc-core` | Identity, signed event model, ids, invites, attachments, policy types, timeline and the `Storage` trait |
| `ovvc-store` | Default storage backend: redb index plus segmented append-only event files |
| `ovvc-crypto` | Per-call X25519 key agreement, ChaCha20-Poly1305 media sealing, key epochs and key topology |
| `ovvc-media` | Transport-free call state, topology, codec profiles, media packet, jitter buffer, level detection, loopback harness and audio pipelines |
| `ovvc-audio` | Opus 48 kHz mono, device backends, the mesh audio engine, the star hub mixer and the call service |
| `ovvc-rtc` | WebRTC audio transport, SDP/ICE signalling values and per-peer RTP sample handling |
| `ovvc-control` | Signed ephemeral control plane: roster, presence, typing, voice signalling, RTC signalling and the media election |
| `ovvc-net` | libp2p transport, sync protocol, mDNS, Kademlia, relay/DCUtR, UPnP and NAT-PMP |
| `ovvc-client` | Frontend facade composing `ovvc-core`, `ovvc-store` and `ovvc-net` |
| `ovvc-ipc` | Local daemon/client protocol and its Unix socket transport |
| `ovvc-remote` | Daemon-attached facade shared by the three frontends |
| `ovvc-config` | Layered TOML configuration, profiles and validation |
| `ovvc-theme` | Shared `theme.toml` palette for the frontends |
| `ovvc-daemon` | The per-user process that owns state, networking, audio and the control session |
| `ovvc-cli` | Headless CLI binary |
| `ovvc-tui` | Terminal frontend |
| `ovvc-gui` | egui desktop frontend |
| `ovvc-server` | Optional libp2p relay server |

Dependency direction is downwards in that table. `ovvc-core` has no internal
dependency, and no frontend depends on a transport or a device API directly.

## Event log and storage

An **event** is the only durable unit. A guild creation, a channel creation, a
message, a reply, an edit, a delete, a reaction, a read marker, a member join,
a profile update or a direct-message announcement is an event with the same
envelope.

* **Identity.** An Ed25519 keypair is the author identity. The public key is
  the author id, and the same key derives the libp2p transport identity, so a
  peer connection pins the author key at the transport layer.
* **Envelope.** Each event carries a version, guild id, optional channel id,
  author id, author sequence number, Lamport clock, timestamp, the previous
  event id from that author and a signed body.
* **Versioning.** Event version 2 stores the event kind as a `(tag, payload)`
  pair, so an unknown kind is preserved and ignored instead of making the log
  undecodable. Version 1 events remain readable and verifiable forever.
* **Canonical encoding.** Signed bytes are postcard encoded and decoded with an
  exact re-encode check, so a value with trailing bytes or a non-canonical
  varint cannot claim an identity it does not have.
* **Validation before indexing.** The store verifies the signature, the
  supported version, the guild and channel relationship, per-kind structural
  rules, size caps, the Lamport range, the sequence and previous-hash chain,
  and the timestamp skew window before an event enters the index.
* **Storage layout.** redb holds the index and the verification state, and
  per-guild append-only segment files hold the events. Opening a store scans
  and verifies the log before serving it.
* **Sync cursor.** Pagination uses `(lamport, event_id)` as an exclusive lower
  bound, not a bare Lamport clock, so a tie group that straddles a page
  boundary cannot lose events.

The `Storage` trait is the boundary a different backend implements. The default
redb backend is one implementation, and a frontend can supply a custom backend
through `ovvc-client`.

## Networking

`ovvc-net` builds one libp2p node per connected commune. The node uses TCP with
Noise encryption and Yamux multiplexing, a request-response behaviour for the
sync protocol, mDNS for local discovery, Kademlia for wider discovery, relay
and DCUtR for NAT traversal, and UPnP and NAT-PMP for port mapping.

Connections are deliberately conservative. Every dial is pinned to an expected
peer id, a connected or dialling peer is never dialled again, addresses are
filtered to IP and TCP, own and unusable addresses are removed, and the
announce list is ranked and capped. Connection limits bound established and
pending connections per peer and across the node.

After an import, a newly learned event is forwarded to the other connected
members of that commune, with retry and backoff. A line topology A to B to C
therefore converges without a direct A to C link. A failed sync clears its
marker and retries with exponential backoff, strict progress is required, and
a periodic low-priority resync fills any gap.

## Control plane

The control plane is ephemeral. Presence, typing, call membership, codec
negotiation, WebRTC SDP and ICE exchange, media key announcements and the media
master/backup election are latency-sensitive, expire on their own, and would
bloat a log that every member replicates forever. They travel as signed control
frames instead of events.

Every control frame is versioned, signed by the sender's identity key, bounded
in size, and carries a timestamp and a nonce. A replay guard and a rate limiter
are applied per session, and a frame outside the clock-skew window is refused.
Session ids are derived deterministically, either from the guild id for
presence and typing, or from the guild and channel ids for a voice call.

The election keeps a master and a backup, with a lease, a heartbeat, a
freshness rule for promotion and a health window that can replace an unhealthy
master. A claim that is merely fresh before the old master expired is not
enough, because the candidate must prove positive liveness after the expiry.
The election `Priority` carries capacity fields such as CPU load, download
headroom, hardware encode and decode, active calls, RTT, loss and reachability,
so the hub is chosen on capacity as well as liveness.

The control plane contains no networking. The daemon decides how a frame
reaches a peer, and `ovvc-control` decides what it means.

## Media plane

Voice is described in detail in [MEDIA.md](MEDIA.md). The short version is that
`ovvc-media` owns the transport-free call state, `ovvc-crypto` owns the
pairwise sealing, `ovvc-audio` owns Opus and the devices, and `ovvc-rtc` owns
the WebRTC transport. The daemon wires them into one call.

A two-member call is always a direct pair. An `Auto` scope keeps a mesh up to
six members, then moves to a flat star, and an explicit `Star` scope moves as
soon as a call has three members. In a star, a leaf sends only to the hub and
receives only from the hub. The hub decodes each leaf once, mixes a separate
downlink for each listener, and never sends a leaf another leaf's encrypted
frame.

## Frontends and configuration

The CLI, TUI and GUI share the same daemon and the same resolved configuration.
The configuration loader applies built-in defaults, the system file, system
drop-ins in name order, the user file, the active named profile, environment
variables and explicit flags. Every struct uses serde defaults and rejects
unknown fields, so a misspelt key is an error, not a setting that silently
does nothing.

The GUI and TUI render the same phosphor terminal aesthetic and the same
detail levels. The GUI is an egui application, the TUI uses ratatui, and both
load a `theme.toml` or a named preset from `ovvc-theme`.

The visible v0.1 surface is text communes and direct messages, presence,
attachments, search, reactions, replies, edit and delete, themes, settings, the
setup flow, the network map and star voice. The frontends do not show video,
camera, screen share, relay-only, hide-my-IP, plugin, sandbox, avatar-picking or
channel-deletion controls, and the policy view is read-only.

## Security boundaries

* **Local socket.** The daemon checks the peer's OS uid before accepting a
  frontend, so another user on the same host cannot attach.
* **Wire input.** Every event, control frame and IPC frame is bounded and
  parsed with a maximum size. Signatures are checked before state changes.
* **Display input.** Names and message bodies are sanitised before they reach a
  terminal or a log line, so control characters and bidi overrides cannot
  rewrite the screen or forge a log entry.
* **Media.** Each call has fresh X25519 keys, each direction has its own
  ChaCha20-Poly1305 key and counter, and the nonce is the epoch and counter
  header. A replay window rejects duplicates and out-of-window counters.
* **Trust boundary.** `ovvc-crypto` does not verify control-frame signatures
  itself. The daemon installs a peer's media key only after the signed frame
  carrying it has been verified.

## Current limits

v0.1 replicates the full history of a commune and does not shard it. Guild
policy is a read model, so the axes are signed and visible but not yet
enforced. Video is a descriptor and a negotiated name only, with no capture or
codec path, and the frontends hide the video and screen-share controls. There
is no plugin loader or sandbox, no server-authoritative mode and no relay-only
path, and the frontends hide those controls as well. Media keys rotate per
epoch, not per message, so there is forward secrecy for an old epoch but no
full ratchet. The live audio backend is PipeWire on Linux and is compiled only
with the `pipewire` feature, while the default build uses the null backend.
