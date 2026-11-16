use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use rand::RngCore;
use rand::rngs::OsRng;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize};

use ovvc_core::attachment::{
    Attachment, AttachmentId, MAX_ATTACHMENT_BYTES, hash_bytes, validate_attachments,
};
use ovvc_core::error::{CoreError, Result as CoreResult};
use ovvc_core::identity::Identity;
use ovvc_core::model::{
    AuthorId, AuthorState, CategoryMeta, ChannelId, ChannelKind, ChannelMeta, ChannelView,
    EVENT_VERSION, EventBody, EventId, EventKind, GuildClock, GuildId, GuildMeta, MAX_EMOJI_BYTES,
    MAX_MENTIONS, MAX_PROFILE_ABOUT_BYTES, MAX_PROFILE_LINKS, MAX_PROFILE_LINK_BYTES,
    MAX_PROFILE_PRONOUNS_BYTES, MemberInfo, MemberProfile, SignedEvent, direct_guild_id, now_unix,
    supported_event_version,
};
use ovvc_core::timeline::{TimelineEntry, timeline};
use ovvc_core::{CreatedGuild, EventPage, Storage, SyncCursor};

use crate::error::Result;
use crate::fsutil::{create_dir_private, restrict};
use crate::log::{AppendLog, EventLocation, read_event, scan_guild_streaming};

const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_NAME_BYTES: usize = 100;

/// Characters that must never reach a terminal or a log line.
///
/// C0/C1 controls and DEL let a peer move the cursor, rewrite the screen, set
/// the window title (OSC 0), write the clipboard (OSC 52), or forge a new log
/// line with an embedded newline. The bidi embedding/override/isolate marks
/// let a display name reorder the text around it (the "Trojan Source" trick).
/// Plain right-to-left text does not need them: the bidi algorithm derives
/// direction from the characters themselves.
fn contains_unsafe_display_chars(text: &str) -> bool {
    text.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
            )
    })
}

/// The rules a message body or an edit must satisfy, shared by local writes
/// and imports so a peer cannot smuggle in something a local author could not
/// write. Edits are held to exactly the same bar as the messages they replace.
fn validate_message_text(text: &str) -> Result<()> {
    validate_message_text_inner(text, false)
}

/// The same rules, but a message that carries files may have no text at all.
///
/// Sending only an attachment is ordinary chat behaviour, and an empty caption
/// is clearer than making the sender invent one just to satisfy validation.
fn validate_message_text_inner(text: &str, allow_empty: bool) -> Result<()> {
    if text.trim().is_empty() && !allow_empty {
        return Err(CoreError::InvalidInput("message body cannot be empty".to_string()).into());
    }
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(CoreError::InvalidInput(format!(
            "message body exceeds {MAX_MESSAGE_BYTES} bytes"
        ))
        .into());
    }
    if contains_unsafe_display_chars(text) {
        return Err(CoreError::InvalidInput(
            "message body contains control or bidi-format characters".to_string(),
        )
        .into());
    }
    Ok(())
}

/// A reaction emoji is a tiny display string. It is held to the same display
/// safety bar as a message so it cannot move the cursor or rewrite a terminal.
fn validate_emoji(emoji: &str) -> Result<()> {
    if emoji.trim().is_empty() {
        return Err(CoreError::InvalidInput("reaction emoji cannot be empty".to_string()).into());
    }
    if emoji.len() > MAX_EMOJI_BYTES {
        return Err(CoreError::InvalidInput(format!(
            "reaction emoji exceeds {MAX_EMOJI_BYTES} bytes"
        ))
        .into());
    }
    if contains_unsafe_display_chars(emoji) {
        return Err(CoreError::InvalidInput(
            "reaction emoji contains control or bidi-format characters".to_string(),
        )
        .into());
    }
    Ok(())
}

/// The rules a profile event must satisfy, shared by local writes and imports.
///
/// An empty `about` or `pronouns` clears that field, which is how a person
/// removes something they no longer want to show. Links are the opposite: an
/// empty entry is refused rather than silently dropped, so the stored event
/// stays exactly what the author signed.
fn validate_profile(about: &str, pronouns: &str, links: &[String]) -> Result<()> {
    if about.len() > MAX_PROFILE_ABOUT_BYTES {
        return Err(CoreError::InvalidInput(format!(
            "profile about exceeds {MAX_PROFILE_ABOUT_BYTES} bytes"
        ))
        .into());
    }
    if pronouns.len() > MAX_PROFILE_PRONOUNS_BYTES {
        return Err(CoreError::InvalidInput(format!(
            "profile pronouns exceed {MAX_PROFILE_PRONOUNS_BYTES} bytes"
        ))
        .into());
    }
    if contains_unsafe_display_chars(about) || contains_unsafe_display_chars(pronouns) {
        return Err(CoreError::InvalidInput(
            "profile text contains control or bidi-format characters".to_string(),
        )
        .into());
    }
    if links.len() > MAX_PROFILE_LINKS {
        return Err(CoreError::InvalidInput(format!(
            "a profile may carry at most {MAX_PROFILE_LINKS} links"
        ))
        .into());
    }
    for link in links {
        if link.is_empty() {
            return Err(CoreError::InvalidInput("profile link cannot be empty".to_string()).into());
        }
        if link.len() > MAX_PROFILE_LINK_BYTES {
            return Err(CoreError::InvalidInput(format!(
                "profile link exceeds {MAX_PROFILE_LINK_BYTES} bytes"
            ))
            .into());
        }
        if contains_unsafe_display_chars(link) || link.chars().any(char::is_whitespace) {
            return Err(CoreError::InvalidInput(
                "profile link contains whitespace or control characters".to_string(),
            )
            .into());
        }
        if !(link.starts_with("http://") || link.starts_with("https://")) {
            return Err(CoreError::InvalidInput(
                "profile links must use http or https".to_string(),
            )
            .into());
        }
        // A scheme prefix alone is not a link.
        if link.len() <= "https://".len() {
            return Err(CoreError::InvalidInput("profile link has no host".to_string()).into());
        }
    }
    Ok(())
}

/// How far ahead of the local guild clock an imported event may be. A peer
/// that has a longer history than us syncs that history first, so a genuine
/// event is never far ahead; an absurd value would poison our own clock (every
/// local event would then be stamped near `u64::MAX`).
const MAX_LAMPORT_JUMP: u64 = 1_000_000;

/// Events may not claim to be from more than a day in the future. Clocks
/// disagree between machines; a day is well beyond any plausible skew.
const MAX_FUTURE_SKEW_SECS: u64 = 86_400;

const GUILDS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("guilds");
const CHANNELS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("channels");
const MEMBERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("members");
const EVENTS_BY_ID: TableDefinition<&[u8], &[u8]> = TableDefinition::new("events_by_id");
const EVENTS_BY_GUILD: TableDefinition<&[u8], &[u8]> = TableDefinition::new("events_by_guild");
const EVENTS_BY_CHANNEL: TableDefinition<&[u8], &[u8]> = TableDefinition::new("events_by_channel");
const AUTHOR_STATE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("author_state");
const GUILD_CLOCK: TableDefinition<&[u8], &[u8]> = TableDefinition::new("guild_clock");
/// The `(lamport, event_id)` that last won each guild/channel/member metadata
/// row. Without it, two peers that receive the same events in different orders
/// would disagree about names.
const META_SOURCE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("meta_source");
const READ_STATE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("read_state");
/// Attachment bytes, keyed by the 32-byte BLAKE3 hash that names them.
const BLOBS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("blobs");
/// Channel categories, keyed by guild and category id.
const CATEGORIES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("categories");
/// A channel's kind and parent category, kept beside `CHANNELS` so the frozen
/// `ChannelMeta` encoding never has to change under an existing store.
const CHANNEL_LAYOUT: TableDefinition<&[u8], &[u8]> = TableDefinition::new("channel_layout");

/// One channel's sidebar placement.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct ChannelLayout {
    kind: ChannelKind,
    parent: Option<ChannelId>,
}

impl Default for ChannelLayout {
    fn default() -> Self {
        Self {
            kind: ChannelKind::Text,
            parent: None,
        }
    }
}

pub struct Store {
    db: Database,
    root: PathBuf,
    logs: HashMap<GuildId, AppendLog>,
}

impl Store {
    /// Open (or create) the store rooted at `root`.
    pub fn open(root: &Path) -> CoreResult<Self> {
        Self::open_inner(root).map_err(Into::into)
    }

    fn open_inner(root: &Path) -> Result<Self> {
        create_dir_private(root)?;
        let db_path = root.join("index.redb");
        let db = if db_path.exists() {
            Database::open(&db_path)?
        } else {
            Database::create(&db_path)?
        };
        // The index holds guild, channel and member metadata in the clear.
        restrict(&db_path, 0o600)?;

        let mut store = Self {
            db,
            root: root.to_path_buf(),
            logs: HashMap::new(),
        };
        store.initialize_tables()?;
        store.recover()?;
        Ok(store)
    }

