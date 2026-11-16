use std::fmt;
use std::str::FromStr;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::attachment::Attachment;
use crate::error::{CoreError, Result};

pub const EVENT_VERSION: u8 = 2;
/// Events written before the tagged-kind envelope existed. They remain
/// readable and verifiable forever; only new writes use [`EVENT_VERSION`].
pub const LEGACY_EVENT_VERSION: u8 = 1;

/// Whether this binary can decode and verify events of `version`.
///
/// Version 1 stores its `EventKind` as a positional postcard enum, so a kind
/// this binary does not know makes the whole body undecodable. Version 2 moves
/// the kind into a `(tag, payload)` pair: an unknown tag is preserved verbatim
/// and simply not applied, which is what lets newer peers add event kinds
/// without making older ones unable to open their own log.
pub const fn supported_event_version(version: u8) -> bool {
    matches!(version, LEGACY_EVENT_VERSION | EVENT_VERSION)
}
const EVENT_SIGNING_DOMAIN: &[u8] = b"ovvc/event/v1\0";

/// Decode a postcard value only if `bytes` is exactly the encoding this crate
/// would emit. `postcard::from_bytes` accepts trailing bytes and overlong
/// varints, which makes the same logical value have many wire spellings; at a
/// trust boundary that turns raw bytes into an unreliable identity key.
pub fn decode_canonical<T>(bytes: &[u8]) -> Result<T>
where
    T: DeserializeOwned + Serialize,
{
    let value: T = postcard::from_bytes(bytes)?;
    let canonical = postcard::to_allocvec(&value)?;
    if canonical.as_slice() != bytes {
        return Err(CoreError::InvalidInput(
            "non-canonical encoding".to_string(),
        ));
    }
    Ok(value)
}

macro_rules! id_type {
    ($name:ident, $len:expr) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub [u8; $len]);

        impl $name {
            pub fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }

            pub fn to_hex(&self) -> String {
                hex::encode(self.0)
            }

            pub fn short(&self) -> String {
                self.to_hex()[..8].to_string()
            }

            pub fn from_slice(bytes: &[u8]) -> Result<Self> {
                if bytes.len() != $len {
                    return Err(CoreError::InvalidId(format!(
                        "expected {} bytes, got {}",
                        $len,
                        bytes.len()
                    )));
                }
                let mut out = [0u8; $len];
                out.copy_from_slice(bytes);
                Ok(Self(out))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name))
                    .field(&self.to_hex())
                    .finish()
            }
        }

        impl FromStr for $name {
            type Err = CoreError;

            fn from_str(s: &str) -> Result<Self> {
                let s = s.trim();
                let bytes = hex::decode(s)
                    .map_err(|_| CoreError::InvalidId(format!("invalid hex id: {s}")))?;
                Self::from_slice(&bytes)
            }
        }
    };
}

id_type!(GuildId, 32);
id_type!(ChannelId, 16);
id_type!(EventId, 32);
id_type!(AuthorId, 32);

/// Domain separator for the deterministic direct-message guild id.
///
/// # Confidentiality limit
///
/// The id is derived from two public keys, so anyone who knows both authors
/// (or guesses them) can compute the same id, start a node for that guild id
/// and, because guild membership is currently an open event log rather than a
/// keyed capability, sync the DM log. The roadmap's Phase 5 guild key closes
/// this: the id will name the conversation, while access will require a key
/// the two people exchange. Until then a DM is private only in the sense that
/// its guild id is not published in the shared guild's metadata.
const DM_GUILD_DOMAIN: &[u8] = b"ovvc/direct-guild/v1\0";

