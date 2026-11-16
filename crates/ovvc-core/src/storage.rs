//! The storage boundary between the domain and any concrete backend.
//!
//! Frontends and transports only ever depend on this trait, so a frontend can
//! plug in its own persistence (in-memory, browser storage, SQLite, …) without
//! touching the event model, the network layer, or the client facade.
//!
//! Implementations must uphold these invariants:
//!
//! * Every stored event is verified before it is indexed; `import_event`
//!   returns `Ok(false)` for an event that is already present.
//! * Events returned by `history`, `events_after`, and `get_event` are signed
//!   and verified; their bodies are unchanged from what the author signed.
//! * `create_guild` and friends derive sequence numbers and Lamport clocks from
//!   the stored state, never from a local wall clock alone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::attachment::{Attachment, AttachmentId};
use crate::error::{CoreError, Result};
use crate::identity::Identity;
use crate::model::{
    AuthorId, ChannelId, ChannelMeta, EventId, GuildId, GuildMeta, MemberInfo, SearchHit,
    SignedEvent,
};
use crate::timeline::{TimelineEntry, timeline};

/// A guild created together with its default `#general` channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedGuild {
    pub guild: GuildMeta,
    pub general: ChannelMeta,
}

/// A position in a guild's `(lamport, event_id)` event order.
///
/// Lamport clocks are per guild and shared by every author, so ties are not an
/// edge case — they are the normal way two authors' events interleave. A bare
/// Lamport cursor therefore cannot name a page boundary; the event id breaks
/// the tie deterministically.
///
/// The default value is the position before the first event of a guild.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct SyncCursor {
    pub lamport: u64,
    /// `None` means "before every event"; otherwise only events strictly after
    /// this `(lamport, event_id)` pair are returned.
    pub event_id: Option<EventId>,
}

impl SyncCursor {
    /// The cursor that returns a guild's history from the beginning.
    pub const fn start() -> Self {
        Self {
            lamport: 0,
            event_id: None,
        }
    }

    /// The cursor immediately after `event`.
    pub fn after(event: &SignedEvent) -> Result<Self> {
        Ok(Self {
            lamport: event.body.lamport,
            event_id: Some(event.id()?),
        })
    }

    /// The cursor at `event`. Paging backwards returns events strictly older
    /// than this position.
    pub fn at(event: &SignedEvent) -> Result<Self> {
        Self::after(event)
    }

    /// True when `(lamport, event_id)` sorts strictly after this cursor.
    pub fn is_before(&self, lamport: u64, event_id: EventId) -> bool {
        match self.event_id {
            None => true,
            Some(cursor_id) => {
                lamport > self.lamport || (lamport == self.lamport && event_id > cursor_id)
            }
        }
    }

    /// True when `(lamport, event_id)` sorts strictly before this cursor. The
    /// start cursor has no preceding position, so it returns `false`.
    pub fn is_strictly_before(&self, lamport: u64, event_id: EventId) -> bool {
        match self.event_id {
            None => false,
            Some(cursor_id) => {
                lamport < self.lamport || (lamport == self.lamport && event_id < cursor_id)
            }
        }
    }
}

/// One page of a guild's event stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPage {
    pub events: Vec<SignedEvent>,
    /// Whether at least one event exists after the last returned event.
    pub more: bool,
}

/// Storage shared between the frontend, the transport, and the client facade.
///
/// A `dyn Storage` behind a mutex is the minimum shared state every layer can
/// agree on. Frontends that want to share storage across several clients (for
/// example one client per guild) construct one and pass it to each.
pub type SharedStorage = Arc<Mutex<dyn Storage>>;

/// Wrap a concrete storage backend for use across layers.
pub fn shared_storage<S: Storage + 'static>(storage: S) -> SharedStorage {
    let shared: SharedStorage = Arc::new(Mutex::new(storage));
    shared
}

/// Domain operations required from any persistence backend.
///
/// Methods taking `&mut self` may append or modify state; the rest are reads.
/// The default implementations of `find_guild` and `find_channel` are built on
/// the primitive lookups, so a backend only needs to provide those.
pub trait Storage: Send {
    // --- guild lifecycle -------------------------------------------------
    fn create_guild(&mut self, name: &str, identity: &Identity) -> Result<CreatedGuild>;

