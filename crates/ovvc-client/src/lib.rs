//! Frontend-facing client facade for ovvc.
//!
//! A frontend (GUI, TUI, web shell, bot) depends on this crate alone and gets:
//!
//! * a verified, signed event log behind [`Storage`] (with a default on-disk
//!   backend from [`Store`], or a custom one), and
//! * optional peer-to-peer networking per guild, exposed as an event stream a
//!   frontend can render however it likes.
//!
//! Nothing here is tied to a terminal, a window toolkit, or a particular
//! transport. The event model, the storage contract, and the network protocol
//! live in `ovvc-core` and `ovvc-net`; this crate composes them.
//!
//! # Example
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use ovvc_client::{Client, ClientEvent, NetworkOptions};
//!
//! let client = Client::open_local("/tmp/ovvc")?;
//! let guild = client.find_guild("my guild")?;
//! let channel = client.default_channel(guild.id)?;
//!
//! let mut events = client.subscribe();
//! client.connect(guild.id, NetworkOptions::default()).await?;
//!
//! client.post_message(guild.id, channel.id, "hello from a custom frontend")?;
//!
//! while let Ok(event) = events.recv().await {
//!     if let ClientEvent::Event { event, .. } = event {
//!         println!("{:?}", event.body.kind);
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Designing a frontend
//!
//! * Storage is a trait. Implement [`Storage`] to persist events anywhere you
//!   like, then hand the client [`StorageConfig::Custom`]. Any operation the
//!   frontend cannot do offline (joining a guild, syncing) still works because
//!   the network layer only ever touches the trait.
//! * Networking is per guild and opt-in. A client with no connected guild is
//!   fully usable offline; [`Client::connect`] adds peers later.
//! * All peer/traffic notifications arrive on one [`ClientEvent`] stream, so a
//!   frontend can drive its UI from a single `select!`/`recv` loop.

use std::collections::{HashMap, HashSet};
#[cfg(feature = "store")]
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, anyhow};
use ovvc_core::model::now_unix;
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;

use ovvc_net::node::{MAX_MEDIA_TARGETS, NodeConfig, NodeEvent, NodeHandle};

pub use ovvc_core::error::CoreError;
pub use ovvc_core::{
    Attachment, AttachmentId, AuthorId, BLOB_CHUNK_BYTES, CategoryMeta, ChannelId, ChannelKind,
    ChannelMeta, ChannelView, DirectConversation, EventBody, EventId, EventKind, EventPage,
    GuildId, GuildMeta, Identity, Invite, InviteBody, MAX_ATTACHMENT_BYTES,
    MAX_ATTACHMENT_MIME_BYTES, MAX_ATTACHMENT_NAME_BYTES, MAX_ATTACHMENTS_PER_MESSAGE,
    MAX_EMOJI_BYTES, MAX_MENTIONS, MAX_PROFILE_ABOUT_BYTES, MAX_PROFILE_LINK_BYTES,
    MAX_PROFILE_LINKS, MAX_PROFILE_PRONOUNS_BYTES, MemberInfo, MemberProfile, NotificationSettings,
    QuietHours, Reaction, ReplyRef, SearchHit, SharedStorage, SignedEvent, Storage, SyncCursor,
    TimelineEntry, direct_guild_id, has_mention_syntax, hash_bytes, human_size, parse_mentions,
    shared_storage, timeline,
};
pub use ovvc_core::{Diagnostic, ErrorClass};
pub use ovvc_media::VoiceAudioStats;
pub use ovvc_net::{Multiaddr, PeerId, peer_id_for_author};
#[cfg(feature = "store")]
pub use ovvc_store::Store;

/// Errors produced by the client facade.
///
/// Storage and protocol failures keep their original error type so a frontend
/// can distinguish "the user asked for something that does not exist" from
/// "the network is down".
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error(transparent)]
    Network(#[from] anyhow::Error),
    #[error("commune {0} is not connected")]
    NotConnected(GuildId),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
}

impl ClientError {
    /// The taxonomy class for this facade error.
    ///
    /// A storage or protocol failure keeps its core class; a network,
    /// disconnected or timeout failure is an operational condition.
    pub fn class(&self) -> ErrorClass {
        match self {
            ClientError::Core(error) => ErrorClass::from_core_error(error),
            ClientError::Network(_) | ClientError::NotConnected(_) | ClientError::Timeout(_) => {
                ErrorClass::Operational
            }
        }
    }
}

pub type Result<T> = std::result::Result<T, ClientError>;

/// Where a [`Client`] keeps its events.
pub enum StorageConfig {
    /// Open the bundled redb-backed store inside this directory.
    #[cfg(feature = "store")]
    Directory(std::path::PathBuf),
    /// Use a caller-provided backend, for example an in-memory store or a
    /// frontend's own database.
    Custom(SharedStorage),
}

/// How a client should listen and which peers it should dial first.
#[derive(Debug, Clone, Default)]
pub struct NetworkOptions {
    /// Addresses to listen on. Empty means the node's default wildcard bind.
    pub listen: Vec<Multiaddr>,
    /// Peers to dial immediately. These also seed the DHT routing table.
    pub dial: Vec<Multiaddr>,
    /// Relay address used for NAT traversal and hole punching.
    pub relay: Option<Multiaddr>,
    /// Ask the router for a port mapping over UPnP IGD.
    pub upnp: bool,
    /// Ask the router for a port mapping over NAT-PMP.
    pub natpmp: bool,
}

/// Everything needed to construct a [`Client`].
pub struct ClientConfig {
    pub identity: Identity,
    pub storage: StorageConfig,
}

/// Events emitted by a connected client.
///
/// One stream covers every connected guild; each variant carries the guild it
/// belongs to. Events are best-effort: a slow subscriber may lag behind, which
/// it learns from `broadcast::error::RecvError::Lagged`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ClientEvent {
    Listening {
        guild_id: GuildId,
        address: Multiaddr,
    },
    ExternalAddr {
        guild_id: GuildId,
        address: Multiaddr,
    },
    ExternalAddrRemoved {
        guild_id: GuildId,
        address: Multiaddr,
    },
    PeerConnected {
        guild_id: GuildId,
        peer: PeerId,
    },
    PeerDisconnected {
        guild_id: GuildId,
        peer: PeerId,
    },
    HolePunch {
        guild_id: GuildId,
        peer: PeerId,
        success: bool,
    },
    /// A peer finished sending us their history for this guild.
    Synced {
        guild_id: GuildId,
        peer: PeerId,
    },
    /// An event that was newly imported from the network.
    ///
    /// This is every event kind, not only chat messages: member, channel, and
    /// guild metadata events arrive here too so a frontend can keep its
    /// roster and channel list in sync without polling storage.
    Event {
        guild_id: GuildId,
        event: SignedEvent,
    },
    /// A verified, ephemeral control frame from a peer.
    Control {
        guild_id: GuildId,
        frame: ovvc_control::ControlFrame,
    },
    /// One media frame from a peer in a voice call.
    ///
    /// The daemon holds the author-to-peer mapping and checks the packet's
    /// author against the authenticated source, so a frontend can treat the
    /// packet as one call member's frame. `from` is the source peer id.
    Media {
        guild_id: GuildId,
        channel_id: ChannelId,
        packet: ovvc_media::MediaPacket,
        from: String,
    },
    /// An ephemeral presence change that did not arrive as a control frame.
    ///
    /// The daemon emits this when a presence lease expires, and for a frame
    /// it applied locally on a frontend's behalf. Without it a frontend only
    /// learns about presence from control frames, so a member whose lease ran
    /// out would stay painted with their last status forever.
    Presence {
        guild_id: GuildId,
        author: AuthorId,
        status: ovvc_control::PresenceStatus,
    },
    /// A typing marker that changed or expired on the daemon.
    Typing {
        guild_id: GuildId,
        channel_id: ChannelId,
        author: AuthorId,
        until_ms: u64,
    },
    /// A typing marker that expired or was cleared.
    TypingEnded {
        guild_id: GuildId,
        channel_id: ChannelId,
        author: AuthorId,
    },
    /// A voice call changed. The snapshot is the call after the change, and is
    /// present even when its member list is empty so a frontend can clear the
    /// channel's call state.
    Voice {
        guild_id: GuildId,
        channel_id: ChannelId,
        change: ovvc_control::VoiceChange,
        snapshot: Option<ovvc_control::VoiceSnapshot>,
    },
    /// The local audio engine's counters for a call, sampled once a second
    /// while the daemon's audio service runs.
    ///
    /// These describe the audio device owned by the daemon, not a peer's
    /// stream, so a frontend can show capture, playback and device health
    /// without opening a device of its own.
    VoiceAudio {
        guild_id: GuildId,
        channel_id: ChannelId,
        stats: ovvc_media::VoiceAudioStats,
    },
    LookingUp {
        guild_id: GuildId,
        author: AuthorId,
    },
    PeerFound {
        guild_id: GuildId,
        author: AuthorId,
        addresses: Vec<Multiaddr>,
    },
    LookupFailed {
        guild_id: GuildId,
        author: AuthorId,
        reason: String,
    },
    /// A legacy, unclassified error from an older daemon.
    Error {
        guild_id: Option<GuildId>,
        message: String,
    },
    /// A classified diagnostic forwarded from the daemon, including the log
    /// path for a bug. A frontend colours and retains each class differently.
    Diagnostic(Diagnostic),
}