/// The deterministic guild id for a direct conversation between `a` and `b`.
///
/// Both sides must derive the same id from the same pair, so the keys are
/// sorted before hashing; the argument order therefore never matters. A
/// different pair produces a different id with overwhelming probability.
///
/// See [`DM_GUILD_DOMAIN`] for the confidentiality limits of a derivable id.
pub fn direct_guild_id(a: &AuthorId, b: &AuthorId) -> GuildId {
    let (first, second) = if a.as_bytes() <= b.as_bytes() {
        (a.as_bytes(), b.as_bytes())
    } else {
        (b.as_bytes(), a.as_bytes())
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(DM_GUILD_DOMAIN);
    hasher.update(first);
    hasher.update(second);
    GuildId(*hasher.finalize().as_bytes())
}

impl AuthorId {
    /// Verify a detached ed25519 signature made by this identity.
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        if signature.len() != 64 {
            return false;
        }
        let Ok(signature) = Signature::from_slice(signature) else {
            return false;
        };
        let Ok(key) = VerifyingKey::from_bytes(&self.0) else {
            return false;
        };
        key.verify_strict(message, &signature).is_ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    GuildCreated {
        name: String,
    },
    ChannelCreated {
        name: String,
    },
    MemberJoined {
        display_name: String,
    },
    Message {
        body: String,
    },
    // New variants are appended, never inserted: postcard encodes an enum by
    // its variant index, so existing indexes must stay stable.
    MemberRenamed {
        display_name: String,
    },
    MemberLeft,
    /// Replaces the body of the `Message` identified by `target`.
    ///
    /// Only the message's author may edit it. Renderers apply the newest
    /// authorized edit; the original event stays in the log untouched.
    MessageEdited {
        target: EventId,
        body: String,
    },
    /// Tombstones the `Message` identified by `target`. Only its author may
    /// delete it. The body remains in the log on peers that already hold it;
    /// deletion is a rendering instruction, not a cryptographic erasure.
    MessageDeleted {
        target: EventId,
    },
    /// A kind this binary does not understand.
    ///
    /// Version-2 events carry the kind as a `(tag, payload)` pair. When the tag
    /// is unknown, the payload is kept byte-for-byte so the event still
    /// verifies and re-encodes identically. Indexing stores it, renderers
    /// ignore it, and a newer binary that knows `tag` applies it.
    Unknown {
        tag: u32,
        payload: Vec<u8>,
    },
    /// A message that replies to another message and/or mentions members.
    ///
    /// Kept separate from [`EventKind::Message`] so a peer that predates
    /// replies still renders ordinary messages: an older binary treats this
    /// kind as `Unknown` and drops it, rather than misreading a plain message
    /// as one with a reply header. `reply_to` names the event being answered;
    /// `mentions` stores roster [`AuthorId`]s, never display-name strings, so a
    /// rename cannot redirect a past mention.
    MessageV2 {
        body: String,
        reply_to: Option<EventId>,
        mentions: Vec<AuthorId>,
    },
    /// Adds one author's reaction to a message.
    ///
    /// Reactions are events like any other, so every peer converges on the
    /// same set without a separate mutable table, and an offline peer receives
    /// the full reaction history when it next syncs.
    ReactionAdded {
        target: EventId,
        emoji: String,
    },
    /// Removes one author's own reaction from a message.
    ReactionRemoved {
        target: EventId,
        emoji: String,
    },
    /// Records that this identity has read a channel up to an event.
    ///
    /// The marker is a signed event so read state follows the identity across
    /// devices. It carries the channel in the event envelope and the position
    /// in `event_id`; unread counting uses the `(lamport, event_id)` order.
    ReadUpTo {
        event_id: EventId,
    },
    /// A message with attachments, and optionally a reply and mentions.
    ///
    /// The event carries only the attachment metadata (hash, size, MIME type,
    /// name). The bytes live in the content-addressed blob store and are
    /// fetched separately, so a message stays small no matter how large its
    /// files are, and a peer that never downloads a file still reads the
    /// conversation. A peer that predates this kind treats it as `Unknown`
    /// and drops it rather than rendering a message whose files it cannot
    /// represent, which is the same rule [`EventKind::MessageV2`] follows.
    MessageV3 {
        body: String,
        reply_to: Option<EventId>,
        mentions: Vec<AuthorId>,
        attachments: Vec<Attachment>,
    },
    /// A named group of channels in the sidebar.
    ///
    /// The category's own id travels in the event envelope's `channel_id`, so
    /// it shares the [`ChannelId`] namespace and no new id type is needed. A
    /// category is metadata, not a place messages can be posted.
    CategoryCreated {
        name: String,
    },
    /// A channel that knows what it is and where it sits.
    ///
    /// `ChannelCreated` stays frozen for older peers; this kind adds the
    /// channel's [`ChannelKind`] and its optional parent category. A peer that
    /// predates it treats it as `Unknown`, which is the same compatibility rule
    /// the message variants follow.
    ChannelCreatedV2 {
        name: String,
        kind: ChannelKind,
        parent: Option<ChannelId>,
    },
    /// Renames an existing channel. The target travels in `channel_id`.
    ///
    /// Any member may rename for now; Phase 2.1 makes this a permission once
    /// roles exist. Peers apply the greatest `(lamport, event_id)` rename, so a
    /// late joiner converges on the same name.
    ChannelRenamed {
        target: ChannelId,
        name: String,
    },
    /// Moves a channel into a category, or back to the root with `None`.
    ChannelMoved {
        target: ChannelId,
        parent: Option<ChannelId>,
    },
    /// Replaces the free-form profile of the event's author.
    ///
    /// The display name stays on [`EventKind::MemberRenamed`], so a peer that
    /// predates profiles still observes renames correctly. Like every other
    /// metadata event, the latest `(lamport, event_id)` profile for an author
    /// wins, which makes the result independent of import order.
    MemberProfileUpdated {
        about: String,
        pronouns: String,
        links: Vec<String>,
    },
    /// Announces that its author opened a direct-message guild with `peer`.
    ///
    /// The event is appended to a shared guild both people already belong to,
    /// with `dm` in the payload rather than as the event's own guild. A peer
    /// that imports it can start a node for the same DM guild and join it once
    /// the opener's creation event has synced. The DM itself is an ordinary
    /// two-member guild, so messages, edits, deletes and membership all reuse
    /// the existing event paths.
    DirectMessageOpened {
        peer: AuthorId,
        dm: GuildId,
    },
}

/// What a channel is for.
///
/// Text and announcement channels are fully backed today. Voice and forum are
/// declared so the sidebar, dialogs and event log can carry them now; media
/// sessions land in Phase 3 and threads in Phase 2.5, and until then the
/// frontends show an honest placeholder instead of pretending to join.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ChannelKind {
    #[default]
    Text,
    Announcement,
    Voice,
    Forum,
}