    /// Create a guild under a caller-chosen id.
    ///
    /// Direct messages need this because both sides derive the same id from
    /// their two keys with [`crate::direct_guild_id`]; a random id would give
    /// them two unrelated logs. The backend records the same creation events
    /// as [`Self::create_guild`], plus `MemberJoined` for `display_name`, and
    /// must refuse to overwrite a guild that already exists.
    fn create_guild_with_id(
        &mut self,
        guild_id: GuildId,
        name: &str,
        display_name: &str,
        identity: &Identity,
    ) -> Result<CreatedGuild> {
        let _ = (guild_id, name, display_name, identity);
        Err(CoreError::InvalidInput(
            "this storage backend cannot create a commune with a chosen id".to_string(),
        ))
    }

    fn list_guilds(&self) -> Result<Vec<GuildMeta>>;
    fn get_guild(&self, guild_id: GuildId) -> Result<Option<GuildMeta>>;

    // --- channels --------------------------------------------------------
    fn create_channel(
        &mut self,
        guild_id: GuildId,
        name: &str,
        identity: &Identity,
    ) -> Result<ChannelMeta>;

    /// `create_channel`, plus the signed event that created it when the
    /// backend records one.
    ///
    /// The client broadcasts this event so peers that are online right now
    /// learn about the new channel immediately instead of waiting for the next
    /// history sync. Backends that do not model channels as events return
    /// `None` and everything else keeps working.
    fn create_channel_with_event(
        &mut self,
        guild_id: GuildId,
        name: &str,
        identity: &Identity,
    ) -> Result<(ChannelMeta, Option<SignedEvent>)> {
        Ok((self.create_channel(guild_id, name, identity)?, None))
    }
    fn list_channels(&self, guild_id: GuildId) -> Result<Vec<ChannelMeta>>;
    fn get_channel(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<Option<ChannelMeta>>;

    /// Create a channel with a kind and an optional parent category.
    ///
    /// Returns the channel and the signed event when the backend records one,
    /// so the facade can broadcast it to peers that are already online.
    fn create_channel_in(
        &mut self,
        guild_id: GuildId,
        name: &str,
        kind: crate::model::ChannelKind,
        parent: Option<ChannelId>,
        identity: &Identity,
    ) -> Result<(ChannelMeta, Option<SignedEvent>)> {
        if kind == crate::model::ChannelKind::Text && parent.is_none() {
            let (channel, event) = self.create_channel_with_event(guild_id, name, identity)?;
            return Ok((channel, event));
        }
        let _ = (guild_id, name, kind, parent, identity);
        Err(CoreError::InvalidInput(
            "this storage backend does not support channel kinds or categories".to_string(),
        ))
    }

    /// Rename a channel. Returns the signed event when the backend records one.
    fn rename_channel(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        name: &str,
        identity: &Identity,
    ) -> Result<Option<SignedEvent>> {
        let _ = (guild_id, channel_id, name, identity);
        Err(CoreError::InvalidInput(
            "this storage backend does not support channel renaming".to_string(),
        ))
    }

    /// Move a channel into `parent`, or back to the root with `None`.
    fn move_channel(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        parent: Option<ChannelId>,
        identity: &Identity,
    ) -> Result<Option<SignedEvent>> {
        let _ = (guild_id, channel_id, parent, identity);
        Err(CoreError::InvalidInput(
            "this storage backend does not support moving channels".to_string(),
        ))
    }

    /// Create a named category for grouping channels.
    fn create_category(
        &mut self,
        guild_id: GuildId,
        name: &str,
        identity: &Identity,
    ) -> Result<(crate::model::CategoryMeta, Option<SignedEvent>)> {
        let _ = (guild_id, name, identity);
        Err(CoreError::InvalidInput(
            "this storage backend does not support categories".to_string(),
        ))
    }

    /// Every category in a guild, in name order.
    fn list_categories(&self, guild_id: GuildId) -> Result<Vec<crate::model::CategoryMeta>> {
        let _ = guild_id;
        Ok(Vec::new())
    }

    /// Channels with their kind and category resolved, in sidebar order.
    ///
    /// The default builds plain text channels from [`Self::list_channels`], so
    /// a backend that has not modelled channel metadata still works.
    fn list_channel_views(&self, guild_id: GuildId) -> Result<Vec<crate::model::ChannelView>> {
        Ok(self
            .list_channels(guild_id)?
            .into_iter()
            .map(|channel| crate::model::ChannelView {
                id: channel.id,
                guild_id: channel.guild_id,
                name: channel.name,
                created_by: channel.created_by,
                created_at: channel.created_at,
                kind: crate::model::ChannelKind::Text,
                parent: None,
            })
            .collect())
    }

    // --- messages and events --------------------------------------------
    fn post_message(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        identity: &Identity,
    ) -> Result<SignedEvent>;

    /// Append a message that replies to `reply_to` and/or mentions `mentions`.
    ///
    /// `reply_to` must name a message in `channel_id`; `mentions` holds roster
    /// ids (use [`crate::mentions::parse_mentions`] to derive them from text)
    /// and is de-duplicated. A call with neither a reply nor a mention is
    /// exactly [`Self::post_message`], and should be done through that method
    /// so the common path keeps its compact event kind.
    fn post_message_full(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
        identity: &Identity,
    ) -> Result<SignedEvent> {
        self.post_message_rich(
            guild_id,
            channel_id,
            body,
            reply_to,
            mentions,
            &[],
            identity,
        )
    }

    /// Append a message with any combination of reply, mentions and files.
    ///
    /// This is the one method a backend implements; the two narrower posting
    /// methods route here with the fields they do not carry left empty. A call
    /// with nothing set is exactly [`Self::post_message`] and is routed there
    /// so the common case keeps its compact event kind. Attachment entries are
    /// metadata only: the bytes go to the blob store through
    /// [`Self::put_blob`] and are fetched by hash.
    ///
    /// The argument list is long on purpose: each one is a field of the signed
    /// event, and bundling them into a struct would make every backend
    /// construct and destructure it for no gain.
    #[allow(clippy::too_many_arguments)]
    fn post_message_rich(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
        attachments: &[Attachment],
        identity: &Identity,
    ) -> Result<SignedEvent> {
        if reply_to.is_none() && mentions.is_empty() && attachments.is_empty() {
            return self.post_message(guild_id, channel_id, body, identity);
        }
        Err(crate::error::CoreError::InvalidInput(
            "this storage backend does not support replies or attachments".to_string(),
        ))
    }

    /// Append a signed `MessageEdited` for a message `identity` authored.
    ///
    /// The event is stored in the channel of the target message. Backends
    /// reject the edit when the target is unknown locally or was written by
    /// another author; peers that lack the target ignore the edit at render
    /// time, so ordering never has to be perfect.
    fn edit_message(
        &mut self,
        guild_id: GuildId,
        target: EventId,
        body: &str,
        identity: &Identity,
    ) -> Result<SignedEvent>;

    /// Append a signed `MessageDeleted` for a message `identity` authored.
    ///
    /// Deletion is a tombstone, not an erasure: peers that already hold the
    /// body keep it on disk and only stop displaying it.
    fn delete_message(
        &mut self,
        guild_id: GuildId,
        target: EventId,
        identity: &Identity,
    ) -> Result<SignedEvent>;

    /// Add one reaction from `identity` to a message.
    fn add_reaction(
        &mut self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        let _ = (guild_id, target, emoji, identity);
        Err(crate::error::CoreError::InvalidInput(
            "this storage backend does not support reactions".to_string(),
        ))
    }

    /// Remove one reaction that `identity` previously added.
    fn remove_reaction(
        &mut self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        let _ = (guild_id, target, emoji, identity);
        Err(crate::error::CoreError::InvalidInput(
            "this storage backend does not support reactions".to_string(),
        ))
    }

    /// Record that `identity` has read `channel_id` up to `event_id`.
    fn mark_read(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        event_id: EventId,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        let _ = (guild_id, channel_id, event_id, identity);
        Err(crate::error::CoreError::InvalidInput(
            "this storage backend does not support read markers".to_string(),
        ))
    }

    /// Unread message counts per channel for `author`.
    ///
    /// Backends that do not track read markers return an empty map rather than
    /// failing, because unread badges are a convenience and not a sync rule.
    fn unread_counts(
        &self,
        guild_id: GuildId,
        author: AuthorId,
    ) -> Result<HashMap<ChannelId, usize>> {
        let _ = (guild_id, author);
        Ok(HashMap::new())
    }

    /// Search folded message bodies in one guild, newest first.
    ///
    /// The default implementation walks the guild in `(lamport, event_id)`
    /// order and folds edits, deletes, and reactions with [`timeline`], so a
    /// backend that only implements `events_after` still gets correct search.
    /// A backend with a text index can override it.
    fn search(&self, guild_id: GuildId, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let needle = query.to_lowercase();
        let mut events = Vec::new();
        let mut cursor = SyncCursor::start();
        loop {
            let page = self.events_after(guild_id, cursor, 500, 8 * 1024 * 1024)?;
            if page.events.is_empty() {
                break;
            }
            cursor = SyncCursor::after(page.events.last().expect("non-empty page"))?;
            events.extend(page.events);
            if !page.more {
                break;
            }
        }

        let folded = timeline(&events)?;
        let mut hits = Vec::new();
        for entry in folded.into_iter().rev() {
            if hits.len() == limit {
                break;
            }
            if let TimelineEntry::Message { event, body, .. } = entry
                && body.to_lowercase().contains(&needle)
            {
                hits.push(SearchHit {
                    event: event.clone(),
                    body,
                });
            }
        }
        Ok(hits)
    }

    /// Import a signed event received from a peer.
    ///
    /// Returns `true` when the event was new and appended, `false` when it was
    /// already present.
    fn import_event(&mut self, event: SignedEvent) -> Result<bool>;

    fn has_event(&self, event_id: EventId) -> Result<bool>;

    /// Return events strictly after `cursor`, in `(lamport, event_id)` order.
    ///
    /// At most `limit` events and roughly `max_bytes` of encoded payloads are
    /// returned; a single event larger than `max_bytes` is still returned alone
    /// so that a page always makes progress. `EventPage::more` is true only
    /// when another event exists after the last returned one — it is not a
    /// guess based on `events.len() == limit`.
    fn events_after(
        &self,
        guild_id: GuildId,
        cursor: SyncCursor,
        limit: usize,
        max_bytes: usize,
    ) -> Result<EventPage>;

    fn history(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        limit: usize,
    ) -> Result<Vec<SignedEvent>>;

    /// Return up to `limit` events of one channel strictly before `cursor`,
    /// oldest first.
    ///
    /// `cursor = None` means the newest events. A `Some` cursor is usually
    /// [`SyncCursor::at`] of the oldest event already loaded, so repeated
    /// calls walk backwards without overlapping or skipping a Lamport tie.
    /// `EventPage::more` is true only when at least one older event exists.
    fn history_before(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        cursor: Option<SyncCursor>,
        limit: usize,
    ) -> Result<EventPage>;

    fn get_event(&self, guild_id: GuildId, event_id: EventId) -> Result<Option<SignedEvent>>;

    // --- content-addressed blobs -----------------------------------------
    //
    // Attachments are announced in messages but stored outside the event log.
    // A backend that does not persist blobs may keep the defaults, which
    // report that the feature is unavailable rather than pretending the blob
    // is absent, so a caller can tell "not downloaded yet" from "this backend
    // cannot hold it".

    /// Store bytes under the id their hash names.
    ///
    /// Implementations must reject a blob when `hash_bytes(bytes)` is not
    /// `id`, when it exceeds [`crate::MAX_ATTACHMENT_BYTES`], and must not
    /// leave a partially written blob visible.
    fn put_blob(&mut self, id: AttachmentId, bytes: &[u8]) -> Result<()> {
        let _ = (id, bytes);
        Err(CoreError::Backend(
            "this storage backend does not support blobs".to_string(),
        ))
    }

    /// Read a blob, or `None` when it has not been downloaded.
    fn get_blob(&self, id: AttachmentId) -> Result<Option<Vec<u8>>> {
        let _ = id;
        Err(CoreError::Backend(
            "this storage backend does not support blobs".to_string(),
        ))
    }

    /// Whether a blob is already local.
    fn has_blob(&self, id: AttachmentId) -> Result<bool> {
        Ok(self.get_blob(id)?.is_some())
    }

    fn guild_lamport(&self, guild_id: GuildId) -> Result<u64>;

    // --- members ---------------------------------------------------------
    fn add_member(&mut self, guild_id: GuildId, author: AuthorId, display_name: &str)
    -> Result<()>;
    fn list_members(&self, guild_id: GuildId) -> Result<Vec<MemberInfo>>;

    /// Record `identity`'s display name in a guild.
    ///
    /// When the stored name differs this appends a signed `MemberRenamed`
    /// event, so peers learn the name through the normal event path. Returns
    /// the new event, or `None` when the stored name already matches.
    fn set_display_name(
        &mut self,
        guild_id: GuildId,
        display_name: &str,
        identity: &Identity,
    ) -> Result<Option<SignedEvent>>;

    /// Append a signed `MemberJoined` for `identity` when it is not on the
    /// roster yet.
    ///
    /// Returns the new event, or `None` when the identity is already a member.
    /// A recipient that imports a `DirectMessageOpened` uses this to put its
    /// own member row into the freshly created DM guild.
    fn append_member_joined(
        &mut self,
        guild_id: GuildId,
        display_name: &str,
        identity: &Identity,
    ) -> Result<Option<SignedEvent>> {
        let _ = (guild_id, display_name, identity);
        Err(CoreError::InvalidInput(
            "this storage backend does not support event-backed membership".to_string(),
        ))
    }

    /// Publish `identity`'s profile in a guild.
    ///
    /// Appends a signed `MemberProfileUpdated`. The display name is not part of
    /// this event: it stays on `MemberRenamed`, so a peer that predates
    /// profiles still sees renames.
    fn set_profile(
        &mut self,
        guild_id: GuildId,
        identity: &Identity,
        about: &str,
        pronouns: &str,
        links: &[String],
    ) -> Result<SignedEvent> {
        let _ = (guild_id, identity, about, pronouns, links);
        Err(CoreError::InvalidInput(
            "this storage backend does not support profiles".to_string(),
        ))
    }

    /// Resolve a member's profile and derived activity.
    ///
    /// Fails with [`CoreError::NotFound`] when `author` is not on the roster.
    fn member_profile(&self, guild_id: GuildId, author: AuthorId) -> Result<crate::MemberProfile> {
        let _ = (guild_id, author);
        Err(CoreError::InvalidInput(
            "this storage backend does not support profiles".to_string(),
        ))
    }

    /// Append a signed `DirectMessageOpened` to `channel_id`.
    ///
    /// The event lives in the shared `guild_id`; `dm` is the deterministic DM
    /// guild id and `peer` the other member. The backend must reject a `dm`
    /// that is not [`crate::direct_guild_id`] of the author and `peer`.
    fn announce_direct_message(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        peer: AuthorId,
        dm: GuildId,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        let _ = (guild_id, channel_id, peer, dm, identity);
        Err(CoreError::InvalidInput(
            "this storage backend does not support direct messages".to_string(),
        ))
    }

    /// Append a signed `MemberLeft` for `identity` in a guild.
    ///
    /// Returns the new event, or `None` when the identity is not currently a
    /// member. The roster row is removed locally and on every peer that
    /// imports the event.
    fn leave_guild(
        &mut self,
        guild_id: GuildId,
        identity: &Identity,
    ) -> Result<Option<SignedEvent>>;

    // --- convenience lookups --------------------------------------------
    /// Resolve a guild by exact name, hex id, or hex id prefix.
    fn find_guild(&self, needle: &str) -> Result<GuildMeta> {
        let needle = needle.trim();
        if let Ok(id) = needle.parse::<GuildId>()
            && let Some(guild) = self.get_guild(id)?
        {
            return Ok(guild);
        }

        let matches: Vec<_> = self
            .list_guilds()?
            .into_iter()
            .filter(|guild| guild.name == needle || guild.id.to_hex().starts_with(needle))
            .collect();
        choose_one(matches, "commune", needle)
    }

    /// Resolve a channel inside a guild by exact name or hex id prefix.
    fn find_channel(&self, guild_id: GuildId, needle: &str) -> Result<ChannelMeta> {
        let needle = needle.trim().trim_start_matches('#');
        let channels = self.list_channels(guild_id)?;
        if let Some(channel) = channels.iter().find(|channel| channel.name == needle) {
            return Ok(channel.clone());
        }

        let matches: Vec<_> = channels
            .into_iter()
            .filter(|channel| channel.id.to_hex().starts_with(needle))
            .collect();
        choose_one(matches, "channel", needle)
    }
}

fn choose_one<T>(matches: Vec<T>, kind: &str, needle: &str) -> Result<T> {
    match matches.len() {
        0 => Err(crate::error::CoreError::NotFound(format!(
            "{kind} '{needle}'"
        ))),
        1 => Ok(matches.into_iter().next().expect("one match")),
        _ => Err(crate::error::CoreError::InvalidInput(format!(
            "{kind} '{needle}' is ambiguous"
        ))),
    }
}