    fn initialize_tables(&self) -> Result<()> {
        let txn = self.db.begin_write()?;
        txn.open_table(GUILDS)?;
        txn.open_table(CHANNELS)?;
        txn.open_table(MEMBERS)?;
        txn.open_table(EVENTS_BY_ID)?;
        txn.open_table(EVENTS_BY_GUILD)?;
        txn.open_table(EVENTS_BY_CHANNEL)?;
        txn.open_table(AUTHOR_STATE)?;
        txn.open_table(GUILD_CLOCK)?;
        txn.open_table(META_SOURCE)?;
        txn.open_table(READ_STATE)?;
        txn.open_table(BLOBS)?;
        txn.open_table(CATEGORIES)?;
        txn.open_table(CHANNEL_LAYOUT)?;
        txn.commit()?;
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Store {
    fn create_guild_inner(&mut self, name: &str, identity: &Identity) -> Result<CreatedGuild> {
        let name = validate_name(name, "commune name")?;
        let guild_id = random_guild_id();
        let channel_id = random_channel_id();

        let guild_event = self.append_new_event(
            guild_id,
            None,
            EventKind::GuildCreated {
                name: name.to_string(),
            },
            identity,
        )?;
        let channel_event = self.append_new_event(
            guild_id,
            Some(channel_id),
            EventKind::ChannelCreated {
                name: "general".to_string(),
            },
            identity,
        )?;

        let guild = self
            .get_guild_inner(guild_id)?
            .ok_or_else(|| CoreError::NotFound(format!("commune {guild_id}")))?;
        let general = self
            .get_channel_inner(guild_id, channel_id)?
            .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?;

        debug_assert_eq!(guild.created_at, guild_event.body.timestamp);
        debug_assert_eq!(general.created_at, channel_event.body.timestamp);
        Ok(CreatedGuild { guild, general })
    }

    /// Create a guild under an id chosen by the caller.
    ///
    /// The DM path needs this because both people derive the same id from
    /// their keys. The founder is recorded through `MemberJoined`, which is
    /// the same roster event every other join uses.
    fn create_guild_with_id_inner(
        &mut self,
        guild_id: GuildId,
        name: &str,
        display_name: &str,
        identity: &Identity,
    ) -> Result<CreatedGuild> {
        let name = validate_name(name, "commune name")?;
        let display_name = validate_name(display_name, "display name")?;
        if self.get_guild_inner(guild_id)?.is_some() {
            return Err(CoreError::InvalidInput(format!(
                "commune {guild_id} already exists"
            ))
            .into());
        }
        let channel_id = random_channel_id();

        let guild_event = self.append_new_event(
            guild_id,
            None,
            EventKind::GuildCreated {
                name: name.to_string(),
            },
            identity,
        )?;
        let channel_event = self.append_new_event(
            guild_id,
            Some(channel_id),
            EventKind::ChannelCreated {
                name: "general".to_string(),
            },
            identity,
        )?;
        self.append_new_event(
            guild_id,
            None,
            EventKind::MemberJoined {
                display_name: display_name.to_string(),
            },
            identity,
        )?;

        let guild = self
            .get_guild_inner(guild_id)?
            .ok_or_else(|| CoreError::NotFound(format!("commune {guild_id}")))?;
        let general = self
            .get_channel_inner(guild_id, channel_id)?
            .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?;

        debug_assert_eq!(guild.created_at, guild_event.body.timestamp);
        debug_assert_eq!(general.created_at, channel_event.body.timestamp);
        Ok(CreatedGuild { guild, general })
    }

    fn append_member_joined_inner(
        &mut self,
        guild_id: GuildId,
        display_name: &str,
        identity: &Identity,
    ) -> Result<Option<SignedEvent>> {
        if self.get_guild_inner(guild_id)?.is_none() {
            return Err(CoreError::NotFound(format!("commune {guild_id}")).into());
        }
        let display_name = validate_name(display_name, "display name")?.to_string();
        if self
            .get_member_inner(guild_id, identity.author_id())?
            .is_some()
        {
            return Ok(None);
        }
        let event = self.append_new_event(
            guild_id,
            None,
            EventKind::MemberJoined { display_name },
            identity,
        )?;
        Ok(Some(event))
    }

    fn create_channel_inner(
        &mut self,
        guild_id: GuildId,
        name: &str,
        identity: &Identity,
    ) -> Result<(ChannelMeta, SignedEvent)> {
        if self.get_guild_inner(guild_id)?.is_none() {
            return Err(CoreError::NotFound(format!("commune {guild_id}")).into());
        }
        let name = validate_name(name, "channel name")?;
        let channel_id = random_channel_id();
        let event = self.append_new_event(
            guild_id,
            Some(channel_id),
            EventKind::ChannelCreated {
                name: name.to_string(),
            },
            identity,
        )?;
        let channel = self
            .get_channel_inner(guild_id, channel_id)?
            .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?;
        Ok((channel, event))
    }

    fn create_channel_in_inner(
        &mut self,
        guild_id: GuildId,
        name: &str,
        kind: ChannelKind,
        parent: Option<ChannelId>,
        identity: &Identity,
    ) -> Result<(ChannelMeta, SignedEvent)> {
        if kind == ChannelKind::Text && parent.is_none() {
            return self.create_channel_inner(guild_id, name, identity);
        }
        if self.get_guild_inner(guild_id)?.is_none() {
            return Err(CoreError::NotFound(format!("commune {guild_id}")).into());
        }
        let name = validate_name(name, "channel name")?;
        if let Some(parent) = parent
            && self.get_category_inner(guild_id, parent)?.is_none()
        {
            return Err(CoreError::NotFound(format!("category {parent}")).into());
        }
        let channel_id = random_channel_id();
        let event = self.append_new_event(
            guild_id,
            Some(channel_id),
            EventKind::ChannelCreatedV2 {
                name: name.to_string(),
                kind,
                parent,
            },
            identity,
        )?;
        let channel = self
            .get_channel_inner(guild_id, channel_id)?
            .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?;
        Ok((channel, event))
    }

    fn create_category_inner(
        &mut self,
        guild_id: GuildId,
        name: &str,
        identity: &Identity,
    ) -> Result<(CategoryMeta, SignedEvent)> {
        if self.get_guild_inner(guild_id)?.is_none() {
            return Err(CoreError::NotFound(format!("commune {guild_id}")).into());
        }
        let name = validate_name(name, "category name")?;
        let category_id = random_channel_id();
        let event = self.append_new_event(
            guild_id,
            Some(category_id),
            EventKind::CategoryCreated {
                name: name.to_string(),
            },
            identity,
        )?;
        let category = self
            .get_category_inner(guild_id, category_id)?
            .ok_or_else(|| CoreError::NotFound(format!("category {category_id}")))?;
        Ok((category, event))
    }

    fn rename_channel_inner(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        name: &str,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        self.get_channel_inner(guild_id, channel_id)?
            .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?;
        let name = validate_name(name, "channel name")?;
        self.append_new_event(
            guild_id,
            Some(channel_id),
            EventKind::ChannelRenamed {
                target: channel_id,
                name: name.to_string(),
            },
            identity,
        )
    }

    fn move_channel_inner(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        parent: Option<ChannelId>,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        self.get_channel_inner(guild_id, channel_id)?
            .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?;
        if let Some(parent) = parent
            && self.get_category_inner(guild_id, parent)?.is_none()
        {
            return Err(CoreError::NotFound(format!("category {parent}")).into());
        }
        self.append_new_event(
            guild_id,
            Some(channel_id),
            EventKind::ChannelMoved {
                target: channel_id,
                parent,
            },
            identity,
        )
    }

    fn post_message_inner(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        self.post_message_rich_inner(guild_id, channel_id, body, None, &[], &[], identity)
    }

    // Mirrors `Storage::post_message_rich`, so the long argument list is the
    // shape of the event rather than an accident; see that method's note.
    #[allow(clippy::too_many_arguments)]
    fn post_message_rich_inner(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
        attachments: &[Attachment],
        identity: &Identity,
    ) -> Result<SignedEvent> {
        if self.get_guild_inner(guild_id)?.is_none() {
            return Err(CoreError::NotFound(format!("commune {guild_id}")).into());
        }
        let channel = self
            .get_channel_inner(guild_id, channel_id)?
            .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?;
        if channel.guild_id != guild_id {
            return Err(
                CoreError::InvalidInput("channel does not belong to commune".to_string()).into(),
            );
        }
        validate_message_text_inner(body, !attachments.is_empty())?;

        if let Some(target) = reply_to {
            let target_event = self
                .get_event_inner(guild_id, target)?
                .ok_or_else(|| CoreError::NotFound(format!("message {target}")))?;
            if !target_event.body.kind.is_message() {
                return Err(CoreError::InvalidInput(format!(
                    "reply target {target} is not a message"
                ))
                .into());
            }
            if target_event.body.channel_id != Some(channel_id) {
                return Err(CoreError::InvalidInput(
                    "a reply must target a message in the same channel".to_string(),
                )
                .into());
            }
        }

        // De-duplicate while preserving the order the sender wrote them in, so
        // the stored event is deterministic for a given input text.
        let mut unique: Vec<AuthorId> = Vec::with_capacity(mentions.len());
        for mention in mentions {
            if !unique.contains(mention) {
                unique.push(*mention);
            }
        }
        if unique.len() > MAX_MENTIONS {
            return Err(CoreError::InvalidInput(format!(
                "a message may mention at most {MAX_MENTIONS} members"
            ))
            .into());
        }

        if !attachments.is_empty() {
            validate_attachments(attachments)?;
        }

        let kind = if !attachments.is_empty() {
            EventKind::MessageV3 {
                body: body.to_string(),
                reply_to,
                mentions: unique,
                attachments: attachments.to_vec(),
            }
        } else if reply_to.is_none() && unique.is_empty() {
            EventKind::Message {
                body: body.to_string(),
            }
        } else {
            EventKind::MessageV2 {
                body: body.to_string(),
                reply_to,
                mentions: unique,
            }
        };
        self.append_new_event(guild_id, Some(channel_id), kind, identity)
    }

    /// The channel of a message that `author` is allowed to modify.
    ///
    /// Local writes check this before appending so a frontend gets a useful
    /// error; peers enforce the same rule again when folding the timeline, so a
    /// forged edit that skips this check never reaches a rendered message.
    fn message_channel_for_author(
        &self,
        guild_id: GuildId,
        target: EventId,
        author: AuthorId,
        action: &str,
    ) -> Result<ChannelId> {
        let event = self
            .get_event_inner(guild_id, target)?
            .ok_or_else(|| CoreError::NotFound(format!("message {target}")))?;
        if !event.body.kind.is_message() {
            return Err(CoreError::InvalidInput(format!(
                "{action} target {target} is not a message"
            ))
            .into());
        }
        if event.body.author != author {
            return Err(CoreError::InvalidInput(
                "only the author may edit or delete a message".to_string(),
            )
            .into());
        }
        event.body.channel_id.ok_or_else(|| {
            CoreError::InvalidInput(format!("message {target} has no channel")).into()
        })
    }

    fn edit_message_inner(
        &mut self,
        guild_id: GuildId,
        target: EventId,
        body: &str,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        validate_message_text(body)?;
        let channel_id =
            self.message_channel_for_author(guild_id, target, identity.author_id(), "edit")?;
        self.append_new_event(
            guild_id,
            Some(channel_id),
            EventKind::MessageEdited {
                target,
                body: body.to_string(),
            },
            identity,
        )
    }

    fn delete_message_inner(
        &mut self,
        guild_id: GuildId,
        target: EventId,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        let channel_id =
            self.message_channel_for_author(guild_id, target, identity.author_id(), "delete")?;
        self.append_new_event(
            guild_id,
            Some(channel_id),
            EventKind::MessageDeleted { target },
            identity,
        )
    }

    /// The channel of a message any member is allowed to react to.
    fn message_channel_for_reaction(
        &self,
        guild_id: GuildId,
        target: EventId,
        action: &str,
    ) -> Result<ChannelId> {
        let event = self
            .get_event_inner(guild_id, target)?
            .ok_or_else(|| CoreError::NotFound(format!("message {target}")))?;
        if !event.body.kind.is_message() {
            return Err(CoreError::InvalidInput(format!(
                "{action} target {target} is not a message"
            ))
            .into());
        }
        event.body.channel_id.ok_or_else(|| {
            CoreError::InvalidInput(format!("message {target} has no channel")).into()
        })
    }

    fn add_reaction_inner(
        &mut self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        validate_emoji(emoji)?;
        let channel_id = self.message_channel_for_reaction(guild_id, target, "reaction")?;
        self.append_new_event(
            guild_id,
            Some(channel_id),
            EventKind::ReactionAdded {
                target,
                emoji: emoji.to_string(),
            },
            identity,
        )
    }

    fn remove_reaction_inner(
        &mut self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        validate_emoji(emoji)?;
        let channel_id = self.message_channel_for_reaction(guild_id, target, "reaction")?;
        self.append_new_event(
            guild_id,
            Some(channel_id),
            EventKind::ReactionRemoved {
                target,
                emoji: emoji.to_string(),
            },
            identity,
        )
    }

    fn mark_read_inner(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        event_id: EventId,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        self.validate_channel(guild_id, channel_id)?;
        // A marker for an event that has not synced yet is still valid: the
        // identity may have read it on another device. When it is local it
        // must belong to the same channel, otherwise the count would skip
        // unrelated history.
        if let Some(event) = self.get_event_inner(guild_id, event_id)?
            && event.body.channel_id != Some(channel_id)
        {
            return Err(CoreError::InvalidInput(
                "read marker names an event in another channel".to_string(),
            )
            .into());
        }
        self.append_new_event(
            guild_id,
            Some(channel_id),
            EventKind::ReadUpTo { event_id },
            identity,
        )
    }

    fn read_cursor_inner(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        author: AuthorId,
    ) -> Result<SyncCursor> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(READ_STATE)?;
        let key = read_key(guild_id, channel_id, author);
        match table.get(key.as_slice())? {
            Some(value) => Ok(postcard::from_bytes(value.value())?),
            None => Ok(SyncCursor::start()),
        }
    }

    /// Every event in a channel strictly after `cursor`, in log order.
    fn channel_events_after_inner(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        cursor: SyncCursor,
    ) -> Result<Vec<SignedEvent>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(EVENTS_BY_CHANNEL)?;
        let prefix = channel_prefix(guild_id, channel_id);
        let mut locations = Vec::new();
        for item in table.iter()? {
            let (key, value) = item?;
            let key = key.value();
            if !key.starts_with(&prefix) || key.len() != prefix.len() + 8 + 32 {
                continue;
            }
            let mut lamport_bytes = [0u8; 8];
            lamport_bytes.copy_from_slice(&key[prefix.len()..prefix.len() + 8]);
            let lamport = u64::from_be_bytes(lamport_bytes);
            let event_id = EventId::from_slice(&key[prefix.len() + 8..])?;
            if !cursor.is_before(lamport, event_id) {
                continue;
            }
            locations.push(EventLocation::from_bytes(value.value())?);
        }
        drop(table);
        drop(txn);

        let mut events = Vec::with_capacity(locations.len());
        for location in locations {
            let event = read_event(&self.guild_dir(guild_id), location)?;
            event.verify()?;
            events.push(event);
        }
        Ok(events)
    }