impl ChannelKind {
    /// A one-character terminal glyph, ASCII so every monospace face has it.
    pub const fn glyph(self) -> char {
        match self {
            ChannelKind::Text => '#',
            ChannelKind::Announcement => '!',
            ChannelKind::Voice => '~',
            ChannelKind::Forum => '?',
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            ChannelKind::Text => "text",
            ChannelKind::Announcement => "announcement",
            ChannelKind::Voice => "voice",
            ChannelKind::Forum => "forum",
        }
    }

    /// Whether the channel can carry messages today. A voice or forum channel
    /// opens a placeholder rather than a transcript.
    pub const fn is_textual(self) -> bool {
        matches!(self, ChannelKind::Text | ChannelKind::Announcement)
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "text" => Some(ChannelKind::Text),
            "announcement" | "announce" => Some(ChannelKind::Announcement),
            "voice" => Some(ChannelKind::Voice),
            "forum" => Some(ChannelKind::Forum),
            _ => None,
        }
    }
}

/// The resolved form of one channel, with its kind and category joined from the
/// metadata event stream. [`ChannelMeta`] stays as it is so old stores and old
/// peers keep decoding; this is a view assembled by the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelView {
    pub id: ChannelId,
    pub guild_id: GuildId,
    pub name: String,
    pub created_by: AuthorId,
    pub created_at: u64,
    pub kind: ChannelKind,
    pub parent: Option<ChannelId>,
}

/// A named channel category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryMeta {
    pub id: ChannelId,
    pub guild_id: GuildId,
    pub name: String,
    pub created_by: AuthorId,
    pub created_at: u64,
}

/// Upper bound on mentions carried by one message event.
///
/// Duplicates are removed before posting, so a single message can notify at
/// most this many distinct members. The cap keeps the event comfortably below
/// the record-size limits in the store.
pub const MAX_MENTIONS: usize = 50;

/// Upper bound on a reaction emoji string.
///
/// Most emoji are a handful of bytes, but a ZWJ sequence with modifiers can be
/// longer. The cap keeps a reaction from becoming a message-sized payload.
pub const MAX_EMOJI_BYTES: usize = 64;

