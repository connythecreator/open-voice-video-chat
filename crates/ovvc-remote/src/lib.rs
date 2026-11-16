//! A daemon-attached frontend client.
//!
//! [`RemoteClient`] mirrors the local [`ovvc_client::Client`] surface, but
//! every call becomes an [`ovvc_ipc::Op`] on the Unix socket. The daemon stays
//! the only process that opens redb, so several frontends (CLI, TUI, GUI) can
//! share one data directory without fighting over the store lock.
//!
//! [`Frontend`] is the small trait the terminal frontend programs against. The
//! direct [`ovvc_client::Client`] implements it too, so the TUI's widget tests
//! can keep using an in-memory store while the real binary talks to the daemon.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use ovvc_client::{
    Attachment, AttachmentId, AuthorId, CategoryMeta, ChannelId, ChannelKind, ChannelMeta,
    ChannelView, Client, ClientError, ClientEvent, DirectConversation, EventId, EventPage, GuildId,
    GuildMeta, Invite, MemberInfo, MemberProfile, Multiaddr, NetworkOptions, PeerId, QuietHours,
    SignedEvent, SyncCursor,
};
use ovvc_control::{PresenceStatus, SessionId, VoiceMemberState, VoiceSnapshot};
use ovvc_core::CoreError;
use ovvc_ipc::{
    CAP_CAPABILITIES, DaemonCapabilities, DaemonClient, DaemonEvent, DaemonStatusDto, ErrorKind,
    IPC_PROTOCOL, NotificationSettings, Op, PolicyView, Reply, VoiceAudioStats, default_socket_path,
    protocol_major, read_daemon_info,
};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

pub type ClientResult<T> = std::result::Result<T, ClientError>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The slice of the client surface the terminal frontend uses.
///
/// Calls are synchronous because the TUI's input and draw code is synchronous;
/// the daemon-backed implementation runs each request on a private runtime.
pub trait Frontend: Send + Sync + 'static {
    fn author_id(&self) -> AuthorId;
    /// The daemon's version and process details, for the status line.
    fn daemon_status(&self) -> ClientResult<DaemonStatusDto>;
    fn list_guilds(&self) -> ClientResult<Vec<GuildMeta>>;
    fn find_guild(&self, needle: &str) -> ClientResult<GuildMeta>;
    fn create_guild(&self, name: &str) -> ClientResult<GuildMeta>;
    fn create_channel(&self, guild_id: GuildId, name: &str) -> ClientResult<ChannelMeta>;
    /// Create a channel with a kind and an optional parent category.
    fn create_channel_in(
        &self,
        guild_id: GuildId,
        name: &str,
        kind: ChannelKind,
        parent: Option<ChannelId>,
    ) -> ClientResult<ChannelMeta>;
    fn create_category(&self, guild_id: GuildId, name: &str) -> ClientResult<CategoryMeta>;
    fn rename_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        name: &str,
    ) -> ClientResult<()>;
    fn move_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        parent: Option<ChannelId>,
    ) -> ClientResult<()>;
    fn list_categories(&self, guild_id: GuildId) -> ClientResult<Vec<CategoryMeta>>;
    /// Channels with their kind and category resolved, in sidebar order.
    fn list_channel_views(&self, guild_id: GuildId) -> ClientResult<Vec<ChannelView>>;
    /// Full-text search across the guild's messages.
    fn search(
        &self,
        guild_id: GuildId,
        query: &str,
        limit: usize,
    ) -> ClientResult<Vec<ovvc_ipc::SearchHit>>;
    /// Join the guild's control session, which puts this identity in the
    /// roster with a display name and starts the presence lease.
    fn control_join(
        &self,
        guild_id: GuildId,
        session_id: SessionId,
        display_name: &str,
    ) -> ClientResult<()>;
    /// Publish this identity's presence to the guild's control session.
    fn set_presence(&self, guild_id: GuildId, status: PresenceStatus) -> ClientResult<()>;
    /// Join the voice call on `channel` and return the call after the join.
    fn voice_join(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        muted: bool,
        deafened: bool,
        push_to_talk: bool,
    ) -> ClientResult<VoiceSnapshot>;
    /// Leave a voice call, returning the call after the departure (`None` when
    /// the person who left was the last member).
    fn voice_leave(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<VoiceSnapshot>>;
    /// Publish this identity's audio/video flags in a call.
    fn set_voice_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        state: VoiceMemberState,
    ) -> ClientResult<VoiceSnapshot>;
    /// Tell the local audio service whether the push-to-talk key is held.
    fn set_push_to_talk_held(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        held: bool,
    ) -> ClientResult<()>;
    /// The current call, or `None` when nobody is in the channel.
    fn voice_snapshot(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<VoiceSnapshot>>;
    /// The local audio engine's counters for this daemon's call on `channel`,
    /// or `None` when no audio service is running for it. Only the daemon can
    /// answer this, because only it owns the device.
    fn voice_audio(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<VoiceAudioStats>>;
    /// The guild policy read model: what the guild published, this client's
    /// floor, the effective axes and which axes the floor raised.
    ///
    /// This is a read model only. Policy enforcement, signed policy events and
    /// media keys are not implemented yet.
    fn policy(&self, guild_id: GuildId) -> ClientResult<PolicyView>;
    fn notification_settings(&self) -> ClientResult<NotificationSettings>;
    /// Change one preference through the daemon, so another frontend's save
    /// cannot replace the rest of the file with a stale copy.
    fn set_desktop_notifications(&self, enabled: bool) -> ClientResult<()>;
    fn set_do_not_disturb(&self, enabled: bool) -> ClientResult<()>;
    fn set_quiet_hours(&self, quiet_hours: Option<QuietHours>) -> ClientResult<()>;
    fn set_channel_muted(&self, channel_id: ChannelId, muted: bool) -> ClientResult<()>;
    fn set_guild_muted(&self, guild_id: GuildId, muted: bool) -> ClientResult<()>;
    fn list_channels(&self, guild_id: GuildId) -> ClientResult<Vec<ChannelMeta>>;
    fn find_channel(&self, guild_id: GuildId, needle: &str) -> ClientResult<ChannelMeta>;
    fn default_channel(&self, guild_id: GuildId) -> ClientResult<ChannelMeta>;
    fn list_members(&self, guild_id: GuildId) -> ClientResult<Vec<MemberInfo>>;
    /// One channel by id, or `None` when it does not exist in the guild.
    fn get_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<ChannelMeta>>;
    /// One roster member by author id, or `None` when they never joined.
    fn get_member(&self, guild_id: GuildId, author: AuthorId) -> ClientResult<Option<MemberInfo>>;
    fn set_display_name(&self, guild_id: GuildId, display_name: &str) -> ClientResult<()>;
    /// Publish this identity's profile fields and return the signed event.
    fn set_profile(
        &self,
        guild_id: GuildId,
        about: &str,
        pronouns: &str,
        links: &[String],
    ) -> ClientResult<SignedEvent>;
    /// One member's profile plus the counts derived from the guild log.
    fn member_profile(&self, guild_id: GuildId, author: AuthorId) -> ClientResult<MemberProfile>;
    /// The other author when `guild_id` is a direct conversation, else `None`.
    fn direct_peer(&self, guild_id: GuildId) -> ClientResult<Option<AuthorId>>;
    /// Every direct conversation visible in the local store, newest first.
    fn list_direct_messages(&self) -> ClientResult<Vec<DirectConversation>>;
    /// Open or reopen the direct conversation with `peer` from `context`.
    fn open_direct_message(&self, context: GuildId, peer: AuthorId) -> ClientResult<GuildMeta>;
    fn history(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        limit: usize,
    ) -> ClientResult<Vec<SignedEvent>>;
    /// Older events of a channel, walking backwards from `cursor`.
    fn history_before(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        cursor: Option<SyncCursor>,
        limit: usize,
    ) -> ClientResult<EventPage>;
    /// Record that this identity has read `channel_id` up to `event_id`.
    fn mark_read(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        event_id: EventId,
    ) -> ClientResult<()>;
    /// Unread message counts per channel, from the signed read markers.
    fn unread_counts(&self, guild_id: GuildId) -> ClientResult<HashMap<ChannelId, usize>>;
    fn post_message_full(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
    ) -> ClientResult<SignedEvent>;
    /// Post a message with files attached. Every attachment must already be
    /// in the store, which [`Self::put_blob`] does.
    fn post_message_rich(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
        attachments: &[Attachment],
    ) -> ClientResult<SignedEvent>;
    /// Replace the body of a message this identity wrote.
    fn edit_message(
        &self,
        guild_id: GuildId,
        target: EventId,
        body: &str,
    ) -> ClientResult<SignedEvent>;
    /// Tombstone a message this identity wrote.
    fn delete_message(&self, guild_id: GuildId, target: EventId) -> ClientResult<SignedEvent>;
    /// Add this identity's reaction to a message.
    fn add_reaction(
        &self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
    ) -> ClientResult<SignedEvent>;
    /// Remove this identity's own reaction from a message.
    fn remove_reaction(
        &self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
    ) -> ClientResult<SignedEvent>;
    /// Store bytes under the id their hash names and return that id.
    fn put_blob(&self, bytes: &[u8]) -> ClientResult<AttachmentId>;
    /// Read a local blob, or `None` when it has not been downloaded.
    fn get_blob(&self, id: AttachmentId) -> ClientResult<Option<Vec<u8>>>;
    /// Whether a blob is already local.
    fn has_blob(&self, id: AttachmentId) -> ClientResult<bool>;
    /// Fetch a blob from the guild's peers, or return the local copy.
    ///
    /// This blocks for the length of the download. A terminal frontend that
    /// calls it should say so on its status line first, because the loop cannot
    /// redraw until the bytes are here.
    fn fetch_blob(&self, guild_id: GuildId, id: AttachmentId) -> ClientResult<Vec<u8>>;
    fn create_invite(
        &self,
        guild_id: GuildId,
        peers: Vec<String>,
        expires_in_secs: Option<u64>,
    ) -> ClientResult<Invite>;
    fn peer_id(&self, guild_id: GuildId) -> Option<PeerId>;
    fn subscribe(&self) -> broadcast::Receiver<ClientEvent>;
    fn connect<'a>(
        &'a self,
        guild_id: GuildId,
        options: NetworkOptions,
    ) -> BoxFuture<'a, ClientResult<()>>;
    fn join_guild<'a>(
        &'a self,
        invite: &'a Invite,
        options: NetworkOptions,
    ) -> BoxFuture<'a, ClientResult<GuildMeta>>;
    fn disconnect(&self, guild_id: GuildId);
}