    fn unread_counts_inner(
        &self,
        guild_id: GuildId,
        author: AuthorId,
    ) -> Result<HashMap<ChannelId, usize>> {
        let mut counts = HashMap::new();
        for channel in self.list_channels_inner(guild_id)? {
            let cursor = self.read_cursor_inner(guild_id, channel.id, author)?;
            let events = self.channel_events_after_inner(guild_id, channel.id, cursor)?;
            if events.is_empty() {
                continue;
            }
            let unread = timeline(&events)?
                .into_iter()
                .filter(|entry| match entry {
                    TimelineEntry::Message { event, .. } => event.body.author != author,
                    _ => false,
                })
                .count();
            if unread > 0 {
                counts.insert(channel.id, unread);
            }
        }
        Ok(counts)
    }

    /// Import a signed event received from a peer.
    ///
    /// Returns `true` when the event was new and appended, `false` when it was
    /// already present.
    fn import_event_inner(&mut self, event: SignedEvent) -> Result<bool> {
        // Validate cheap, purely structural fields before paying for an
        // Ed25519 verification. An oversized body must be rejected without
        // first hashing and verifying it.
        self.validate_import(&event)?;
        let event_id = event.verify()?;
        if self.has_event_inner(event_id)? {
            return Ok(false);
        }

        let guild_id = event.body.guild_id;
        let payload = event.to_bytes()?;
        let guild_dir = self.guild_dir(guild_id);
        create_dir_private(&guild_dir)?;
        let log = log_for(&self.root, &mut self.logs, guild_id)?;
        let location = log.append(&payload)?;

        let txn = self.db.begin_write()?;
        insert_event_indexes(&txn, event_id, location, &event)?;
        txn.commit()?;
        Ok(true)
    }

    /// Everything a peer-supplied event must satisfy before it is allowed into
    /// the log. The signature was already checked by `SignedEvent::verify`.
    ///
    /// This is deliberately the same bar as a locally created event: an
    /// imported event must be well-formed *and* consistent with the guild it
    /// claims to belong to. Without this, any peer could grow the log without
    /// bound, inject events into another guild, poison the guild clock, or
    /// reference a channel that does not exist.
    fn validate_import(&self, event: &SignedEvent) -> Result<()> {
        let body = &event.body;

        if !supported_event_version(body.version) {
            return Err(CoreError::InvalidInput(format!(
                "unsupported event version: {}",
                body.version
            ))
            .into());
        }
        if body.lamport == 0 || body.lamport == u64::MAX {
            return Err(CoreError::InvalidInput(format!(
                "invalid lamport clock: {}",
                body.lamport
            ))
            .into());
        }
        if body.seq == u64::MAX {
            return Err(CoreError::InvalidInput("invalid event sequence".to_string()).into());
        }
        // The author's first event has no predecessor; every later one does.
        // This is how local events are built, so a peer that disagrees is
        // malformed even if the signature checks out.
        if (body.seq == 0 && body.prev.is_some()) || (body.seq > 0 && body.prev.is_none()) {
            return Err(CoreError::InvalidInput(format!(
                "event sequence {} does not match its predecessor",
                body.seq
            ))
            .into());
        }
        if body.timestamp > now_unix().saturating_add(MAX_FUTURE_SKEW_SECS) {
            return Err(
                CoreError::InvalidInput("event timestamp is in the future".to_string()).into(),
            );
        }

        let guild = self.get_guild_inner(body.guild_id)?;
        let local_clock = self.guild_lamport_inner(body.guild_id)?;
        if local_clock != 0 {
            if body.lamport > local_clock.saturating_add(MAX_LAMPORT_JUMP) {
                return Err(CoreError::InvalidInput(format!(
                    "lamport clock {} is too far ahead of {}",
                    body.lamport, local_clock
                ))
                .into());
            }
        } else {
            // This store has never seen the guild. The first event must be its
            // creation, and its clock must be near the start: otherwise a peer
            // could park the guild clock at u64::MAX on an empty store and no
            // local event would ever be ordered after it.
            if !matches!(body.kind, EventKind::GuildCreated { .. }) {
                return Err(CoreError::InvalidInput(format!(
                    "first event for commune {} is not its creation",
                    body.guild_id
                ))
                .into());
            }
            if body.lamport > MAX_LAMPORT_JUMP {
                return Err(CoreError::InvalidInput(format!(
                    "first event for commune {} has lamport clock {}",
                    body.guild_id, body.lamport
                ))
                .into());
            }
        }

        match &body.kind {
            EventKind::GuildCreated { name } => {
                validate_name(name, "commune name")?;
                if body.channel_id.is_some() {
                    return Err(CoreError::InvalidInput(
                        "commune creation carries a channel".to_string(),
                    )
                    .into());
                }
                // A second, conflicting creation of an existing guild is an
                // attempt to overwrite its identity. Only the founder may
                // restate it (which the deterministic metadata work will make
                // a max-tuple operation).
                if let Some(existing) = guild
                    && existing.founder != body.author
                {
                    return Err(CoreError::InvalidInput(format!(
                        "commune {} already belongs to another founder",
                        body.guild_id
                    ))
                    .into());
                }
            }
            EventKind::ChannelCreated { name } => {
                validate_name(name, "channel name")?;
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
                let Some(channel_id) = body.channel_id else {
                    return Err(CoreError::InvalidInput(
                        "channel creation has no channel id".to_string(),
                    )
                    .into());
                };
                if let Some(existing) = self.get_channel_inner(body.guild_id, channel_id)?
                    && existing.created_by != body.author
                {
                    return Err(CoreError::InvalidInput(format!(
                        "channel {channel_id} already belongs to another creator"
                    ))
                    .into());
                }
            }
            EventKind::CategoryCreated { name } => {
                validate_name(name, "category name")?;
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
                let Some(category_id) = body.channel_id else {
                    return Err(CoreError::InvalidInput(
                        "category creation has no category id".to_string(),
                    )
                    .into());
                };
                // Categories share the channel-id space but never a row: an id
                // may name either a channel or a category, never both.
                if self
                    .get_channel_inner(body.guild_id, category_id)?
                    .is_some()
                {
                    return Err(CoreError::InvalidInput(format!(
                        "channel id {category_id} is already a channel"
                    ))
                    .into());
                }
                if let Some(existing) = self.get_category_inner(body.guild_id, category_id)?
                    && existing.created_by != body.author
                {
                    return Err(CoreError::InvalidInput(format!(
                        "category {category_id} already belongs to another creator"
                    ))
                    .into());
                }
            }
            EventKind::ChannelCreatedV2 { name, kind, parent } => {
                validate_name(name, "channel name")?;
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
                let Some(channel_id) = body.channel_id else {
                    return Err(CoreError::InvalidInput(
                        "channel creation has no channel id".to_string(),
                    )
                    .into());
                };
                let _ = kind;
                if self
                    .get_category_inner(body.guild_id, channel_id)?
                    .is_some()
                {
                    return Err(CoreError::InvalidInput(format!(
                        "channel id {channel_id} is already a category"
                    ))
                    .into());
                }
                if let Some(existing) = self.get_channel_inner(body.guild_id, channel_id)?
                    && existing.created_by != body.author
                {
                    return Err(CoreError::InvalidInput(format!(
                        "channel {channel_id} already belongs to another creator"
                    ))
                    .into());
                }
                if let Some(parent) = parent
                    && self.get_category_inner(body.guild_id, *parent)?.is_none()
                {
                    return Err(CoreError::NotFound(format!(
                        "category {parent} in commune {}",
                        body.guild_id
                    ))
                    .into());
                }
            }
            EventKind::ChannelRenamed { target, name } => {
                validate_name(name, "channel name")?;
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
                let Some(channel_id) = body.channel_id else {
                    return Err(CoreError::InvalidInput(
                        "channel rename has no channel id".to_string(),
                    )
                    .into());
                };
                if channel_id != *target {
                    return Err(CoreError::InvalidInput(
                        "channel rename is carried in another channel".to_string(),
                    )
                    .into());
                }
                self.validate_channel(body.guild_id, *target)?;
            }
            EventKind::ChannelMoved { target, parent } => {
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
                let Some(channel_id) = body.channel_id else {
                    return Err(CoreError::InvalidInput(
                        "channel move has no channel id".to_string(),
                    )
                    .into());
                };
                if channel_id != *target {
                    return Err(CoreError::InvalidInput(
                        "channel move is carried in another channel".to_string(),
                    )
                    .into());
                }
                self.validate_channel(body.guild_id, *target)?;
                if let Some(parent) = parent
                    && self.get_category_inner(body.guild_id, *parent)?.is_none()
                {
                    return Err(CoreError::NotFound(format!(
                        "category {parent} in commune {}",
                        body.guild_id
                    ))
                    .into());
                }
            }
            EventKind::MemberJoined { display_name }
            | EventKind::MemberRenamed { display_name } => {
                validate_name(display_name, "display name")?;
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
            }
            EventKind::MemberLeft => {
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
            }
            EventKind::Message { body: text } => {
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
                validate_message_text(text)?;
                let Some(channel_id) = body.channel_id else {
                    return Err(
                        CoreError::InvalidInput("message has no channel id".to_string()).into(),
                    );
                };
                if self.get_channel_inner(body.guild_id, channel_id)?.is_none() {
                    return Err(CoreError::NotFound(format!(
                        "channel {channel_id} in commune {}",
                        body.guild_id
                    ))
                    .into());
                }
            }
            EventKind::MessageV2 {
                body: text,
                reply_to,
                mentions,
            } => {
                self.validate_message(body, text, guild.is_some(), *reply_to, mentions, &[])?;
            }
            EventKind::MessageV3 {
                body: text,
                reply_to,
                mentions,
                attachments,
            } => {
                self.validate_message(
                    body,
                    text,
                    guild.is_some(),
                    *reply_to,
                    mentions,
                    attachments,
                )?;
            }
            EventKind::MessageEdited { target, body: text } => {
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
                validate_message_text(text)?;
                let Some(channel_id) = body.channel_id else {
                    return Err(CoreError::InvalidInput(
                        "message edit has no channel id".to_string(),
                    )
                    .into());
                };
                self.validate_channel(body.guild_id, channel_id)?;
                // When the target is already local, enforce the same rules a
                // local edit must satisfy. When it is not, accept the event as
                // an orphan: the target may simply not have synced yet, and the
                // timeline fold ignores an edit whose target is missing or was
                // written by somebody else.
                if let Some(target_event) = self.get_event_inner(body.guild_id, *target)? {
                    self.validate_modification_target(
                        &target_event,
                        body.author,
                        channel_id,
                        "message edit",
                    )?;
                }
            }
            EventKind::MessageDeleted { target } => {
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
                let Some(channel_id) = body.channel_id else {
                    return Err(CoreError::InvalidInput(
                        "message deletion has no channel id".to_string(),
                    )
                    .into());
                };
                self.validate_channel(body.guild_id, channel_id)?;
                if let Some(target_event) = self.get_event_inner(body.guild_id, *target)? {
                    self.validate_modification_target(
                        &target_event,
                        body.author,
                        channel_id,
                        "message deletion",
                    )?;
                }
            }
            EventKind::ReactionAdded { target, emoji }
            | EventKind::ReactionRemoved { target, emoji } => {
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
                validate_emoji(emoji)?;
                let Some(channel_id) = body.channel_id else {
                    return Err(
                        CoreError::InvalidInput("reaction has no channel id".to_string()).into(),
                    );
                };
                self.validate_channel(body.guild_id, channel_id)?;
                // Any member may react. When the target is local it must be a
                // message in this channel; an unknown target is accepted as an
                // orphan and shown only if the message syncs later.
                if let Some(target_event) = self.get_event_inner(body.guild_id, *target)? {
                    if !target_event.body.kind.is_message() {
                        return Err(CoreError::InvalidInput(format!(
                            "reaction target {target} is not a message"
                        ))
                        .into());
                    }
                    if target_event.body.channel_id != Some(channel_id) {
                        return Err(CoreError::InvalidInput(
                            "a reaction must target a message in the same channel".to_string(),
                        )
                        .into());
                    }
                }
            }
            EventKind::ReadUpTo { event_id } => {
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
                let Some(channel_id) = body.channel_id else {
                    return Err(CoreError::InvalidInput(
                        "read marker has no channel id".to_string(),
                    )
                    .into());
                };
                self.validate_channel(body.guild_id, channel_id)?;
                if let Some(event) = self.get_event_inner(body.guild_id, *event_id)?
                    && event.body.channel_id != Some(channel_id)
                {
                    return Err(CoreError::InvalidInput(
                        "read marker names an event in another channel".to_string(),
                    )
                    .into());
                }
            }
            EventKind::MemberProfileUpdated {
                about,
                pronouns,
                links,
            } => {
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
                validate_profile(about, pronouns, links)?;
            }
            EventKind::DirectMessageOpened { peer, dm } => {
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
                let Some(channel_id) = body.channel_id else {
                    return Err(CoreError::InvalidInput(
                        "direct-message announcement has no channel id".to_string(),
                    )
                    .into());
                };
                self.validate_channel(body.guild_id, channel_id)?;
                if *peer == body.author {
                    return Err(CoreError::InvalidInput(
                        "a direct-message announcement names its own author as the peer".to_string(),
                    )
                    .into());
                }
                // The id is a function of the pair, so a peer cannot point an
                // announcement at an arbitrary guild the recipient would then
                // join. This is also what lets the recipient recreate it.
                if *dm != direct_guild_id(&body.author, peer) {
                    return Err(CoreError::InvalidInput(
                        "direct-message commune id does not match the two authors".to_string(),
                    )
                    .into());
                }
            }
            EventKind::Unknown { .. } => {
                // An event kind from a newer binary. It is signed and belongs
                // to this guild; the only structural rule that still applies
                // is that a named channel must exist, so it cannot be indexed
                // into another guild's stream. It is stored and shown to
                // nobody until a binary that knows the tag applies it.
                if guild.is_none() {
                    return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
                }
                if let Some(channel_id) = body.channel_id {
                    self.validate_channel(body.guild_id, channel_id)?;
                }
            }
        }

        Ok(())
    }