/// A snapshot of one guild's network node.
///
/// `connected` is false when the guild has no running node, in which case
/// every list is empty and `peer_id` is `None`. The address lists are best
/// first, as the node would dial them.
#[derive(Debug, Clone, Default)]
pub struct ConnectionState {
    pub connected: bool,
    pub peer_id: Option<PeerId>,
    pub peers: Vec<PeerId>,
    pub listen_addresses: Vec<Multiaddr>,
    pub external_addresses: Vec<Multiaddr>,
    pub dial_addresses: Vec<Multiaddr>,
}

const EVENT_CHANNEL_CAPACITY: usize = 1024;
/// Largest page [`Client::events_since`] and [`Client::history_before`] return
/// from one call, so a typo cannot ask the store for an unbounded walk.
const MAX_PAGE: usize = 500;
/// How long a blob download may take before the caller gets a timeout. A
/// 25 MiB attachment is fetched in 256 KiB chunks, so a slow link can spend
/// several minutes on one file, but a peer that stops answering must not leave
/// a UI waiting forever. Each chunk already has its own 30 second request
/// timeout in the network layer, which is what fails a dead peer quickly, so
/// this budget only has to be generous enough for throughput, not liveness.
const BLOB_FETCH_TIMEOUT: Duration = Duration::from_secs(300);
/// How long [`Client::join_guild`] waits for the first sign of life (a synced
/// peer or the first imported event) before it declares the invite unreachable.
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);
/// Once the inviter is clearly there, a large history may take much longer to
/// finish. The join is still working, so it gets its own budget instead of
/// failing at the connection timeout.
const JOIN_SYNC_TIMEOUT: Duration = Duration::from_secs(180);

/// A frontend-facing handle to one identity's local history and peer network.
///
/// `Client` is `Send + Sync`; share it behind an [`Arc`] (or use
/// [`Client::into_shared`]) if several UI tasks need it.
pub struct Client {
    /// The parts the per-guild network event loops need. It is behind an `Arc`
    /// so an imported `DirectMessageOpened` can be handled from the event loop
    /// without borrowing the `Client` it belongs to.
    runtime: Arc<Runtime>,
    /// In-process notification preferences. A daemon owns the real copy for
    /// attached frontends; this keeps the facade honest when it is used
    /// without one, for embedding and widget tests.
    settings: Mutex<NotificationSettings>,
}

struct Runtime {
    identity: Identity,
    storage: SharedStorage,
    events: broadcast::Sender<ClientEvent>,
    nodes: Mutex<HashMap<GuildId, NodeHandle>>,
    /// Serialises `connect`: without it two concurrent calls for the same
    /// guild can both pass the "already connected" check across the await and
    /// start two nodes that share one peer id.
    connect_lock: tokio::sync::Mutex<()>,
}

/// A DM mapping recovered from a `DirectMessageOpened` event in some guild
/// log, read from this identity's point of view.
///
/// The announcement exists before the DM guild does, and the recipient may not
/// have synced the opener's log yet, so this is the only way to name the
/// conversation while the DM guild has fewer than two members. `opener` is the
/// announcement author, which is also the founder of the DM guild the opener
/// creates, and `announced_at` stands in for a creation time until the real
/// `GuildMeta` syncs.
struct DirectAnnouncement {
    dm: GuildId,
    peer: AuthorId,
    opener: AuthorId,
    context: GuildId,
    announced_at: u64,
}

impl Client {
    /// Open a client with an explicit identity and storage backend.
    pub fn open(config: ClientConfig) -> Result<Self> {
        let storage = match config.storage {
            #[cfg(feature = "store")]
            StorageConfig::Directory(dir) => {
                let store = Store::open(&dir)?;
                shared_storage(store)
            }
            StorageConfig::Custom(storage) => storage,
        };
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Ok(Self {
            runtime: Arc::new(Runtime {
                identity: config.identity,
                storage,
                events,
                nodes: Mutex::new(HashMap::new()),
                connect_lock: tokio::sync::Mutex::new(()),
            }),
            settings: Mutex::new(NotificationSettings::default()),
        })
    }