impl Frontend for RemoteClient {
    fn author_id(&self) -> AuthorId {
        RemoteClient::author_id(self)
    }

    fn daemon_status(&self) -> ClientResult<DaemonStatusDto> {
        RemoteClient::daemon_status(self)
    }

    fn list_guilds(&self) -> ClientResult<Vec<GuildMeta>> {
        RemoteClient::list_guilds(self)
    }

    fn find_guild(&self, needle: &str) -> ClientResult<GuildMeta> {
        RemoteClient::find_guild(self, needle)
    }

    fn create_guild(&self, name: &str) -> ClientResult<GuildMeta> {
        RemoteClient::create_guild(self, name)
    }

    fn create_channel(&self, guild_id: GuildId, name: &str) -> ClientResult<ChannelMeta> {
        RemoteClient::create_channel(self, guild_id, name)
    }

    fn create_channel_in(
        &self,
        guild_id: GuildId,
        name: &str,
        kind: ChannelKind,
        parent: Option<ChannelId>,
    ) -> ClientResult<ChannelMeta> {
        RemoteClient::create_channel_in(self, guild_id, name, kind, parent)
    }

    fn create_category(&self, guild_id: GuildId, name: &str) -> ClientResult<CategoryMeta> {
        RemoteClient::create_category(self, guild_id, name)
    }

    fn rename_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        name: &str,
    ) -> ClientResult<()> {
        RemoteClient::rename_channel(self, guild_id, channel_id, name)
    }

    fn move_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        parent: Option<ChannelId>,
    ) -> ClientResult<()> {
        RemoteClient::move_channel(self, guild_id, channel_id, parent)
    }

    fn list_categories(&self, guild_id: GuildId) -> ClientResult<Vec<CategoryMeta>> {
        RemoteClient::list_categories(self, guild_id)
    }

    fn list_channel_views(&self, guild_id: GuildId) -> ClientResult<Vec<ChannelView>> {
        RemoteClient::list_channel_views(self, guild_id)
    }

    fn search(
        &self,
        guild_id: GuildId,
        query: &str,
        limit: usize,
    ) -> ClientResult<Vec<ovvc_ipc::SearchHit>> {
        RemoteClient::search(self, guild_id, query, limit)
    }

    fn control_join(
        &self,
        guild_id: GuildId,
        session_id: SessionId,
        display_name: &str,
    ) -> ClientResult<()> {
        RemoteClient::control_join(self, guild_id, session_id, display_name)
    }

    fn set_presence(&self, guild_id: GuildId, status: PresenceStatus) -> ClientResult<()> {
        RemoteClient::set_presence(self, guild_id, status)
    }

    fn voice_join(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        muted: bool,
        deafened: bool,
        push_to_talk: bool,
    ) -> ClientResult<VoiceSnapshot> {
        RemoteClient::voice_join(self, guild_id, channel_id, muted, deafened, push_to_talk)
    }

    fn voice_leave(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<VoiceSnapshot>> {
        RemoteClient::voice_leave(self, guild_id, channel_id)
    }

    fn set_voice_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        state: VoiceMemberState,
    ) -> ClientResult<VoiceSnapshot> {
        RemoteClient::set_voice_state(self, guild_id, channel_id, state)
    }

    fn set_push_to_talk_held(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        held: bool,
    ) -> ClientResult<()> {
        RemoteClient::set_push_to_talk_held(self, guild_id, channel_id, held)
    }

    fn voice_snapshot(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<VoiceSnapshot>> {
        RemoteClient::voice_snapshot(self, guild_id, channel_id)
    }

    fn voice_audio(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<VoiceAudioStats>> {
        RemoteClient::voice_audio(self, guild_id, channel_id)
    }

    fn policy(&self, guild_id: GuildId) -> ClientResult<PolicyView> {
        RemoteClient::policy(self, guild_id)
    }

    fn notification_settings(&self) -> ClientResult<NotificationSettings> {
        RemoteClient::notification_settings(self)
    }

    fn set_desktop_notifications(&self, enabled: bool) -> ClientResult<()> {
        RemoteClient::set_desktop_notifications(self, enabled)
    }

    fn set_do_not_disturb(&self, enabled: bool) -> ClientResult<()> {
        RemoteClient::set_do_not_disturb(self, enabled)
    }

    fn set_quiet_hours(&self, quiet_hours: Option<QuietHours>) -> ClientResult<()> {
        RemoteClient::set_quiet_hours(self, quiet_hours)
    }

    fn set_channel_muted(&self, channel_id: ChannelId, muted: bool) -> ClientResult<()> {
        RemoteClient::set_channel_muted(self, channel_id, muted)
    }

    fn set_guild_muted(&self, guild_id: GuildId, muted: bool) -> ClientResult<()> {
        RemoteClient::set_guild_muted(self, guild_id, muted)
    }

    fn list_channels(&self, guild_id: GuildId) -> ClientResult<Vec<ChannelMeta>> {
        RemoteClient::list_channels(self, guild_id)
    }

    fn find_channel(&self, guild_id: GuildId, needle: &str) -> ClientResult<ChannelMeta> {
        RemoteClient::find_channel(self, guild_id, needle)
    }

    fn default_channel(&self, guild_id: GuildId) -> ClientResult<ChannelMeta> {
        RemoteClient::default_channel(self, guild_id)
    }

    fn list_members(&self, guild_id: GuildId) -> ClientResult<Vec<MemberInfo>> {
        RemoteClient::list_members(self, guild_id)
    }

    fn get_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<ChannelMeta>> {
        RemoteClient::get_channel(self, guild_id, channel_id)
    }

    fn get_member(&self, guild_id: GuildId, author: AuthorId) -> ClientResult<Option<MemberInfo>> {
        RemoteClient::get_member(self, guild_id, author)
    }

    fn set_display_name(&self, guild_id: GuildId, display_name: &str) -> ClientResult<()> {
        RemoteClient::set_display_name(self, guild_id, display_name)
    }

    fn set_profile(
        &self,
        guild_id: GuildId,
        about: &str,
        pronouns: &str,
        links: &[String],
    ) -> ClientResult<SignedEvent> {
        RemoteClient::set_profile(self, guild_id, about, pronouns, links)
    }

    fn member_profile(&self, guild_id: GuildId, author: AuthorId) -> ClientResult<MemberProfile> {
        RemoteClient::member_profile(self, guild_id, author)
    }

    fn direct_peer(&self, guild_id: GuildId) -> ClientResult<Option<AuthorId>> {
        RemoteClient::direct_peer(self, guild_id)
    }

    fn list_direct_messages(&self) -> ClientResult<Vec<DirectConversation>> {
        RemoteClient::list_direct_messages(self)
    }

    fn open_direct_message(&self, context: GuildId, peer: AuthorId) -> ClientResult<GuildMeta> {
        RemoteClient::open_direct_message(self, context, peer)
    }

    fn history(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        limit: usize,
    ) -> ClientResult<Vec<SignedEvent>> {
        RemoteClient::history(self, guild_id, channel_id, limit)
    }

    fn history_before(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        cursor: Option<SyncCursor>,
        limit: usize,
    ) -> ClientResult<EventPage> {
        RemoteClient::history_before(self, guild_id, channel_id, cursor, limit)
    }

    fn mark_read(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        event_id: EventId,
    ) -> ClientResult<()> {
        RemoteClient::mark_read(self, guild_id, channel_id, event_id)
    }

    fn unread_counts(&self, guild_id: GuildId) -> ClientResult<HashMap<ChannelId, usize>> {
        RemoteClient::unread_counts(self, guild_id)
    }

    fn post_message_full(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
    ) -> ClientResult<SignedEvent> {
        RemoteClient::post_message_full(self, guild_id, channel_id, body, reply_to, mentions)
    }

    fn post_message_rich(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
        attachments: &[Attachment],
    ) -> ClientResult<SignedEvent> {
        RemoteClient::post_message_rich(
            self,
            guild_id,
            channel_id,
            body,
            reply_to,
            mentions,
            attachments,
        )
    }

    fn edit_message(
        &self,
        guild_id: GuildId,
        target: EventId,
        body: &str,
    ) -> ClientResult<SignedEvent> {
        RemoteClient::edit_message(self, guild_id, target, body)
    }

    fn delete_message(&self, guild_id: GuildId, target: EventId) -> ClientResult<SignedEvent> {
        RemoteClient::delete_message(self, guild_id, target)
    }

    fn add_reaction(
        &self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
    ) -> ClientResult<SignedEvent> {
        RemoteClient::add_reaction(self, guild_id, target, emoji)
    }

    fn remove_reaction(
        &self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
    ) -> ClientResult<SignedEvent> {
        RemoteClient::remove_reaction(self, guild_id, target, emoji)
    }

    fn put_blob(&self, bytes: &[u8]) -> ClientResult<AttachmentId> {
        RemoteClient::put_blob(self, bytes)
    }

    fn get_blob(&self, id: AttachmentId) -> ClientResult<Option<Vec<u8>>> {
        RemoteClient::get_blob(self, id)
    }

    fn has_blob(&self, id: AttachmentId) -> ClientResult<bool> {
        RemoteClient::has_blob(self, id)
    }

    fn fetch_blob(&self, guild_id: GuildId, id: AttachmentId) -> ClientResult<Vec<u8>> {
        RemoteClient::fetch_blob(self, guild_id, id)
    }

    fn create_invite(
        &self,
        guild_id: GuildId,
        peers: Vec<String>,
        expires_in_secs: Option<u64>,
    ) -> ClientResult<Invite> {
        RemoteClient::create_invite(self, guild_id, peers, expires_in_secs)
    }

    fn peer_id(&self, guild_id: GuildId) -> Option<PeerId> {
        RemoteClient::peer_id(self, guild_id)
    }

    fn subscribe(&self) -> broadcast::Receiver<ClientEvent> {
        RemoteClient::subscribe(self)
    }

    fn connect<'a>(
        &'a self,
        guild_id: GuildId,
        options: NetworkOptions,
    ) -> BoxFuture<'a, ClientResult<()>> {
        self.connect_async(guild_id, options)
    }

    fn join_guild<'a>(
        &'a self,
        invite: &'a Invite,
        options: NetworkOptions,
    ) -> BoxFuture<'a, ClientResult<GuildMeta>> {
        self.join_guild_async(invite, options)
    }

    fn disconnect(&self, guild_id: GuildId) {
        RemoteClient::disconnect(self, guild_id);
    }
}