    /// A referenced channel must exist in the same guild. Channel ids are
    /// global-capable, so the guild check is what stops an event from one guild
    /// being indexed into another's channel stream.
    fn validate_channel(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<()> {
        if self.get_channel_inner(guild_id, channel_id)?.is_none() {
            return Err(
                CoreError::NotFound(format!("channel {channel_id} in commune {guild_id}")).into(),
            );
        }
        Ok(())
    }

    /// Rules shared by every message kind that can reply, mention or carry
    /// files. `Message`, `MessageV2` and `MessageV3` differ only in which of
    /// those fields they hold, so the checks live here and cannot drift apart:
    /// a peer accepts exactly what a local post could have produced.
    fn validate_message(
        &self,
        body: &EventBody,
        text: &str,
        guild_present: bool,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
        attachments: &[Attachment],
    ) -> Result<()> {
        if !guild_present {
            return Err(CoreError::NotFound(format!("commune {}", body.guild_id)).into());
        }
        validate_message_text_inner(text, !attachments.is_empty())?;
        let Some(channel_id) = body.channel_id else {
            return Err(CoreError::InvalidInput("message has no channel id".to_string()).into());
        };
        self.validate_channel(body.guild_id, channel_id)?;
        if mentions.len() > MAX_MENTIONS {
            return Err(CoreError::InvalidInput(format!(
                "message mentions {} members, limit is {MAX_MENTIONS}",
                mentions.len()
            ))
            .into());
        }
        if !attachments.is_empty() {
            validate_attachments(attachments)?;
        }
        // A reply target that is already local must be a message in the same
        // channel. An unknown target is an inert orphan: the referenced message
        // may simply not have synced yet, and the timeline fold resolves the
        // reference only when it can.
        if let Some(target) = reply_to
            && let Some(target_event) = self.get_event_inner(body.guild_id, target)?
        {
            if !target_event.body.kind.is_message() {
                return Err(CoreError::InvalidInput(format!(
                    "reply target {target} is not a message"
                ))
                .into());
            }
            if target_event.body.channel_id != Some(channel_id) {
                return Err(CoreError::InvalidInput(
                    "a reply must target a message in the same channel".to_string(),
                )
                .into());
            }
        }
        Ok(())
    }

    /// Rules for an edit/delete whose target is already stored locally.
    fn validate_modification_target(
        &self,
        target: &SignedEvent,
        author: AuthorId,
        channel_id: ChannelId,
        action: &str,
    ) -> Result<()> {
        if !target.body.kind.is_message() {
            return Err(
                CoreError::InvalidInput(format!("{action} target is not a message")).into(),
            );
        }
        if target.body.channel_id != Some(channel_id) {
            return Err(CoreError::InvalidInput(format!(
                "{action} names a different channel than its target"
            ))
            .into());
        }
        if target.body.author != author {
            return Err(CoreError::InvalidInput(
                "only the author may edit or delete a message".to_string(),
            )
            .into());
        }
        Ok(())
    }

    fn has_event_inner(&self, event_id: EventId) -> Result<bool> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(EVENTS_BY_ID)?;
        Ok(table.get(event_id.as_bytes().as_slice())?.is_some())
    }

    fn events_after_inner(
        &self,
        guild_id: GuildId,
        cursor: SyncCursor,
        limit: usize,
        max_bytes: usize,
    ) -> Result<EventPage> {
        if limit == 0 {
            return Ok(EventPage {
                events: Vec::new(),
                more: true,
            });
        }

        let txn = self.db.begin_read()?;
        let table = txn.open_table(EVENTS_BY_GUILD)?;
        let prefix = guild_id.as_bytes().as_slice();

        // Keys are `guild_id || lamport_be || event_id`, so a redb range from
        // the cursor key starts exactly after the events we already sent.
        let start = guild_event_key(
            guild_id,
            cursor.lamport,
            cursor.event_id.unwrap_or(EventId([0u8; 32])),
        );
        let mut locations = Vec::new();
        let mut bytes = 0usize;
        let mut more = false;

        for item in table.range::<&[u8]>(start.as_slice()..)? {
            let (key, value) = item?;
            let key = key.value();
            if !key.starts_with(prefix) {
                break;
            }
            if key.len() != 72 {
                continue;
            }
            let lamport = u64::from_be_bytes(key[32..40].try_into().expect("key length checked"));
            let mut event_id_bytes = [0u8; 32];
            event_id_bytes.copy_from_slice(&key[40..72]);
            let event_id = EventId(event_id_bytes);
            if !cursor.is_before(lamport, event_id) {
                continue;
            }

            let location = EventLocation::from_bytes(value.value())?;

            // Always make progress: an event larger than the whole byte budget
            // is returned by itself when the page is still empty.
            if locations.len() >= limit
                || (!locations.is_empty() && bytes + location.len as usize > max_bytes)
            {
                more = true;
                break;
            }

            bytes += location.len as usize;
            locations.push(location);
        }
        drop(table);
        drop(txn);

        let mut events = Vec::with_capacity(locations.len());
        for location in locations {
            let event = read_event(&self.guild_dir(guild_id), location)?;
            event.verify()?;
            events.push(event);
        }
        Ok(EventPage { events, more })
    }

    fn guild_lamport_inner(&self, guild_id: GuildId) -> Result<u64> {
        Ok(self.read_guild_clock(guild_id)?.last_lamport)
    }

