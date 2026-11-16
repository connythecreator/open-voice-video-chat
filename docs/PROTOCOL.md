# Protocol

ovvc has four protocol surfaces:

1. **The signed event log**, which is durable, replicated and verified by every
   member.
2. **The peer sync protocol**, which moves events between libp2p nodes.
3. **The control plane**, which carries ephemeral signed frames such as
   presence, typing and call signalling.
4. **The local IPC protocol**, which connects a frontend to its daemon.

Each surface has its own version number and its own compatibility rule. A
shape change that an old peer cannot decode bumps a major version and is
refused at the boundary instead of being misread.

## Version summary

| Surface | Current version | Constant |
| --- | --- | --- |
| Event log | 2, with version 1 readable forever | `EVENT_VERSION` |
| Control plane | 7 | `CONTROL_VERSION` |
| Local IPC | 6.0, flat counter 17 | `IPC_PROTOCOL`, `IPC_VERSION` |
| Guild policy | 2 | `POLICY_VERSION` |
| Configuration file | 2 | `CONFIG_VERSION` |
| Media packet | 1 | `MEDIA_VERSION` |
| Media key schedule | 1 | `MEDIA_KEY_VERSION` |

## 1. Signed event log

An event is the durable unit of state. Every event has the same envelope,
whatever its kind, and the envelope is what a signature covers.

```text
EventBody {
  version: u8,
  guild_id: GuildId,
  channel_id: Option<ChannelId>,
  author: AuthorId,
  seq: u64,
  lamport: u64,
  timestamp: u64,
  prev: Option<EventId>,
  kind: EventKind,
}

SignedEvent {
  body: EventBody,
  signature: [u8; 64],
}
```

* **Identity.** The author id is an Ed25519 public key. The signature domain is
  `ovvc/event/v1\0`, so a signature from another ovvc surface cannot be
  replayed as an event.
* **Canonical encoding.** A value is postcard encoded and then re-encoded
  after decoding. If the two byte strings differ, the value is rejected. This
  closes the gap where trailing bytes or overlong varints let one logical value
  have several wire spellings.
* **Event versions.** Version 1 encodes `EventKind` as a positional enum, so an
  unknown kind makes the body undecodable. Version 2 encodes a `(tag, payload)`
  pair, so an unknown tag is preserved and ignored. A reader accepts versions 1
  and 2.
* **Event kinds.** `GuildCreated`, `ChannelCreated`, `CategoryCreated`,
  `MemberJoined`, `MemberRenamed`, `MemberLeft`, `Message` and its later
  versions, `MessageEdited`, `MessageDeleted`, `ReactionAdded`,
  `ReactionRemoved`, `ReadUpTo`, `MemberProfileUpdated`, `DirectMessageOpened`
  and an `Unknown` carrier for a newer tag.
* **Causality.** `seq` is the author's sequence number and `prev` is the
  previous event id from that author. `lamport` is the guild-wide logical
  clock, advanced on import to at least the highest value seen plus one.
  `checked_add` is used at the top of the range, so a guild fails cleanly
  instead of wrapping.
* **Validation.** The store checks the supported version, the guild and channel
  relationship, the per-kind structural rules, the Lamport range, the
  `seq`/`prev` chain, the timestamp inside a 24-hour future-skew window, and
  the size caps before indexing. The first imported event in a guild must be
  its creation event with a bounded clock, so a peer cannot park the clock at
  `u64::MAX` on an empty store.
* **Edits and deletes.** An edit or delete names its target event and must
  match the target author and channel. A delete is a tombstone, not a rewrite
  of history.
* **Message caps.** A message body is at most 16 KiB, a name is at most 100
  bytes, and a message may mention at most 50 members. A display name or body
  that contains control characters or bidi overrides is rejected.
* **Attachments.** An attachment is announced as a BLAKE3 hash, a size, a
  media type and a display name. At most 10 attachments are allowed per
  message, each at most 25 MiB, with a name of at most 200 bytes and a media
  type of at most 100 bytes. The blob itself is content addressed and fetched
  separately.
* **Profiles.** About text is at most 2000 bytes, pronouns at most 64 bytes,
  and a profile has at most 5 links of at most 512 bytes each.
* **Invites.** An invite is a signed `InviteBody` with a guild id, guild name,
  host author id, dial addresses, creation time and optional expiry. The
  signing domain is `ovvc/invite/v1\0`, the invite version is 1, and the
  encoded form is limited to 8192 characters. The signature binds the guild id
  and name to the addresses, so a peer cannot rewrite the destination of an
  invite.

## 2. Peer sync and forwarding

`ovvc-net` uses libp2p. The transport is TCP with Noise and Yamux, and the
request-response behaviour carries the sync exchange on protocol `/ovvc/1`.
Kademlia uses `/ovvc/kad/1` and identify uses `/ovvc/identify/1`.

The libp2p peer id is derived from the same Ed25519 key as the author id, so a
dial can be pinned to the expected author and a relay cannot substitute a
different key at the transport layer.

Sync is a request for events after a cursor, with a bounded page size. The
cursor is `(lamport, event_id)` and it is an exclusive lower bound. A page
whose boundary falls inside a Lamport tie group therefore resumes at the next
event id instead of skipping the rest of the tie. A sync page reports whether
more events remain, and the requester must make strict progress, so an empty
`more: true` page terminates instead of looping.

Imported events are verified and validated before they are stored. After a
successful import, the daemon forwards the event to the other connected
members of the same commune, with retry and backoff, so a line topology
converges without a direct link between every pair. A failed or timed-out sync
clears its marker and retries with exponential backoff, and a periodic
low-priority resync fills any gap left by a missed forward.