impl Frontend for Client {
    fn author_id(&self) -> AuthorId {
        Client::author_id(self)
    }

    fn daemon_status(&self) -> ClientResult<DaemonStatusDto> {
        // The in-process client has no daemon, but frontends still want a
        // version to show. Report this process's facade version.
        Ok(DaemonStatusDto {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            data_dir: String::new(),
            socket: String::new(),
            started_at: 0,
            uptime_secs: 0,
            author_id: Client::author_id(self),
            connected_guilds: Vec::new(),
            // An in-process client has no daemon log; the daemon-backed
            // `Status` reply carries the real path.
            log_path: String::new(),
        })
    }

    fn list_guilds(&self) -> ClientResult<Vec<GuildMeta>> {
        Client::list_guilds(self)
    }

    fn find_guild(&self, needle: &str) -> ClientResult<GuildMeta> {
        Client::find_guild(self, needle)
    }

    fn create_guild(&self, name: &str) -> ClientResult<GuildMeta> {
        Client::create_guild(self, name)
    }

    fn create_channel(&self, guild_id: GuildId, name: &str) -> ClientResult<ChannelMeta> {
        Client::create_channel(self, guild_id, name)
    }

    fn create_channel_in(
        &self,
        guild_id: GuildId,
        name: &str,
        kind: ChannelKind,
        parent: Option<ChannelId>,
    ) -> ClientResult<ChannelMeta> {
        Client::create_channel_in(self, guild_id, name, kind, parent)
    }

    fn create_category(&self, guild_id: GuildId, name: &str) -> ClientResult<CategoryMeta> {
        Client::create_category(self, guild_id, name)
    }

