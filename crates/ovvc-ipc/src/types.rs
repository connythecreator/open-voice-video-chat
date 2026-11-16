use std::collections::HashMap;

use ovvc_control::{ControlFrame, ControlSnapshot, SessionId};
use ovvc_core::{
    Attachment, AttachmentId, AuthorId, CategoryMeta, ChannelId, ChannelKind, ChannelMeta,
    ChannelView, ClientPolicyFloor, DirectConversation, EventId, EventPage, GuildId, GuildMeta,
    GuildPolicy, Invite, MAX_ATTACHMENT_BYTES, MemberInfo, MemberProfile, SignedEvent, SyncCursor,
};
use serde::{Deserialize, Serialize};

/// The protocol version exchanged in `Hello` and `Welcome`, as `major.minor`.
///
/// The flat counter in [`IPC_VERSION`] records each shape change but is not
/// itself sent. Version 3.0 is the first handshake after `VoiceAudioStats`
/// gained the end-to-end encryption fields (`e2ee`, `epoch`, `e2ee_errors`);
/// 2.0 fenced the earlier media transport fields the same way. Version 3.1
/// adds the runtime capability report as new variants at the end of [`Op`] and
/// [`Reply`], leaving the existing variant indices unchanged.
///
/// Version 4.0 adds [`DaemonEvent::Diagnostic`] and the `log_path` field of
/// [`DaemonStatusDto`]. Both are postcard shape changes: an old frontend would
/// fail to decode a diagnostic event, so the major number is bumped rather
/// than relying on capability negotiation for an event class.
///
/// Compatibility is decided on the major number alone, so a 6.x client and a
/// 6.y daemon keep talking when only additive frames changed.
pub const IPC_PROTOCOL: &str = "6.0";
/// Numeric form of [`IPC_PROTOCOL`], encoded as `major * 10 + minor`, so 3.0
/// is 30.
///
/// A daemon and a client that disagree on the major refuse the handshake
/// instead of misreading a request.
///
/// 2: `HistoryBefore` and `Reply::HistoryPage` were added.
/// 3: `DaemonEvent::Notification` and `NotificationReason` were added.
/// 4: attachment ops (`PostMessageRich`, `PutBlob`, `GetBlob`, `HasBlob`,
///    `FetchBlob`) and their replies were added.
/// 5: channel organisation (categories, kinds, rename, move) and notification
///    settings ops were added.
/// 6: the whole-struct `SetNotificationSettings` was replaced by per-field
///    ops, so two frontends changing different preferences cannot overwrite
///    each other with a stale copy of the file.
/// 7: member profiles (`SetProfile`, `GetMember`) and direct messages
///    (`OpenDirectMessage`, `ListDirectMessages`) were added. A daemon started
///    before this bump must be restarted; the handshake refuses it.
/// 8: voice-call signalling (`VoiceJoin`, `VoiceLeave`, `SetVoiceState`,
///    `VoiceSnapshot`), the `Reply::Voice` and `DaemonEvent::Voice` variants,
///    and the `VoiceChange`/`VoiceSnapshot` payloads were added.
/// 9: call audio counters (`VoiceAudio`) and the `VoiceAudioStats` payload,
///    plus the `Reply::VoiceAudio` and `DaemonEvent::VoiceAudio` variants,
///    were added.
/// 10: protocol 1.0. `Hello`/`Welcome` carry a `protocol` string and
///    capability lists, `Welcome` advertises `max_frame_bytes`, `Rejected`
///    was added, and `Op::Policy`/`Reply::Policy` expose the guild policy
///    read model. The flat counter is not continued because an old daemon
///    must refuse the new `Hello` rather than parse its capabilities as the
///    version-9 fields.
/// 11: `VoiceAudioStats` gained the media transport fields `transport`,
///    `rtt_ms`, `packets_lost` and `relayed`.
/// 12: protocol 2.0. The `VoiceAudioStats` change above is not wire-compatible
///    in postcard, so the major number is bumped to refuse an old daemon at
///    the handshake rather than fail on the first audio event.
/// 13: protocol 3.0. `VoiceAudioStats` gained the end-to-end encryption
///    fields `e2ee`, `epoch` and `e2ee_errors`. Like the 11-to-12 change this
///    is not wire-compatible in postcard, so the major number is bumped to
///    refuse an old daemon at the handshake.
/// 14: `Op::Capabilities` and `Reply::Capabilities` expose what this build
///    actually compiled and selected. They are appended so no existing variant
///    moves, and the `capabilities` handshake name lets a frontend negotiate
///    the new request without an old daemon seeing it.
/// 15: protocol 4.0. `DaemonEvent::Diagnostic` carries a classified,
///    log-backed diagnostic, and `DaemonStatusDto` names the daemon log path.
///    The old `DaemonEvent::Error` stays in the enum for a legacy daemon but
///    this build no longer emits it.
/// 16: protocol 5.0. `VoiceSnapshot` gained the media hub, call scope, hub
///    policy and key epoch, and `GuildPolicy`/`MediaPolicy` gained the
///    per-commune `media`, `call_scope` and `hub` fields. The new fields are
///    not postcard-compatible, so the major number is bumped to refuse an old
///    daemon at the handshake rather than misdecode a voice or policy frame.
/// 17: protocol 6.0. `CallTopology` replaces the tree-shaped
///    `Forwarding { root, parents }` with the flat `Star { hub, leaves }` that
///    the leaf-to-hub key rule can key. The two variants have different
///    postcard shapes, so the major number is bumped to refuse an old daemon
///    at the handshake rather than misdecode a voice snapshot.
pub const IPC_VERSION: u16 = 17;
/// Frames larger than this are refused before allocation. It has to clear one
/// whole attachment plus op overhead, because `PutBlob` carries the file in a
/// single frame rather than teaching both ends a chunked upload protocol.
pub const MAX_FRAME_BYTES: usize = MAX_ATTACHMENT_BYTES as usize + 1024 * 1024;
/// The largest frame either end may accept before the handshake completes.
///
/// An unauthenticated peer on the socket could otherwise announce a huge frame
/// and make the daemon allocate it. `Hello`, `Welcome` and `Rejected` are all
/// far below this, so the handshake is read with the small cap and only the
/// post-handshake reader uses [`MAX_FRAME_BYTES`].
pub const MAX_HANDSHAKE_BYTES: usize = 64 * 1024;
pub const EVENT_CHANNEL_CAPACITY: usize = 512;
pub const CLIENT_NAME: &str = "ovvc-client";