    /// Open the default on-disk client for `data_dir`.
    ///
    /// The identity is loaded from `<data_dir>/identity.key`, creating it on
    /// first use, and the event store lives in the same directory. Requires the
    /// `store` feature (enabled by default).
    #[cfg(feature = "store")]
    pub fn open_local(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref();
        std::fs::create_dir_all(data_dir).context("creating data directory")?;
        // The store holds plaintext signed events and the guild index. On a
        // multi-user host, keep the whole tree owner-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))
                .context("restricting data directory permissions")?;
        }
        let (identity, _) = Identity::load_or_create(&data_dir.join("identity.key"))?;
        Self::open(ClientConfig {
            identity,
            storage: StorageConfig::Directory(data_dir.to_path_buf()),
        })
    }

    /// Open a client backed by caller-provided storage.
    ///
    /// Nothing is written to disk by the client itself; the identity must come
    /// from somewhere the caller controls.
    pub fn with_storage(identity: Identity, storage: SharedStorage) -> Result<Self> {
        Self::open(ClientConfig {
            identity,
            storage: StorageConfig::Custom(storage),
        })
    }

    pub fn identity(&self) -> &Identity {
        &self.runtime.identity
    }

    pub fn author_id(&self) -> AuthorId {
        self.runtime.identity.author_id()
    }

    /// The storage backend, for frontends that need to share it with their own
    /// components.
    pub fn storage(&self) -> SharedStorage {
        Arc::clone(&self.runtime.storage)
    }

    /// Subscribe to network events. Each call returns an independent receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<ClientEvent> {
        self.runtime.events.subscribe()
    }

    fn store<'a>(&'a self) -> MutexGuard<'a, dyn Storage + 'static> {
        self.runtime.storage.lock().expect("storage mutex poisoned")
    }

    // --- local history ---------------------------------------------------

    pub fn list_guilds(&self) -> Result<Vec<GuildMeta>> {
        Ok(self.store().list_guilds()?)
    }

    pub fn find_guild(&self, needle: &str) -> Result<GuildMeta> {
        Ok(self.store().find_guild(needle)?)
    }

    pub fn get_guild(&self, guild_id: GuildId) -> Result<Option<GuildMeta>> {
        Ok(self.store().get_guild(guild_id)?)
    }

    pub fn create_guild(&self, name: &str) -> Result<GuildMeta> {
        Ok(self
            .store()
            .create_guild(name, &self.runtime.identity)?
            .guild)
    }

    pub fn list_channels(&self, guild_id: GuildId) -> Result<Vec<ChannelMeta>> {
        Ok(self.store().list_channels(guild_id)?)
    }

    pub fn find_channel(&self, guild_id: GuildId, needle: &str) -> Result<ChannelMeta> {
        Ok(self.store().find_channel(guild_id, needle)?)
    }

    /// Create a channel and broadcast its `ChannelCreated` event.
    ///
    /// Peers that are online learn about the channel immediately. Without the
    /// broadcast they would not see it until the next history sync, and any
    /// message posted in the new channel in the meantime would be rejected by
    /// their importer as a message in an unknown channel.
    pub fn create_channel(&self, guild_id: GuildId, name: &str) -> Result<ChannelMeta> {
        let (channel, event) =
            self.store()
                .create_channel_with_event(guild_id, name, &self.runtime.identity)?;
        if let Some(event) = event {
            let _ = self.runtime.events.send(ClientEvent::Event {
                guild_id,
                event: event.clone(),
            });
            self.broadcast_event(&event);
        }
        Ok(channel)
    }

    /// Every category in a guild, in creation order.
    pub fn list_categories(&self, guild_id: GuildId) -> Result<Vec<CategoryMeta>> {
        Ok(self.store().list_categories(guild_id)?)
    }

    /// Channels with their kind and parent category resolved, in sidebar order.
    pub fn list_channel_views(&self, guild_id: GuildId) -> Result<Vec<ChannelView>> {
        Ok(self.store().list_channel_views(guild_id)?)
    }

    /// Create a category and broadcast its `CategoryCreated` event.
    pub fn create_category(&self, guild_id: GuildId, name: &str) -> Result<CategoryMeta> {
        let (category, event) =
            self.store()
                .create_category(guild_id, name, &self.runtime.identity)?;
        if let Some(event) = event {
            self.publish(guild_id, event);
        }
        Ok(category)
    }

    /// Create a channel with a kind and optional parent category, and broadcast
    /// its `ChannelCreatedV2` event.
    pub fn create_channel_in(
        &self,
        guild_id: GuildId,
        name: &str,
        kind: ChannelKind,
        parent: Option<ChannelId>,
    ) -> Result<ChannelMeta> {
        let (channel, event) =
            self.store()
                .create_channel_in(guild_id, name, kind, parent, &self.runtime.identity)?;
        if let Some(event) = event {
            self.publish(guild_id, event);
        }
        Ok(channel)
    }

    /// Rename a channel and broadcast the rename.
    pub fn rename_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        name: &str,
    ) -> Result<()> {
        if let Some(event) =
            self.store()
                .rename_channel(guild_id, channel_id, name, &self.runtime.identity)?
        {
            self.publish(guild_id, event);
        }
        Ok(())
    }

    /// Move a channel into a category, or to the root with `None`.
    pub fn move_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        parent: Option<ChannelId>,
    ) -> Result<()> {
        if let Some(event) =
            self.store()
                .move_channel(guild_id, channel_id, parent, &self.runtime.identity)?
        {
            self.publish(guild_id, event);
        }
        Ok(())
    }

    fn publish(&self, guild_id: GuildId, event: SignedEvent) {
        let _ = self.runtime.events.send(ClientEvent::Event {
            guild_id,
            event: event.clone(),
        });
        self.broadcast_event(&event);
    }

    /// The `#general` channel if present, otherwise the first channel.
    pub fn default_channel(&self, guild_id: GuildId) -> Result<ChannelMeta> {
        let channels = self.list_channels(guild_id)?;
        channels
            .iter()
            .find(|channel| channel.name == "general")
            .or_else(|| channels.first())
            .cloned()
            .ok_or_else(|| CoreError::NotFound(format!("channel in commune {guild_id}")).into())
    }

    pub fn list_members(&self, guild_id: GuildId) -> Result<Vec<MemberInfo>> {
        Ok(self.store().list_members(guild_id)?)
    }

    /// One channel by id, or `None` when it is unknown locally.
    pub fn get_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<Option<ChannelMeta>> {
        Ok(self.store().get_channel(guild_id, channel_id)?)
    }

    /// One member by author id, or `None` when they are not on the roster.
    pub fn get_member(&self, guild_id: GuildId, author: AuthorId) -> Result<Option<MemberInfo>> {
        Ok(self
            .store()
            .list_members(guild_id)?
            .into_iter()
            .find(|member| member.author == author))
    }

    /// Set the display name recorded for this identity in a guild.
    ///
    /// When the name actually changes this appends a signed `MemberRenamed`
    /// event and broadcasts it, so peers learn the new name through the normal
    /// event path instead of keeping a stale local roster entry.
    pub fn set_display_name(&self, guild_id: GuildId, display_name: &str) -> Result<()> {
        let event =
            self.store()
                .set_display_name(guild_id, display_name, &self.runtime.identity)?;
        if let Some(event) = event {
            self.broadcast_event(&event);
        }
        Ok(())
    }

    /// Append and broadcast a signed `MemberLeft` for this identity. The guild
    /// and its history remain on disk; leaving is a roster change, not a local
    /// delete.
    pub fn leave_guild(&self, guild_id: GuildId) -> Result<()> {
        let event = self.store().leave_guild(guild_id, &self.runtime.identity)?;
        if let Some(event) = event {
            self.broadcast_event(&event);
        }
        Ok(())
    }

    // --- profiles ---------------------------------------------------------

    /// Publish this identity's profile in a guild and broadcast the event.
    ///
    /// The display name is deliberately not part of this call: it stays on
    /// [`Self::set_display_name`] / `MemberRenamed`, so peers that predate
    /// profiles still see renames. The local event is echoed on
    /// [`Client::subscribe`] as well as sent to peers, which is how an attached
    /// daemon and its frontends learn to refresh the profile.
    pub fn set_profile(
        &self,
        guild_id: GuildId,
        about: &str,
        pronouns: &str,
        links: &[String],
    ) -> Result<SignedEvent> {
        let event =
            self.store()
                .set_profile(guild_id, &self.runtime.identity, about, pronouns, links)?;
        let _ = self.runtime.events.send(ClientEvent::Event {
            guild_id,
            event: event.clone(),
        });
        self.broadcast_event(&event);
        Ok(event)
    }

    /// Resolve one member's profile and derived activity.
    ///
    /// Returns [`CoreError::NotFound`] when the author is not on the roster.
    pub fn member_profile(&self, guild_id: GuildId, author: AuthorId) -> Result<MemberProfile> {
        Ok(self.store().member_profile(guild_id, author)?)
    }

    // --- direct messages --------------------------------------------------

    /// The other author of `guild_id` when it is a direct conversation with
    /// this identity.
    ///
    /// Returns `Some` when either the guild has exactly two members, this
    /// identity is one of them, and the id equals [`direct_guild_id`] of the
    /// pair, or a local `DirectMessageOpened` announcement names this guild as
    /// the conversation with one other person. The announcement covers the
    /// window before the recipient has synced the opener's log, when the DM
    /// guild has fewer than two members or does not exist locally yet. A guild
    /// with two members but a non-matching id is never a direct conversation,
    /// even if an unrelated announcement shares its id.
    pub fn direct_peer(&self, guild_id: GuildId) -> Result<Option<AuthorId>> {
        let me = self.author_id();
        let members = self.list_members(guild_id)?;
        if members.len() >= 2 {
            if members.len() != 2 {
                return Ok(None);
            }
            let Some(peer) = members.iter().find(|member| member.author != me) else {
                return Ok(None);
            };
            if direct_guild_id(&me, &peer.author) != guild_id {
                return Ok(None);
            }
            return Ok(Some(peer.author));
        }
        // Fewer than two members, or no local guild at all. The announcement
        // carries the complete pair, and `direct_announcements` re-derives the
        // id before trusting it, so a forged announcement cannot invent a
        // conversation.
        Ok(self
            .direct_announcements()?
            .into_iter()
            .find(|announcement| announcement.dm == guild_id)
            .map(|announcement| announcement.peer))
    }

    /// Every direct conversation this store knows about.
    ///
    /// A conversation is included when its guild has exactly two members and
    /// its id matches [`direct_guild_id`], or when a local
    /// `DirectMessageOpened` announcement names it, whichever is available
    /// first. The announcement matters for the opener between creating the DM
    /// guild and the recipient joining, and for the recipient between
    /// importing the announcement and syncing the opener's log: in both cases
    /// the guild has fewer than two members, so a roster-only view would drop
    /// the conversation from a sidebar right after it was opened.
    ///
    /// When only the announcement exists, the returned [`GuildMeta`] is a
    /// placeholder built from it, with the peer's display name, the
    /// announcement author as founder and the announcement timestamp as the
    /// creation time. It is replaced by the real guild metadata as soon as the
    /// log syncs. The `id` is always genuine, because it is checked against
    /// [`direct_guild_id`].
    pub fn list_direct_messages(&self) -> Result<Vec<DirectConversation>> {
        let me = self.author_id();
        let mut conversations = Vec::new();
        let mut seen = HashSet::new();
        for guild in self.list_guilds()? {
            let members = self.list_members(guild.id)?;
            if members.len() != 2 {
                continue;
            }
            let Some(peer) = members.iter().find(|member| member.author != me) else {
                continue;
            };
            if direct_guild_id(&me, &peer.author) != guild.id {
                continue;
            }
            seen.insert(guild.id);
            conversations.push(DirectConversation {
                guild,
                peer: peer.clone(),
            });
        }
        for announcement in self.direct_announcements()? {
            if !seen.insert(announcement.dm) {
                continue;
            }
            let guild = match self.get_guild(announcement.dm)? {
                Some(guild) => guild,
                None => self.direct_placeholder(
                    announcement.context,
                    announcement.dm,
                    announcement.peer,
                    announcement.opener,
                    announcement.announced_at,
                )?,
            };
            let peer = match self.get_member(announcement.dm, announcement.peer)? {
                Some(member) => member,
                None => MemberInfo {
                    guild_id: announcement.dm,
                    author: announcement.peer,
                    display_name: self
                        .direct_display_name(announcement.context, announcement.peer)?,
                    joined_at: 0,
                },
            };
            conversations.push(DirectConversation { guild, peer });
        }
        conversations.sort_by(|a, b| {
            b.guild
                .created_at
                .cmp(&a.guild.created_at)
                .then_with(|| a.guild.name.cmp(&b.guild.name))
                .then_with(|| a.guild.id.cmp(&b.guild.id))
        });
        Ok(conversations)
    }

    /// Open (or reopen) the direct conversation with `peer` from `context`.
    ///
    /// `peer` must be a member of `context`. Both sides derive the same guild
    /// id from their keys, so calling this twice, or both people calling it
    /// independently, converges on one guild and one log. The opener appends a
    /// `DirectMessageOpened` announcement to the context guild's first text
    /// channel, broadcasts it to anyone currently online, creates the local DM
    /// guild with its own `MemberJoined`, and dials the peer with the addresses
    /// the context node already knows (mDNS remains the fallback).
    ///
    /// A recipient that has already imported the peer's announcement does not
    /// create the guild: doing so would give the deterministic id a second
    /// founder and a log that cannot be merged. It starts the DM node, joins
    /// the opener's log once it syncs, and returns a placeholder until then.
    ///
    /// See [`direct_guild_id`] for why the id is derivable and what that means
    /// for confidentiality before the Phase 5 guild key exists.
    pub async fn open_direct_message(&self, context: GuildId, peer: AuthorId) -> Result<GuildMeta> {
        let me = self.author_id();
        if peer == me {
            return Err(CoreError::InvalidInput(
                "cannot open a direct message with yourself".to_string(),
            )
            .into());
        }
        let (peer_name, own_name) = {
            let members = self.list_members(context)?;
            let peer_member = members
                .iter()
                .find(|member| member.author == peer)
                .ok_or_else(|| {
                    CoreError::NotFound(format!("member {peer} in commune {context}"))
                })?;
            let own_name = members
                .iter()
                .find(|member| member.author == me)
                .map(|member| member.display_name.clone())
                .unwrap_or_else(|| me.short());
            (peer_member.display_name.clone(), own_name)
        };

        let dm_id = direct_guild_id(&me, &peer);
        // A `DirectMessageOpened` written by the peer means the peer founded
        // the conversation. Creating the guild here as well would give the
        // deterministic id two roots signed by two different founders, and a
        // log with two roots cannot be merged. The recipient joins only the
        // log the opener sends, so it waits for it and returns the same
        // placeholder the DM list uses in the meantime.
        let announcement = self
            .direct_announcements()?
            .into_iter()
            .find(|announcement| announcement.dm == dm_id && announcement.peer == peer);
        let recipient = announcement
            .as_ref()
            .is_some_and(|announcement| announcement.opener == peer);
        if !recipient || self.get_guild(dm_id)?.is_some() {
            // The opener creates the log; either side appends its own join to
            // a log that already exists, which is idempotent.
            self.runtime
                .ensure_direct_guild(dm_id, &peer_name, &own_name)?;
        }

        if !recipient && !self.has_direct_announcement(context, peer, dm_id)? {
            let channel = self.first_text_channel(context)?;
            let event = self.store().announce_direct_message(
                context,
                channel.id,
                peer,
                dm_id,
                &self.runtime.identity,
            )?;
            // Local echo first so an attached daemon's frontends refresh even
            // when no peer is online to receive the broadcast.
            let _ = self.runtime.events.send(ClientEvent::Event {
                guild_id: context,
                event: event.clone(),
            });
            self.runtime.broadcast_event(&event);
        }

        self.runtime
            .ensure_direct_connected(dm_id, peer, context)
            .await?;
        match self.get_guild(dm_id)? {
            Some(guild) => Ok(guild),
            None => {
                let (opener, created_at) = match &announcement {
                    Some(announcement) => (announcement.opener, announcement.announced_at),
                    None => (me, now_unix()),
                };
                self.direct_placeholder(context, dm_id, peer, opener, created_at)
            }
        }
    }

    /// Start the DM node for every conversation known from a stored
    /// announcement that is not already running.
    ///
    /// [`Runtime::handle_imported_event`] starts a DM node when the
    /// announcement first arrives, but a daemon restart does not replay stored
    /// events. Without this, a conversation listed from an announcement stays
    /// offline until a frontend explicitly reopens it, so its log never syncs
    /// and `history`/`send` can only ever report that it is still syncing.
    pub async fn resume_direct_messages(self: &Arc<Self>) -> Result<()> {
        for announcement in self.direct_announcements()? {
            if self.is_connected(announcement.dm) {
                continue;
            }
            if let Err(error) = self
                .runtime
                .ensure_direct_connected(announcement.dm, announcement.peer, announcement.context)
                .await
            {
                let _ = self.runtime.events.send(ClientEvent::Error {
                    guild_id: Some(announcement.dm),
                    message: format!("could not resume direct message: {error}"),
                });
            }
        }
        Ok(())
    }

    fn first_text_channel(&self, guild_id: GuildId) -> Result<ChannelMeta> {
        if let Some(view) = self
            .list_channel_views(guild_id)?
            .into_iter()
            .find(|view| view.kind.is_textual())
            && let Some(channel) = self.get_channel(guild_id, view.id)?
        {
            return Ok(channel);
        }
        self.default_channel(guild_id)
    }

    /// Whether this identity already announced a DM with `peer` in `context`.
    fn has_direct_announcement(
        &self,
        context: GuildId,
        peer: AuthorId,
        dm: GuildId,
    ) -> Result<bool> {
        let me = self.author_id();
        let mut cursor = None;
        loop {
            let page = self.events_since(context, cursor, MAX_PAGE)?;
            for event in &page.events {
                if event.body.author == me
                    && matches!(
                        &event.body.kind,
                        EventKind::DirectMessageOpened { peer: announced_peer, dm: announced_dm }
                            if *announced_peer == peer && *announced_dm == dm
                    )
                {
                    return Ok(true);
                }
            }
            let Some(last) = page.events.last() else {
                return Ok(false);
            };
            if !page.more {
                return Ok(false);
            }
            cursor = Some(SyncCursor::after(last)?);
        }
    }

    /// Every DM this identity can learn from a stored announcement.
    ///
    /// Each guild log is walked in pages, which is the same traversal sync uses
    /// and needs no in-memory state, so the result is identical right after a
    /// restart. The event is accepted from either direction: the opener wrote
    /// `peer` in the payload and reads it back, while the recipient recognises
    /// itself in `peer` and takes the author as the other party. The id is
    /// re-derived from the pair, so an announcement with a mismatched id is
    /// discarded rather than trusted.
    fn direct_announcements(&self) -> Result<Vec<DirectAnnouncement>> {
        let me = self.author_id();
        let mut found: HashMap<GuildId, DirectAnnouncement> = HashMap::new();
        for guild in self.list_guilds()? {
            let mut cursor = None;
            loop {
                let page = self.events_since(guild.id, cursor, MAX_PAGE)?;
                for event in &page.events {
                    let EventKind::DirectMessageOpened { peer, dm } = &event.body.kind else {
                        continue;
                    };
                    let author = event.body.author;
                    let (other, opener) = if *peer == me && author != me {
                        (author, author)
                    } else if author == me && *peer != me {
                        (*peer, me)
                    } else {
                        continue;
                    };
                    if direct_guild_id(&me, &other) != *dm {
                        continue;
                    }
                    found.entry(*dm).or_insert(DirectAnnouncement {
                        dm: *dm,
                        peer: other,
                        opener,
                        context: guild.id,
                        announced_at: event.body.timestamp,
                    });
                }
                let Some(last) = page.events.last() else {
                    break;
                };
                if !page.more {
                    break;
                }
                cursor = Some(SyncCursor::after(last)?);
            }
        }
        Ok(found.into_values().collect())
    }

    /// A peer's display name from `context`, or the short author id when the
    /// roster has not synced that member yet.
    fn direct_display_name(&self, context: GuildId, peer: AuthorId) -> Result<String> {
        Ok(self
            .list_members(context)?
            .into_iter()
            .find(|member| member.author == peer)
            .map(|member| member.display_name)
            .unwrap_or_else(|| peer.short()))
    }

    /// Metadata for a direct conversation whose log has not reached this
    /// store yet.
    ///
    /// Built from a `DirectMessageOpened` announcement: the caller has already
    /// checked the id against [`direct_guild_id`], so the conversation is
    /// genuine, and the name comes from the roster of the commune where it was
    /// opened. The real guild metadata replaces it as soon as the log syncs.
    fn direct_placeholder(
        &self,
        context: GuildId,
        dm: GuildId,
        peer: AuthorId,
        opener: AuthorId,
        created_at: u64,
    ) -> Result<GuildMeta> {
        Ok(GuildMeta {
            id: dm,
            name: self.direct_display_name(context, peer)?,
            founder: opener,
            created_at,
        })
    }

    /// This client's in-process notification preferences.
    ///
    /// A daemon-backed frontend gets the daemon's copy instead, which is the
    /// one that persists and filters notifications.
    pub fn notification_settings(&self) -> NotificationSettings {
        self.settings
            .lock()
            .expect("settings mutex poisoned")
            .clone()
    }

    /// Replace every in-process preference at once.
    ///
    /// Daemon-backed frontends use the per-field setters below against the
    /// daemon's copy, so one window's stale save cannot undo another's change.
    pub fn set_notification_settings(&self, settings: NotificationSettings) {
        *self.settings.lock().expect("settings mutex poisoned") = settings;
    }

    pub fn set_desktop_notifications(&self, enabled: bool) {
        self.settings
            .lock()
            .expect("settings mutex poisoned")
            .desktop = enabled;
    }

    pub fn set_do_not_disturb(&self, enabled: bool) {
        self.settings.lock().expect("settings mutex poisoned").dnd = enabled;
    }

    pub fn set_quiet_hours(&self, quiet_hours: Option<QuietHours>) {
        self.settings
            .lock()
            .expect("settings mutex poisoned")
            .quiet_hours = quiet_hours;
    }

    pub fn set_channel_muted(&self, channel_id: ChannelId, muted: bool) {
        let mut settings = self.settings.lock().expect("settings mutex poisoned");
        toggle_id(&mut settings.muted_channels, channel_id, muted);
    }

    pub fn set_guild_muted(&self, guild_id: GuildId, muted: bool) {
        let mut settings = self.settings.lock().expect("settings mutex poisoned");
        toggle_id(&mut settings.muted_guilds, guild_id, muted);
    }

    pub fn history(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        limit: usize,
    ) -> Result<Vec<SignedEvent>> {
        Ok(self.store().history(guild_id, channel_id, limit)?)
    }

    /// Walk a channel's history backwards from `cursor`; `None` starts at the
    /// newest event. The returned page is ordered oldest first and can be
    /// prepended directly to a transcript.
    pub fn history_before(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        cursor: Option<SyncCursor>,
        limit: usize,
    ) -> Result<EventPage> {
        Ok(self
            .store()
            .history_before(guild_id, channel_id, cursor, limit.clamp(1, MAX_PAGE))?)
    }

    /// Walk a guild's whole event stream forwards from `cursor`.
    ///
    /// `None` starts at the beginning. Unlike [`Self::history`] this is not
    /// limited to one channel: channel creation, membership, and message
    /// events all appear in `(lamport, event_id)` order, which is what a
    /// frontend needs to rebuild its view from a stored cursor after a
    /// restart.
    pub fn events_since(
        &self,
        guild_id: GuildId,
        cursor: Option<SyncCursor>,
        limit: usize,
    ) -> Result<EventPage> {
        Ok(self.store().events_after(
            guild_id,
            cursor.unwrap_or_else(SyncCursor::start),
            limit.clamp(1, MAX_PAGE),
            8 * 1024 * 1024,
        )?)
    }

    pub fn get_event(&self, guild_id: GuildId, event_id: EventId) -> Result<Option<SignedEvent>> {
        Ok(self.store().get_event(guild_id, event_id)?)
    }

    /// Store bytes as a content-addressed blob and return the id that names
    /// them.
    ///
    /// This is the local half of sharing an attachment. A message that
    /// announces the returned id is a separate signed write.
    pub fn put_blob(&self, bytes: &[u8]) -> Result<AttachmentId> {
        let id = hash_bytes(bytes);
        self.store().put_blob(id, bytes)?;
        Ok(id)
    }

    /// Read a local blob, or `None` when it has not been downloaded.
    pub fn get_blob(&self, id: AttachmentId) -> Result<Option<Vec<u8>>> {
        Ok(self.store().get_blob(id)?)
    }

    /// Whether a blob is already local.
    pub fn has_blob(&self, id: AttachmentId) -> Result<bool> {
        Ok(self.store().has_blob(id)?)
    }

    /// Fetch a blob from a connected peer, or return the local copy.
    ///
    /// The bytes are verified against `id` before they are cached or returned,
    /// so a malicious peer cannot substitute different contents. The guild
    /// must be connected and at least one peer must have completed Hello.
    pub async fn fetch_blob(&self, guild_id: GuildId, id: AttachmentId) -> Result<Vec<u8>> {
        if let Some(bytes) = self.get_blob(id)? {
            return Ok(bytes);
        }
        let receiver = {
            let nodes = self.runtime.nodes.lock().expect("node map mutex poisoned");
            let node = nodes
                .get(&guild_id)
                .ok_or(ClientError::NotConnected(guild_id))?;
            node.fetch_blob(id)
                .ok_or_else(|| ClientError::Network(anyhow!("the network node is not running")))?
        };
        match timeout(BLOB_FETCH_TIMEOUT, receiver).await {
            Ok(Ok(Ok(bytes))) => Ok(bytes),
            Ok(Ok(Err(message))) => Err(ClientError::Network(anyhow!(message))),
            Ok(Err(_)) => Err(ClientError::Network(anyhow!(
                "the network node dropped the blob request"
            ))),
            Err(_) => Err(ClientError::Timeout(BLOB_FETCH_TIMEOUT)),
        }
    }

    /// Sign and store a message, then send it to every connected peer in the
    /// guild. The returned event is the one that was stored.
    pub fn post_message(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
    ) -> Result<SignedEvent> {
        let event =
            self.store()
                .post_message(guild_id, channel_id, body, &self.runtime.identity)?;
        // Local echo: a frontend that renders from the event stream sees its
        // own message without polling storage after every send. Receivers
        // filter on the author when they only want remote messages.
        let _ = self.runtime.events.send(ClientEvent::Event {
            guild_id,
            event: event.clone(),
        });
        self.broadcast_event(&event);
        Ok(event)
    }

    /// Sign and store a message that replies to `reply_to` and/or mentions
    /// `mentions`, then send it to every connected peer in the guild.
    ///
    /// `reply_to` must name a message in `channel_id`. `mentions` holds roster
    /// [`AuthorId`]s; [`ovvc_core::parse_mentions`] turns composer text into
    /// them. Sending with neither is the same as [`Self::post_message`].
    pub fn post_message_full(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
    ) -> Result<SignedEvent> {
        let event = self.store().post_message_full(
            guild_id,
            channel_id,
            body,
            reply_to,
            mentions,
            &self.runtime.identity,
        )?;
        let _ = self.runtime.events.send(ClientEvent::Event {
            guild_id,
            event: event.clone(),
        });
        self.broadcast_event(&event);
        Ok(event)
    }

    /// Sign and store a message that may reply, mention members and carry
    /// files, then send it to every connected peer in the guild.
    ///
    /// Every attachment must already be in the local blob store, with the size
    /// its metadata claims, because the author is the first source peers ask
    /// for those bytes and an announcement nobody can serve is worse than an
    /// error here. Call [`Self::put_blob`] first and build each [`Attachment`]
    /// from the [`AttachmentId`] it returns.
    pub fn post_message_rich(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
        attachments: &[Attachment],
    ) -> Result<SignedEvent> {
        for attachment in attachments {
            let bytes = self.store().get_blob(attachment.hash)?.ok_or_else(|| {
                ClientError::Core(CoreError::InvalidInput(format!(
                    "attachment {} is not in the local blob store; put_blob it first",
                    attachment.hash
                )))
            })?;
            if bytes.len() as u64 != attachment.size {
                return Err(ClientError::Core(CoreError::InvalidInput(format!(
                    "attachment {} is {} bytes locally but its metadata says {}",
                    attachment.hash,
                    bytes.len(),
                    attachment.size
                ))));
            }
        }
        let event = self.store().post_message_rich(
            guild_id,
            channel_id,
            body,
            reply_to,
            mentions,
            attachments,
            &self.runtime.identity,
        )?;
        let _ = self.runtime.events.send(ClientEvent::Event {
            guild_id,
            event: event.clone(),
        });
        self.broadcast_event(&event);
        Ok(event)
    }

    /// Replace the body of a message this identity authored, then send the
    /// signed `MessageEdited` to every connected peer.
    pub fn edit_message(
        &self,
        guild_id: GuildId,
        target: EventId,
        body: &str,
    ) -> Result<SignedEvent> {
        let event = self
            .store()
            .edit_message(guild_id, target, body, &self.runtime.identity)?;
        let _ = self.runtime.events.send(ClientEvent::Event {
            guild_id,
            event: event.clone(),
        });
        self.broadcast_event(&event);
        Ok(event)
    }

    /// Tombstone a message this identity authored, then send the signed
    /// `MessageDeleted` to every connected peer.
    pub fn delete_message(&self, guild_id: GuildId, target: EventId) -> Result<SignedEvent> {
        let event = self
            .store()
            .delete_message(guild_id, target, &self.runtime.identity)?;
        let _ = self.runtime.events.send(ClientEvent::Event {
            guild_id,
            event: event.clone(),
        });
        self.broadcast_event(&event);
        Ok(event)
    }

    /// Add this identity's reaction to a message and broadcast it.
    pub fn add_reaction(
        &self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
    ) -> Result<SignedEvent> {
        let event = self
            .store()
            .add_reaction(guild_id, target, emoji, &self.runtime.identity)?;
        let _ = self.runtime.events.send(ClientEvent::Event {
            guild_id,
            event: event.clone(),
        });
        self.broadcast_event(&event);
        Ok(event)
    }

    /// Remove this identity's own reaction and broadcast the removal.
    pub fn remove_reaction(
        &self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
    ) -> Result<SignedEvent> {
        let event =
            self.store()
                .remove_reaction(guild_id, target, emoji, &self.runtime.identity)?;
        let _ = self.runtime.events.send(ClientEvent::Event {
            guild_id,
            event: event.clone(),
        });
        self.broadcast_event(&event);
        Ok(event)
    }

    /// Record that this identity has read `channel_id` up to `event_id`.
    pub fn mark_read(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        event_id: EventId,
    ) -> Result<SignedEvent> {
        let event =
            self.store()
                .mark_read(guild_id, channel_id, event_id, &self.runtime.identity)?;
        let _ = self.runtime.events.send(ClientEvent::Event {
            guild_id,
            event: event.clone(),
        });
        self.broadcast_event(&event);
        Ok(event)
    }

    /// Unread message counts per channel for this identity.
    pub fn unread_counts(
        &self,
        guild_id: GuildId,
    ) -> Result<std::collections::HashMap<ChannelId, usize>> {
        Ok(self.store().unread_counts(guild_id, self.author_id())?)
    }

    /// Search folded message bodies in one guild, newest first.
    pub fn search(&self, guild_id: GuildId, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        Ok(self.store().search(guild_id, query, limit)?)
    }

    // --- invites ---------------------------------------------------------

    /// Create a signed invite for a guild.
    ///
    /// `peers` are multiaddrs (including `/p2p/<peer-id>`) the joining node
    /// should dial. `expires_in_secs` is relative to now; `None` means the
    /// invite never expires.
    pub fn create_invite(
        &self,
        guild_id: GuildId,
        peers: Vec<String>,
        expires_in_secs: Option<u64>,
    ) -> Result<Invite> {
        let guild = self
            .store()
            .get_guild(guild_id)?
            .ok_or_else(|| CoreError::NotFound(format!("commune {guild_id}")))?;
        let expires_at = expires_in_secs.map(|seconds| now_unix().saturating_add(seconds));
        Ok(Invite::new(
            guild.id,
            guild.name,
            &self.runtime.identity,
            peers,
            expires_at,
        )?)
    }

    /// Verify an invite, connect to its peers, and wait for the first sync.
    ///
    /// The network node stays connected after the call returns, so incoming
    /// messages keep arriving on [`Client::subscribe`]. A successful join also
    /// appends and broadcasts this identity's `MemberJoined`, so the founder
    /// and every other peer can see and profile the new member immediately.
    pub async fn join_guild(&self, invite: &Invite, options: NetworkOptions) -> Result<GuildMeta> {
        invite.verify()?;
        if invite.body.addresses.is_empty() {
            return Err(CoreError::InvalidInput("invite contains no peer addresses".into()).into());
        }
        let mut options = options;
        for address in &invite.body.addresses {
            options
                .dial
                .push(address.parse::<Multiaddr>().map_err(|error| {
                    CoreError::InvalidInput(format!("invalid peer multiaddr in invite: {error}"))
                })?);
        }

        let guild_id = invite.body.guild_id;
        // Joining a guild this client is already online in cannot produce a new
        // Synced event (the node is already connected, so `connect` is a
        // no-op), and waiting out the timeout would only tear down a healthy
        // connection. The invite's addresses are already covered by the
        // running node's want-list.
        let already_connected = self.is_connected(guild_id);
        if already_connected {
            // Read the guild before taking the storage lock again: a guard
            // held in an `if` condition would outlive the call inside the
            // body and deadlock on the non-reentrant mutex.
            let existing = self.store().get_guild(guild_id)?;
            if let Some(guild) = existing {
                self.ensure_joined_membership(guild_id)?;
                return Ok(guild);
            }
        }

        let mut events = self.subscribe();
        self.connect(guild_id, options).await?;

        // Phase one: an unreachable inviter produces nothing at all. Any event
        // imported from this guild is proof that the peer is there.
        let first = timeout(JOIN_TIMEOUT, async {
            loop {
                match events.recv().await {
                    Ok(ClientEvent::Synced { guild_id: id, .. }) if id == guild_id => break true,
                    Ok(ClientEvent::Event { guild_id: id, .. }) if id == guild_id => break false,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break false,
                }
            }
        })
        .await;

        // Phase two: the peer answered, so let a long history finish syncing.
        let unreachable = first.is_err();
        let synced = match first {
            Ok(true) => true,
            Ok(false) => timeout(JOIN_SYNC_TIMEOUT, async {
                loop {
                    match events.recv().await {
                        Ok(ClientEvent::Synced { guild_id: id, .. }) if id == guild_id => {
                            break true;
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break false,
                    }
                }
            })
            .await
            .unwrap_or(false),
            Err(_) => false,
        };

        // Joining means reaching the inviter, not merely finding the guild in
        // local history. A previous partial sync must not make an unreachable
        // invite look like a success.
        if !synced {
            // Leave no node behind: otherwise a retry with a corrected invite
            // would see the guild as "already connected", skip the dial, and
            // wait out the same timeout again. Only undo what this call
            // started; a pre-existing connection must survive a failed join.
            if !already_connected {
                self.disconnect(guild_id);
            }
            let waited = if unreachable {
                JOIN_TIMEOUT
            } else {
                JOIN_SYNC_TIMEOUT
            };
            return Err(ClientError::Timeout(waited));
        }

        // A joiner owns no roster entry until it authors an event, and a
        // silent joiner may never do so. Without this, the founder and every
        // other peer cannot see, profile, or open a direct message with the
        // new member until that member happens to post. The short author id
        // is the same placeholder the store gives any unknown author, and a
        // later rename replaces it.
        self.ensure_joined_membership(guild_id)?;

        self.store()
            .get_guild(guild_id)?
            .ok_or_else(|| CoreError::NotFound(format!("commune {guild_id} was not synced")).into())
    }

    /// Append this identity's `MemberJoined` when the roster does not have it.
    ///
    /// Idempotent: the store returns `None` when the author is already known.
    /// The event is echoed locally and broadcast so peers learn about the
    /// member at join time rather than at their first message.
    fn ensure_joined_membership(&self, guild_id: GuildId) -> Result<()> {
        let name = self.author_id().short();
        let event = self
            .store()
            .append_member_joined(guild_id, &name, &self.runtime.identity)?;
        if let Some(event) = event {
            let _ = self.runtime.events.send(ClientEvent::Event {
                guild_id,
                event: event.clone(),
            });
            self.broadcast_event(&event);
        }
        Ok(())
    }

    // --- networking ------------------------------------------------------

    /// Start the network node for one guild.
    ///
    /// Connecting an already connected guild is a no-op.
    ///
    /// # One guild per transport node
    ///
    /// Each call starts an independent libp2p node, but every node shares this
    /// client's identity and therefore advertises the same peer id. Running
    /// several nodes at once makes their DHT announcements collide and lands
    /// inbound connections on a node that only accepts one guild. A frontend
    /// that needs to be online in several guilds should stay offline per guild
    /// or use separate clients over the same [`SharedStorage`]:
    ///
    /// ```no_run
    /// # use ovvc_client::{Client, ClientConfig, StorageConfig};
    /// # fn demo(client: &Client) -> Result<(), Box<dyn std::error::Error>> {
    /// let other = Client::open(ClientConfig {
    ///     identity: client.identity().clone(),
    ///     storage: StorageConfig::Custom(client.storage()),
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn connect(&self, guild_id: GuildId, options: NetworkOptions) -> Result<()> {
        self.runtime.connect(guild_id, options).await
    }

    /// Stop the network node for a guild. Local history stays available.
    pub fn disconnect(&self, guild_id: GuildId) {
        self.runtime
            .nodes
            .lock()
            .expect("node map mutex poisoned")
            .remove(&guild_id);
    }

    pub fn is_connected(&self, guild_id: GuildId) -> bool {
        self.runtime
            .nodes
            .lock()
            .expect("node map mutex poisoned")
            .get(&guild_id)
            .is_some_and(ovvc_net::node::NodeHandle::is_alive)
    }

    /// The local peer id, if the guild is connected.
    pub fn peer_id(&self, guild_id: GuildId) -> Option<PeerId> {
        self.runtime
            .nodes
            .lock()
            .expect("node map mutex poisoned")
            .get(&guild_id)
            .map(|node| node.peer_id)
    }

    /// The node's current addresses and peer connections.
    ///
    /// This is a snapshot, not a live view: the node refreshes it once per
    /// event-loop turn, so a polling UI sees a value at most one event stale.
    /// It never fails, which keeps a status bar simple when a guild is not
    /// connected at all.
    pub fn connection_state(&self, guild_id: GuildId) -> ConnectionState {
        let nodes = self.runtime.nodes.lock().expect("node map mutex poisoned");
        let Some(node) = nodes.get(&guild_id) else {
            return ConnectionState::default();
        };
        let status = node.status();
        ConnectionState {
            // A node whose loop has exited no longer hears or sends anything,
            // so reporting it as connected would leave every frontend showing
            // "online" for a swarm that is gone.
            connected: node.is_alive(),
            peer_id: Some(node.peer_id),
            peers: status.connected,
            listen_addresses: status.listening,
            external_addresses: status.external,
            dial_addresses: status.dial,
        }
    }

    /// Peers this guild's node has a live connection to.
    pub fn connected_peers(&self, guild_id: GuildId) -> Vec<PeerId> {
        self.connection_state(guild_id).peers
    }

    /// Addresses this guild's node is listening on.
    pub fn listen_addresses(&self, guild_id: GuildId) -> Vec<Multiaddr> {
        self.connection_state(guild_id).listen_addresses
    }

    /// Addresses known for guild peers, best first.
    pub fn dial_addresses(&self, guild_id: GuildId) -> Vec<Multiaddr> {
        self.connection_state(guild_id).dial_addresses
    }

    /// Ask the DHT for an author's announced addresses and dial them.
    pub fn lookup(&self, guild_id: GuildId, author: AuthorId) -> Result<()> {
        let nodes = self.runtime.nodes.lock().expect("node map mutex poisoned");
        let node = nodes
            .get(&guild_id)
            .ok_or(ClientError::NotConnected(guild_id))?;
        node.lookup(author);
        Ok(())
    }

    /// Send an already-signed event to every connected peer in its guild.
    pub fn broadcast_event(&self, event: &SignedEvent) {
        self.runtime.broadcast_event(event);
    }

    /// Send an ephemeral control frame to every connected peer in its guild.
    pub fn broadcast_control(&self, frame: &ovvc_control::ControlFrame) {
        self.runtime.broadcast_control(frame);
    }

    /// Send an ephemeral control frame to one guild member.
    ///
    /// WebRTC signalling names a target author. Resolving it to the peer's
    /// libp2p identity and sending only there keeps SDP and ICE candidates
    /// (which carry private addresses) away from the rest of the guild.
    pub fn send_control_to(
        &self,
        guild_id: GuildId,
        to: AuthorId,
        frame: &ovvc_control::ControlFrame,
    ) {
        let Some(peer) = peer_id_for_author(to) else {
            return;
        };
        let nodes = self.runtime.nodes.lock().expect("node map mutex poisoned");
        if let Some(node) = nodes.get(&guild_id) {
            node.send_control_to(peer, frame.clone());
        }
    }

    /// Send one media frame for `channel_id` to the call members in `authors`.
    ///
    /// `authors` is the call roster; the local author is skipped, duplicates
    /// collapse to one peer and the target list is capped, so a caller never
    /// has to filter it first. The guild must be connected. The frame is not
    /// retried: this is the interim reliable/ordered phase 0 path, not RTP,
    /// and the next frame replaces it.
    pub fn send_media(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        packet: ovvc_media::MediaPacket,
        authors: &[AuthorId],
    ) -> Result<()> {
        self.runtime
            .send_media(guild_id, channel_id, packet, authors)
    }

    /// Stop every running node. Local history stays available and each guild
    /// may be reconnected later.
    pub fn shutdown(&self) {
        let mut nodes = self.runtime.nodes.lock().expect("node map mutex poisoned");
        for (_, node) in nodes.drain() {
            node.shutdown();
        }
    }

    /// Wrap this client in an [`Arc`] for sharing between tasks.
    pub fn into_shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

// A frontend stores the client in a shared UI state or spawns tasks against
// it, so losing either bound is a breaking API change. Assert it here rather
// than discovering it at a call site.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Client>();
    assert_send_sync::<ClientEvent>();
    assert_send_sync::<ConnectionState>();
};