/// Upper bound on the "about" text of a member profile.
pub const MAX_PROFILE_ABOUT_BYTES: usize = 2_000;
/// Upper bound on the pronouns field of a member profile.
pub const MAX_PROFILE_PRONOUNS_BYTES: usize = 64;
/// How many links one profile may carry.
pub const MAX_PROFILE_LINKS: usize = 5;
/// Upper bound on one profile link.
pub const MAX_PROFILE_LINK_BYTES: usize = 512;

impl EventKind {
    pub fn is_message(&self) -> bool {
        matches!(
            self,
            EventKind::Message { .. } | EventKind::MessageV2 { .. } | EventKind::MessageV3 { .. }
        )
    }

    pub fn message_body(&self) -> Option<&str> {
        match self {
            EventKind::Message { body }
            | EventKind::MessageV2 { body, .. }
            | EventKind::MessageV3 { body, .. } => Some(body),
            _ => None,
        }
    }

    pub fn reply_to(&self) -> Option<EventId> {
        match self {
            EventKind::MessageV2 { reply_to, .. } | EventKind::MessageV3 { reply_to, .. } => {
                *reply_to
            }
            _ => None,
        }
    }

    pub fn mentions(&self) -> &[AuthorId] {
        match self {
            EventKind::MessageV2 { mentions, .. } | EventKind::MessageV3 { mentions, .. } => {
                mentions
            }
            _ => &[],
        }
    }

    /// Attachment metadata announced by this message, if it is one that can
    /// carry files. The bytes are not here: they are blobs keyed by
    /// [`crate::AttachmentId`] and fetched on demand.
    pub fn attachments(&self) -> &[Attachment] {
        match self {
            EventKind::MessageV3 { attachments, .. } => attachments,
            _ => &[],
        }
    }

    pub fn mentions_author(&self, author: AuthorId) -> bool {
        self.mentions().contains(&author)
    }

    /// The message this reaction names, if the kind is a reaction.
    pub fn reaction_target(&self) -> Option<EventId> {
        match self {
            EventKind::ReactionAdded { target, .. } | EventKind::ReactionRemoved { target, .. } => {
                Some(*target)
            }
            _ => None,
        }
    }