    fn rename_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        name: &str,
    ) -> ClientResult<()> {
        Client::rename_channel(self, guild_id, channel_id, name)
    }

    fn move_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        parent: Option<ChannelId>,
    ) -> ClientResult<()> {
        Client::move_channel(self, guild_id, channel_id, parent)
    }

    fn list_categories(&self, guild_id: GuildId) -> ClientResult<Vec<CategoryMeta>> {
        Client::list_categories(self, guild_id)
    }

    fn list_channel_views(&self, guild_id: GuildId) -> ClientResult<Vec<ChannelView>> {
        Client::list_channel_views(self, guild_id)
    }

    fn search(
        &self,
        guild_id: GuildId,
        query: &str,
        limit: usize,
    ) -> ClientResult<Vec<ovvc_ipc::SearchHit>> {
        Ok(Client::search(self, guild_id, query, limit)?
            .into_iter()
            .filter_map(|hit| {
                let channel_id = hit.event.body.channel_id?;
                Some(ovvc_ipc::SearchHit {
                    guild_id,
                    channel_id,
                    event: hit.event,
                    body: hit.body,
                })
            })
            .collect())
    }

    fn control_join(
        &self,
        guild_id: GuildId,
        session_id: SessionId,
        display_name: &str,
    ) -> ClientResult<()> {
        // The in-process client has no control session: presence is owned by
        // the daemon's `ControlHub`, and pretending to publish it here would
        // report a state no peer ever receives.
        let _ = (guild_id, session_id, display_name);
        Err(ClientError::Network(anyhow!(
            "control sessions need a daemon; attach through ovvc-remote"
        )))
    }

    fn set_presence(&self, guild_id: GuildId, status: PresenceStatus) -> ClientResult<()> {
        // The in-process client has no control session: presence is owned by
        // the daemon's `ControlHub`, and pretending to publish it here would
        // report a state no peer ever receives.
        let _ = (guild_id, status);
        Err(ClientError::Network(anyhow!(
            "presence needs a daemon control session; attach through ovvc-remote"
        )))
    }

    fn voice_join(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        muted: bool,
        deafened: bool,
        push_to_talk: bool,
    ) -> ClientResult<VoiceSnapshot> {
        let _ = (guild_id, channel_id, muted, deafened, push_to_talk);
        Err(ClientError::Network(anyhow!(
            "voice calls need a daemon control session; attach through ovvc-remote"
        )))
    }

    fn voice_leave(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<VoiceSnapshot>> {
        let _ = (guild_id, channel_id);
        Err(ClientError::Network(anyhow!(
            "voice calls need a daemon control session; attach through ovvc-remote"
        )))
    }

    fn set_voice_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        state: VoiceMemberState,
    ) -> ClientResult<VoiceSnapshot> {
        let _ = (guild_id, channel_id, state);
        Err(ClientError::Network(anyhow!(
            "voice calls need a daemon control session; attach through ovvc-remote"
        )))
    }

    fn set_push_to_talk_held(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        held: bool,
    ) -> ClientResult<()> {
        let _ = (guild_id, channel_id, held);
        Err(ClientError::Network(anyhow!(
            "voice calls need a daemon control session; attach through ovvc-remote"
        )))
    }

    fn voice_snapshot(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<VoiceSnapshot>> {
        let _ = (guild_id, channel_id);
        Err(ClientError::Network(anyhow!(
            "voice calls need a daemon control session; attach through ovvc-remote"
        )))
    }

    fn voice_audio(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<VoiceAudioStats>> {
        let _ = (guild_id, channel_id);
        Err(ClientError::Network(anyhow!(
            "call audio needs the daemon's audio service; attach through ovvc-remote"
        )))
    }

    fn policy(&self, guild_id: GuildId) -> ClientResult<PolicyView> {
        let _ = guild_id;
        Err(ClientError::Network(anyhow!(
            "the guild policy read model needs the daemon's configuration; attach through ovvc-remote"
        )))
    }

    fn notification_settings(&self) -> ClientResult<NotificationSettings> {
        Ok(Client::notification_settings(self))
    }

    fn set_desktop_notifications(&self, enabled: bool) -> ClientResult<()> {
        Client::set_desktop_notifications(self, enabled);
        Ok(())
    }

    fn set_do_not_disturb(&self, enabled: bool) -> ClientResult<()> {
        Client::set_do_not_disturb(self, enabled);
        Ok(())
    }

    fn set_quiet_hours(&self, quiet_hours: Option<QuietHours>) -> ClientResult<()> {
        Client::set_quiet_hours(self, quiet_hours);
        Ok(())
    }

    fn set_channel_muted(&self, channel_id: ChannelId, muted: bool) -> ClientResult<()> {
        Client::set_channel_muted(self, channel_id, muted);
        Ok(())
    }

    fn set_guild_muted(&self, guild_id: GuildId, muted: bool) -> ClientResult<()> {
        Client::set_guild_muted(self, guild_id, muted);
        Ok(())
    }

    fn list_channels(&self, guild_id: GuildId) -> ClientResult<Vec<ChannelMeta>> {
        Client::list_channels(self, guild_id)
    }

    fn find_channel(&self, guild_id: GuildId, needle: &str) -> ClientResult<ChannelMeta> {
        Client::find_channel(self, guild_id, needle)
    }

    fn default_channel(&self, guild_id: GuildId) -> ClientResult<ChannelMeta> {
        Client::default_channel(self, guild_id)
    }

    fn list_members(&self, guild_id: GuildId) -> ClientResult<Vec<MemberInfo>> {
        Client::list_members(self, guild_id)
    }

    fn get_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<ChannelMeta>> {
        Client::get_channel(self, guild_id, channel_id)
    }

    fn get_member(&self, guild_id: GuildId, author: AuthorId) -> ClientResult<Option<MemberInfo>> {
        Client::get_member(self, guild_id, author)
    }

    fn set_display_name(&self, guild_id: GuildId, display_name: &str) -> ClientResult<()> {
        Client::set_display_name(self, guild_id, display_name)
    }

    fn set_profile(
        &self,
        guild_id: GuildId,
        about: &str,
        pronouns: &str,
        links: &[String],
    ) -> ClientResult<SignedEvent> {
        Client::set_profile(self, guild_id, about, pronouns, links)
    }

    fn member_profile(&self, guild_id: GuildId, author: AuthorId) -> ClientResult<MemberProfile> {
        Client::member_profile(self, guild_id, author)
    }

    fn direct_peer(&self, guild_id: GuildId) -> ClientResult<Option<AuthorId>> {
        Client::direct_peer(self, guild_id)
    }

    fn list_direct_messages(&self) -> ClientResult<Vec<DirectConversation>> {
        Client::list_direct_messages(self)
    }

    fn open_direct_message(&self, context: GuildId, peer: AuthorId) -> ClientResult<GuildMeta> {
        // The direct client only has an async open (it dials the peer). As with
        // blob fetches, a synchronous frontend has to supply the runtime.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| {
                handle.block_on(Client::open_direct_message(self, context, peer))
            }),
            Err(_) => Err(ClientError::Network(anyhow!(
                "opening a direct message needs an async runtime to drive the local node"
            ))),
        }
    }

    fn history(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        limit: usize,
    ) -> ClientResult<Vec<SignedEvent>> {
        Client::history(self, guild_id, channel_id, limit)
    }

    fn history_before(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        cursor: Option<SyncCursor>,
        limit: usize,
    ) -> ClientResult<EventPage> {
        Client::history_before(self, guild_id, channel_id, cursor, limit)
    }

    fn mark_read(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        event_id: EventId,
    ) -> ClientResult<()> {
        Client::mark_read(self, guild_id, channel_id, event_id).map(|_| ())
    }

    fn unread_counts(&self, guild_id: GuildId) -> ClientResult<HashMap<ChannelId, usize>> {
        Client::unread_counts(self, guild_id)
    }

    fn post_message_full(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
    ) -> ClientResult<SignedEvent> {
        Client::post_message_full(self, guild_id, channel_id, body, reply_to, mentions)
    }

    fn post_message_rich(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
        attachments: &[Attachment],
    ) -> ClientResult<SignedEvent> {
        Client::post_message_rich(
            self,
            guild_id,
            channel_id,
            body,
            reply_to,
            mentions,
            attachments,
        )
    }

    fn edit_message(
        &self,
        guild_id: GuildId,
        target: EventId,
        body: &str,
    ) -> ClientResult<SignedEvent> {
        Client::edit_message(self, guild_id, target, body)
    }

    fn delete_message(&self, guild_id: GuildId, target: EventId) -> ClientResult<SignedEvent> {
        Client::delete_message(self, guild_id, target)
    }

    fn add_reaction(
        &self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
    ) -> ClientResult<SignedEvent> {
        Client::add_reaction(self, guild_id, target, emoji)
    }

    fn remove_reaction(
        &self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
    ) -> ClientResult<SignedEvent> {
        Client::remove_reaction(self, guild_id, target, emoji)
    }

    fn put_blob(&self, bytes: &[u8]) -> ClientResult<AttachmentId> {
        Client::put_blob(self, bytes)
    }

    fn get_blob(&self, id: AttachmentId) -> ClientResult<Option<Vec<u8>>> {
        Client::get_blob(self, id)
    }

    fn has_blob(&self, id: AttachmentId) -> ClientResult<bool> {
        Client::has_blob(self, id)
    }

    fn fetch_blob(&self, guild_id: GuildId, id: AttachmentId) -> ClientResult<Vec<u8>> {
        // The direct client only has an async fetch. A frontend using it from
        // synchronous code has to provide the runtime to drive it, so the
        // blocking wrapper is best effort rather than pretending to be free.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| {
                handle.block_on(Client::fetch_blob(self, guild_id, id))
            }),
            Err(_) => Err(ClientError::Network(anyhow!(
                "a blob fetch needs an async runtime to drive the local node"
            ))),
        }
    }

    fn create_invite(
        &self,
        guild_id: GuildId,
        peers: Vec<String>,
        expires_in_secs: Option<u64>,
    ) -> ClientResult<Invite> {
        Client::create_invite(self, guild_id, peers, expires_in_secs)
    }

    fn peer_id(&self, guild_id: GuildId) -> Option<PeerId> {
        Client::peer_id(self, guild_id)
    }

    fn subscribe(&self) -> broadcast::Receiver<ClientEvent> {
        Client::subscribe(self)
    }

    fn connect<'a>(
        &'a self,
        guild_id: GuildId,
        options: NetworkOptions,
    ) -> BoxFuture<'a, ClientResult<()>> {
        Box::pin(Client::connect(self, guild_id, options))
    }

    fn join_guild<'a>(
        &'a self,
        invite: &'a Invite,
        options: NetworkOptions,
    ) -> BoxFuture<'a, ClientResult<GuildMeta>> {
        Box::pin(Client::join_guild(self, invite, options))
    }

    fn disconnect(&self, guild_id: GuildId) {
        Client::disconnect(self, guild_id);
    }
}