impl Runtime {
    fn store(&self) -> MutexGuard<'_, dyn Storage + 'static> {
        self.storage.lock().expect("storage mutex poisoned")
    }

    fn broadcast_event(&self, event: &SignedEvent) {
        if let Some(node) = self
            .nodes
            .lock()
            .expect("node map mutex poisoned")
            .get(&event.body.guild_id)
        {
            node.broadcast(event.clone());
        }
    }

    fn broadcast_control(&self, frame: &ovvc_control::ControlFrame) {
        if let Some(node) = self
            .nodes
            .lock()
            .expect("node map mutex poisoned")
            .get(&frame.guild_id)
        {
            node.send_control(frame.clone());
        }
    }

    /// Forward one media frame to the call members named by `authors`.
    fn send_media(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        packet: ovvc_media::MediaPacket,
        authors: &[AuthorId],
    ) -> Result<()> {
        let targets = media_recipients(authors, self.identity.author_id());
        let nodes = self.nodes.lock().expect("node map mutex poisoned");
        let node = nodes
            .get(&guild_id)
            .ok_or(ClientError::NotConnected(guild_id))?;
        node.send_media(channel_id, packet, targets);
        Ok(())
    }

    async fn connect(self: &Arc<Self>, guild_id: GuildId, options: NetworkOptions) -> Result<()> {
        let _guard = self.connect_lock.lock().await;
        {
            let mut nodes = self.nodes.lock().expect("node map mutex poisoned");
            if nodes
                .get(&guild_id)
                .is_some_and(ovvc_net::node::NodeHandle::is_alive)
            {
                return Ok(());
            }
            // A node whose loop has exited is no longer serving the guild, so
            // it must not make `connect` answer Ok without a replacement.
            nodes.remove(&guild_id);
        }

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let node = ovvc_net::node::start(
            NodeConfig {
                identity_seed: self.identity.seed(),
                guild_id,
                store: Arc::clone(&self.storage),
                listen: options.listen,
                dial: options.dial,
                relay: options.relay,
                enable_upnp: options.upnp,
                enable_natpmp: options.natpmp,
            },
            event_tx,
        )
        .await
        .context("starting network node")?;

        let runtime = Arc::clone(self);
        tokio::spawn(forward_events(guild_id, event_rx, runtime));
        self.nodes
            .lock()
            .expect("node map mutex poisoned")
            .insert(guild_id, node);
        Ok(())
    }