    fn add_member_inner(
        &mut self,
        guild_id: GuildId,
        author: AuthorId,
        display_name: &str,
    ) -> Result<()> {
        if self.get_guild_inner(guild_id)?.is_none() {
            return Err(CoreError::NotFound(format!("commune {guild_id}")).into());
        }
        let member = MemberInfo {
            guild_id,
            author,
            display_name: display_name.to_string(),
            joined_at: now_unix(),
        };
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(MEMBERS)?;
            table.insert(
                member_key(guild_id, author).as_slice(),
                postcard::to_allocvec(&member)?.as_slice(),
            )?;
        }
        txn.commit()?;
        Ok(())
    }

    fn get_member_inner(&self, guild_id: GuildId, author: AuthorId) -> Result<Option<MemberInfo>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(MEMBERS)?;
        let Some(value) = table.get(member_key(guild_id, author).as_slice())? else {
            return Ok(None);
        };
        Ok(Some(postcard::from_bytes(value.value())?))
    }

    fn set_display_name_inner(
        &mut self,
        guild_id: GuildId,
        display_name: &str,
        identity: &Identity,
    ) -> Result<Option<SignedEvent>> {
        if self.get_guild_inner(guild_id)?.is_none() {
            return Err(CoreError::NotFound(format!("commune {guild_id}")).into());
        }
        let display_name = validate_name(display_name, "display name")?.to_string();
        let author = identity.author_id();
        if let Some(member) = self.get_member_inner(guild_id, author)?
            && member.display_name == display_name
        {
            return Ok(None);
        }

        let event = self.append_new_event(
            guild_id,
            None,
            EventKind::MemberRenamed { display_name },
            identity,
        )?;
        Ok(Some(event))
    }

    fn set_profile_inner(
        &mut self,
        guild_id: GuildId,
        about: &str,
        pronouns: &str,
        links: &[String],
        identity: &Identity,
    ) -> Result<SignedEvent> {
        if self.get_guild_inner(guild_id)?.is_none() {
            return Err(CoreError::NotFound(format!("commune {guild_id}")).into());
        }
        let author = identity.author_id();
        if self.get_member_inner(guild_id, author)?.is_none() {
            return Err(CoreError::NotFound(format!(
                "member {author} in commune {guild_id}"
            ))
            .into());
        }
        validate_profile(about, pronouns, links)?;
        self.append_new_event(
            guild_id,
            None,
            EventKind::MemberProfileUpdated {
                about: about.to_string(),
                pronouns: pronouns.to_string(),
                links: links.to_vec(),
            },
            identity,
        )
    }

    /// Resolve the latest profile and derive activity by folding the guild log.
    ///
    /// There is deliberately no profile table: the latest `(lamport, event_id)`
    /// profile is a pure function of the event log, and the same fold supplies
    /// the message count and last activity. The whole guild is walked, so this
    /// is O(log) in the guild's event count and allocates only what the fold
    /// needs; calling it once per opened profile page is fine.
    fn member_profile_inner(
        &self,
        guild_id: GuildId,
        author: AuthorId,
    ) -> Result<MemberProfile> {
        let member = self
            .get_member_inner(guild_id, author)?
            .ok_or_else(|| CoreError::NotFound(format!("member {author} in commune {guild_id}")))?;
        let events = self.all_guild_events_inner(guild_id)?;

        let mut latest: Option<(u64, EventId, &SignedEvent)> = None;
        let mut last_activity = 0u64;
        for event in &events {
            if event.body.author != author {
                continue;
            }
            last_activity = last_activity.max(event.body.timestamp);
            if matches!(event.body.kind, EventKind::MemberProfileUpdated { .. }) {
                let event_id = event.id()?;
                let candidate = (event.body.lamport, event_id);
                let better = latest
                    .as_ref()
                    .is_none_or(|(lamport, current, _)| candidate > (*lamport, *current));
                if better {
                    latest = Some((candidate.0, candidate.1, event));
                }
            }
        }

        let (about, pronouns, links, updated_at) = match latest {
            Some((_, _, event)) => match &event.body.kind {
                EventKind::MemberProfileUpdated {
                    about,
                    pronouns,
                    links,
                } => (
                    about.clone(),
                    pronouns.clone(),
                    links.clone(),
                    event.body.timestamp,
                ),
                _ => (String::new(), String::new(), Vec::new(), 0),
            },
            None => (String::new(), String::new(), Vec::new(), 0),
        };

        let message_count = timeline(&events)?
            .into_iter()
            .filter(|entry| {
                matches!(
                    entry,
                    TimelineEntry::Message { event, .. } if event.body.author == author
                )
            })
            .count() as u64;

        Ok(MemberProfile {
            guild_id,
            author,
            display_name: member.display_name,
            about,
            pronouns,
            links,
            joined_at: member.joined_at,
            updated_at,
            message_count,
            last_activity,
        })
    }

    fn announce_direct_message_inner(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        peer: AuthorId,
        dm: GuildId,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        if self.get_guild_inner(guild_id)?.is_none() {
            return Err(CoreError::NotFound(format!("commune {guild_id}")).into());
        }
        self.validate_channel(guild_id, channel_id)?;
        let author = identity.author_id();
        if peer == author {
            return Err(
                CoreError::InvalidInput("a direct message needs another member".to_string())
                    .into(),
            );
        }
        if self.get_member_inner(guild_id, peer)?.is_none() {
            return Err(CoreError::NotFound(format!(
                "member {peer} in commune {guild_id}"
            ))
            .into());
        }
        if dm != direct_guild_id(&author, &peer) {
            return Err(CoreError::InvalidInput(
                "direct-message commune id does not match the two authors".to_string(),
            )
            .into());
        }
        self.append_new_event(
            guild_id,
            Some(channel_id),
            EventKind::DirectMessageOpened { peer, dm },
            identity,
        )
    }

    /// Every event in a guild in `(lamport, event_id)` order.
    fn all_guild_events_inner(&self, guild_id: GuildId) -> Result<Vec<SignedEvent>> {
        let mut events = Vec::new();
        let mut cursor = SyncCursor::start();
        loop {
            let page = self.events_after_inner(guild_id, cursor, 500, 8 * 1024 * 1024)?;
            if page.events.is_empty() {
                break;
            }
            cursor = SyncCursor::after(page.events.last().expect("non-empty page"))?;
            events.extend(page.events);
            if !page.more {
                break;
            }
        }
        Ok(events)
    }

    fn leave_guild_inner(
        &mut self,
        guild_id: GuildId,
        identity: &Identity,
    ) -> Result<Option<SignedEvent>> {
        if self.get_guild_inner(guild_id)?.is_none() {
            return Err(CoreError::NotFound(format!("commune {guild_id}")).into());
        }
        let author = identity.author_id();
        if self.get_member_inner(guild_id, author)?.is_none() {
            return Ok(None);
        }
        let event = self.append_new_event(guild_id, None, EventKind::MemberLeft, identity)?;
        Ok(Some(event))
    }

    fn list_guilds_inner(&self) -> Result<Vec<GuildMeta>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(GUILDS)?;
        let mut guilds = Vec::new();
        for item in table.iter()? {
            let (_, value) = item?;
            guilds.push(postcard::from_bytes(value.value())?);
        }
        guilds.sort_by(|a: &GuildMeta, b: &GuildMeta| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(guilds)
    }

    fn get_guild_inner(&self, guild_id: GuildId) -> Result<Option<GuildMeta>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(GUILDS)?;
        let Some(value) = table.get(guild_id.as_bytes().as_slice())? else {
            return Ok(None);
        };
        Ok(Some(postcard::from_bytes(value.value())?))
    }

    fn list_channels_inner(&self, guild_id: GuildId) -> Result<Vec<ChannelMeta>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHANNELS)?;
        let prefix = guild_id.as_bytes().as_slice();
        let mut channels = Vec::new();
        for item in table.iter()? {
            let (key, value) = item?;
            if !key.value().starts_with(prefix) {
                continue;
            }
            channels.push(postcard::from_bytes(value.value())?);
        }
        channels.sort_by(|a: &ChannelMeta, b: &ChannelMeta| a.name.cmp(&b.name));
        Ok(channels)
    }

    fn get_channel_inner(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<Option<ChannelMeta>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHANNELS)?;
        let Some(value) = table.get(channel_key(guild_id, channel_id).as_slice())? else {
            return Ok(None);
        };
        Ok(Some(postcard::from_bytes(value.value())?))
    }

    fn get_category_inner(
        &self,
        guild_id: GuildId,
        category_id: ChannelId,
    ) -> Result<Option<CategoryMeta>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CATEGORIES)?;
        let Some(value) = table.get(channel_key(guild_id, category_id).as_slice())? else {
            return Ok(None);
        };
        Ok(Some(postcard::from_bytes(value.value())?))
    }

    fn list_categories_inner(&self, guild_id: GuildId) -> Result<Vec<CategoryMeta>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CATEGORIES)?;
        let prefix = guild_id.as_bytes().as_slice();
        let mut categories = Vec::new();
        for item in table.iter()? {
            let (key, value) = item?;
            if !key.value().starts_with(prefix) {
                continue;
            }
            categories.push(postcard::from_bytes(value.value())?);
        }
        categories.sort_by(|a: &CategoryMeta, b: &CategoryMeta| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(categories)
    }

    fn list_channel_views_inner(&self, guild_id: GuildId) -> Result<Vec<ChannelView>> {
        let channels = self.list_channels_inner(guild_id)?;
        let categories = self.list_categories_inner(guild_id)?;
        let rank: HashMap<ChannelId, (u64, String)> = categories
            .iter()
            .map(|category| (category.id, (category.created_at, category.name.clone())))
            .collect();

        let txn = self.db.begin_read()?;
        let layouts = txn.open_table(CHANNEL_LAYOUT)?;
        let mut views = Vec::with_capacity(channels.len());
        for channel in channels {
            let layout = match layouts.get(channel_key(guild_id, channel.id).as_slice())? {
                Some(value) => postcard::from_bytes::<ChannelLayout>(value.value())?,
                None => ChannelLayout::default(),
            };
            views.push(ChannelView {
                id: channel.id,
                guild_id: channel.guild_id,
                name: channel.name,
                created_by: channel.created_by,
                created_at: channel.created_at,
                kind: layout.kind,
                parent: layout.parent,
            });
        }
        drop(layouts);
        drop(txn);

        // Sidebar order: uncategorised channels first, then each category in
        // creation order with its channels underneath. Names break ties so two
        // peers with the same events list the sidebar identically.
        views.sort_by(|a, b| {
            let key_a = view_sort_key(a, &rank);
            let key_b = view_sort_key(b, &rank);
            key_a.cmp(&key_b)
        });
        Ok(views)
    }

    fn list_members_inner(&self, guild_id: GuildId) -> Result<Vec<MemberInfo>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(MEMBERS)?;
        let prefix = guild_id.as_bytes().as_slice();
        let mut members = Vec::new();
        for item in table.iter()? {
            let (key, value) = item?;
            if !key.value().starts_with(prefix) {
                continue;
            }
            members.push(postcard::from_bytes(value.value())?);
        }
        members.sort_by(|a: &MemberInfo, b: &MemberInfo| {
            a.display_name
                .cmp(&b.display_name)
                .then_with(|| a.author.cmp(&b.author))
        });
        Ok(members)
    }

    fn history_inner(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        limit: usize,
    ) -> Result<Vec<SignedEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let txn = self.db.begin_read()?;
        let table = txn.open_table(EVENTS_BY_CHANNEL)?;
        let prefix = channel_prefix(guild_id, channel_id);
        // Keep only the last `limit` locations instead of collecting the whole
        // channel and then discarding all but that many. Iterating the index is
        // still O(channel length), but the expensive part — reading and
        // verifying every event — is bounded by `limit`.
        let mut tail = VecDeque::with_capacity(limit);
        for item in table.iter()? {
            let (key, value) = item?;
            if !key.value().starts_with(&prefix) {
                continue;
            }
            if tail.len() == limit {
                tail.pop_front();
            }
            tail.push_back(EventLocation::from_bytes(value.value())?);
        }
        drop(table);
        drop(txn);

        let mut events = Vec::with_capacity(tail.len());
        for location in tail {
            let event = read_event(&self.guild_dir(guild_id), location)?;
            event.verify()?;
            events.push(event);
        }
        Ok(events)
    }

    fn history_before_inner(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        cursor: Option<SyncCursor>,
        limit: usize,
    ) -> Result<EventPage> {
        if limit == 0 {
            return Ok(EventPage {
                events: Vec::new(),
                more: true,
            });
        }

        let txn = self.db.begin_read()?;
        let table = txn.open_table(EVENTS_BY_CHANNEL)?;
        let prefix = channel_prefix(guild_id, channel_id);
        // An exclusive upper bound. A cursor names its event exactly; without
        // one, `prefix || 0xff * 41` sorts after every key that starts with
        // this channel prefix and before the next prefix in the table.
        let end = match cursor.and_then(|cursor| cursor.event_id.map(|id| (cursor.lamport, id))) {
            Some((lamport, event_id)) => channel_event_key(guild_id, channel_id, lamport, event_id),
            None => {
                let mut end = prefix.clone();
                end.extend_from_slice(&[0xff; 41]);
                end
            }
        };

        let mut locations = Vec::with_capacity(limit);
        let mut more = false;
        for item in table
            .range::<&[u8]>(prefix.as_slice()..end.as_slice())?
            .rev()
        {
            let (key, value) = item?;
            let key = key.value();
            if !key.starts_with(&prefix) || key.len() != prefix.len() + 8 + 32 {
                continue;
            }
            if locations.len() == limit {
                more = true;
                break;
            }
            locations.push(EventLocation::from_bytes(value.value())?);
        }
        drop(table);
        drop(txn);

        // The scan runs newest first; callers prepend a page, so hand it back
        // oldest first.
        locations.reverse();
        let mut events = Vec::with_capacity(locations.len());
        for location in locations {
            let event = read_event(&self.guild_dir(guild_id), location)?;
            event.verify()?;
            events.push(event);
        }
        Ok(EventPage { events, more })
    }

    fn get_event_inner(&self, guild_id: GuildId, event_id: EventId) -> Result<Option<SignedEvent>> {
        let location = {
            let txn = self.db.begin_read()?;
            let table = txn.open_table(EVENTS_BY_ID)?;
            let Some(value) = table.get(event_id.as_bytes().as_slice())? else {
                return Ok(None);
            };
            EventLocation::from_bytes(value.value())?
        };
        let event = read_event(&self.guild_dir(guild_id), location)?;
        // `EVENTS_BY_ID` is global while locations are guild-local. A location
        // that happens to exist in the requested guild (every guild's first
        // record starts at the same offset) must not turn into an id-confusion
        // bug that returns somebody else's event.
        if event.verify()? != event_id || event.body.guild_id != guild_id {
            return Ok(None);
        }
        Ok(Some(event))
    }

    fn put_blob_inner(&mut self, id: AttachmentId, bytes: &[u8]) -> Result<()> {
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            return Err(CoreError::InvalidInput(format!(
                "attachment is {} bytes, over the {MAX_ATTACHMENT_BYTES} byte limit",
                bytes.len()
            ))
            .into());
        }
        let actual = hash_bytes(bytes);
        if actual != id {
            return Err(CoreError::InvalidInput(format!(
                "attachment bytes hash to {}, not {}",
                actual.short(),
                id.short()
            ))
            .into());
        }
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(BLOBS)?;
            table.insert(id.as_bytes().as_slice(), bytes)?;
        }
        txn.commit()?;
        Ok(())
    }

    fn get_blob_inner(&self, id: AttachmentId) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(BLOBS)?;
        let Some(value) = table.get(id.as_bytes().as_slice())? else {
            return Ok(None);
        };
        Ok(Some(value.value().to_vec()))
    }
}

impl Store {
    fn append_new_event(
        &mut self,
        guild_id: GuildId,
        channel_id: Option<ChannelId>,
        kind: EventKind,
        identity: &Identity,
    ) -> Result<SignedEvent> {
        let author = identity.author_id();
        let state = self.read_author_state(guild_id, author)?;
        let clock = self.read_guild_clock(guild_id)?;
        let seq = state.next_seq;
        let lamport = state
            .last_lamport
            .max(clock.last_lamport)
            .checked_add(1)
            .ok_or_else(|| {
                CoreError::InvalidInput("commune lamport clock is exhausted".to_string())
            })?;

        let body = EventBody {
            version: EVENT_VERSION,
            guild_id,
            channel_id,
            author,
            seq,
            lamport,
            timestamp: now_unix(),
            prev: state.prev,
            kind,
        };
        let event = identity.sign_body(body)?;
        let event_id = event.id()?;
        let payload = event.to_bytes()?;

        let guild_dir = self.guild_dir(guild_id);
        create_dir_private(&guild_dir)?;
        let log = log_for(&self.root, &mut self.logs, guild_id)?;
        let location = log.append(&payload)?;

        let txn = self.db.begin_write()?;
        insert_event_indexes(&txn, event_id, location, &event)?;
        txn.commit()?;

        Ok(event)
    }