/// How [`RemoteClient::connect`] reaches the daemon.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// Start `ovvc-daemon` when nothing is listening on the socket.
    pub spawn_daemon: bool,
    /// Ask a freshly spawned daemon to bring the peer network up (`--online`).
    /// Interactive frontends set this; a one-shot CLI command leaves it off,
    /// because `ovvc history` should not start listening for peers.
    pub online: bool,
    /// Override socket discovery. By default the record in `daemon.json` is
    /// used, falling back to the per-user runtime path.
    pub socket: Option<PathBuf>,
    /// Configuration file handed to a daemon this frontend spawns. Without
    /// this, `--config` on the frontend would be lost the moment it started
    /// the daemon, and the daemon would read the user config instead.
    pub config: Option<PathBuf>,
    /// Named configuration profile handed to a spawned daemon, for the same
    /// reason as `config`.
    pub profile: Option<String>,
    /// How long to wait for the handshake.
    pub timeout: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            spawn_daemon: true,
            online: false,
            socket: None,
            config: None,
            profile: None,
            timeout: Duration::from_secs(10),
        }
    }
}

/// A frontend client that talks to the local daemon over its Unix socket.
pub struct RemoteClient {
    client: DaemonClient,
    events: broadcast::Sender<ClientEvent>,
    forwarder: JoinHandle<()>,
    author_id: AuthorId,
    public_key_hex: String,
    peer_id: Option<String>,
    data_dir: PathBuf,
    socket: PathBuf,
    /// Kept last so it is shut down after the handles above.
    runtime: Option<tokio::runtime::Runtime>,
}

impl RemoteClient {
    /// Attach to the daemon for `data_dir`, starting one if allowed and
    /// needed. Blocks until the handshake completes.
    pub fn attach(data_dir: impl AsRef<Path>, options: ConnectOptions) -> ClientResult<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        let socket = options
            .socket
            .clone()
            .unwrap_or_else(|| resolve_socket_path(&data_dir));