    /// Create the local DM guild if it is missing, and make sure this identity
    /// is on its roster.
    ///
    /// Only the side that opens the conversation may create the guild. The
    /// recipient joins an existing log: two founders for one deterministic id
    /// conflict in [`Storage::import_event`], which would make the DM log
    /// unmergeable. When the log already exists, either side may call this,
    /// because appending the local `MemberJoined` is idempotent. A recipient
    /// without the log starts the node and waits; see
    /// [`Runtime::maybe_join_direct_guild`].
    fn ensure_direct_guild(&self, dm_id: GuildId, peer_name: &str, own_name: &str) -> Result<()> {
        let events = {
            let mut store = self.store();
            if store.get_guild(dm_id)?.is_some() {
                match store.append_member_joined(dm_id, own_name, &self.identity)? {
                    Some(event) => vec![event],
                    None => Vec::new(),
                }
            } else {
                store.create_guild_with_id(dm_id, peer_name, own_name, &self.identity)?;
                store
                    .events_after(dm_id, SyncCursor::start(), 64, 1024 * 1024)?
                    .events
            }
        };
        // A local subscriber (the daemon, a GUI) only learns a guild exists
        // through events or an explicit reply, so echo the creation and join.
        // Broadcasting them too covers the case where the peer's node is
        // already connected and completed its empty sync before the guild
        // existed here; otherwise it would have to wait for a reconnect.
        for event in events {
            let _ = self.events.send(ClientEvent::Event {
                guild_id: dm_id,
                event: event.clone(),
            });
            self.broadcast_event(&event);
        }
        Ok(())
    }