Address hygiene is part of the protocol, not a cosmetic detail. A dial accepts
only IP and TCP or QUIC components, rejects unspecified, link-local, multicast
and reserved addresses, removes the node's own listen and external addresses,
deduplicates, ranks, and caps the announce list. Every dial uses the expected
peer id and a disconnected-and-not-dialling condition, so a peer is never
dialled twice for the same reason.

## 3. Control plane

The control plane is `ovvc-c`. Every frame is signed, versioned, bounded,
timestamped and nonce-tagged. The signing domain is `ovvc/control/v1\0`.

```text
ControlFrame {
  version: u16,
  kind: ControlKind,
  session: SessionId,
  author: AuthorId,
  timestamp_ms: u64,
  nonce: u64,
  body: ...
  signature: [u8; 64],
}
```

* **Sessions.** `SessionId::for_guild` derives the guild-wide session for
  presence and typing. `SessionId::for_voice_channel` hashes the guild and
  channel ids, so every member computes the same call session with no round
  trip and two channels never share a replay namespace.
* **Replay and rate limits.** A frame outside a 5-minute clock-skew window is
  refused. A per-session replay guard holds at most 256 nonces and tracks at
  most 4096 sessions. A rate limiter bounds how often a peer may send.
* **Bounded frames.** A control frame is at most 64 KiB. An SDP body is at
  most 48 KiB, an ICE candidate at most 2 KiB, and a media stream id at most
  64 bytes. A display name in a roster entry is at most 100 bytes.
* **Kinds.** `Join`, `Leave`, `Claim`, `Heartbeat`, `State`, `Roster`,
  `Presence`, `Typing`, `Ping`, `Pong`, `VoiceJoin`, `VoiceLeave`,
  `VoiceState`, `VoiceHeartbeat`, `RtcOffer`, `RtcAnswer`, `RtcCandidate`,
  `RtcBye`, `VoiceKey` and `VoiceSyncRequest`.
* **Voice keys.** `VoiceKey` carries a member's ephemeral X25519 public key for
  a key epoch. The frame signature binds the key to the sender's author id, and
  the receiver installs the key only after verifying that signature.
* **RTC signalling.** SDP offers, answers and ICE candidates travel in signed
  control frames. The daemon verifies and routes them and keeps no media state
  in the control session.
* **Late join.** `VoiceSyncRequest` asks the existing call members to
  re-announce their membership and current epoch key, which is the recovery
  path for a member that joins a commune after a call is already live.
* **Election.** A `Priority` claim carries trust tier, reachability, RTT,
  upload and download capacity, battery, uptime, CPU load, memory use, recent
  transport failures, cores, hardware encode and decode, active calls, hub
  cooldown, worst RTT, loss, unreachable members and the number of members
  measured. A candidate below an eligibility floor, or one that reports an
  unreachable member, is not ranked. The ranking is deterministic and falls
  back to the lower author id. The master holds a lease, a backup stands by,
  and promotion requires positive liveness after the old lease plus grace.

## 4. Local IPC

The local protocol connects a frontend to its daemon over a Unix domain
socket. The daemon checks the peer's OS uid before accepting a connection, so
another user on the same host cannot attach.

* **Handshake.** The client sends `Hello` with a client name, protocol string
  and capability list. The daemon answers `Welcome` with its version, protocol,
  maximum frame size and capability list, or `Rejected` with a reason. The
  handshake is read with a 64 KiB cap, and a major protocol mismatch is
  refused.
* **Framing.** Frames are length-prefixed postcard with an explicit maximum.
  After the handshake the limit is `MAX_ATTACHMENT_BYTES` plus 1 MiB, because
  `PutBlob` carries a whole attachment in one frame, so neither end needs a
  chunked upload protocol.
* **Compatibility.** `IPC_PROTOCOL` is 6.0 and the flat `IPC_VERSION` counter
  is 17. Compatibility is decided on the major number alone, so a 6.x client
  and a 6.y daemon keep talking when only additive frames changed. A
  postcard-incompatible shape change bumps the major number.
* **Capabilities.** The registry is closed and both ends only advertise known
  names. The daemon advertises `voice`, `voice-audio`, `control`, `presence`,
  `typing`, `dm`, `attachments`, `notifications`, `policy` and `capabilities`.
  A frontend also advertises `theme`. An unknown client capability is rejected
  so a new client cannot assume an old daemon understands it.
* **Runtime report.** `Op::Capabilities` returns what this build actually
  compiled and selected: audio backends, the selected backend, implemented
  codecs, media transports, feature flags and whether video is descriptor only.
  This is the honest answer a frontend uses instead of guessing from its own
  feature flags.
* **Events.** The daemon fans events out to attached frontends over a bounded
  broadcast channel. A slow frontend sees a lag notification and reloads
  history from storage, so the queue cannot grow without bound.

## Compatibility rules

* A durable event version is accepted only when the receiver implements it.
  Version 1 stays readable forever. A new event version is a deliberate
  migration, not an implicit bump.
* A control frame carries a version and is refused when it does not match.
  Because the frame is postcard decoded before the version field is checked, an
  incompatible shape can fail at decode or at the version check, so both
  outcomes must be treated as a refusal.
* An IPC handshake compares the major protocol number. A major mismatch is
  refused before any request is parsed.
* A configuration file is migrated from version 1 to version 2 before
  deserialisation. A missing additive table takes its default and does not
  force a rewrite.
* Adding a variant at the end of an enum is additive for postcard only when
  the existing indices do not move. A whole-struct shape change is not
  additive and bumps the major version.