    fn read_author_state(&self, guild_id: GuildId, author: AuthorId) -> Result<AuthorState> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(AUTHOR_STATE)?;
        let Some(value) = table.get(author_key(guild_id, author).as_slice())? else {
            return Ok(AuthorState::default());
        };
        Ok(postcard::from_bytes(value.value())?)
    }

    fn read_guild_clock(&self, guild_id: GuildId) -> Result<GuildClock> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(GUILD_CLOCK)?;
        let Some(value) = table.get(guild_id.as_bytes().as_slice())? else {
            return Ok(GuildClock::default());
        };
        Ok(postcard::from_bytes(value.value())?)
    }

    fn guild_dir(&self, guild_id: GuildId) -> PathBuf {
        guild_dir(&self.root, guild_id)
    }

    fn recover(&mut self) -> Result<()> {
        let guilds_dir = self.root.join("guilds");
        create_dir_private(&guilds_dir)?;

        let mut guilds: Vec<(GuildId, PathBuf)> = Vec::new();
        for entry in fs::read_dir(&guilds_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(CoreError::CorruptLog {
                    path,
                    reason: "guild directory must not be a symlink".to_string(),
                }
                .into());
            }
            if !file_type.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(guild_id) = name.parse::<GuildId>() else {
                continue;
            };
            // Every read builds the path from `guild_id.to_hex()`. A directory
            // spelled differently (uppercase, whitespace-padded) would be
            // recovered but never read, and a later post would fork the guild
            // into a second directory.
            if name != guild_id.to_hex() {
                return Err(CoreError::CorruptLog {
                    path,
                    reason: format!(
                        "guild directory name {name:?} is not canonical; expected {:?}",
                        guild_id.to_hex()
                    ),
                }
                .into());
            }
            guilds.push((guild_id, path));
        }

        // Pass one repairs each segment tail, streams the surviving records and
        // prunes stale index rows. Events are dropped as soon as their id and
        // location have been read; only the compact id/location pairs survive
        // this pass, never the event bodies.
        let mut valid_ids: HashMap<[u8; 32], [u8; 16]> = HashMap::new();
        let mut visited_guilds: HashSet<[u8; 32]> = HashSet::new();
        for (guild_id, path) in &guilds {
            // Repair a crash-torn tail now, not on the first append. A store
            // opened only for reading would otherwise keep a partial record
            // forever, and every recovery pass would have to skip it.
            AppendLog::open(path)?;
            visited_guilds.insert(*guild_id.as_bytes());

            let mut valid_locations: HashSet<[u8; 16]> = HashSet::new();
            scan_guild_streaming(path, false, |location, event| {
                let event_id = event.id()?;
                if event.body.guild_id != *guild_id {
                    return Err(CoreError::CorruptLog {
                        path: path.clone(),
                        reason: format!(
                            "event {event_id} belongs to guild {} but was found under {guild_id}",
                            event.body.guild_id
                        ),
                    }
                    .into());
                }
                let location = location.to_bytes();
                valid_locations.insert(location);
                valid_ids.insert(*event_id.as_bytes(), location);
                Ok(())
            })?;

            // Locations are unique only within a guild: every guild's first
            // record lives at the same segment and offset. Pruning while this
            // guild's own set is in hand keeps the check exact, and the set is
            // dropped before the next guild is scanned.
            let txn = self.db.begin_write()?;
            prune_stale_guild_indexes(&txn, *guild_id, &valid_locations)?;
            txn.commit()?;
        }

        // A torn tail is truncated before this runs, but the index may still
        // point at records that no longer exist (or never existed). Drop those
        // before anything is validated: validation resolves reply, edit and
        // reaction targets through the index, and a dangling row would turn a
        // harmless stale reference into a read failure.
        {
            let txn = self.db.begin_write()?;
            // A guild directory that disappeared entirely is never visited by
            // the per-guild prune above, so sweep its rows out here.
            prune_unvisited_guild_indexes(&txn, &visited_guilds)?;
            prune_stale_event_ids(&txn, &valid_ids)?;
            txn.commit()?;
        }
        // The global sets are needed only for the prunes above; free them
        // before the validation pass so they do not sit alongside it.
        drop(valid_ids);
        drop(visited_guilds);

        // Pass two validates and indexes one event at a time. A record is not
        // trustworthy merely because it is on disk: it runs through the same
        // domain checks as a peer import. Events that were already indexed need
        // only the cheap checks; genuinely new records also have their
        // signature verified before they are indexed.
        for (guild_id, path) in &guilds {
            scan_guild_streaming(path, false, |location, event| {
                let event_id = event.id()?;
                if event.body.guild_id != *guild_id {
                    return Err(CoreError::CorruptLog {
                        path: path.clone(),
                        reason: format!(
                            "event {event_id} belongs to guild {} but was found under {guild_id}",
                            event.body.guild_id
                        ),
                    }
                    .into());
                }
                self.validate_import(&event)?;
                if self.has_event_inner(event_id)? {
                    return Ok(());
                }
                event.verify()?;
                let txn = self.db.begin_write()?;
                insert_event_indexes(&txn, event_id, location, &event)?;
                txn.commit()?;
                Ok(())
            })?;
        }
        Ok(())
    }
}

/// Remove guild-scoped index rows that do not point at a record actually found
/// in that guild's segment logs.
///
/// `EVENTS_BY_GUILD` and `EVENTS_BY_CHANNEL` keys start with the guild id, so
/// only the matching prefix is visited. An `EventLocation` is unique only
/// within one guild's `events/` directory, which is why the guild is part of
/// the check.
fn prune_stale_guild_indexes(
    txn: &WriteTransaction,
    guild_id: GuildId,
    valid_locations: &HashSet<[u8; 16]>,
) -> Result<()> {
    let prefix = guild_id.as_bytes();
    prune_location_table(txn, EVENTS_BY_GUILD, prefix, valid_locations)?;
    prune_location_table(txn, EVENTS_BY_CHANNEL, prefix, valid_locations)?;
    Ok(())
}

fn prune_location_table(
    txn: &WriteTransaction,
    table: TableDefinition<&[u8], &[u8]>,
    prefix: &[u8],
    valid_locations: &HashSet<[u8; 16]>,
) -> Result<()> {
    let stale: Vec<Vec<u8>> = {
        let table = txn.open_table(table)?;
        let mut stale = Vec::new();
        for item in table.range::<&[u8]>(prefix..)? {
            let (key, value) = item?;
            let key = key.value();
            // Keys are sorted, so once the prefix no longer matches there is
            // nothing left for this guild.
            if !key.starts_with(prefix) {
                break;
            }
            let value = value.value();
            let valid = value.len() == 16
                && valid_locations.contains(&<[u8; 16]>::try_from(value).expect("length checked"));
            if !valid {
                stale.push(key.to_vec());
            }
        }
        stale
    };
    if !stale.is_empty() {
        let mut table = txn.open_table(table)?;
        for key in stale {
            table.remove(key.as_slice())?;
        }
    }
    Ok(())
}

/// Remove guild-scoped rows whose guild directory was not recovered at all.
///
/// A per-guild prune cannot see a guild whose directory vanished, and with no
/// guild directories at all it would never run. Locations are guild-scoped, so
/// every row for a guild with no recovered log is dangling by definition.
fn prune_unvisited_guild_indexes(
    txn: &WriteTransaction,
    visited: &HashSet<[u8; 32]>,
) -> Result<()> {
    prune_unvisited_table(txn, EVENTS_BY_GUILD, visited)?;
    prune_unvisited_table(txn, EVENTS_BY_CHANNEL, visited)?;
    Ok(())
}

fn prune_unvisited_table(
    txn: &WriteTransaction,
    table: TableDefinition<&[u8], &[u8]>,
    visited: &HashSet<[u8; 32]>,
) -> Result<()> {
    let stale: Vec<Vec<u8>> = {
        let table = txn.open_table(table)?;
        let mut stale = Vec::new();
        for item in table.iter()? {
            let (key, _) = item?;
            let key = key.value();
            let Some(guild) = key.get(..32) else {
                stale.push(key.to_vec());
                continue;
            };
            let guild: [u8; 32] = guild.try_into().expect("length checked");
            if !visited.contains(&guild) {
                stale.push(key.to_vec());
            }
        }
        stale
    };
    if !stale.is_empty() {
        let mut table = txn.open_table(table)?;
        for key in stale {
            table.remove(key.as_slice())?;
        }
    }
    Ok(())
}

/// Remove `EVENTS_BY_ID` rows that do not name a record actually found in any
/// guild log.
///
/// The table has no guild in its key, so it is checked against the exact
/// id/location pair. Another guild having a record at the same location must
/// not keep a lost guild's row alive.
fn prune_stale_event_ids(
    txn: &WriteTransaction,
    valid_ids: &HashMap<[u8; 32], [u8; 16]>,
) -> Result<()> {
    let stale: Vec<Vec<u8>> = {
        let table = txn.open_table(EVENTS_BY_ID)?;
        let mut stale = Vec::new();
        for item in table.iter()? {
            let (key, value) = item?;
            let key = key.value();
            let value: &[u8] = value.value();
            let valid = if value.len() == 16 {
                let location = <[u8; 16]>::try_from(value).expect("length checked");
                <[u8; 32]>::try_from(key)
                    .ok()
                    .and_then(|event_id| valid_ids.get(&event_id).copied())
                    == Some(location)
            } else {
                false
            };
            if !valid {
                stale.push(key.to_vec());
            }
        }
        stale
    };
    if !stale.is_empty() {
        let mut table = txn.open_table(EVENTS_BY_ID)?;
        for key in stale {
            table.remove(key.as_slice())?;
        }
    }
    Ok(())
}

/// The `(lamport, event_id)` that last wrote a metadata row.
///
/// Metadata must be a pure function of the event set, not of arrival order.
/// Every guild/channel/member row records which event produced it and only a
/// strictly greater tuple may replace it, so any two peers that have seen the
/// same events agree on names even if they applied them in a different order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct EventSource {
    lamport: u64,
    event_id: EventId,
    /// Set when the winning event removed the member. Kept so a later,
    /// unrelated event from before the leave cannot recreate the row.
    removed: bool,
}

impl EventSource {
    fn is_newer_than(self, other: Self) -> bool {
        (self.lamport, self.event_id.as_bytes()) > (other.lamport, other.event_id.as_bytes())
    }
}

fn guild_meta_key(guild_id: GuildId) -> Vec<u8> {
    let mut key = Vec::with_capacity(33);
    key.push(b'g');
    key.extend_from_slice(guild_id.as_bytes());
    key
}

fn channel_meta_key(guild_id: GuildId, channel_id: ChannelId) -> Vec<u8> {
    let mut key = Vec::with_capacity(49);
    key.push(b'c');
    key.extend_from_slice(guild_id.as_bytes());
    key.extend_from_slice(channel_id.as_bytes());
    key
}

fn member_meta_key(guild_id: GuildId, author: AuthorId) -> Vec<u8> {
    let mut key = Vec::with_capacity(65);
    key.push(b'm');
    key.extend_from_slice(guild_id.as_bytes());
    key.extend_from_slice(author.as_bytes());
    key
}

fn layout_meta_key(guild_id: GuildId, channel_id: ChannelId) -> Vec<u8> {
    let mut key = Vec::with_capacity(49);
    key.push(b'l');
    key.extend_from_slice(guild_id.as_bytes());
    key.extend_from_slice(channel_id.as_bytes());
    key
}

fn category_meta_key(guild_id: GuildId, category_id: ChannelId) -> Vec<u8> {
    let mut key = Vec::with_capacity(49);
    key.push(b'k');
    key.extend_from_slice(guild_id.as_bytes());
    key.extend_from_slice(category_id.as_bytes());
    key
}

/// Sidebar order for a channel: uncategorised ones first, then by the parent
/// category's creation time and name, then by channel name.
fn view_sort_key(
    view: &ChannelView,
    ranks: &HashMap<ChannelId, (u64, String)>,
) -> (u8, u64, String, String) {
    match view.parent {
        None => (0, 0, String::new(), view.name.clone()),
        Some(parent) => {
            let (created_at, name) = ranks
                .get(&parent)
                .cloned()
                .unwrap_or((u64::MAX, String::new()));
            (1, created_at, name, view.name.clone())
        }
    }
}