    /// The emoji and whether it was added (`true`) or removed (`false`).
    pub fn reaction(&self) -> Option<(EventId, &str, bool)> {
        match self {
            EventKind::ReactionAdded { target, emoji } => Some((*target, emoji, true)),
            EventKind::ReactionRemoved { target, emoji } => Some((*target, emoji, false)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventBody {
    pub version: u8,
    pub guild_id: GuildId,
    pub channel_id: Option<ChannelId>,
    pub author: AuthorId,
    pub seq: u64,
    pub lamport: u64,
    pub timestamp: u64,
    pub prev: Option<EventId>,
    pub kind: EventKind,
}

/// Version-2 wire form: the kind is a numeric tag plus its canonical field
/// bytes instead of a positional enum variant.
const KIND_TAG_GUILD_CREATED: u32 = 0;
const KIND_TAG_CHANNEL_CREATED: u32 = 1;
const KIND_TAG_MEMBER_JOINED: u32 = 2;
const KIND_TAG_MESSAGE: u32 = 3;
const KIND_TAG_MEMBER_LEFT: u32 = 4;
const KIND_TAG_MEMBER_RENAMED: u32 = 5;
const KIND_TAG_MESSAGE_EDITED: u32 = 6;
const KIND_TAG_MESSAGE_DELETED: u32 = 7;
const KIND_TAG_MESSAGE_V2: u32 = 8;
const KIND_TAG_REACTION_ADDED: u32 = 9;
const KIND_TAG_REACTION_REMOVED: u32 = 10;
const KIND_TAG_READ_UP_TO: u32 = 11;
const KIND_TAG_MESSAGE_V3: u32 = 12;
const KIND_TAG_CATEGORY_CREATED: u32 = 13;
const KIND_TAG_CHANNEL_CREATED_V2: u32 = 14;
const KIND_TAG_CHANNEL_RENAMED: u32 = 15;
const KIND_TAG_CHANNEL_MOVED: u32 = 16;
const KIND_TAG_MEMBER_PROFILE_UPDATED: u32 = 17;
const KIND_TAG_DIRECT_MESSAGE_OPENED: u32 = 18;

fn encode_kind(kind: &EventKind) -> std::result::Result<(u32, Vec<u8>), postcard::Error> {
    Ok(match kind {
        EventKind::GuildCreated { name } => (KIND_TAG_GUILD_CREATED, postcard::to_allocvec(name)?),
        EventKind::ChannelCreated { name } => {
            (KIND_TAG_CHANNEL_CREATED, postcard::to_allocvec(name)?)
        }
        EventKind::MemberJoined { display_name } => {
            (KIND_TAG_MEMBER_JOINED, postcard::to_allocvec(display_name)?)
        }
        EventKind::Message { body } => (KIND_TAG_MESSAGE, postcard::to_allocvec(body)?),
        EventKind::MemberLeft => (KIND_TAG_MEMBER_LEFT, Vec::new()),
        EventKind::MemberRenamed { display_name } => (
            KIND_TAG_MEMBER_RENAMED,
            postcard::to_allocvec(display_name)?,
        ),
        EventKind::MessageEdited { target, body } => (
            KIND_TAG_MESSAGE_EDITED,
            postcard::to_allocvec(&(*target, body))?,
        ),
        EventKind::MessageDeleted { target } => {
            (KIND_TAG_MESSAGE_DELETED, postcard::to_allocvec(target)?)
        }
        EventKind::MessageV2 {
            body,
            reply_to,
            mentions,
        } => (
            KIND_TAG_MESSAGE_V2,
            postcard::to_allocvec(&(body, reply_to, mentions))?,
        ),
        EventKind::Unknown { tag, payload } => (*tag, payload.clone()),
        EventKind::ReactionAdded { target, emoji } => (
            KIND_TAG_REACTION_ADDED,
            postcard::to_allocvec(&(target, emoji))?,
        ),
        EventKind::ReactionRemoved { target, emoji } => (
            KIND_TAG_REACTION_REMOVED,
            postcard::to_allocvec(&(target, emoji))?,
        ),
        EventKind::ReadUpTo { event_id } => (KIND_TAG_READ_UP_TO, postcard::to_allocvec(event_id)?),
        EventKind::MessageV3 {
            body,
            reply_to,
            mentions,
            attachments,
        } => (
            KIND_TAG_MESSAGE_V3,
            postcard::to_allocvec(&(body, reply_to, mentions, attachments))?,
        ),
        EventKind::CategoryCreated { name } => {
            (KIND_TAG_CATEGORY_CREATED, postcard::to_allocvec(name)?)
        }
        EventKind::ChannelCreatedV2 { name, kind, parent } => (
            KIND_TAG_CHANNEL_CREATED_V2,
            postcard::to_allocvec(&(name, kind, parent))?,
        ),
        EventKind::ChannelRenamed { target, name } => (
            KIND_TAG_CHANNEL_RENAMED,
            postcard::to_allocvec(&(target, name))?,
        ),
        EventKind::ChannelMoved { target, parent } => (
            KIND_TAG_CHANNEL_MOVED,
            postcard::to_allocvec(&(target, parent))?,
        ),
        EventKind::MemberProfileUpdated {
            about,
            pronouns,
            links,
        } => (
            KIND_TAG_MEMBER_PROFILE_UPDATED,
            postcard::to_allocvec(&(about, pronouns, links))?,
        ),
        EventKind::DirectMessageOpened { peer, dm } => (
            KIND_TAG_DIRECT_MESSAGE_OPENED,
            postcard::to_allocvec(&(peer, dm))?,
        ),
    })
}

fn decode_kind(tag: u32, payload: &[u8]) -> std::result::Result<EventKind, postcard::Error> {
    Ok(match tag {
        KIND_TAG_GUILD_CREATED => EventKind::GuildCreated {
            name: postcard::from_bytes(payload)?,
        },
        KIND_TAG_CHANNEL_CREATED => EventKind::ChannelCreated {
            name: postcard::from_bytes(payload)?,
        },
        KIND_TAG_MEMBER_JOINED => EventKind::MemberJoined {
            display_name: postcard::from_bytes(payload)?,
        },
        KIND_TAG_MESSAGE => EventKind::Message {
            body: postcard::from_bytes(payload)?,
        },
        KIND_TAG_MEMBER_LEFT => EventKind::MemberLeft,
        KIND_TAG_MEMBER_RENAMED => EventKind::MemberRenamed {
            display_name: postcard::from_bytes(payload)?,
        },
        KIND_TAG_MESSAGE_EDITED => {
            let (target, body) = postcard::from_bytes(payload)?;
            EventKind::MessageEdited { target, body }
        }
        KIND_TAG_MESSAGE_DELETED => EventKind::MessageDeleted {
            target: postcard::from_bytes(payload)?,
        },
        KIND_TAG_MESSAGE_V2 => {
            let (body, reply_to, mentions) = postcard::from_bytes(payload)?;
            EventKind::MessageV2 {
                body,
                reply_to,
                mentions,
            }
        }
        KIND_TAG_REACTION_ADDED => {
            let (target, emoji) = postcard::from_bytes(payload)?;
            EventKind::ReactionAdded { target, emoji }
        }
        KIND_TAG_REACTION_REMOVED => {
            let (target, emoji) = postcard::from_bytes(payload)?;
            EventKind::ReactionRemoved { target, emoji }
        }
        KIND_TAG_READ_UP_TO => EventKind::ReadUpTo {
            event_id: postcard::from_bytes(payload)?,
        },
        KIND_TAG_MESSAGE_V3 => {
            let (body, reply_to, mentions, attachments) = postcard::from_bytes(payload)?;
            EventKind::MessageV3 {
                body,
                reply_to,
                mentions,
                attachments,
            }
        }
        KIND_TAG_CATEGORY_CREATED => EventKind::CategoryCreated {
            name: postcard::from_bytes(payload)?,
        },
        KIND_TAG_CHANNEL_CREATED_V2 => {
            let (name, kind, parent) = postcard::from_bytes(payload)?;
            EventKind::ChannelCreatedV2 { name, kind, parent }
        }
        KIND_TAG_CHANNEL_RENAMED => {
            let (target, name) = postcard::from_bytes(payload)?;
            EventKind::ChannelRenamed { target, name }
        }
        KIND_TAG_CHANNEL_MOVED => {
            let (target, parent) = postcard::from_bytes(payload)?;
            EventKind::ChannelMoved { target, parent }
        }
        KIND_TAG_MEMBER_PROFILE_UPDATED => {
            let (about, pronouns, links) = postcard::from_bytes(payload)?;
            EventKind::MemberProfileUpdated {
                about,
                pronouns,
                links,
            }
        }
        KIND_TAG_DIRECT_MESSAGE_OPENED => {
            let (peer, dm) = postcard::from_bytes(payload)?;
            EventKind::DirectMessageOpened { peer, dm }
        }
        _ => EventKind::Unknown {
            tag,
            payload: payload.to_vec(),
        },
    })
}

impl Serialize for EventBody {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        // Postcard ignores the declared length, but CBOR (used on the wire)
        // writes a bounded map and rejects a mismatch, so the count has to be
        // exact for the layout being produced.
        let field_count = if self.version == LEGACY_EVENT_VERSION {
            9
        } else {
            10
        };
        let mut state = serializer.serialize_struct("EventBody", field_count)?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("guild_id", &self.guild_id)?;
        state.serialize_field("channel_id", &self.channel_id)?;
        state.serialize_field("author", &self.author)?;
        state.serialize_field("seq", &self.seq)?;
        state.serialize_field("lamport", &self.lamport)?;
        state.serialize_field("timestamp", &self.timestamp)?;
        state.serialize_field("prev", &self.prev)?;
        if self.version == LEGACY_EVENT_VERSION {
            state.serialize_field("kind", &self.kind)?;
        } else {
            let (tag, payload) = encode_kind(&self.kind).map_err(serde::ser::Error::custom)?;
            state.serialize_field("kind_tag", &tag)?;
            state.serialize_field("kind_payload", &payload)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for EventBody {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as DeError, MapAccess, SeqAccess, Visitor};

        const FIELDS: &[&str] = &[
            "version",
            "guild_id",
            "channel_id",
            "author",
            "seq",
            "lamport",
            "timestamp",
            "prev",
            "kind",
            "kind_tag",
            "kind_payload",
        ];

        struct BodyVisitor;

        impl<'de> Visitor<'de> for BodyVisitor {
            type Value = EventBody;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an event body")
            }

            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let missing = |index: usize| DeError::invalid_length(index, &self);
                let version = seq.next_element()?.ok_or_else(|| missing(0))?;
                let guild_id = seq.next_element()?.ok_or_else(|| missing(1))?;
                let channel_id = seq.next_element()?.ok_or_else(|| missing(2))?;
                let author = seq.next_element()?.ok_or_else(|| missing(3))?;
                let seq_number = seq.next_element()?.ok_or_else(|| missing(4))?;
                let lamport = seq.next_element()?.ok_or_else(|| missing(5))?;
                let timestamp = seq.next_element()?.ok_or_else(|| missing(6))?;
                let prev = seq.next_element()?.ok_or_else(|| missing(7))?;
                let kind = if version == LEGACY_EVENT_VERSION {
                    seq.next_element()?.ok_or_else(|| missing(8))?
                } else {
                    let tag: u32 = seq.next_element()?.ok_or_else(|| missing(8))?;
                    let payload: Vec<u8> = seq.next_element()?.ok_or_else(|| missing(9))?;
                    decode_kind(tag, &payload).map_err(DeError::custom)?
                };
                Ok(EventBody {
                    version,
                    guild_id,
                    channel_id,
                    author,
                    seq: seq_number,
                    lamport,
                    timestamp,
                    prev,
                    kind,
                })
            }

            /// CBOR (the peer protocol) is self-describing: fields arrive as a
            /// map keyed by name, in any order.
            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let missing = |field: &'static str| DeError::missing_field(field);
                let mut version: Option<u8> = None;
                let mut guild_id: Option<GuildId> = None;
                let mut channel_id: Option<Option<ChannelId>> = None;
                let mut author: Option<AuthorId> = None;
                let mut seq_number: Option<u64> = None;
                let mut lamport: Option<u64> = None;
                let mut timestamp: Option<u64> = None;
                let mut prev: Option<Option<EventId>> = None;
                let mut legacy_kind: Option<EventKind> = None;
                let mut tag: Option<u32> = None;
                let mut payload: Option<Vec<u8>> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "version" => version = Some(map.next_value()?),
                        "guild_id" => guild_id = Some(map.next_value()?),
                        "channel_id" => channel_id = Some(map.next_value()?),
                        "author" => author = Some(map.next_value()?),
                        "seq" => seq_number = Some(map.next_value()?),
                        "lamport" => lamport = Some(map.next_value()?),
                        "timestamp" => timestamp = Some(map.next_value()?),
                        "prev" => prev = Some(map.next_value()?),
                        "kind" => legacy_kind = Some(map.next_value()?),
                        "kind_tag" => tag = Some(map.next_value()?),
                        "kind_payload" => payload = Some(map.next_value()?),
                        other => return Err(DeError::unknown_field(other, FIELDS)),
                    }
                }

                let version = version.ok_or_else(|| missing("version"))?;
                let guild_id = guild_id.ok_or_else(|| missing("guild_id"))?;
                let channel_id = channel_id.ok_or_else(|| missing("channel_id"))?;
                let author = author.ok_or_else(|| missing("author"))?;
                let seq_number = seq_number.ok_or_else(|| missing("seq"))?;
                let lamport = lamport.ok_or_else(|| missing("lamport"))?;
                let timestamp = timestamp.ok_or_else(|| missing("timestamp"))?;
                let prev = prev.ok_or_else(|| missing("prev"))?;
                let kind = if version == LEGACY_EVENT_VERSION {
                    legacy_kind.ok_or_else(|| missing("kind"))?
                } else {
                    let tag = tag.ok_or_else(|| missing("kind_tag"))?;
                    let payload = payload.ok_or_else(|| missing("kind_payload"))?;
                    decode_kind(tag, &payload).map_err(DeError::custom)?
                };
                Ok(EventBody {
                    version,
                    guild_id,
                    channel_id,
                    author,
                    seq: seq_number,
                    lamport,
                    timestamp,
                    prev,
                    kind,
                })
            }
        }