    /// Start the DM node if it is not running, dialling addresses already
    /// known for `peer` on the context node. mDNS remains the fallback when
    /// the context node never met the peer.
    async fn ensure_direct_connected(
        self: &Arc<Self>,
        dm: GuildId,
        peer: AuthorId,
        context: GuildId,
    ) -> Result<()> {
        {
            let nodes = self.nodes.lock().expect("node map mutex poisoned");
            if nodes
                .get(&dm)
                .is_some_and(ovvc_net::node::NodeHandle::is_alive)
            {
                if let Some(node) = nodes.get(&dm) {
                    node.lookup(peer);
                }
                return Ok(());
            }
        }

        let mut options = NetworkOptions::default();
        if let Some(peer_id) = ovvc_net::peer_id_for_author(peer) {
            let nodes = self.nodes.lock().expect("node map mutex poisoned");
            if let Some(node) = nodes.get(&context) {
                options.dial = node.known_addrs(peer_id);
            }
        }
        self.connect(dm, options).await
    }

    /// Imported-event hook for the per-guild node loop.
    ///
    /// Two things live here that a frontend must not have to know about:
    /// opening the DM when the announcement arrives, and joining a DM guild
    /// that the opener created. Both run from the node's event loop, before
    /// the event is published, so the DM guild exists by the time a daemon or
    /// UI reacts to the announcement.
    fn handle_imported_event(self: &Arc<Self>, guild_id: GuildId, event: &SignedEvent) {
        let me = self.identity.author_id();
        match &event.body.kind {
            EventKind::DirectMessageOpened { peer, dm } if *peer == me => {
                // The sender created the DM guild. Start our node for it so it
                // can sync the opener's log; our own join waits for the
                // opening `MemberJoined` (see below). The connect is spawned
                // rather than awaited so one guild's node loop never waits on
                // another node starting, and so the announcement reaches
                // frontends immediately.
                let runtime = Arc::clone(self);
                let dm = *dm;
                let sender = event.body.author;
                tokio::spawn(async move {
                    if let Err(error) = runtime.ensure_direct_connected(dm, sender, guild_id).await
                    {
                        let _ = runtime.events.send(ClientEvent::Error {
                            guild_id: Some(dm),
                            message: format!("could not open direct message: {error}"),
                        });
                    }
                });
            }
            EventKind::MemberJoined { .. } if event.body.author != me => {
                self.maybe_join_direct_guild(guild_id, event.body.author);
            }
            _ => {}
        }
    }