// The capability registry is closed: both ends only advertise these names, and
// an unknown string in a handshake is a peer bug rather than a negotiation
// point. Adding a capability is a protocol change and belongs with a minor
// bump, because an old peer that does not know the name must not assume it.

/// Voice signalling: join, leave, roster and call state.
pub const CAP_VOICE: &str = "voice";
/// Local audio engine counters for an active call.
pub const CAP_VOICE_AUDIO: &str = "voice-audio";
/// Control sessions and snapshots.
pub const CAP_CONTROL: &str = "control";
/// Presence status.
pub const CAP_PRESENCE: &str = "presence";
/// Typing indicators.
pub const CAP_TYPING: &str = "typing";
/// Direct conversations.
pub const CAP_DM: &str = "dm";
/// Attachment upload, fetch and caching.
pub const CAP_ATTACHMENTS: &str = "attachments";
/// Notification settings and notification events.
pub const CAP_NOTIFICATIONS: &str = "notifications";
/// The guild policy read model.
pub const CAP_POLICY: &str = "policy";
/// A frontend can apply a theme or colour scheme.
pub const CAP_THEME: &str = "theme";
/// The daemon can answer [`Op::Capabilities`] with a runtime report of the
/// audio backends, transports, codecs and features this build compiled.
///
/// It is advertised by the daemon only. A new client checks the daemon's
/// `Welcome` for it before asking; keeping it out of [`CLIENT_CAPABILITIES`]
/// means a new client's `Hello` is still accepted by an older daemon, which
/// rejects unknown client capability names.
pub const CAP_CAPABILITIES: &str = "capabilities";

/// Everything this build's daemon can serve.
pub const DAEMON_CAPABILITIES: &[&str] = &[
    CAP_VOICE,
    CAP_VOICE_AUDIO,
    CAP_CONTROL,
    CAP_PRESENCE,
    CAP_TYPING,
    CAP_DM,
    CAP_ATTACHMENTS,
    CAP_NOTIFICATIONS,
    CAP_POLICY,
    CAP_CAPABILITIES,
];

/// Everything this build's client can render, including capabilities only a
/// frontend has an opinion about, such as `theme`.
///
/// Voice and voice-audio are listed because the daemon gates those event
/// classes on this list; a frontend that cannot show a call can send its own
/// narrower `Hello` with [`Hello::with_capabilities`].
pub const CLIENT_CAPABILITIES: &[&str] = &[
    CAP_VOICE,
    CAP_VOICE_AUDIO,
    CAP_CONTROL,
    CAP_PRESENCE,
    CAP_TYPING,
    CAP_DM,
    CAP_ATTACHMENTS,
    CAP_NOTIFICATIONS,
    CAP_POLICY,
    CAP_THEME,
];

/// The major number of a `major.minor` protocol string, `None` when it is not
/// exactly two dot-separated decimal numbers.
///
/// Only the major decides wire compatibility. A minor difference is additive
/// and is left to the capability lists to settle.
pub fn protocol_major(protocol: &str) -> Option<u32> {
    let (major, minor) = protocol.split_once('.')?;
    if major.is_empty()
        || minor.is_empty()
        || minor.contains('.')
        || !major.bytes().all(|byte| byte.is_ascii_digit())
        || !minor.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let major = major.parse().ok()?;
    minor.parse::<u32>().ok()?;
    Some(major)
}

/// True when every name in `required` appears in `have`.
///
/// The names are compared exactly. An empty `required` is satisfied by any
/// list, including an empty one.
pub fn capabilities_ok(have: &[String], required: &[&str]) -> bool {
    required
        .iter()
        .all(|needed| have.iter().any(|capability| capability == needed))
}

/// Names in `advertised` that are not in the closed `registry`.
///
/// A typo such as `voice_audio` would otherwise be silently ignored by
/// [`capabilities_ok`], so a peer would stop receiving the events it expects
/// with no error on either side.
pub fn unknown_capabilities<'a>(advertised: &'a [String], registry: &[&str]) -> Vec<&'a str> {
    advertised
        .iter()
        .map(String::as_str)
        .filter(|name| !registry.contains(name))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// `major.minor`, for example [`IPC_PROTOCOL`].
    pub protocol: String,
    pub client_name: String,
    pub client_version: String,
    /// Names drawn from the closed registry; see [`CAP_VOICE`] and friends.
    pub capabilities: Vec<String>,
}

impl Hello {
    /// A hello advertising [`CLIENT_CAPABILITIES`].
    pub fn new(client_name: impl Into<String>) -> Self {
        Self::with_capabilities(client_name, CLIENT_CAPABILITIES)
    }