        if options.spawn_daemon && !socket_ready(&socket, Duration::from_millis(600)) {
            spawn_daemon(
                &data_dir,
                &socket,
                options.online,
                options.config.as_deref(),
                options.profile.as_deref(),
            );
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("ovvc-remote")
            .build()
            .map_err(|error| {
                ClientError::Network(anyhow!("creating the frontend runtime: {error}"))
            })?;

        let client = {
            let _enter = runtime.enter();
            DaemonClient::new(&socket)
        };
        let connection = match drive(&runtime, client.wait_connected(options.timeout)) {
            Ok(connection) => connection,
            Err(error) => {
                // A rejected handshake and a protocol major mismatch both end
                // the connection before it is ever marked connected. Surface
                // the supervisor's last reason, which names what the daemon
                // refused, instead of a bare timeout.
                let state = client.state();
                let reason = state.last_error.unwrap_or(error.message);
                return Err(ClientError::Network(anyhow!(
                    "daemon at {} could not be attached: {reason}",
                    socket.display()
                )));
            }
        };
        let welcome = match connection.daemon {
            Some(welcome) => welcome,
            None => {
                return Err(ClientError::Network(anyhow!(
                    "daemon at {} did not send a welcome",
                    socket.display()
                )));
            }
        };
        if protocol_major(&welcome.protocol) != protocol_major(IPC_PROTOCOL) {
            return Err(ClientError::Network(anyhow!(
                "daemon at {} speaks IPC protocol {:?}, this client speaks {IPC_PROTOCOL}",
                socket.display(),
                welcome.protocol
            )));
        }

        let (events, _) = broadcast::channel(1024);
        let mut daemon_events = client.subscribe();
        let forward = events.clone();
        let forwarder = runtime.spawn(async move {
            loop {
                match daemon_events.recv().await {
                    Ok(event) => {
                        if let Some(event) = map_event(event) {
                            let _ = forward.send(event);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        Ok(Self {
            client,
            events,
            forwarder,
            author_id: welcome.author_id,
            public_key_hex: welcome.public_key_hex,
            peer_id: welcome.peer_id,
            data_dir,
            socket,
            runtime: Some(runtime),
        })
    }

    pub fn author_id(&self) -> AuthorId {
        self.author_id
    }

    /// The daemon's version and process details, for a frontend status line.
    pub fn daemon_status(&self) -> ClientResult<DaemonStatusDto> {
        match self.call(Op::Status)? {
            Reply::Status(status) => Ok(status),
            other => Err(unexpected(other)),
        }
    }

    /// What the daemon build compiled and selected, `None` when the daemon is
    /// older than the capability report and did not advertise it.
    ///
    /// The handshake decides this, so a new client never sends an op an older
    /// daemon cannot parse.
    pub fn daemon_capabilities(&self) -> ClientResult<Option<DaemonCapabilities>> {
        let supported = self
            .client
            .state()
            .capabilities
            .iter()
            .any(|name| name == CAP_CAPABILITIES);
        if !supported {
            return Ok(None);
        }
        match self.call(Op::Capabilities)? {
            Reply::Capabilities(capabilities) => Ok(Some(capabilities)),
            other => Err(unexpected(other)),
        }
    }

    pub fn public_key_hex(&self) -> &str {
        &self.public_key_hex
    }

    /// The daemon's transport peer id, as announced during the handshake.
    pub fn daemon_peer_id(&self) -> Option<&str> {
        self.peer_id.as_deref()
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn list_guilds(&self) -> ClientResult<Vec<GuildMeta>> {
        match self.call(Op::ListGuilds)? {
            Reply::Guilds(guilds) => Ok(guilds),
            other => Err(unexpected(other)),
        }
    }

    pub fn get_guild(&self, guild_id: GuildId) -> ClientResult<Option<GuildMeta>> {
        match self.call(Op::GetGuild { guild_id })? {
            Reply::Guild(guild) => Ok(guild),
            other => Err(unexpected(other)),
        }
    }

    pub fn find_guild(&self, needle: &str) -> ClientResult<GuildMeta> {
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

    pub fn create_guild(&self, name: &str) -> ClientResult<GuildMeta> {
        match self.call(Op::CreateGuild {
            name: name.to_string(),
        })? {
            Reply::GuildCreated(guild) => Ok(guild),
            other => Err(unexpected(other)),
        }
    }

    pub fn list_channels(&self, guild_id: GuildId) -> ClientResult<Vec<ChannelMeta>> {
        match self.call(Op::ListChannels { guild_id })? {
            Reply::Channels(channels) => Ok(channels),
            other => Err(unexpected(other)),
        }
    }

    pub fn find_channel(&self, guild_id: GuildId, needle: &str) -> ClientResult<ChannelMeta> {
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

    pub fn default_channel(&self, guild_id: GuildId) -> ClientResult<ChannelMeta> {
        let channels = self.list_channels(guild_id)?;
        channels
            .iter()
            .find(|channel| channel.name == "general")
            .or_else(|| channels.first())
            .cloned()
            .ok_or_else(|| {
                ClientError::Core(CoreError::NotFound(format!(
                    "channel in commune {guild_id}"
                )))
            })
    }

    pub fn create_channel(&self, guild_id: GuildId, name: &str) -> ClientResult<ChannelMeta> {
        match self.call(Op::CreateChannel {
            guild_id,
            name: name.to_string(),
        })? {
            Reply::Channel(channel) => Ok(channel),
            other => Err(unexpected(other)),
        }
    }

    pub fn create_channel_in(
        &self,
        guild_id: GuildId,
        name: &str,
        kind: ChannelKind,
        parent: Option<ChannelId>,
    ) -> ClientResult<ChannelMeta> {
        match self.call(Op::CreateChannelIn {
            guild_id,
            name: name.to_string(),
            kind,
            parent,
        })? {
            Reply::Channel(channel) => Ok(channel),
            other => Err(unexpected(other)),
        }
    }

    pub fn create_category(&self, guild_id: GuildId, name: &str) -> ClientResult<CategoryMeta> {
        match self.call(Op::CreateCategory {
            guild_id,
            name: name.to_string(),
        })? {
            Reply::Category(category) => Ok(category),
            other => Err(unexpected(other)),
        }
    }

    pub fn rename_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        name: &str,
    ) -> ClientResult<()> {
        self.expect_unit(Op::RenameChannel {
            guild_id,
            channel_id,
            name: name.to_string(),
        })
    }

    pub fn move_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        parent: Option<ChannelId>,
    ) -> ClientResult<()> {
        self.expect_unit(Op::MoveChannel {
            guild_id,
            channel_id,
            parent,
        })
    }

    pub fn list_categories(&self, guild_id: GuildId) -> ClientResult<Vec<CategoryMeta>> {
        match self.call(Op::ChannelTree { guild_id })? {
            Reply::ChannelTree(tree) => Ok(tree.categories),
            other => Err(unexpected(other)),
        }
    }

    pub fn list_channel_views(&self, guild_id: GuildId) -> ClientResult<Vec<ChannelView>> {
        match self.call(Op::ChannelTree { guild_id })? {
            Reply::ChannelTree(tree) => Ok(tree.channels),
            other => Err(unexpected(other)),
        }
    }

    /// Categories and channels in one round trip.
    pub fn channel_tree(
        &self,
        guild_id: GuildId,
    ) -> ClientResult<(Vec<CategoryMeta>, Vec<ChannelView>)> {
        match self.call(Op::ChannelTree { guild_id })? {
            Reply::ChannelTree(tree) => Ok((tree.categories, tree.channels)),
            other => Err(unexpected(other)),
        }
    }

    pub fn search(
        &self,
        guild_id: GuildId,
        query: &str,
        limit: usize,
    ) -> ClientResult<Vec<ovvc_ipc::SearchHit>> {
        match self.call(Op::Search {
            guild_id,
            query: query.to_string(),
            limit,
        })? {
            Reply::Search(hits) => Ok(hits),
            other => Err(unexpected(other)),
        }
    }

    pub fn control_join(
        &self,
        guild_id: GuildId,
        session_id: SessionId,
        display_name: &str,
    ) -> ClientResult<()> {
        match self.call(Op::ControlJoin {
            guild_id,
            session_id,
            display_name: display_name.to_string(),
        })? {
            Reply::Control(_) => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    pub fn control_snapshot(
        &self,
        guild_id: GuildId,
        session_id: SessionId,
    ) -> ClientResult<ovvc_control::ControlSnapshot> {
        match self.call(Op::ControlSnapshot {
            guild_id,
            session_id,
        })? {
            Reply::Control(snapshot) => Ok(snapshot),
            other => Err(unexpected(other)),
        }
    }

    pub fn set_presence(&self, guild_id: GuildId, status: PresenceStatus) -> ClientResult<()> {
        self.expect_unit(Op::SetPresence { guild_id, status })
    }

    pub fn voice_join(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        muted: bool,
        deafened: bool,
        push_to_talk: bool,
    ) -> ClientResult<VoiceSnapshot> {
        match self.call(Op::VoiceJoin {
            guild_id,
            channel_id,
            muted,
            deafened,
            push_to_talk,
        })? {
            Reply::Voice(Some(snapshot)) => Ok(snapshot),
            other => Err(unexpected(other)),
        }
    }

    pub fn voice_leave(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<VoiceSnapshot>> {
        match self.call(Op::VoiceLeave {
            guild_id,
            channel_id,
        })? {
            Reply::Voice(snapshot) => Ok(snapshot),
            other => Err(unexpected(other)),
        }
    }

    pub fn set_voice_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        state: VoiceMemberState,
    ) -> ClientResult<VoiceSnapshot> {
        match self.call(Op::SetVoiceState {
            guild_id,
            channel_id,
            muted: state.muted,
            deafened: state.deafened,
            push_to_talk: state.push_to_talk,
            speaking: state.speaking,
            video: state.video,
            screen_share: state.screen_share,
        })? {
            Reply::Voice(Some(snapshot)) => Ok(snapshot),
            other => Err(unexpected(other)),
        }
    }

    pub fn set_push_to_talk_held(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        held: bool,
    ) -> ClientResult<()> {
        self.expect_unit(Op::SetPushToTalkHeld {
            guild_id,
            channel_id,
            held,
        })
    }

    pub fn voice_snapshot(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<VoiceSnapshot>> {
        match self.call(Op::VoiceSnapshot {
            guild_id,
            channel_id,
        })? {
            Reply::Voice(snapshot) => Ok(snapshot),
            other => Err(unexpected(other)),
        }
    }

    pub fn voice_audio(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<VoiceAudioStats>> {
        match self.call(Op::VoiceAudio {
            guild_id,
            channel_id,
        })? {
            Reply::VoiceAudio(stats) => Ok(stats),
            other => Err(unexpected(other)),
        }
    }

    /// The guild policy read model from the daemon.
    pub fn policy(&self, guild_id: GuildId) -> ClientResult<PolicyView> {
        match self.call(Op::Policy { guild_id })? {
            Reply::Policy(view) => Ok(view),
            other => Err(unexpected(other)),
        }
    }

    pub fn notification_settings(&self) -> ClientResult<NotificationSettings> {
        match self.call(Op::NotificationSettings)? {
            Reply::NotificationSettings(settings) => Ok(settings),
            other => Err(unexpected(other)),
        }
    }

    pub fn set_desktop_notifications(&self, enabled: bool) -> ClientResult<()> {
        self.expect_unit(Op::SetDesktopNotifications { enabled })
    }

    pub fn set_do_not_disturb(&self, enabled: bool) -> ClientResult<()> {
        self.expect_unit(Op::SetDoNotDisturb { enabled })
    }

    pub fn set_quiet_hours(&self, quiet_hours: Option<QuietHours>) -> ClientResult<()> {
        self.expect_unit(Op::SetQuietHours { quiet_hours })
    }

    pub fn set_channel_muted(&self, channel_id: ChannelId, muted: bool) -> ClientResult<()> {
        self.expect_unit(Op::SetChannelMuted { channel_id, muted })
    }

    pub fn set_guild_muted(&self, guild_id: GuildId, muted: bool) -> ClientResult<()> {
        self.expect_unit(Op::SetGuildMuted { guild_id, muted })
    }

    pub fn list_members(&self, guild_id: GuildId) -> ClientResult<Vec<MemberInfo>> {
        match self.call(Op::ListMembers { guild_id })? {
            Reply::Members(members) => Ok(members),
            other => Err(unexpected(other)),
        }
    }

    pub fn get_channel(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ClientResult<Option<ChannelMeta>> {
        // The daemon has no single-channel op, and a guild's channel list is
        // small enough that filtering it here costs less than another round
        // trip shape to keep in sync on both sides.
        Ok(self
            .list_channels(guild_id)?
            .into_iter()
            .find(|channel| channel.id == channel_id))
    }

    pub fn get_member(
        &self,
        guild_id: GuildId,
        author: AuthorId,
    ) -> ClientResult<Option<MemberInfo>> {
        Ok(self
            .list_members(guild_id)?
            .into_iter()
            .find(|member| member.author == author))
    }

    pub fn set_display_name(&self, guild_id: GuildId, display_name: &str) -> ClientResult<()> {
        self.expect_unit(Op::SetDisplayName {
            guild_id,
            display_name: display_name.to_string(),
        })
    }

    pub fn leave_guild(&self, guild_id: GuildId) -> ClientResult<()> {
        self.expect_unit(Op::LeaveGuild { guild_id })
    }

    pub fn set_profile(
        &self,
        guild_id: GuildId,
        about: &str,
        pronouns: &str,
        links: &[String],
    ) -> ClientResult<SignedEvent> {
        match self.call(Op::SetProfile {
            guild_id,
            about: about.to_string(),
            pronouns: pronouns.to_string(),
            links: links.to_vec(),
        })? {
            Reply::Event(event) => Ok(event),
            other => Err(unexpected(other)),
        }
    }

    pub fn member_profile(
        &self,
        guild_id: GuildId,
        author: AuthorId,
    ) -> ClientResult<MemberProfile> {
        match self.call(Op::GetMember { guild_id, author })? {
            Reply::Member(profile) => Ok(profile),
            other => Err(unexpected(other)),
        }
    }

    pub fn direct_peer(&self, guild_id: GuildId) -> ClientResult<Option<AuthorId>> {
        // The daemon owns the announcement bookkeeping, so asking it for the
        // full conversation list is the faithful way to answer this remotely.
        Ok(self
            .list_direct_messages()?
            .into_iter()
            .find(|conversation| conversation.guild.id == guild_id)
            .map(|conversation| conversation.peer.author))
    }

    pub fn list_direct_messages(&self) -> ClientResult<Vec<DirectConversation>> {
        match self.call(Op::ListDirectMessages)? {
            Reply::DirectMessages(conversations) => Ok(conversations),
            other => Err(unexpected(other)),
        }
    }

    pub fn open_direct_message(&self, context: GuildId, peer: AuthorId) -> ClientResult<GuildMeta> {
        match self.call(Op::OpenDirectMessage { context, peer })? {
            Reply::GuildCreated(guild) => Ok(guild),
            other => Err(unexpected(other)),
        }
    }

    pub fn history(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        limit: usize,
    ) -> ClientResult<Vec<SignedEvent>> {
        match self.call(Op::History {
            guild_id,
            channel_id,
            limit,
        })? {
            Reply::Events(events) => Ok(events),
            other => Err(unexpected(other)),
        }
    }

    pub fn history_before(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        cursor: Option<SyncCursor>,
        limit: usize,
    ) -> ClientResult<EventPage> {
        match self.call(Op::HistoryBefore {
            guild_id,
            channel_id,
            cursor,
            limit,
        })? {
            Reply::HistoryPage(page) => Ok(page),
            other => Err(unexpected(other)),
        }
    }

    pub fn mark_read(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        event_id: EventId,
    ) -> ClientResult<()> {
        match self.call(Op::MarkRead {
            guild_id,
            channel_id,
            event_id,
        })? {
            // The daemon replies with the signed marker; the frontend only
            // needs to know it was accepted.
            Reply::Event(_) => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    pub fn unread_counts(&self, guild_id: GuildId) -> ClientResult<HashMap<ChannelId, usize>> {
        match self.call(Op::UnreadCounts { guild_id })? {
            Reply::Unread(counts) => Ok(counts),
            other => Err(unexpected(other)),
        }
    }

    pub fn post_message_full(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
    ) -> ClientResult<SignedEvent> {
        match self.call(Op::PostMessageFull {
            guild_id,
            channel_id,
            body: body.to_string(),
            reply_to,
            mentions: mentions.to_vec(),
        })? {
            Reply::Event(event) => Ok(event),
            other => Err(unexpected(other)),
        }
    }

    pub fn post_message_rich(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: &str,
        reply_to: Option<EventId>,
        mentions: &[AuthorId],
        attachments: &[Attachment],
    ) -> ClientResult<SignedEvent> {
        match self.call(Op::PostMessageRich {
            guild_id,
            channel_id,
            body: body.to_string(),
            reply_to,
            mentions: mentions.to_vec(),
            attachments: attachments.to_vec(),
        })? {
            Reply::Event(event) => Ok(event),
            other => Err(unexpected(other)),
        }
    }

    pub fn put_blob(&self, bytes: &[u8]) -> ClientResult<AttachmentId> {
        match self.call(Op::PutBlob {
            bytes: bytes.to_vec(),
        })? {
            Reply::BlobId(id) => Ok(id),
            other => Err(unexpected(other)),
        }
    }

    pub fn get_blob(&self, id: AttachmentId) -> ClientResult<Option<Vec<u8>>> {
        match self.call(Op::GetBlob { hash: id })? {
            Reply::Blob(bytes) => Ok(bytes),
            other => Err(unexpected(other)),
        }
    }

    pub fn has_blob(&self, id: AttachmentId) -> ClientResult<bool> {
        match self.call(Op::HasBlob { hash: id })? {
            Reply::Bool(present) => Ok(present),
            other => Err(unexpected(other)),
        }
    }

    pub fn fetch_blob(&self, guild_id: GuildId, id: AttachmentId) -> ClientResult<Vec<u8>> {
        // The daemon holds the node, so the download happens there and only the
        // finished bytes cross the socket. See the trait doc for why this blocks.
        match self.call(Op::FetchBlob { guild_id, hash: id })? {
            Reply::Blob(Some(bytes)) => Ok(bytes),
            Reply::Blob(None) => Err(ClientError::Network(anyhow!(
                "the daemon returned no bytes for {id}"
            ))),
            other => Err(unexpected(other)),
        }
    }

    pub fn edit_message(
        &self,
        guild_id: GuildId,
        target: EventId,
        body: &str,
    ) -> ClientResult<SignedEvent> {
        match self.call(Op::EditMessage {
            guild_id,
            target,
            body: body.to_string(),
        })? {
            Reply::Event(event) => Ok(event),
            other => Err(unexpected(other)),
        }
    }

    pub fn delete_message(&self, guild_id: GuildId, target: EventId) -> ClientResult<SignedEvent> {
        match self.call(Op::DeleteMessage { guild_id, target })? {
            Reply::Event(event) => Ok(event),
            other => Err(unexpected(other)),
        }
    }

    pub fn add_reaction(
        &self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
    ) -> ClientResult<SignedEvent> {
        match self.call(Op::AddReaction {
            guild_id,
            target,
            emoji: emoji.to_string(),
        })? {
            Reply::Event(event) => Ok(event),
            other => Err(unexpected(other)),
        }
    }

    pub fn remove_reaction(
        &self,
        guild_id: GuildId,
        target: EventId,
        emoji: &str,
    ) -> ClientResult<SignedEvent> {
        match self.call(Op::RemoveReaction {
            guild_id,
            target,
            emoji: emoji.to_string(),
        })? {
            Reply::Event(event) => Ok(event),
            other => Err(unexpected(other)),
        }
    }

    pub fn create_invite(
        &self,
        guild_id: GuildId,
        peers: Vec<String>,
        expires_in_secs: Option<u64>,
    ) -> ClientResult<Invite> {
        match self.call(Op::CreateInvite {
            guild_id,
            peers,
            expires_in_secs,
        })? {
            Reply::Invite(invite) => Ok(invite),
            other => Err(unexpected(other)),
        }
    }

    /// The daemon's peer id for a guild, or `None` while the guild has no
    /// running network node.
    pub fn peer_id(&self, guild_id: GuildId) -> Option<PeerId> {
        let reply = self.call(Op::ConnectionState { guild_id }).ok()?;
        match reply {
            Reply::Connection(state) => state.peer_id.and_then(|peer| peer.parse().ok()),
            _ => None,
        }
    }

    pub fn lookup(&self, guild_id: GuildId, author: AuthorId) -> ClientResult<()> {
        self.expect_unit(Op::Lookup { guild_id, author })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ClientEvent> {
        self.events.subscribe()
    }

    /// Blocking form of [`RemoteClient::connect_async`].
    pub fn connect(&self, guild_id: GuildId, options: NetworkOptions) -> ClientResult<()> {
        let runtime = self
            .runtime
            .as_ref()
            .expect("remote client used after drop");
        drive(runtime, self.connect_async(guild_id, options))
    }

    /// Blocking form of [`RemoteClient::join_guild_async`].
    pub fn join_guild(&self, invite: &Invite, options: NetworkOptions) -> ClientResult<GuildMeta> {
        let runtime = self
            .runtime
            .as_ref()
            .expect("remote client used after drop");
        drive(runtime, self.join_guild_async(invite, options))
    }

    pub fn connect_async<'a>(
        &'a self,
        guild_id: GuildId,
        options: NetworkOptions,
    ) -> BoxFuture<'a, ClientResult<()>> {
        Box::pin(async move {
            match self
                .client
                .call(Op::Connect {
                    guild_id,
                    options: options_dto(&options),
                })
                .await
                .map_err(to_client_error)?
            {
                Reply::Unit => Ok(()),
                other => Err(unexpected(other)),
            }
        })
    }

    pub fn join_guild_async<'a>(
        &'a self,
        invite: &'a Invite,
        options: NetworkOptions,
    ) -> BoxFuture<'a, ClientResult<GuildMeta>> {
        Box::pin(async move {
            let encoded = invite.encode().map_err(ClientError::Core)?;
            let reply = self
                .client
                .call(Op::JoinGuild {
                    invite: encoded,
                    options: options_dto(&options),
                })
                .await
                .map_err(to_client_error)?;
            match reply {
                Reply::Guild(Some(guild)) => Ok(guild),
                other => Err(unexpected(other)),
            }
        })
    }

    pub fn disconnect(&self, guild_id: GuildId) {
        let _ = self.call(Op::Disconnect { guild_id });
    }

    fn expect_unit(&self, op: Op) -> ClientResult<()> {
        match self.call(op)? {
            Reply::Unit => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    fn call(&self, op: Op) -> ClientResult<Reply> {
        let runtime = self
            .runtime
            .as_ref()
            .expect("remote client used after drop");
        drive(runtime, async { self.client.call(op).await }).map_err(to_client_error)
    }
}

impl Drop for RemoteClient {
    fn drop(&mut self) {
        self.forwarder.abort();
        if let Some(runtime) = self.runtime.take() {
            // The TUI holds this value inside its own async runtime, where
            // dropping a runtime would block or panic.
            runtime.shutdown_background();
        }
    }
}

/// Run `future` on a private runtime from synchronous code.
///
/// Inside an async context the current worker is released first; this assumes
/// a multi-threaded runtime, which every ovvc frontend uses.
fn drive<F: Future>(runtime: &tokio::runtime::Runtime, future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(_) => tokio::task::block_in_place(|| runtime.block_on(future)),
        Err(_) => runtime.block_on(future),
    }
}

fn to_client_error(error: ovvc_ipc::IpcError) -> ClientError {
    match error.kind {
        ErrorKind::NotFound => ClientError::Core(CoreError::NotFound(error.message)),
        ErrorKind::InvalidInput => ClientError::Core(CoreError::InvalidInput(error.message)),
        ErrorKind::NotConnected => ClientError::Network(anyhow!(
            "{}; run `ovvc-daemon --data-dir <dir>` or start a frontend that spawns it",
            error.message
        )),
        _ => ClientError::Network(anyhow!(error.message)),
    }
}

fn unexpected(reply: Reply) -> ClientError {
    ClientError::Network(anyhow!("unexpected daemon reply: {reply:?}"))
}

fn options_dto(options: &NetworkOptions) -> ovvc_ipc::NetworkOptionsDto {
    ovvc_ipc::NetworkOptionsDto {
        listen: options.listen.iter().map(ToString::to_string).collect(),
        dial: options.dial.iter().map(ToString::to_string).collect(),
        relay: options.relay.as_ref().map(ToString::to_string),
        upnp: options.upnp,
        natpmp: options.natpmp,
    }
}

fn choose_one<T>(matches: Vec<T>, kind: &str, needle: &str) -> ClientResult<T> {
    match matches.len() {
        0 => Err(ClientError::Core(CoreError::NotFound(format!(
            "{kind} '{needle}'"
        )))),
        1 => Ok(matches.into_iter().next().expect("one match")),
        _ => Err(ClientError::Core(CoreError::InvalidInput(format!(
            "{kind} '{needle}' is ambiguous"
        )))),
    }
}

/// Resolve the control socket for a data directory.
///
/// A `daemon.json` record wins, because a daemon may have been started with an
/// explicit `--socket`; otherwise the per-user runtime path derived from the
/// data directory is used. Frontends call this before spawning a daemon so the
/// socket they pass to it is the same one they then attach to.
pub fn resolve_socket_path(data_dir: &Path) -> PathBuf {
    read_daemon_info(data_dir)
        .ok()
        .flatten()
        .map(|info| PathBuf::from(info.socket))
        .unwrap_or_else(|| default_socket_path(data_dir))
}

fn socket_ready(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Start the sibling daemon binary, then wait for its socket.
///
/// The frontend's `--config` and `--profile` are forwarded, so a daemon this
/// process starts reads the same configuration the frontend did. The socket
/// and data directory are always passed explicitly, because the frontend may
/// have chosen them from that same configuration.
fn spawn_daemon(
    data_dir: &Path,
    socket: &Path,
    online: bool,
    config: Option<&Path>,
    profile: Option<&str>,
) {
    let binary = daemon_binary();
    let mut command = std::process::Command::new(&binary);
    command
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--socket")
        .arg(socket);
    if online {
        command.arg("--online");
    }
    if let Some(config) = config {
        command.arg("--config").arg(config);
    }
    if let Some(profile) = profile {
        command.arg("--profile").arg(profile);
    }
    let _ = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let _ = socket_ready(socket, Duration::from_secs(5));
}

fn daemon_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("OVVC_DAEMON") {
        return PathBuf::from(path);
    }
    if let Some(sibling) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|parent| parent.join("ovvc-daemon")))
        .filter(|path| path.is_file())
    {
        return sibling;
    }
    PathBuf::from("ovvc-daemon")
}

fn map_event(event: DaemonEvent) -> Option<ClientEvent> {
    Some(match event {
        DaemonEvent::Listening { guild_id, address } => ClientEvent::Listening {
            guild_id,
            address: address.parse::<Multiaddr>().ok()?,
        },
        DaemonEvent::ExternalAddr { guild_id, address } => ClientEvent::ExternalAddr {
            guild_id,
            address: address.parse::<Multiaddr>().ok()?,
        },
        DaemonEvent::ExternalAddrRemoved { guild_id, address } => {
            ClientEvent::ExternalAddrRemoved {
                guild_id,
                address: address.parse::<Multiaddr>().ok()?,
            }
        }
        DaemonEvent::PeerConnected { guild_id, peer, .. } => ClientEvent::PeerConnected {
            guild_id,
            peer: peer.parse::<PeerId>().ok()?,
        },
        DaemonEvent::PeerDisconnected { guild_id, peer, .. } => ClientEvent::PeerDisconnected {
            guild_id,
            peer: peer.parse::<PeerId>().ok()?,
        },
        DaemonEvent::HolePunch {
            guild_id,
            peer,
            success,
        } => ClientEvent::HolePunch {
            guild_id,
            peer: peer.parse::<PeerId>().ok()?,
            success,
        },
        DaemonEvent::Synced { guild_id, peer } => ClientEvent::Synced {
            guild_id,
            peer: peer.parse::<PeerId>().ok()?,
        },
        DaemonEvent::Event {
            guild_id, event, ..
        } => ClientEvent::Event { guild_id, event },
        DaemonEvent::Control {
            guild_id, frame, ..
        } => ClientEvent::Control { guild_id, frame },
        // Lease expiry never produces a control frame, so these are the only
        // way a frontend hears that a member went offline.
        DaemonEvent::Presence {
            guild_id,
            author,
            status,
        } => ClientEvent::Presence {
            guild_id,
            author,
            status,
        },
        DaemonEvent::Typing {
            guild_id,
            channel_id,
            author,
            until_ms,
        } => ClientEvent::Typing {
            guild_id,
            channel_id,
            author,
            until_ms,
        },
        DaemonEvent::TypingEnded {
            guild_id,
            channel_id,
            author,
        } => ClientEvent::TypingEnded {
            guild_id,
            channel_id,
            author,
        },
        DaemonEvent::Voice {
            guild_id,
            channel_id,
            change,
            snapshot,
        } => ClientEvent::Voice {
            guild_id,
            channel_id,
            change,
            snapshot,
        },
        DaemonEvent::VoiceAudio {
            guild_id,
            channel_id,
            stats,
        } => ClientEvent::VoiceAudio {
            guild_id,
            channel_id,
            stats,
        },
        DaemonEvent::Error { guild_id, message } => ClientEvent::Error { guild_id, message },
        DaemonEvent::Diagnostic(diagnostic) => ClientEvent::Diagnostic(diagnostic),
        DaemonEvent::Connected { .. }
        | DaemonEvent::Disconnected { .. }
        | DaemonEvent::ControlState { .. }
        | DaemonEvent::Notification { .. } => return None,
        // `DaemonEvent` is non-exhaustive: a frontend ignores variants its
        // event surface cannot represent, including ones added by a newer
        // daemon.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_core::{Diagnostic, ErrorClass, GuildId};

    #[test]
    fn a_diagnostic_event_keeps_its_class_and_log_path() {
        let diagnostic = Diagnostic::bug("an impossible state")
            .with_guild(Some(GuildId([5u8; 32])))
            .with_context("test")
            .with_log_path("/tmp/ovvc/daemon.log");
        match map_event(DaemonEvent::Diagnostic(diagnostic.clone())) {
            Some(ClientEvent::Diagnostic(mapped)) => {
                assert_eq!(mapped.class, ErrorClass::Bug);
                assert_eq!(mapped, diagnostic);
            }
            other => panic!("expected a diagnostic client event, got {other:?}"),
        }
    }

    #[test]
    fn a_legacy_error_still_maps_for_an_older_daemon() {
        let guild_id = GuildId([6u8; 32]);
        match map_event(DaemonEvent::Error {
            guild_id: Some(guild_id),
            message: "peer offline".to_string(),
        }) {
            Some(ClientEvent::Error {
                guild_id: mapped_guild,
                message,
            }) => {
                assert_eq!(mapped_guild, Some(guild_id));
                assert_eq!(message, "peer offline");
            }
            other => panic!("expected a legacy error client event, got {other:?}"),
        }
    }
}