        deserializer.deserialize_struct("EventBody", FIELDS, BodyVisitor)
    }
}

impl EventBody {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        Ok(postcard::to_allocvec(self)?)
    }

    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        let body = self.canonical_bytes()?;
        let mut bytes = Vec::with_capacity(EVENT_SIGNING_DOMAIN.len() + body.len());
        bytes.extend_from_slice(EVENT_SIGNING_DOMAIN);
        bytes.extend_from_slice(&body);
        Ok(bytes)
    }

    pub fn id(&self) -> Result<EventId> {
        let bytes = self.canonical_bytes()?;
        Ok(EventId(*blake3::hash(&bytes).as_bytes()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedEvent {
    pub body: EventBody,
    pub signature: Vec<u8>,
}

impl SignedEvent {
    pub fn sign(body: EventBody, key: &SigningKey) -> Result<Self> {
        let signing_bytes = body.signing_bytes()?;
        let signature = key.sign(&signing_bytes);
        Ok(Self {
            body,
            signature: signature.to_bytes().to_vec(),
        })
    }

    pub fn id(&self) -> Result<EventId> {
        self.body.id()
    }

    pub fn verify(&self) -> Result<EventId> {
        if !supported_event_version(self.body.version) {
            return Err(CoreError::InvalidInput(format!(
                "unsupported event version: {}",
                self.body.version
            )));
        }
        if self.signature.len() != 64 {
            return Err(CoreError::InvalidSignature);
        }
        let signature = Signature::from_slice(&self.signature)?;
        let verifying_key = VerifyingKey::from_bytes(&self.body.author.0)?;
        let signing_bytes = self.body.signing_bytes()?;
        verifying_key
            .verify_strict(&signing_bytes, &signature)
            .map_err(|_| CoreError::InvalidSignature)?;
        self.id()
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(postcard::to_allocvec(self)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        decode_canonical(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildMeta {
    pub id: GuildId,
    pub name: String,
    pub founder: AuthorId,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMeta {
    pub id: ChannelId,
    pub guild_id: GuildId,
    pub name: String,
    pub created_by: AuthorId,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub guild_id: GuildId,
    pub author: AuthorId,
    pub display_name: String,
    pub joined_at: u64,
}

/// A member's profile resolved against their guild roster and event log.
///
/// The `about`, `pronouns` and `links` fields come from the latest
/// [`EventKind::MemberProfileUpdated`] by `(lamport, event_id)`, so every peer
/// resolves the same value regardless of arrival order. `display_name` and
/// `joined_at` come from the roster row (`MemberJoined`/`MemberRenamed`), which
/// keeps the name on its original event kind.
///
/// The derived fields are computed by folding the guild's log on demand rather
/// than maintained in a mutable table, so they cannot drift from the events:
/// `message_count` counts visible (not deleted) messages authored by this
/// member, and `last_activity` is the timestamp of their newest event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberProfile {
    pub guild_id: GuildId,
    pub author: AuthorId,
    pub display_name: String,
    pub about: String,
    pub pronouns: String,
    pub links: Vec<String>,
    pub joined_at: u64,
    /// When the winning profile event was written, or `0` if the member has
    /// never published one.
    pub updated_at: u64,
    /// Visible messages authored by this member.
    pub message_count: u64,
    /// Timestamp of the newest event authored by this member, or `0` when the
    /// only trace is the implicit roster row from their first event.
    pub last_activity: u64,
}

/// One direct-message conversation as a frontend lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectConversation {
    pub guild: GuildMeta,
    pub peer: MemberInfo,
}

/// One local search result: a message with its folded body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub event: SignedEvent,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AuthorState {
    pub next_seq: u64,
    pub last_lamport: u64,
    pub prev: Option<EventId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GuildClock {
    pub last_lamport: u64,
}

pub fn now_unix() -> u64 {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    u64::try_from(now).unwrap_or(0)
}