    /// Append our `MemberJoined` to a DM guild the peer created.
    ///
    /// The trigger is the peer's own `MemberJoined`: by that point the opening
    /// `GuildCreated` and default channel are already in the log, so the store
    /// accepts the join. A guild whose id is not `direct_guild_id` of this
    /// identity and the event author is an ordinary guild and is left alone.
    fn maybe_join_direct_guild(&self, guild_id: GuildId, peer: AuthorId) {
        let me = self.identity.author_id();
        if direct_guild_id(&me, &peer) != guild_id {
            return;
        }
        // Resolve a display name before taking the store lock; the lookup
        // itself walks the roster and must not re-enter the same mutex.
        let own_name = {
            let store = self.store();
            let members = match store.list_members(guild_id) {
                Ok(members) => members,
                Err(_) => return,
            };
            if members.iter().any(|member| member.author == me) {
                return;
            }
            if !members.iter().any(|member| member.author == peer) {
                return;
            }
            drop(store);
            self.display_name_shared_with(peer)
        };
        let joined = {
            let mut store = self.store();
            match store.append_member_joined(guild_id, &own_name, &self.identity) {
                Ok(Some(event)) => event,
                Ok(None) => return,
                Err(error) => {
                    let _ = self.events.send(ClientEvent::Error {
                        guild_id: Some(guild_id),
                        message: format!("could not join direct message: {error}"),
                    });
                    return;
                }
            }
        };
        let _ = self.events.send(ClientEvent::Event {
            guild_id,
            event: joined.clone(),
        });
        self.broadcast_event(&joined);
    }

