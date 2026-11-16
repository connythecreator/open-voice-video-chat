pub mod attachment;
pub mod diagnostic;
pub mod error;
pub mod identity;
pub mod invite;
pub mod markdown;
pub mod mentions;
pub mod model;
pub mod policy;
pub mod settings;
pub mod storage;
pub mod timeline;

pub use attachment::{
    Attachment, AttachmentId, BLOB_CHUNK_BYTES, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_MIME_BYTES,
    MAX_ATTACHMENT_NAME_BYTES, MAX_ATTACHMENTS_PER_MESSAGE, attachment_for_file, hash_bytes,
    hash_reader, human_size, mime_for_path, validate_attachments,
};
pub use diagnostic::{Diagnostic, ErrorClass};
pub use error::{CoreError, Result};
pub use identity::Identity;
pub use invite::{Invite, InviteBody};
pub use mentions::{has_mention_syntax, parse_mentions};
pub use policy::{
    CallScope, ClientPolicyFloor, Discovery, GuildPolicy, GuildProfile, GuildTopology, HardwareKeys,
    HubMode, HubPolicy, HubTrust, IdentityPolicy, KeyCustody, MediaPolicy, MediaTopology,
    POLICY_VERSION, PaddingPolicy, PluginPolicy, RetentionPolicy, StoragePolicy, TimestampPolicy,
    TransportPolicy,
};
pub use model::{
    AuthorId, CategoryMeta, ChannelId, ChannelKind, ChannelMeta, ChannelView, DirectConversation,
    EventBody, EventId, EventKind, GuildId, GuildMeta, MAX_EMOJI_BYTES, MAX_MENTIONS,
    MAX_PROFILE_ABOUT_BYTES, MAX_PROFILE_LINKS, MAX_PROFILE_LINK_BYTES, MAX_PROFILE_PRONOUNS_BYTES,
    MemberInfo, MemberProfile, SearchHit, SignedEvent, direct_guild_id,
};
pub use settings::{NotificationSettings, QuietHours};
pub use storage::{CreatedGuild, EventPage, SharedStorage, Storage, SyncCursor, shared_storage};
pub use timeline::{Reaction, ReplyRef, TimelineEntry, timeline};