    /// A hello advertising exactly `capabilities`.
    pub fn with_capabilities(client_name: impl Into<String>, capabilities: &[&str]) -> Self {
        Self {
            protocol: IPC_PROTOCOL.to_string(),
            client_name: client_name.into(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: capabilities
                .iter()
                .map(|capability| (*capability).to_string())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// `major.minor`, for example [`IPC_PROTOCOL`].
    pub protocol: String,
    pub daemon_version: String,
    /// Names drawn from the closed registry; see [`DAEMON_CAPABILITIES`].
    pub capabilities: Vec<String>,
    /// The largest frame this daemon will accept or send after the handshake.
    /// A client must validate it before it trusts the connection.
    pub max_frame_bytes: u32,
    pub author_id: AuthorId,
    pub public_key_hex: String,
    pub peer_id: Option<String>,
    pub data_dir: String,
    pub socket: String,
}

impl Welcome {
    /// The welcome this build's daemon sends.
    pub fn for_daemon(
        daemon_version: impl Into<String>,
        author_id: AuthorId,
        public_key_hex: impl Into<String>,
        peer_id: Option<String>,
        data_dir: impl Into<String>,
        socket: impl Into<String>,
    ) -> Self {
        Self {
            protocol: IPC_PROTOCOL.to_string(),
            daemon_version: daemon_version.into(),
            capabilities: DAEMON_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_string())
                .collect(),
            max_frame_bytes: MAX_FRAME_BYTES as u32,
            author_id,
            public_key_hex: public_key_hex.into(),
            peer_id,
            data_dir: data_dir.into(),
            socket: socket.into(),
        }
    }
}

/// A guild's policy as the daemon resolved it for this member.
///
/// `guild` is what the guild published, `client_floor` is this member's local
/// minimum, and `effective` is `guild.with_client_floor(&client_floor)`, the
/// policy actually in force. `raised_by_client` names the axes the floor
/// raised, so a frontend can say why an operation is stricter than the guild
/// alone would require. It is empty when the floor changed nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyView {
    pub guild: GuildPolicy,
    pub client_floor: ClientPolicyFloor,
    pub effective: GuildPolicy,
    pub raised_by_client: Vec<String>,
}

/// Network settings for one guild. Multiaddrs stay strings on this boundary so
/// the IPC crate does not depend on libp2p's types.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkOptionsDto {
    pub listen: Vec<String>,
    pub dial: Vec<String>,
    pub relay: Option<String>,
    pub upnp: bool,
    pub natpmp: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionStateDto {
    pub guild_id: GuildId,
    pub connected: bool,
    pub peer_id: Option<String>,
    pub listen_addresses: Vec<String>,
    pub external_addresses: Vec<String>,
    pub peers: Vec<PeerSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerSnapshot {
    pub peer: String,
    pub author: Option<AuthorId>,
    pub connected: bool,
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub event: SignedEvent,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatusDto {
    pub version: String,
    pub pid: u32,
    pub data_dir: String,
    pub socket: String,
    pub started_at: u64,
    pub uptime_secs: u64,
    pub author_id: AuthorId,
    pub connected_guilds: Vec<GuildId>,
    /// The daemon's diagnostic log, so a user can find it after a bug.
    pub log_path: String,
}

/// What this daemon build actually compiled and selected at runtime.
///
/// The point of this report is honesty before a call: a default build uses the
/// silent null backend, so a frontend or `doctor` can say that rather than
/// discovering it from per-call audio counters, and video can say it is a
/// descriptor only instead of being reported healthy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonCapabilities {
    /// Device backends compiled into this binary, `null` first. `pipewire`
    /// appears only when the daemon was built with that feature.
    pub audio_backends_compiled: Vec<String>,
    /// The backend the configuration selects. `auto` is reported as `auto`
    /// when a live backend is compiled in, because it may silently fall back
    /// to `null` if the device or server cannot open; the per-call stats name
    /// the backend that actually started. A build with no live backend reports
    /// `null` for auto.
    pub audio_backend_selected: String,
    /// Audio codecs with a real implementation. Only `opus`; the lower voice
    /// profiles and Codec2 are not claimed.
    pub audio_codecs: Vec<String>,
    /// Media transports compiled into this binary.
    pub media_transports_compiled: Vec<String>,
    /// The transport selected by configuration, `webrtc` or `libp2p`.
    pub media_transport_selected: String,
    /// Compiled and runtime features: `notify`, `tray`, `live-audio` and
    /// `e2ee`. `live-audio` means a native device backend is compiled in, not
    /// that a call has one open.
    pub features: Vec<String>,
    /// Video is a descriptor only: no capture, track, screen-share op or live
    /// codec path exists in this build.
    pub video_descriptor_only: bool,
}

/// A guild's sidebar: categories plus every channel with its kind resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelTreeDto {
    pub categories: Vec<CategoryMeta>,
    pub channels: Vec<ChannelView>,
}

pub use ovvc_core::{Diagnostic, ErrorClass, NotificationSettings, QuietHours};

/// Counters for the local member's audio engine in one call.
///
/// The definition lives in `ovvc-media` so the client event surface can name
/// it without the frontend SDK depending on the local daemon protocol.
pub use ovvc_control::VoiceAudioStats;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Identity,
    Status,
    ListGuilds,
    GetGuild {
        guild_id: GuildId,
    },
    CreateGuild {
        name: String,
    },
    ListChannels {
        guild_id: GuildId,
    },
    CreateChannel {
        guild_id: GuildId,
        name: String,
    },
    /// Create a channel with a kind and an optional parent category.
    CreateChannelIn {
        guild_id: GuildId,
        name: String,
        kind: ChannelKind,
        parent: Option<ChannelId>,
    },
    CreateCategory {
        guild_id: GuildId,
        name: String,
    },
    RenameChannel {
        guild_id: GuildId,
        channel_id: ChannelId,
        name: String,
    },
    MoveChannel {
        guild_id: GuildId,
        channel_id: ChannelId,
        parent: Option<ChannelId>,
    },
    /// Categories and channels for the sidebar, in display order.
    ChannelTree {
        guild_id: GuildId,
    },
    NotificationSettings,
    /// Change one preference, leaving the others as the daemon has them.
    ///
    /// These exist instead of a whole-struct save because two frontends may
    /// have read the file at different times, and a save from the older copy
    /// used to undo the other window's change.
    SetDesktopNotifications {
        enabled: bool,
    },
    SetDoNotDisturb {
        enabled: bool,
    },
    SetQuietHours {
        quiet_hours: Option<QuietHours>,
    },
    SetChannelMuted {
        channel_id: ChannelId,
        muted: bool,
    },
    SetGuildMuted {
        guild_id: GuildId,
        muted: bool,
    },
    ListMembers {
        guild_id: GuildId,
    },
    SetDisplayName {
        guild_id: GuildId,
        display_name: String,
    },
    /// Publish this identity's profile. The display name stays on
    /// `SetDisplayName`; this carries only the fields profiles added.
    SetProfile {
        guild_id: GuildId,
        about: String,
        pronouns: String,
        links: Vec<String>,
    },
    /// One member's profile plus counts derived from the guild log.
    GetMember {
        guild_id: GuildId,
        author: AuthorId,
    },
    /// Open or reopen the direct conversation with `peer`, a member of
    /// `context`. Idempotent: both sides derive the same DM guild id.
    OpenDirectMessage {
        context: GuildId,
        peer: AuthorId,
    },
    /// Every direct conversation visible in the local store.
    ListDirectMessages,
    LeaveGuild {
        guild_id: GuildId,
    },
    History {
        guild_id: GuildId,
        channel_id: ChannelId,
        limit: usize,
    },
    /// One page of a channel's history, walking backwards from `cursor`.
    /// `cursor = None` asks for the newest events.
    HistoryBefore {
        guild_id: GuildId,
        channel_id: ChannelId,
        cursor: Option<SyncCursor>,
        limit: usize,
    },
    GetEvent {
        guild_id: GuildId,
        event_id: EventId,
    },
    PostMessage {
        guild_id: GuildId,
        channel_id: ChannelId,
        body: String,
    },
    PostMessageFull {
        guild_id: GuildId,
        channel_id: ChannelId,
        body: String,
        reply_to: Option<EventId>,
        mentions: Vec<AuthorId>,
    },
    /// Post a message with files attached. The blobs must already be in the
    /// daemon's store, which `PutBlob` does.
    PostMessageRich {
        guild_id: GuildId,
        channel_id: ChannelId,
        body: String,
        reply_to: Option<EventId>,
        mentions: Vec<AuthorId>,
        attachments: Vec<Attachment>,
    },
    /// Store one attachment's bytes. The daemon derives the id from the bytes
    /// themselves, so the reply names the hash the bytes were filed under and a
    /// caller can compare it with the one it announced.
    PutBlob {
        bytes: Vec<u8>,
    },
    /// Read a blob from the local store, `None` when it was never downloaded.
    GetBlob {
        hash: AttachmentId,
    },
    /// Whether a blob is already local.
    HasBlob {
        hash: AttachmentId,
    },
    /// Fetch a blob over the guild's peer network, from the local store when
    /// possible, and cache what comes back.
    FetchBlob {
        guild_id: GuildId,
        hash: AttachmentId,
    },
    EditMessage {
        guild_id: GuildId,
        target: EventId,
        body: String,
    },
    DeleteMessage {
        guild_id: GuildId,
        target: EventId,
    },
    AddReaction {
        guild_id: GuildId,
        target: EventId,
        emoji: String,
    },
    RemoveReaction {
        guild_id: GuildId,
        target: EventId,
        emoji: String,
    },
    MarkRead {
        guild_id: GuildId,
        channel_id: ChannelId,
        event_id: EventId,
    },
    UnreadCounts {
        guild_id: GuildId,
    },
    Search {
        guild_id: GuildId,
        query: String,
        limit: usize,
    },
    CreateInvite {
        guild_id: GuildId,
        peers: Vec<String>,
        expires_in_secs: Option<u64>,
    },
    JoinGuild {
        invite: String,
        options: NetworkOptionsDto,
    },
    Connect {
        guild_id: GuildId,
        options: NetworkOptionsDto,
    },
    Disconnect {
        guild_id: GuildId,
    },
    ConnectionState {
        guild_id: GuildId,
    },
    Lookup {
        guild_id: GuildId,
        author: AuthorId,
    },
    SetTyping {
        guild_id: GuildId,
        channel_id: ChannelId,
        typing: bool,
    },
    SetPresence {
        guild_id: GuildId,
        status: ovvc_control::PresenceStatus,
    },
    ControlSnapshot {
        guild_id: GuildId,
        session_id: SessionId,
    },
    ControlJoin {
        guild_id: GuildId,
        session_id: SessionId,
        display_name: String,
    },
    ControlLeave {
        guild_id: GuildId,
        session_id: SessionId,
    },
    /// Join the voice call on a voice channel. The daemon signs a `VoiceJoin`
    /// on the channel's control session and starts the media master/backup
    /// election so the call has coordinators before audio flows.
    VoiceJoin {
        guild_id: GuildId,
        channel_id: ChannelId,
        muted: bool,
        deafened: bool,
        push_to_talk: bool,
    },
    /// Leave the voice call on a voice channel.
    VoiceLeave {
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    /// Publish this identity's own audio/video flags in a call.
    SetVoiceState {
        guild_id: GuildId,
        channel_id: ChannelId,
        muted: bool,
        deafened: bool,
        push_to_talk: bool,
        speaking: bool,
        video: bool,
        screen_share: bool,
    },
    /// Tell the local audio service whether the push-to-talk key is held.
    ///
    /// This is device input, not roster state: it is not published to other
    /// members and is only meaningful while `push_to_talk` is set.
    SetPushToTalkHeld {
        guild_id: GuildId,
        channel_id: ChannelId,
        held: bool,
    },
    /// Read the current call, `None` when nobody is in it.
    VoiceSnapshot {
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    /// Read the audio counters for the local member's call on a voice
    /// channel, `None` when this daemon has no audio service for it.
    VoiceAudio {
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    /// Read the guild's durable policy together with the local client floor
    /// and the effective policy they resolve to.
    Policy {
        guild_id: GuildId,
    },
    ShutdownDaemon,
    /// What this daemon build compiled and selected: audio backends,
    /// transports, codecs, features and the video descriptor state.
    Capabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Unit,
    Identity {
        author_id: AuthorId,
        public_key_hex: String,
        peer_id: Option<String>,
    },
    Status(DaemonStatusDto),
    Guilds(Vec<GuildMeta>),
    Guild(Option<GuildMeta>),
    GuildCreated(GuildMeta),
    Channels(Vec<ChannelMeta>),
    Channel(ChannelMeta),
    ChannelTree(ChannelTreeDto),
    Category(CategoryMeta),
    NotificationSettings(NotificationSettings),
    Members(Vec<MemberInfo>),
    Event(SignedEvent),
    Events(Vec<SignedEvent>),
    HistoryPage(EventPage),
    OptionalEvent(Option<SignedEvent>),
    Invite(Invite),
    Connection(ConnectionStateDto),
    Peers(Vec<PeerSnapshot>),
    Unread(HashMap<ChannelId, usize>),
    BlobId(AttachmentId),
    Blob(Option<Vec<u8>>),
    Bool(bool),
    Search(Vec<SearchHit>),
    Control(ControlSnapshot),
    /// A voice call after the requested change, `None` when the call no
    /// longer has any member.
    Voice(Option<ovvc_control::VoiceSnapshot>),
    /// The local audio counters after the requested change, `None` when no
    /// audio service is running for that call.
    VoiceAudio(Option<VoiceAudioStats>),
    /// One member's profile and derived activity.
    Member(MemberProfile),
    /// Direct conversations, newest first.
    DirectMessages(Vec<DirectConversation>),
    /// The guild policy read model for one guild.
    Policy(PolicyView),
    /// The runtime capability report; see [`Op::Capabilities`].
    Capabilities(DaemonCapabilities),
}

/// Why a remote message was raised to the frontends as a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationReason {
    /// The message names the local author, either in `MessageV2::mentions` or
    /// through an `@name` token resolved against the guild roster.
    Mention,
    /// The message replies to a message the local author wrote.
    Reply,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DaemonEvent {
    Connected {
        daemon_version: String,
        author_id: AuthorId,
    },
    Disconnected {
        reason: String,
    },
    Listening {
        guild_id: GuildId,
        address: String,
    },
    ExternalAddr {
        guild_id: GuildId,
        address: String,
    },
    ExternalAddrRemoved {
        guild_id: GuildId,
        address: String,
    },
    PeerConnected {
        guild_id: GuildId,
        peer: String,
        author: Option<AuthorId>,
    },
    PeerDisconnected {
        guild_id: GuildId,
        peer: String,
        author: Option<AuthorId>,
    },
    Synced {
        guild_id: GuildId,
        peer: String,
    },
    HolePunch {
        guild_id: GuildId,
        peer: String,
        success: bool,
    },
    Event {
        guild_id: GuildId,
        event: SignedEvent,
        local: bool,
    },
    /// A remote message that mentions the local author or replies to one of
    /// their messages. The daemon deduplicates by `event_id`, so a resync that
    /// re-imports the same event does not notify twice.
    Notification {
        guild_id: GuildId,
        channel_id: ChannelId,
        event_id: EventId,
        author: AuthorId,
        body: String,
        reason: NotificationReason,
    },
    Control {
        guild_id: GuildId,
        session_id: SessionId,
        frame: ControlFrame,
    },
    ControlState {
        guild_id: GuildId,
        snapshot: ControlSnapshot,
    },
    /// A voice call changed. `snapshot` is the call after the change; it is
    /// `Some` even when the last member left, so a frontend can clear the
    /// channel's call state.
    Voice {
        guild_id: GuildId,
        channel_id: ChannelId,
        change: ovvc_control::VoiceChange,
        snapshot: Option<ovvc_control::VoiceSnapshot>,
    },
    /// The local audio engine's counters for a call, sampled once a second
    /// while the service runs.
    VoiceAudio {
        guild_id: GuildId,
        channel_id: ChannelId,
        stats: VoiceAudioStats,
    },
    Typing {
        guild_id: GuildId,
        channel_id: ChannelId,
        author: AuthorId,
        until_ms: u64,
    },
    TypingEnded {
        guild_id: GuildId,
        channel_id: ChannelId,
        author: AuthorId,
    },
    Presence {
        guild_id: GuildId,
        author: AuthorId,
        status: ovvc_control::PresenceStatus,
    },
    /// A legacy, unclassified error from an older daemon. This build reports
    /// [`DaemonEvent::Diagnostic`] instead, because the class has to be decided
    /// where the error is known rather than guessed from its text in a UI.
    Error {
        guild_id: Option<GuildId>,
        message: String,
    },
    /// A classified diagnostic with its daemon log path.
    ///
    /// `Diagnostic::class` separates an expected transport condition from a
    /// bug, a security rejection and a user error, so a frontend can colour and
    /// retain each one appropriately.
    Diagnostic(Diagnostic),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frame {
    Hello(Hello),
    Welcome(Welcome),
    /// The daemon refused the handshake. It names the protocol it was
    /// answering and gives a human-readable reason, then closes the
    /// connection, so a client records this as the connection error instead of
    /// retrying against a daemon it cannot talk to.
    Rejected {
        protocol: String,
        reason: String,
    },
    Request {
        id: u64,
        op: Op,
    },
    Response {
        id: u64,
        result: std::result::Result<Reply, IpcError>,
    },
    Event(DaemonEvent),
    Ping(u64),
    Pong(u64),
    Goodbye,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    NotFound,
    InvalidInput,
    InvalidSignature,
    Network,
    Timeout,
    NotConnected,
    Unsupported,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcError {
    pub kind: ErrorKind,
    pub message: String,
}

impl IpcError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Network, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Timeout, message)
    }

    pub fn not_connected(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotConnected, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for IpcError {}

/// The taxonomy class for a wire error kind.
///
/// A frontend never has to match on error text: a daemon that knows the cause
/// sets the kind, and this one function turns it into the displayed class.
pub fn classify_error_kind(kind: ErrorKind) -> ErrorClass {
    match kind {
        ErrorKind::NotFound | ErrorKind::InvalidInput | ErrorKind::Unsupported => ErrorClass::User,
        ErrorKind::InvalidSignature => ErrorClass::Security,
        ErrorKind::Network | ErrorKind::Timeout | ErrorKind::NotConnected => {
            ErrorClass::Operational
        }
        ErrorKind::Internal => ErrorClass::Bug,
    }
}

impl IpcError {
    /// The taxonomy class for this error.
    pub fn class(&self) -> ErrorClass {
        classify_error_kind(self.kind)
    }
}

impl From<std::io::Error> for IpcError {
    fn from(error: std::io::Error) -> Self {
        Self::new(ErrorKind::Internal, error.to_string())
    }
}

impl From<ovvc_core::CoreError> for IpcError {
    fn from(error: ovvc_core::CoreError) -> Self {
        use ovvc_core::CoreError;
        let kind = match &error {
            CoreError::NotFound(_) => ErrorKind::NotFound,
            CoreError::InvalidInput(_) | CoreError::InvalidId(_) => ErrorKind::InvalidInput,
            CoreError::InvalidSignature | CoreError::Signature(_) => ErrorKind::InvalidSignature,
            CoreError::Io(_) => ErrorKind::Internal,
            _ => ErrorKind::Internal,
        };
        Self::new(kind, error.to_string())
    }
}

/// What the client last learned about its connection to the daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionState {
    pub connected: bool,
    pub daemon: Option<Welcome>,
    /// The daemon's advertised capability names, empty until a handshake
    /// completes. Kept here as well as in `daemon` so a caller can check
    /// support without unwrapping the welcome.
    pub capabilities: Vec<String>,
    /// The daemon's advertised frame cap, `0` until a handshake completes.
    pub max_frame_bytes: u32,
    pub last_error: Option<String>,
    pub reconnect_attempts: u32,
    /// The daemon gave a definitive answer that retrying this process cannot
    /// change, such as a rejected handshake or a protocol major mismatch.
    /// `wait_connected` fails immediately with `last_error` instead of waiting
    /// for the timeout, and the supervisor stops reconnecting.
    pub terminal: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_core::CoreError;

    fn roundtrip<T>(value: &T) -> T
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de>,
    {
        postcard::from_bytes(&postcard::to_allocvec(value).unwrap()).unwrap()
    }

    #[test]
    fn unknown_capability_names_are_reported() {
        let hello = Hello::with_capabilities("tui", &[CAP_VOICE, CAP_POLICY]);
        assert!(unknown_capabilities(&hello.capabilities, CLIENT_CAPABILITIES).is_empty());

        let typo = Hello::with_capabilities("tui", &["voice_audio"]);
        assert_eq!(
            unknown_capabilities(&typo.capabilities, CLIENT_CAPABILITIES),
            vec!["voice_audio"]
        );
    }

    #[test]
    fn hello_carries_the_protocol_and_package_versions() {
        let hello = Hello::new("frontend");
        assert_eq!(hello.protocol, IPC_PROTOCOL);
        assert_eq!(hello.client_name, "frontend");
        assert_eq!(hello.client_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            hello.capabilities,
            CLIENT_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn hello_capabilities_can_be_narrowed() {
        let hello = Hello::with_capabilities("headless", &[CAP_POLICY, CAP_DM]);
        assert_eq!(hello.protocol, IPC_PROTOCOL);
        assert_eq!(hello.capabilities, vec![CAP_POLICY, CAP_DM]);
    }

    #[test]
    fn welcome_for_daemon_advertises_the_registry_and_frame_cap() {
        let welcome = Welcome::for_daemon(
            "0.1.0",
            AuthorId([7u8; 32]),
            "00",
            None,
            "/tmp/ovvc",
            "/tmp/ovvc/daemon.sock",
        );
        assert_eq!(welcome.protocol, IPC_PROTOCOL);
        assert_eq!(
            welcome.capabilities,
            DAEMON_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(welcome.max_frame_bytes as usize, MAX_FRAME_BYTES);
        assert!(welcome.max_frame_bytes > MAX_HANDSHAKE_BYTES as u32);
    }

    #[test]
    fn the_flat_star_shape_change_is_fenced_by_a_major_bump() {
        assert_eq!(IPC_PROTOCOL, "6.0");
        assert_eq!(IPC_VERSION, 17);
        assert_eq!(protocol_major(IPC_PROTOCOL), Some(6));
        // A 5.x daemon cannot decode the new voice snapshot.
        assert_ne!(protocol_major("5.0"), protocol_major(IPC_PROTOCOL));
        assert_ne!(protocol_major("4.0"), protocol_major(IPC_PROTOCOL));
        assert_ne!(protocol_major("3.1"), protocol_major(IPC_PROTOCOL));
        assert_ne!(protocol_major("2.0"), protocol_major(IPC_PROTOCOL));
        assert_ne!(protocol_major("1.0"), protocol_major(IPC_PROTOCOL));
        // A 6.0 client and a 6.y daemon share a major and keep talking.
        assert_eq!(protocol_major("6.0"), protocol_major(IPC_PROTOCOL));
    }

    #[test]
    fn every_wire_error_kind_maps_to_one_taxonomy_class() {
        let table = [
            (ErrorKind::NotFound, ErrorClass::User),
            (ErrorKind::InvalidInput, ErrorClass::User),
            (ErrorKind::Unsupported, ErrorClass::User),
            (ErrorKind::InvalidSignature, ErrorClass::Security),
            (ErrorKind::Network, ErrorClass::Operational),
            (ErrorKind::Timeout, ErrorClass::Operational),
            (ErrorKind::NotConnected, ErrorClass::Operational),
            (ErrorKind::Internal, ErrorClass::Bug),
        ];
        // The table covers every variant, so a new error kind must be added
        // here and to `classify_error_kind` together.
        for (kind, expected) in table {
            assert_eq!(classify_error_kind(kind), expected, "{kind:?}");
            assert_eq!(IpcError::new(kind, "x").class(), expected);
        }
    }

    #[test]
    fn a_diagnostic_event_and_status_log_path_roundtrip() {
        let diagnostic = Diagnostic::bug("impossible state")
            .with_guild(Some(GuildId([9u8; 32])))
            .with_context("test")
            .with_log_path("/tmp/ovvc/daemon.log")
            .with_hint("report this bug");
        let event = DaemonEvent::Diagnostic(diagnostic.clone());
        assert_eq!(roundtrip(&event), event);
        assert_eq!(diagnostic.class, ErrorClass::Bug);

        let status = DaemonStatusDto {
            version: "0.1.0".to_string(),
            pid: 7,
            data_dir: "/tmp/ovvc".to_string(),
            socket: "/tmp/ovvc.sock".to_string(),
            started_at: 1,
            uptime_secs: 2,
            author_id: AuthorId([1u8; 32]),
            connected_guilds: vec![GuildId([2u8; 32])],
            log_path: "/tmp/ovvc/daemon.log".to_string(),
        };
        assert_eq!(
            roundtrip(&Reply::Status(status.clone())),
            Reply::Status(status)
        );
    }

    #[test]
    fn the_runtime_capability_report_roundtrips() {
        let caps = DaemonCapabilities {
            audio_backends_compiled: vec!["null".to_string(), "pipewire".to_string()],
            audio_backend_selected: "pipewire".to_string(),
            audio_codecs: vec!["opus".to_string()],
            media_transports_compiled: vec!["webrtc".to_string(), "libp2p".to_string()],
            media_transport_selected: "webrtc".to_string(),
            features: vec![
                "notify".to_string(),
                "tray".to_string(),
                "live-audio".to_string(),
                "e2ee".to_string(),
            ],
            video_descriptor_only: true,
        };
        assert_eq!(roundtrip(&Op::Capabilities), Op::Capabilities);
        assert_eq!(
            roundtrip(&Reply::Capabilities(caps.clone())),
            Reply::Capabilities(caps)
        );
    }

    #[test]
    fn the_capability_name_is_advertised_by_the_daemon_only() {
        // A new client must still be accepted by an older daemon, which
        // rejects unknown client capability names, so `capabilities` is not
        // in the client registry.
        assert!(DAEMON_CAPABILITIES.contains(&CAP_CAPABILITIES));
        assert!(!CLIENT_CAPABILITIES.contains(&CAP_CAPABILITIES));
        let welcome = Welcome::for_daemon("0.1.0", AuthorId([1u8; 32]), "00", None, "/d", "/s");
        assert!(welcome.capabilities.iter().any(|name| name == CAP_CAPABILITIES));
    }

    #[test]
    fn protocol_major_accepts_a_tagged_pair_only() {
        assert_eq!(protocol_major("1.0"), Some(1));
        assert_eq!(protocol_major("1.7"), Some(1));
        assert_eq!(protocol_major("2.0"), Some(2));
        assert_eq!(protocol_major("1"), None);
        assert_eq!(protocol_major("1."), None);
        assert_eq!(protocol_major(".0"), None);
        assert_eq!(protocol_major("1.0.1"), None);
        assert_eq!(protocol_major("one.zero"), None);
        assert_eq!(protocol_major("+1.0"), None);
        assert_eq!(protocol_major("1.-0"), None);
        assert_eq!(protocol_major(""), None);
    }

    #[test]
    fn capability_checks_require_every_named_capability() {
        let have = vec![CAP_POLICY.to_string(), CAP_DM.to_string()];
        assert!(capabilities_ok(&have, &[CAP_POLICY]));
        assert!(capabilities_ok(&have, &[CAP_POLICY, CAP_DM]));
        assert!(capabilities_ok(&have, &[]));
        assert!(!capabilities_ok(&have, &[CAP_VOICE]));
        assert!(!capabilities_ok(&[], &[CAP_POLICY]));
        // Exact matching: a prefix is not the capability.
        assert!(!capabilities_ok(&have, &["pol"]));
    }

    #[test]
    fn the_policy_read_model_roundtrips_with_its_ops() {
        let guild = GuildPolicy::standard();
        let client_floor = ClientPolicyFloor {
            transport: Some(ovvc_core::TransportPolicy::Tor),
            ..ClientPolicyFloor::default()
        };
        let view = PolicyView {
            effective: guild.with_client_floor(&client_floor),
            guild,
            client_floor,
            raised_by_client: vec!["transport".to_string()],
        };
        assert_eq!(roundtrip(&view), view);
        assert_eq!(roundtrip(&Reply::Policy(view.clone())), Reply::Policy(view));

        let op = Op::Policy {
            guild_id: GuildId([4u8; 32]),
        };
        assert_eq!(roundtrip(&op), op);
    }

    #[test]
    fn a_rejected_handshake_roundtrips() {
        let frame = Frame::Rejected {
            protocol: "1.0".to_string(),
            reason: "missing capability policy".to_string(),
        };
        assert_eq!(roundtrip(&frame), frame);
    }

    #[test]
    fn errors_expose_their_kind_and_message() {
        let cases = [
            (IpcError::not_found("missing"), ErrorKind::NotFound),
            (IpcError::invalid("bad"), ErrorKind::InvalidInput),
            (IpcError::network("offline"), ErrorKind::Network),
            (IpcError::timeout("slow"), ErrorKind::Timeout),
            (IpcError::not_connected("gone"), ErrorKind::NotConnected),
            (IpcError::unsupported("no"), ErrorKind::Unsupported),
            (IpcError::internal("bug"), ErrorKind::Internal),
            (
                IpcError::new(ErrorKind::InvalidSignature, "sig"),
                ErrorKind::InvalidSignature,
            ),
        ];
        for (error, kind) in cases {
            assert_eq!(error.kind, kind);
            assert_eq!(error.to_string(), error.message);
        }
    }

    #[test]
    fn io_errors_become_internal_errors() {
        let error: IpcError = std::io::Error::other("disk").into();
        assert_eq!(error.kind, ErrorKind::Internal);
        assert!(error.message.contains("disk"));
    }

    #[test]
    fn core_errors_map_to_the_wire_kind() {
        let cases = [
            (
                CoreError::NotFound("guild".to_string()),
                ErrorKind::NotFound,
            ),
            (
                CoreError::InvalidInput("bad".to_string()),
                ErrorKind::InvalidInput,
            ),
            (
                CoreError::InvalidId("id".to_string()),
                ErrorKind::InvalidInput,
            ),
            (CoreError::InvalidSignature, ErrorKind::InvalidSignature),
            (CoreError::InvalidEventId, ErrorKind::Internal),
            (CoreError::EventTooLarge(9), ErrorKind::Internal),
            (CoreError::Backend("db".to_string()), ErrorKind::Internal),
        ];
        for (core, kind) in cases {
            let error: IpcError = core.into();
            assert_eq!(error.kind, kind);
        }
        let io: IpcError = CoreError::Io(std::io::Error::other("io")).into();
        assert_eq!(io.kind, ErrorKind::Internal);
    }

    #[test]
    fn connection_state_starts_disconnected() {
        let state = ConnectionState::default();
        assert!(!state.connected);
        assert!(state.daemon.is_none());
        assert!(state.capabilities.is_empty());
        assert_eq!(state.max_frame_bytes, 0);
        assert!(state.last_error.is_none());
        assert_eq!(state.reconnect_attempts, 0);
    }

    #[test]
    fn frames_roundtrip_with_requests_and_results() {
        let hello = Frame::Hello(Hello::new(CLIENT_NAME));
        assert_eq!(roundtrip(&hello), hello);

        let request = Frame::Request {
            id: 7,
            op: Op::GetGuild {
                guild_id: GuildId([1u8; 32]),
            },
        };
        assert_eq!(roundtrip(&request), request);

        let response = Frame::Response {
            id: 8,
            result: Err(IpcError::not_connected("daemon down")),
        };
        assert_eq!(roundtrip(&response), response);

        let ok = Frame::Response {
            id: 9,
            result: Ok(Reply::Bool(true)),
        };
        assert_eq!(roundtrip(&ok), ok);
    }

    #[test]
    fn ops_with_optional_and_collection_fields_roundtrip() {
        let ops = vec![
            Op::Status,
            Op::CreateChannelIn {
                guild_id: GuildId([2u8; 32]),
                name: "voice".to_string(),
                kind: ChannelKind::Voice,
                parent: Some(ChannelId([3u8; 16])),
            },
            Op::MoveChannel {
                guild_id: GuildId([2u8; 32]),
                channel_id: ChannelId([3u8; 16]),
                parent: None,
            },
            Op::PutBlob {
                bytes: vec![1, 2, 3],
            },
            Op::SetProfile {
                guild_id: GuildId([2u8; 32]),
                about: "hi".to_string(),
                pronouns: "they".to_string(),
                links: vec!["https://example.com".to_string()],
            },
        ];
        for op in ops {
            assert_eq!(roundtrip(&op), op);
        }
    }
}