fn current_metadata(txn: &WriteTransaction, key: &[u8]) -> Result<Option<EventSource>> {
    let table = txn.open_table(META_SOURCE)?;
    match table.get(key)? {
        Some(value) => Ok(Some(postcard::from_bytes::<EventSource>(value.value())?)),
        None => Ok(None),
    }
}

/// Claim a metadata row for `source`. Returns false when an equal or newer
/// event already owns it.
fn claim_metadata(txn: &WriteTransaction, key: &[u8], source: EventSource) -> Result<bool> {
    if current_metadata(txn, key)?.is_some_and(|old| !source.is_newer_than(old)) {
        return Ok(false);
    }
    let mut table = txn.open_table(META_SOURCE)?;
    let value = postcard::to_allocvec(&source)?;
    table.insert(key, value.as_slice())?;
    Ok(true)
}

fn insert_event_indexes(
    txn: &WriteTransaction,
    event_id: EventId,
    location: EventLocation,
    event: &SignedEvent,
) -> Result<()> {
    let body = &event.body;
    let location_bytes = location.to_bytes();
    let source = EventSource {
        lamport: body.lamport,
        event_id,
        removed: false,
    };

    {
        let mut table = txn.open_table(EVENTS_BY_ID)?;
        table.insert(event_id.as_bytes().as_slice(), location_bytes.as_slice())?;
    }
    {
        let mut table = txn.open_table(EVENTS_BY_GUILD)?;
        let key = guild_event_key(body.guild_id, body.lamport, event_id);
        table.insert(key.as_slice(), location_bytes.as_slice())?;
    }
    if let Some(channel_id) = body.channel_id {
        let mut table = txn.open_table(EVENTS_BY_CHANNEL)?;
        let key = channel_event_key(body.guild_id, channel_id, body.lamport, event_id);
        table.insert(key.as_slice(), location_bytes.as_slice())?;
    }

    let old_state = {
        let table = txn.open_table(AUTHOR_STATE)?;
        let key = author_key(body.guild_id, body.author);
        match table.get(key.as_slice())? {
            Some(value) => postcard::from_bytes(value.value())?,
            None => AuthorState::default(),
        }
    };
    let is_latest = body.seq >= old_state.next_seq;
    let new_state = AuthorState {
        next_seq: old_state.next_seq.max(body.seq.saturating_add(1)),
        last_lamport: old_state.last_lamport.max(body.lamport),
        prev: if is_latest {
            Some(event_id)
        } else {
            old_state.prev
        },
    };
    {
        let mut table = txn.open_table(AUTHOR_STATE)?;
        let key = author_key(body.guild_id, body.author);
        let value = postcard::to_allocvec(&new_state)?;
        table.insert(key.as_slice(), value.as_slice())?;
    }

    let old_clock = {
        let table = txn.open_table(GUILD_CLOCK)?;
        match table.get(body.guild_id.as_bytes().as_slice())? {
            Some(value) => postcard::from_bytes::<GuildClock>(value.value())?,
            None => GuildClock::default(),
        }
    };
    let new_clock = GuildClock {
        last_lamport: old_clock.last_lamport.max(body.lamport),
    };
    {
        let mut table = txn.open_table(GUILD_CLOCK)?;
        let value = postcard::to_allocvec(&new_clock)?;
        table.insert(body.guild_id.as_bytes().as_slice(), value.as_slice())?;
    }

    let member_meta_key = member_meta_key(body.guild_id, body.author);
    let member_tombstoned =
        current_metadata(txn, &member_meta_key)?.is_some_and(|source| source.removed);
    let existing_member = {
        let table = txn.open_table(MEMBERS)?;
        table
            .get(member_key(body.guild_id, body.author).as_slice())?
            .is_some()
    };
    if !existing_member && !member_tombstoned {
        let member = MemberInfo {
            guild_id: body.guild_id,
            author: body.author,
            display_name: body.author.short(),
            joined_at: body.timestamp,
        };
        let mut table = txn.open_table(MEMBERS)?;
        let value = postcard::to_allocvec(&member)?;
        table.insert(
            member_key(body.guild_id, body.author).as_slice(),
            value.as_slice(),
        )?;
    }

    match &body.kind {
        EventKind::GuildCreated { name } => {
            let key = guild_meta_key(body.guild_id);
            if claim_metadata(txn, &key, source)? {
                let guild = GuildMeta {
                    id: body.guild_id,
                    name: name.trim().to_string(),
                    founder: body.author,
                    created_at: body.timestamp,
                };
                let mut table = txn.open_table(GUILDS)?;
                let value = postcard::to_allocvec(&guild)?;
                table.insert(body.guild_id.as_bytes().as_slice(), value.as_slice())?;
            }
        }
        EventKind::ChannelCreated { name } => {
            if let Some(channel_id) = body.channel_id {
                let key = channel_meta_key(body.guild_id, channel_id);
                if claim_metadata(txn, &key, source)? {
                    let channel = ChannelMeta {
                        id: channel_id,
                        guild_id: body.guild_id,
                        name: name.trim().to_string(),
                        created_by: body.author,
                        created_at: body.timestamp,
                    };
                    let mut table = txn.open_table(CHANNELS)?;
                    let value = postcard::to_allocvec(&channel)?;
                    table.insert(
                        channel_key(body.guild_id, channel_id).as_slice(),
                        value.as_slice(),
                    )?;
                }
            }
        }
        EventKind::CategoryCreated { name } => {
            if let Some(category_id) = body.channel_id {
                let key = category_meta_key(body.guild_id, category_id);
                if claim_metadata(txn, &key, source)? {
                    let category = CategoryMeta {
                        id: category_id,
                        guild_id: body.guild_id,
                        name: name.trim().to_string(),
                        created_by: body.author,
                        created_at: body.timestamp,
                    };
                    let mut table = txn.open_table(CATEGORIES)?;
                    let value = postcard::to_allocvec(&category)?;
                    table.insert(
                        channel_key(body.guild_id, category_id).as_slice(),
                        value.as_slice(),
                    )?;
                }
            }
        }
        EventKind::ChannelCreatedV2 { name, kind, parent } => {
            if let Some(channel_id) = body.channel_id {
                let key = channel_meta_key(body.guild_id, channel_id);
                if claim_metadata(txn, &key, source)? {
                    let channel = ChannelMeta {
                        id: channel_id,
                        guild_id: body.guild_id,
                        name: name.trim().to_string(),
                        created_by: body.author,
                        created_at: body.timestamp,
                    };
                    let mut table = txn.open_table(CHANNELS)?;
                    let value = postcard::to_allocvec(&channel)?;
                    table.insert(
                        channel_key(body.guild_id, channel_id).as_slice(),
                        value.as_slice(),
                    )?;
                }
                let layout_key = layout_meta_key(body.guild_id, channel_id);
                if claim_metadata(txn, &layout_key, source)? {
                    let layout = ChannelLayout {
                        kind: *kind,
                        parent: *parent,
                    };
                    let mut table = txn.open_table(CHANNEL_LAYOUT)?;
                    let value = postcard::to_allocvec(&layout)?;
                    table.insert(
                        channel_key(body.guild_id, channel_id).as_slice(),
                        value.as_slice(),
                    )?;
                }
            }
        }
        EventKind::ChannelRenamed { target, name } => {
            let key = channel_meta_key(body.guild_id, *target);
            let existing = {
                let table = txn.open_table(CHANNELS)?;
                match table.get(channel_key(body.guild_id, *target).as_slice())? {
                    Some(value) => Some(postcard::from_bytes::<ChannelMeta>(value.value())?),
                    None => None,
                }
            };
            if claim_metadata(txn, &key, source)?
                && let Some(mut channel) = existing
            {
                channel.name = name.trim().to_string();
                let mut table = txn.open_table(CHANNELS)?;
                let value = postcard::to_allocvec(&channel)?;
                table.insert(
                    channel_key(body.guild_id, *target).as_slice(),
                    value.as_slice(),
                )?;
            }
        }
        EventKind::ChannelMoved { target, parent } => {
            let key = layout_meta_key(body.guild_id, *target);
            let existing = {
                let table = txn.open_table(CHANNEL_LAYOUT)?;
                match table.get(channel_key(body.guild_id, *target).as_slice())? {
                    Some(value) => postcard::from_bytes::<ChannelLayout>(value.value())?,
                    None => ChannelLayout::default(),
                }
            };
            if claim_metadata(txn, &key, source)? {
                let layout = ChannelLayout {
                    kind: existing.kind,
                    parent: *parent,
                };
                let mut table = txn.open_table(CHANNEL_LAYOUT)?;
                let value = postcard::to_allocvec(&layout)?;
                table.insert(
                    channel_key(body.guild_id, *target).as_slice(),
                    value.as_slice(),
                )?;
            }
        }
        EventKind::MemberJoined { display_name } | EventKind::MemberRenamed { display_name } => {
            if claim_metadata(txn, &member_meta_key, source)? {
                let existing = {
                    let table = txn.open_table(MEMBERS)?;
                    match table.get(member_key(body.guild_id, body.author).as_slice())? {
                        Some(value) => Some(postcard::from_bytes::<MemberInfo>(value.value())?),
                        None => None,
                    }
                };
                let member = MemberInfo {
                    guild_id: body.guild_id,
                    author: body.author,
                    display_name: display_name.trim().to_string(),
                    // A rename keeps the original join time.
                    joined_at: existing
                        .map(|member| member.joined_at)
                        .unwrap_or(body.timestamp),
                };
                let mut table = txn.open_table(MEMBERS)?;
                let value = postcard::to_allocvec(&member)?;
                table.insert(
                    member_key(body.guild_id, body.author).as_slice(),
                    value.as_slice(),
                )?;
            }
        }
        EventKind::MemberLeft => {
            let source = EventSource {
                removed: true,
                ..source
            };
            if claim_metadata(txn, &member_meta_key, source)? {
                let mut table = txn.open_table(MEMBERS)?;
                table.remove(member_key(body.guild_id, body.author).as_slice())?;
            }
        }
        // Messages and their modifications carry no metadata of their own: the
        // event index is enough, and edits/deletes are folded at render time.
        // An unknown kind is indexed without being applied.
        EventKind::Message { .. }
        | EventKind::MessageV2 { .. }
        | EventKind::MessageV3 { .. }
        | EventKind::MessageEdited { .. }
        | EventKind::MessageDeleted { .. }
        | EventKind::ReactionAdded { .. }
        | EventKind::ReactionRemoved { .. }
        | EventKind::MemberProfileUpdated { .. }
        | EventKind::DirectMessageOpened { .. }
        | EventKind::Unknown { .. } => {}
        EventKind::ReadUpTo { event_id } => {
            if let Some(channel_id) = body.channel_id {
                let cursor = SyncCursor {
                    lamport: body.lamport,
                    event_id: Some(*event_id),
                };
                let key = read_key(body.guild_id, channel_id, body.author);
                let mut table = txn.open_table(READ_STATE)?;
                let existing = match table.get(key.as_slice())? {
                    Some(value) => postcard::from_bytes::<SyncCursor>(value.value())?,
                    None => SyncCursor::start(),
                };
                if existing.is_before(cursor.lamport, *event_id) {
                    let value = postcard::to_allocvec(&cursor)?;
                    table.insert(key.as_slice(), value.as_slice())?;
                }
            }
        }
    }

    Ok(())
}

fn log_for<'a>(
    root: &Path,
    logs: &'a mut HashMap<GuildId, AppendLog>,
    guild_id: GuildId,
) -> Result<&'a mut AppendLog> {
    if let std::collections::hash_map::Entry::Vacant(entry) = logs.entry(guild_id) {
        let dir = guild_dir(root, guild_id);
        create_dir_private(&dir)?;
        entry.insert(AppendLog::open(&dir)?);
    }
    Ok(logs
        .get_mut(&guild_id)
        .ok_or_else(|| CoreError::InvalidInput("failed to open event log".to_string()))?)
}