    /// This identity's display name in a guild that also has `peer`, falling
    /// back to any guild where it has a name, then to the short author id.
    fn display_name_shared_with(&self, peer: AuthorId) -> String {
        let me = self.identity.author_id();
        let store = self.store();
        let mut fallback = None;
        if let Ok(guilds) = store.list_guilds() {
            for guild in guilds {
                let Ok(members) = store.list_members(guild.id) else {
                    continue;
                };
                let Some(own) = members.iter().find(|member| member.author == me) else {
                    continue;
                };
                if members.iter().any(|member| member.author == peer) {
                    return own.display_name.clone();
                }
                fallback.get_or_insert_with(|| own.display_name.clone());
            }
        }
        fallback.unwrap_or_else(|| me.short())
    }
}

async fn forward_events(
    guild_id: GuildId,
    mut node_events: mpsc::UnboundedReceiver<NodeEvent>,
    runtime: Arc<Runtime>,
) {
    while let Some(event) = node_events.recv().await {
        if let NodeEvent::Event(event) = &event {
            runtime.handle_imported_event(guild_id, event);
        }
        let _ = runtime.events.send(map_event(guild_id, event));
    }
}

/// The peers one media frame should be sent to.
///
/// A call roster lists authors, including this identity; media is sent to the
/// other members. An author whose key is not a valid transport identity is
/// skipped, duplicates collapse to one peer, and the list is capped at
/// [`MAX_MEDIA_TARGETS`] exactly as the network command is.
fn media_recipients(authors: &[AuthorId], local: AuthorId) -> Vec<PeerId> {
    let mut peers = Vec::new();
    for author in authors {
        if peers.len() >= MAX_MEDIA_TARGETS {
            break;
        }
        if *author == local {
            continue;
        }
        let Some(peer) = peer_id_for_author(*author) else {
            continue;
        };
        if !peers.contains(&peer) {
            peers.push(peer);
        }
    }
    peers
}

fn map_event(guild_id: GuildId, event: NodeEvent) -> ClientEvent {
    match event {
        NodeEvent::Listening(address) => ClientEvent::Listening { guild_id, address },
        NodeEvent::ExternalAddr(address) => ClientEvent::ExternalAddr { guild_id, address },
        NodeEvent::ExternalAddrRemoved(address) => {
            ClientEvent::ExternalAddrRemoved { guild_id, address }
        }
        NodeEvent::PeerConnected(peer) => ClientEvent::PeerConnected { guild_id, peer },
        NodeEvent::PeerDisconnected(peer) => ClientEvent::PeerDisconnected { guild_id, peer },
        NodeEvent::HolePunch { peer, success } => ClientEvent::HolePunch {
            guild_id,
            peer,
            success,
        },
        NodeEvent::Synced(peer) => ClientEvent::Synced { guild_id, peer },
        NodeEvent::Event(event) => ClientEvent::Event { guild_id, event },
        NodeEvent::Control(frame) => ClientEvent::Control { guild_id, frame },
        NodeEvent::Media {
            channel_id,
            packet,
            from,
        } => ClientEvent::Media {
            guild_id,
            channel_id,
            packet,
            from: from.to_string(),
        },
        NodeEvent::LookingUp(author) => ClientEvent::LookingUp { guild_id, author },
        NodeEvent::PeerFound { author, addresses } => ClientEvent::PeerFound {
            guild_id,
            author,
            addresses,
        },
        NodeEvent::LookupFailed { author, reason } => ClientEvent::LookupFailed {
            guild_id,
            author,
            reason,
        },
        NodeEvent::Error(message) => ClientEvent::Error {
            guild_id: Some(guild_id),
            message,
        },
    }
}

/// Add an id to a preference list, or remove it, without duplicating it.
fn toggle_id<T: PartialEq>(list: &mut Vec<T>, id: T, present: bool) {
    if present {
        if !list.contains(&id) {
            list.push(id);
        }
    } else if let Some(index) = list.iter().position(|item| *item == id) {
        list.remove(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node whose swarm task has exited stays in the map until something
    /// removes it. `connect` used to answer Ok for that dead entry, so the
    /// daemon's repair pass could never bring the guild back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_replaces_a_node_whose_loop_has_exited() {
        let dir = tempfile::tempdir().unwrap();
        let client = Client::open_local(dir.path()).unwrap();
        let guild = client.create_guild("liveness").unwrap();
        let options = NetworkOptions {
            listen: vec!["/ip4/0.0.0.0/tcp/0".parse().unwrap()],
            ..NetworkOptions::default()
        };

        client.connect(guild.id, options.clone()).await.unwrap();
        assert!(client.is_connected(guild.id));

        {
            let nodes = client.runtime.nodes.lock().unwrap();
            nodes.get(&guild.id).expect("node").shutdown();
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while client.is_connected(guild.id) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the shut-down node never left the map as dead"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            client.connection_state(guild.id).peers.is_empty(),
            "a dead node must not claim peers"
        );

        client.connect(guild.id, options).await.unwrap();
        assert!(
            client.is_connected(guild.id),
            "connect returned Ok without starting a replacement node"
        );
    }

    /// The daemon passes the whole call roster; the client is responsible for
    /// turning it into the peers that should receive one frame. No swarm is
    /// needed for this: the live path is covered by the daemon's e2e tests.
    #[test]
    fn media_recipients_skip_the_local_author_and_duplicates() {
        let local = Identity::from_seed([1u8; 32]).author_id();
        let first = Identity::from_seed([2u8; 32]).author_id();
        let second = Identity::from_seed([3u8; 32]).author_id();

        let peers = media_recipients(&[local, first, first, second], local);
        assert_eq!(
            peers,
            vec![
                peer_id_for_author(first).expect("valid author key"),
                peer_id_for_author(second).expect("valid author key"),
            ],
            "the local author and a duplicate must not receive a frame"
        );
        assert!(media_recipients(&[local], local).is_empty());
    }

    #[test]
    fn media_recipients_are_capped() {
        let local = Identity::from_seed([0u8; 32]).author_id();
        let authors: Vec<AuthorId> = (1..=MAX_MEDIA_TARGETS as u8 + 4)
            .map(|seed| Identity::from_seed([seed; 32]).author_id())
            .collect();
        let peers = media_recipients(&authors, local);
        assert_eq!(peers.len(), MAX_MEDIA_TARGETS);
    }
}