fn guild_dir(root: &Path, guild_id: GuildId) -> PathBuf {
    root.join("guilds").join(guild_id.to_hex())
}

fn author_key(guild_id: GuildId, author: AuthorId) -> Vec<u8> {
    let mut key = Vec::with_capacity(64);
    key.extend_from_slice(guild_id.as_bytes());
    key.extend_from_slice(author.as_bytes());
    key
}

fn member_key(guild_id: GuildId, author: AuthorId) -> Vec<u8> {
    author_key(guild_id, author)
}

fn read_key(guild_id: GuildId, channel_id: ChannelId, author: AuthorId) -> Vec<u8> {
    let mut key = Vec::with_capacity(80);
    key.extend_from_slice(guild_id.as_bytes());
    key.extend_from_slice(channel_id.as_bytes());
    key.extend_from_slice(author.as_bytes());
    key
}

fn channel_key(guild_id: GuildId, channel_id: ChannelId) -> Vec<u8> {
    let mut key = Vec::with_capacity(48);
    key.extend_from_slice(guild_id.as_bytes());
    key.extend_from_slice(channel_id.as_bytes());
    key
}

fn channel_prefix(guild_id: GuildId, channel_id: ChannelId) -> Vec<u8> {
    channel_key(guild_id, channel_id)
}

fn guild_event_key(guild_id: GuildId, lamport: u64, event_id: EventId) -> Vec<u8> {
    let mut key = Vec::with_capacity(72);
    key.extend_from_slice(guild_id.as_bytes());
    key.extend_from_slice(&lamport.to_be_bytes());
    key.extend_from_slice(event_id.as_bytes());
    key
}

fn channel_event_key(
    guild_id: GuildId,
    channel_id: ChannelId,
    lamport: u64,
    event_id: EventId,
) -> Vec<u8> {
    let mut key = Vec::with_capacity(88);
    key.extend_from_slice(guild_id.as_bytes());
    key.extend_from_slice(channel_id.as_bytes());
    key.extend_from_slice(&lamport.to_be_bytes());
    key.extend_from_slice(event_id.as_bytes());
    key
}

fn random_guild_id() -> GuildId {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    GuildId(bytes)
}

fn random_channel_id() -> ChannelId {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    ChannelId(bytes)
}

fn validate_name<'a>(name: &'a str, kind: &str) -> Result<&'a str> {
    // Check the raw value, not the trimmed one. Trimming a multi-megabyte
    // string down to 100 bytes and then storing the original would let a peer
    // import an event whose name fills the index and the screen.
    if name.len() > MAX_NAME_BYTES {
        return Err(
            CoreError::InvalidInput(format!("{kind} exceeds {MAX_NAME_BYTES} bytes")).into(),
        );
    }
    if contains_unsafe_display_chars(name) {
        return Err(CoreError::InvalidInput(format!(
            "{kind} contains control or bidi-format characters"
        ))
        .into());
    }
    let name = name.trim();
    if name.is_empty() {
        return Err(CoreError::InvalidInput(format!("{kind} cannot be empty")).into());
    }
    Ok(name)
}

impl Storage for Store {
    fn create_guild(&mut self, name: &str, identity: &Identity) -> CoreResult<CreatedGuild> {
        self.create_guild_inner(name, identity).map_err(Into::into)
    }

    fn create_guild_with_id(
        &mut self,
        guild_id: GuildId,
        name: &str,
        display_name: &str,
        identity: &Identity,
    ) -> CoreResult<CreatedGuild> {
        self.create_guild_with_id_inner(guild_id, name, display_name, identity)
            .map_err(Into::into)
    }

    fn create_channel(
        &mut self,
        guild_id: GuildId,
        name: &str,
        identity: &Identity,
    ) -> CoreResult<ChannelMeta> {
        self.create_channel_inner(guild_id, name, identity)
            .map(|(channel, _)| channel)
            .map_err(Into::into)
    }

    fn create_channel_with_event(
        &mut self,
        guild_id: GuildId,
        name: &str,
        identity: &Identity,
    ) -> CoreResult<(ChannelMeta, Option<SignedEvent>)> {
        self.create_channel_inner(guild_id, name, identity)
            .map(|(channel, event)| (channel, Some(event)))
            .map_err(Into::into)
    }

    fn create_channel_in(
        &mut self,
        guild_id: GuildId,
        name: &str,
        kind: ChannelKind,
        parent: Option<ChannelId>,
        identity: &Identity,
    ) -> CoreResult<(ChannelMeta, Option<SignedEvent>)> {
        self.create_channel_in_inner(guild_id, name, kind, parent, identity)
            .map(|(channel, event)| (channel, Some(event)))
            .map_err(Into::into)
    }

    fn rename_channel(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        name: &str,
        identity: &Identity,
    ) -> CoreResult<Option<SignedEvent>> {
        self.rename_channel_inner(guild_id, channel_id, name, identity)
            .map(Some)
            .map_err(Into::into)
    }

    fn move_channel(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        parent: Option<ChannelId>,
        identity: &Identity,
    ) -> CoreResult<Option<SignedEvent>> {
        self.move_channel_inner(guild_id, channel_id, parent, identity)
            .map(Some)
            .map_err(Into::into)
    }

    fn create_category(
        &mut self,
        guild_id: GuildId,
        name: &str,
        identity: &Identity,
    ) -> CoreResult<(CategoryMeta, Option<SignedEvent>)> {
        self.create_category_inner(guild_id, name, identity)
            .map(|(category, event)| (category, Some(event)))
            .map_err(Into::into)
    }

    fn list_categories(&self, guild_id: GuildId) -> CoreResult<Vec<CategoryMeta>> {
        self.list_categories_inner(guild_id).map_err(Into::into)
    }

    fn list_channel_views(&self, guild_id: GuildId) -> CoreResult<Vec<ChannelView>> {
        self.list_channel_views_inner(guild_id).map_err(Into::into)
    }

    fn post_message(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        identity: &Identity,
    ) -> CoreResult<SignedEvent> {
        self.post_message_inner(guild_id, channel_id, body, identity)
            .map_err(Into::into)
    }

    fn post_message_rich(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
        attachments: &[Attachment],
        identity: &Identity,
    ) -> CoreResult<SignedEvent> {
        self.post_message_rich_inner(
            guild_id,
            channel_id,
            body,
            reply_to,
            mentions,
            attachments,
            identity,
        )
        .map_err(Into::into)
    }

    fn edit_message(
        &mut self,
        guild_id: GuildId,
        target: EventId,
        body: &str,
        identity: &Identity,
    ) -> CoreResult<SignedEvent> {
        self.edit_message_inner(guild_id, target, body, identity)
            .map_err(Into::into)
    }

    fn delete_message(
        &mut self,
        guild_id: GuildId,
        target: EventId,
        identity: &Identity,
    ) -> CoreResult<SignedEvent> {
        self.delete_message_inner(guild_id, target, identity)
            .map_err(Into::into)
    }

    fn add_reaction(
        &mut self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
        identity: &Identity,
    ) -> CoreResult<SignedEvent> {
        self.add_reaction_inner(guild_id, target, emoji, identity)
            .map_err(Into::into)
    }

    fn remove_reaction(
        &mut self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
        identity: &Identity,
    ) -> CoreResult<SignedEvent> {
        self.remove_reaction_inner(guild_id, target, emoji, identity)
            .map_err(Into::into)
    }

    fn mark_read(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        event_id: EventId,
        identity: &Identity,
    ) -> CoreResult<SignedEvent> {
        self.mark_read_inner(guild_id, channel_id, event_id, identity)
            .map_err(Into::into)
    }

    fn unread_counts(
        &self,
        guild_id: GuildId,
        author: AuthorId,
    ) -> CoreResult<HashMap<ChannelId, usize>> {
        self.unread_counts_inner(guild_id, author)
            .map_err(Into::into)
    }

    fn import_event(&mut self, event: SignedEvent) -> CoreResult<bool> {
        self.import_event_inner(event).map_err(Into::into)
    }

    fn has_event(&self, event_id: EventId) -> CoreResult<bool> {
        self.has_event_inner(event_id).map_err(Into::into)
    }

    fn events_after(
        &self,
        guild_id: GuildId,
        cursor: SyncCursor,
        limit: usize,
        max_bytes: usize,
    ) -> CoreResult<EventPage> {
        self.events_after_inner(guild_id, cursor, limit, max_bytes)
            .map_err(Into::into)
    }

    fn guild_lamport(&self, guild_id: GuildId) -> CoreResult<u64> {
        self.guild_lamport_inner(guild_id).map_err(Into::into)
    }

    fn add_member(
        &mut self,
        guild_id: GuildId,
        author: AuthorId,
        display_name: &str,
    ) -> CoreResult<()> {
        self.add_member_inner(guild_id, author, display_name)
            .map_err(Into::into)
    }

    fn list_guilds(&self) -> CoreResult<Vec<GuildMeta>> {
        self.list_guilds_inner().map_err(Into::into)
    }

    fn get_guild(&self, guild_id: GuildId) -> CoreResult<Option<GuildMeta>> {
        self.get_guild_inner(guild_id).map_err(Into::into)
    }

    fn list_channels(&self, guild_id: GuildId) -> CoreResult<Vec<ChannelMeta>> {
        self.list_channels_inner(guild_id).map_err(Into::into)
    }

    fn get_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> CoreResult<Option<ChannelMeta>> {
        self.get_channel_inner(guild_id, channel_id)
            .map_err(Into::into)
    }

    fn list_members(&self, guild_id: GuildId) -> CoreResult<Vec<MemberInfo>> {
        self.list_members_inner(guild_id).map_err(Into::into)
    }

    fn append_member_joined(
        &mut self,
        guild_id: GuildId,
        display_name: &str,
        identity: &Identity,
    ) -> CoreResult<Option<SignedEvent>> {
        self.append_member_joined_inner(guild_id, display_name, identity)
            .map_err(Into::into)
    }

    fn set_profile(
        &mut self,
        guild_id: GuildId,
        identity: &Identity,
        about: &str,
        pronouns: &str,
        links: &[String],
    ) -> CoreResult<SignedEvent> {
        self.set_profile_inner(guild_id, about, pronouns, links, identity)
            .map_err(Into::into)
    }

    fn member_profile(
        &self,
        guild_id: GuildId,
        author: AuthorId,
    ) -> CoreResult<MemberProfile> {
        self.member_profile_inner(guild_id, author)
            .map_err(Into::into)
    }

    fn announce_direct_message(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        peer: AuthorId,
        dm: GuildId,
        identity: &Identity,
    ) -> CoreResult<SignedEvent> {
        self.announce_direct_message_inner(guild_id, channel_id, peer, dm, identity)
            .map_err(Into::into)
    }

    fn set_display_name(
        &mut self,
        guild_id: GuildId,
        display_name: &str,
        identity: &Identity,
    ) -> CoreResult<Option<SignedEvent>> {
        self.set_display_name_inner(guild_id, display_name, identity)
            .map_err(Into::into)
    }

    fn leave_guild(
        &mut self,
        guild_id: GuildId,
        identity: &Identity,
    ) -> CoreResult<Option<SignedEvent>> {
        self.leave_guild_inner(guild_id, identity)
            .map_err(Into::into)
    }

    fn history(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        limit: usize,
    ) -> CoreResult<Vec<SignedEvent>> {
        self.history_inner(guild_id, channel_id, limit)
            .map_err(Into::into)
    }

    fn history_before(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        cursor: Option<SyncCursor>,
        limit: usize,
    ) -> CoreResult<EventPage> {
        self.history_before_inner(guild_id, channel_id, cursor, limit)
            .map_err(Into::into)
    }

    fn get_event(&self, guild_id: GuildId, event_id: EventId) -> CoreResult<Option<SignedEvent>> {
        self.get_event_inner(guild_id, event_id).map_err(Into::into)
    }

    fn put_blob(&mut self, id: AttachmentId, bytes: &[u8]) -> CoreResult<()> {
        self.put_blob_inner(id, bytes).map_err(Into::into)
    }

    fn get_blob(&self, id: AttachmentId) -> CoreResult<Option<Vec<u8>>> {
        self.get_blob_inner(id).map_err(Into::into)
    }
}
