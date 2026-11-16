//! Pure, toolkit-independent GUI state.
//!
//! Everything here can run without a window, which is what the headless
//! rendering tests and the fuzzer exercise. The egui layer reads this state and
//! emits [`Action`]s; the async bridge turns those into daemon requests and
//! feeds [`Message`]s back in.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use ovvc_control::{ControlSnapshot, PresenceStatus, SessionId, VoiceMemberState, VoiceSnapshot};
use ovvc_core::{
    Attachment, AuthorId, CategoryMeta, ChannelId, ChannelKind, ChannelView, Diagnostic,
    DirectConversation, ErrorClass, EventId, EventKind, GuildId, GuildMeta,
    MAX_PROFILE_ABOUT_BYTES, MAX_PROFILE_LINK_BYTES, MAX_PROFILE_LINKS, MAX_PROFILE_PRONOUNS_BYTES,
    MemberInfo, MemberProfile, NotificationSettings, QuietHours, SignedEvent, SyncCursor,
    TimelineEntry, parse_mentions, timeline,
};
use ovvc_ipc::{
    ConnectionState, ConnectionStateDto, DaemonEvent, DaemonStatusDto, IpcError,
    NotificationReason, Op, PeerSnapshot, PolicyView, Reply, SearchHit, VoiceAudioStats,
};

/// Which request a reply belongs to. The daemon allows several requests in
/// flight, so the tag is how a reply finds the piece of state it updates.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RequestTag {
    Identity,
    Guilds,
    Status,
    Channels(GuildId),
    ChannelTree(GuildId),
    CreateCategory(GuildId),
    Members(GuildId),
    History {
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    HistoryBefore {
        guild_id: GuildId,
        channel_id: ChannelId,
        cursor: Option<SyncCursor>,
    },
    Unread(GuildId),
    Search(GuildId),
    Control {
        guild_id: GuildId,
        session_id: SessionId,
    },
    JoinControl {
        guild_id: GuildId,
        session_id: SessionId,
    },
    CreateGuild,
    CreateChannel(GuildId),
    RenameChannel(GuildId),
    MoveChannel(GuildId),
    JoinGuild,
    CreateInvite(GuildId),
    Connection(GuildId),
    Connect(GuildId),
    Disconnect(GuildId),
    SendMessage,
    /// A post that carried staged files, tagged with the send id the composer
    /// allocated, so overlapping sends cannot clear each other's chips.
    SendRichMessage(u64),
    EditMessage,
    DeleteMessage,
    Reaction,
    MarkRead,
    SetDisplayName,
    /// One member's profile for the open profile dialog.
    MemberProfile {
        guild_id: GuildId,
        author: AuthorId,
    },
    SetProfile,
    /// Open or reopen a direct conversation with `peer`.
    OpenDirectMessage {
        peer: AuthorId,
    },
    /// Join, leave, or change the local flags of a voice call.
    VoiceJoin {
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    VoiceLeave {
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    SetVoiceState {
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    /// The daemon's audio counters for a call, sampled once a second.
    VoiceAudio {
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    /// Every direct conversation in the local store.
    DirectMessages,
    /// Unread counts for one direct conversation, so its sidebar row can be
    /// badged without selecting the guild.
    DmUnread(GuildId),
    SetPresence,
    NotificationSettings,
    SetNotificationSettings,
    /// The selected guild's policy read model.
    Policy(GuildId),
    Shutdown,
    Other,
}

/// A message from the async bridge into the UI thread.
#[derive(Debug)]
pub enum Message {
    Reply {
        tag: RequestTag,
        result: Result<Reply, IpcError>,
    },
    Event(DaemonEvent),
    DaemonState(ConnectionState),
    BridgeError(String),
    /// A file read off the disk, ready to be sent with the next message.
    AttachmentStaged {
        path: PathBuf,
        result: Result<StagedAttachment, String>,
    },
    /// A downloaded attachment written to the cache, or the reason it could
    /// not be fetched or written.
    AttachmentSaved {
        name: String,
        result: Result<PathBuf, String>,
    },
}

/// A file the composer will upload with its next message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedAttachment {
    pub path: PathBuf,
    pub attachment: Attachment,
    pub bytes: Vec<u8>,
}

/// An intent produced by the UI layer and handled by the application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Refresh,
    SelectGuild(GuildId),
    SelectChannel {
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    CreateGuild {
        name: String,
    },
    CreateChannel {
        guild_id: GuildId,
        name: String,
    },
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
    SendMessage {
        guild_id: GuildId,
        channel_id: ChannelId,
        body: String,
        reply_to: Option<EventId>,
        mentions: Vec<AuthorId>,
    },
    /// Send a message with files the composer has already read.
    SendRichMessage {
        /// The composer's send id, matched back on the daemon's reply.
        send_id: u64,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: String,
        reply_to: Option<EventId>,
        mentions: Vec<AuthorId>,
        attachments: Vec<StagedAttachment>,
    },
    /// Read a file off the disk and stage it for the next send.
    StageAttachment {
        path: PathBuf,
    },
    ClearStagedAttachment {
        index: usize,
    },
    /// Fetch an attachment's bytes and open the local copy.
    OpenAttachment {
        guild_id: GuildId,
        attachment: Attachment,
    },
    EditMessage {
        guild_id: GuildId,
        target: EventId,
        body: String,
    },
    BeginEdit {
        event_id: EventId,
        body: String,
    },
    Reply {
        event_id: EventId,
    },
    DeleteMessage {
        guild_id: GuildId,
        target: EventId,
    },
    ToggleReaction {
        guild_id: GuildId,
        target: EventId,
        emoji: String,
        mine: bool,
    },
    MarkRead {
        guild_id: GuildId,
        channel_id: ChannelId,
        event_id: EventId,
    },
    LoadOlder {
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    Search {
        guild_id: GuildId,
        query: String,
    },
    /// Open a search hit's channel and scroll the transcript to the message.
    OpenSearchHit {
        guild_id: GuildId,
        channel_id: ChannelId,
        event_id: EventId,
    },
    JoinControl {
        guild_id: GuildId,
        session_id: SessionId,
        display_name: String,
    },
    /// Join a voice channel's call.
    VoiceJoin {
        guild_id: GuildId,
        channel_id: ChannelId,
        muted: bool,
        deafened: bool,
        push_to_talk: bool,
    },
    /// Hang up the local member's call.
    VoiceLeave {
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    /// Publish the local member's three call flags. When this window is not in
    /// that call, the flags are only remembered for the next join.
    SetVoiceState {
        guild_id: GuildId,
        channel_id: ChannelId,
        muted: bool,
        deafened: bool,
        push_to_talk: bool,
    },
    SetDisplayName {
        guild_id: GuildId,
        display_name: String,
    },
    /// Show one member's profile.
    OpenProfile {
        guild_id: GuildId,
        author: AuthorId,
    },
    /// Publish the trimmed profile drafts.
    SaveProfile {
        guild_id: GuildId,
        about: String,
        pronouns: String,
        links: Vec<String>,
    },
    /// Open (or reopen) the direct conversation with a member of `context`.
    OpenDirectMessage {
        context: GuildId,
        peer: AuthorId,
    },
    CreateInvite {
        guild_id: GuildId,
    },
    SetPresence {
        guild_id: GuildId,
        status: PresenceStatus,
    },
    SetDesktopNotifications(bool),
    SetDoNotDisturb(bool),
    SetQuietHours(Option<QuietHours>),
    /// Re-read the settings the daemon holds, for a window that has been open
    /// while another frontend changed them.
    RefreshNotificationSettings,
    SetChannelMuted {
        channel_id: ChannelId,
        muted: bool,
    },
    SetGuildMuted {
        guild_id: GuildId,
        muted: bool,
    },
    /// Close the getting-started window and remember the chosen name.
    FinishWizard,
    /// Close the setup flow without applying anything, and remember that it
    /// was shown so it does not reopen on the next launch.
    CancelSetup,
    /// Change the global detail level and persist it for both frontends.
    SetDetailLevel(DetailLevel),
    JoinGuild {
        code: String,
    },
    RefreshMembers(GuildId),
    /// Take the guild online with the addresses typed in the network tab.
    Connect {
        guild_id: GuildId,
    },
    Disconnect {
        guild_id: GuildId,
    },
    ShutdownDaemon,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionView {
    pub emoji: String,
    pub authors: Vec<AuthorId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyPreview {
    pub author: AuthorId,
    pub body: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageView {
    pub event_id: EventId,
    pub author: AuthorId,
    pub timestamp: u64,
    pub body: String,
    pub edited: bool,
    pub deleted: bool,
    pub reply_to: Option<ReplyPreview>,
    pub mentions: Vec<AuthorId>,
    /// Files announced by the message. The bytes are not here: they are
    /// content-addressed blobs that a frontend downloads on demand.
    pub attachments: Vec<Attachment>,
    pub reactions: Vec<ReactionView>,
}

impl MessageView {
    pub fn reacted_by(&self, emoji: &str, author: AuthorId) -> bool {
        self.reactions
            .iter()
            .any(|reaction| reaction.emoji == emoji && reaction.authors.contains(&author))
    }
}

/// Which overlay window is open. Only one at a time keeps the UI predictable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Dialog {
    #[default]
    None,
    CreateGuild,
    CreateChannel,
    CreateCategory,
    Join,
    Invite,
    ChannelSettings,
    /// A member's profile, with an edit mode when it is your own.
    Profile,
}

/// Which full-page view owns the window. The app chrome belongs to chat only;
/// settings, setup, the network map and diagnostics each replace the page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScreenView {
    #[default]
    Chat,
    Settings,
    NetworkMap,
    Diagnostics,
}

/// How much detail the whole client shows. The level is global and persisted,
/// and it changes which rows are visible, not their values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum DetailLevel {
    #[default]
    Simple,
    Advanced,
    Expert,
}

impl DetailLevel {
    pub const ALL: [DetailLevel; 3] = [
        DetailLevel::Simple,
        DetailLevel::Advanced,
        DetailLevel::Expert,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            DetailLevel::Simple => "simple",
            DetailLevel::Advanced => "advanced",
            DetailLevel::Expert => "expert",
        }
    }

    pub const fn index(self) -> usize {
        match self {
            DetailLevel::Simple => 0,
            DetailLevel::Advanced => 1,
            DetailLevel::Expert => 2,
        }
    }

    pub const fn from_index(index: usize) -> Self {
        match index {
            0 => DetailLevel::Simple,
            1 => DetailLevel::Advanced,
            _ => DetailLevel::Expert,
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "simple" => Some(DetailLevel::Simple),
            "advanced" => Some(DetailLevel::Advanced),
            "expert" => Some(DetailLevel::Expert),
            _ => None,
        }
    }

    /// The level persists next to the theme, so both frontends start in the
    /// same level and a restart does not reset it.
    pub fn path() -> Option<std::path::PathBuf> {
        directories::ProjectDirs::from("org", "ovvc", "ovvc")
            .map(|dirs| dirs.config_dir().join("detail-level"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    pub fn save(self) -> std::io::Result<()> {
        let Some(path) = Self::path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, format!("{}\n", self.label()))
    }
}

/// The tab open in the full-page settings view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsTab {
    #[default]
    User,
    VoiceVideo,
    Network,
    Preferences,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 4] = [
        SettingsTab::User,
        SettingsTab::VoiceVideo,
        SettingsTab::Network,
        SettingsTab::Preferences,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            SettingsTab::User => "user",
            SettingsTab::VoiceVideo => "voice",
            SettingsTab::Network => "network",
            SettingsTab::Preferences => "preferences",
        }
    }

    pub const fn icon(self) -> &'static str {
        match self {
            SettingsTab::User => "[U]",
            SettingsTab::VoiceVideo => "[V]",
            SettingsTab::Network => "[N]",
            SettingsTab::Preferences => "[P]",
        }
    }
}

/// One page of the full-page setup flow. Nothing is assumed: every field is
/// empty or explicitly off until the user chooses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WizardStep {
    Identity,
    Commune,
    Profile,
    Theme,
    VoiceVideo,
    Network,
    Review,
}

impl WizardStep {
    pub const ALL: [WizardStep; 7] = [
        WizardStep::Identity,
        WizardStep::Commune,
        WizardStep::Profile,
        WizardStep::Theme,
        WizardStep::VoiceVideo,
        WizardStep::Network,
        WizardStep::Review,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            WizardStep::Identity => "identity",
            WizardStep::Commune => "commune",
            WizardStep::Profile => "avatar and bio",
            WizardStep::Theme => "theme",
            WizardStep::VoiceVideo => "voice",
            WizardStep::Network => "network",
            WizardStep::Review => "review",
        }
    }

    pub const fn index(self) -> usize {
        match self {
            WizardStep::Identity => 1,
            WizardStep::Commune => 2,
            WizardStep::Profile => 3,
            WizardStep::Theme => 4,
            WizardStep::VoiceVideo => 5,
            WizardStep::Network => 6,
            WizardStep::Review => 7,
        }
    }

    pub const fn from_index(index: usize) -> Self {
        match index {
            1 => WizardStep::Identity,
            2 => WizardStep::Commune,
            3 => WizardStep::Profile,
            4 => WizardStep::Theme,
            5 => WizardStep::VoiceVideo,
            6 => WizardStep::Network,
            _ => WizardStep::Review,
        }
    }

    pub fn next(self) -> Self {
        Self::from_index((self.index() + 1).min(7))
    }

    pub fn previous(self) -> Self {
        Self::from_index(self.index().saturating_sub(1).max(1))
    }
}

/// What the setup flow does about the first commune: start one here, or join
/// one a friend already runs. It is the only step that changes what the
/// daemon creates, so it is asked once and never guessed in the background.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WizardCommune {
    #[default]
    Create,
    Join,
}

impl WizardCommune {
    pub const fn label(self) -> &'static str {
        match self {
            WizardCommune::Create => "start a commune",
            WizardCommune::Join => "join a friend",
        }
    }
}

/// The window's voice state: the call it is in, the snapshot per channel, the
/// audio counters for the local member, and the three flags it publishes.
///
/// The daemon owns the call. This only mirrors what the control snapshot and
/// the `Voice`/`VoiceAudio` events last reported, so a stale window can always
/// be corrected by the next event.
#[derive(Debug, Default)]
pub struct VoiceUi {
    /// The guild and channel of the call the local member joined, if any.
    pub guild: Option<GuildId>,
    pub channel: Option<ChannelId>,
    /// The latest call snapshot per channel in the selected guild. Calls the
    /// local member is not in are kept too, so a selected voice channel can
    /// show its roster and a JOIN button.
    pub snapshots: HashMap<ChannelId, VoiceSnapshot>,
    /// The daemon's counters for the local member's audio device.
    pub audio: Option<VoiceAudioStats>,
    /// The last WebRTC failure the daemon reported for the local call. It is
    /// kept here so the VOICE panel can show the media path's own error even
    /// when the window has moved on to another channel.
    pub error: Option<String>,
    pub muted: bool,
    pub deafened: bool,
    pub push_to_talk: bool,
}

impl VoiceUi {
    pub fn joined(&self) -> bool {
        self.channel.is_some()
    }

    /// Forget the local call. Cached snapshots stay so a channel still shows
    /// the people who remained after this member left.
    pub fn clear_call(&mut self) {
        self.guild = None;
        self.channel = None;
        self.audio = None;
        self.error = None;
    }

    /// Forget the local call and every cached snapshot, for a guild switch.
    pub fn clear_all(&mut self) {
        self.clear_call();
        self.snapshots.clear();
    }

    /// Merge a control snapshot's voice entries into the cache.
    ///
    /// The daemon keeps a separate control session per voice channel, so a
    /// snapshot normally carries one call, and the guild session carries none.
    /// A snapshot therefore adds or refreshes calls but never clears the rest,
    /// and an ended call is removed by its `Voice` event instead.
    pub fn apply_control(&mut self, calls: &[VoiceSnapshot]) {
        for call in calls {
            self.snapshots.insert(call.channel_id, call.clone());
        }
    }

    /// The last state this window read for the local member, used to preserve
    /// speaking, video and screen share when only one flag changes.
    pub fn local_state(&self, author: AuthorId) -> VoiceMemberState {
        self.channel
            .and_then(|channel| self.snapshots.get(&channel))
            .and_then(|snapshot| {
                snapshot
                    .members
                    .iter()
                    .find(|member| member.author == author)
                    .map(|member| member.state)
            })
            .unwrap_or_default()
    }

    /// The full member state to publish, keeping flags the GUI does not own.
    pub fn member_state(&self, previous: VoiceMemberState) -> VoiceMemberState {
        VoiceMemberState {
            muted: self.muted,
            deafened: self.deafened,
            push_to_talk: self.push_to_talk,
            ..previous
        }
    }
}

#[derive(Default)]
pub struct AppState {
    pub connected: bool,
    pub daemon_version: Option<String>,
    pub daemon_error: Option<String>,
    pub reconnect_attempts: u32,
    pub status: Option<DaemonStatusDto>,
    pub identity: Option<AuthorId>,
    pub public_key_hex: Option<String>,

    pub guilds: Vec<GuildMeta>,
    pub selected_guild: Option<GuildId>,
    pub categories: Vec<CategoryMeta>,
    pub channels: Vec<ChannelView>,
    pub selected_channel: Option<ChannelId>,
    pub members: Vec<MemberInfo>,
    pub members_stale: bool,

    /// Raw events for the selected channel, newest last.
    events: Vec<SignedEvent>,
    pub messages: Vec<MessageView>,
    /// The channel the loaded events belong to.
    pub loaded_channel: Option<ChannelId>,
    pub unread: HashMap<ChannelId, usize>,
    /// Whether older events exist before the loaded window.
    pub history_more: bool,
    /// A `HistoryBefore` request is in flight, so the button is not pressed
    /// twice for the same page.
    pub history_loading: bool,
    /// Position of the oldest loaded event, used as the next page cursor.
    pub history_oldest: Option<SyncCursor>,
    /// The message that was first on screen before an older page was
    /// prepended. The transcript scrolls back to it so loading history does
    /// not yank the reader somewhere else.
    pub anchor_to: Option<EventId>,
    /// A search hit the reader asked to open. The draw pass scrolls to it once
    /// the channel's history is loaded, then clears it.
    pub jump_to: Option<EventId>,
    /// How many messages were unread when the channel was opened. The
    /// transcript draws a `new messages` rule before that many messages.
    pub unread_marker: Option<usize>,

    pub search_query: String,
    pub searching: bool,
    /// The text a search was last run for. The empty-result view only shows
    /// when the field still holds that text, so typing a new query does not
    /// replace the transcript with "no matches" before Enter is pressed.
    pub searched_query: Option<String>,
    pub search_hits: Vec<SearchHit>,

    /// Files read from disk and attached to the next message this composer
    /// sends. The bytes stay here until the daemon has stored them.
    pub staged_attachments: Vec<StagedAttachment>,
    /// The files handed to the bridge for the message in flight. They leave
    /// `staged_attachments` only when the daemon confirms the post, so a
    /// refused upload or a failed post leaves the reader's files on screen.
    /// The files handed to the bridge for messages in flight, keyed by the
    /// send id the composer allocated. They leave `staged_attachments` only
    /// when the daemon confirms that particular post, so a refused upload or a
    /// failed post leaves the reader's files on screen, and two sends racing
    /// cannot clear each other's chips.
    pub(crate) pending_attachments: HashMap<u64, Vec<StagedAttachment>>,
    /// Next id for a send that carries files.
    pub next_send_id: u64,
    /// Whether the path field below the composer is open.
    pub attach_open: bool,
    pub attach_path: String,

    pub typing: HashMap<(ChannelId, AuthorId), u64>,
    pub presence: HashMap<AuthorId, PresenceStatus>,
    pub control: Option<ControlSnapshot>,
    /// The selected guild's voice calls and this window's local call state.
    pub voice: VoiceUi,
    pub session_id: Option<SessionId>,
    pub connection: Option<ConnectionStateDto>,
    /// The selected guild's policy read model, once the daemon has answered.
    /// `None` while a guild is selected but its policy has not arrived.
    pub policy: Option<PolicyView>,
    /// The guild the daemon last reported as networked, kept across a guild
    /// switch. The snapshot itself is cleared when another guild is selected,
    /// and taking that guild offline on the way to the new one needs this.
    pub online_hint: Option<GuildId>,
    /// The guild this window last asked the daemon to take online, until the
    /// daemon confirms it. The status line never reads this; it only tells the
    /// GUI to keep asking for connection state while the node starts up.
    pub connect_requested: Option<GuildId>,
    /// Listen and dial addresses as typed in the network tab. Space separated,
    /// so several addresses can be entered without extra widgets.
    pub network_listen: String,
    pub network_dial: String,
    /// The relay a node reserves a circuit on, if the person runs one. The
    /// daemon supports a relay; ovvc itself ships no relay service, so this
    /// stays empty unless the address is real.
    pub network_relay: String,
    /// The address a friend outside this network dials. The daemon reports
    /// wildcard and LAN listeners, not the router's public address, so the
    /// person enters the forwarded address here and it is put into invites.
    pub network_public: String,

    pub composer: String,
    pub reply_to: Option<(EventId, AuthorId, String)>,
    pub editing: Option<(EventId, String)>,
    pub error: Option<String>,
    /// Why the alert strip has the label and colour it has. A daemon
    /// diagnostic sets this; a local frontend message may leave it `None`.
    pub error_class: Option<ErrorClass>,
    /// The daemon log a bug-class error names.
    pub error_log: Option<String>,
    pub notice: Option<String>,

    pub show_members: bool,
    pub dialog: Dialog,
    pub dialog_input: String,
    pub invite_code: String,
    pub display_name: String,
    pub join_code: String,

    /// The member whose profile dialog is open, and the commune it came from.
    pub profile_guild: Option<GuildId>,
    pub profile_author: Option<AuthorId>,
    /// The last profile reply for that dialog.
    pub profile: Option<MemberProfile>,
    /// A `GetMember` request is in flight.
    pub profile_loading: bool,
    /// The open profile changed (or was just opened) and should be fetched.
    pub profile_stale: bool,
    /// Edit mode and the fields it owns. They are only sent after trimming and
    /// validation, so a half-typed link never leaves the window.
    pub profile_editing: bool,
    pub profile_saving: bool,
    pub profile_about: String,
    pub profile_pronouns: String,
    pub profile_links: String,
    /// A direct-message open is in flight for this peer.
    pub dm_opening: Option<AuthorId>,

    /// Direct conversations as the daemon last listed them. The daemon stores
    /// them as ordinary guilds, so they are merged into `guilds` for lookup but
    /// shown in their own sidebar section and kept out of the commune tabs.
    pub direct_messages: Vec<DirectConversation>,
    /// Unread counts for direct conversations that are not selected, so their
    /// sidebar rows can be badged without loading each guild.
    pub dm_unread: HashMap<GuildId, usize>,
    /// A profile or direct-message list change arrived; ask again next frame.
    pub direct_messages_stale: bool,

    /// The channel kind picked in the create-channel dialog.
    pub channel_kind: ChannelKind,
    /// The category picked in the create-channel dialog, `None` for the root.
    pub channel_parent: Option<ChannelId>,
    /// The channel being renamed or moved from its settings dialog.
    pub channel_target: Option<ChannelId>,
    /// Text fields for the channel settings dialog.
    pub rename_input: String,

    pub notification_settings: NotificationSettings,
    /// A settings save was refused, so the daemon holds a different picture
    /// than this window. The next frame asks for the real settings again.
    pub settings_stale: bool,
    pub own_presence: PresenceStatus,
    /// When the GUI last re-announced its presence, in the daemon's clock.
    /// Zero means nothing has been sent since the session joined.
    pub presence_sent_at_ms: u64,
    pub settings_tab: SettingsTab,
    /// Quiet-hours fields as `HH:MM` text so a half-typed value does not jump.
    pub quiet_start: String,
    pub quiet_end: String,

    /// Which full-page view owns the window.
    pub screen: ScreenView,
    /// The global detail level. Simple and advanced keep the clean product
    /// detail; expert adds one connection block per screen.
    pub detail_level: DetailLevel,
    /// The diagnostics screen's two-step shutdown confirmation. A destructive
    /// action is never one click.
    pub diagnostics_confirm_shutdown: bool,

    /// `Some` while the full-page setup flow is open.
    pub wizard_step: Option<WizardStep>,
    pub wizard_name: String,
    /// Start a commune here or join the one a friend already invited us to.
    pub wizard_commune: WizardCommune,
    pub wizard_commune_name: String,
    pub wizard_join_code: String,
    /// `None` until the user chooses a theme; the flow does not preselect one.
    pub wizard_theme_index: Option<usize>,
    /// Set once the wizard has been finished or skipped, so it does not reopen.
    pub wizard_done: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            show_members: true,
            // A fixed port makes the address in an invite reach the daemon
            // through a firewall rule, and both peers can also find each other
            // over mDNS. `/ip4/0.0.0.0/tcp/0` lets the OS pick a port instead.
            network_listen: "/ip4/0.0.0.0/tcp/41000".to_string(),
            ..Self::default()
        }
    }

    /// Whether the daemon reports `guild_id` as networked.
    ///
    /// This reads the daemon's connection snapshot, not what this window asked
    /// for, so a GUI restarted against an already-online daemon and a guild
    /// disconnected by another client both show the truth.
    pub fn is_online(&self, guild_id: GuildId) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|connection| connection.guild_id == guild_id && connection.connected)
    }

    /// How often the chosen status is re-announced.
    ///
    /// Presence in a control session is a lease, not a stored flag: a member
    /// that sends nothing for two lease windows is shown offline, and the
    /// window is 3 s wide. Six seconds into an idle window the local member,
    /// and every peer's view of it, would flip to offline while the app is
    /// still running, so the GUI re-sends its status well inside that margin.
    pub const PRESENCE_KEEPALIVE_MS: u64 = 2_000;

    /// The presence op that keeps the local lease alive, when one is due and
    /// there is a lease to keep.
    ///
    /// Only a joined control session with a networked guild has a lease, and
    /// re-announcing is a no-op for a guild that is offline.
    pub fn presence_keepalive(&mut self, now_ms: u64, guild_id: GuildId) -> Option<Op> {
        if self.control.is_none() || !self.is_online(guild_id) {
            return None;
        }
        if self.presence_sent_at_ms > 0
            && now_ms.saturating_sub(self.presence_sent_at_ms) < Self::PRESENCE_KEEPALIVE_MS
        {
            return None;
        }
        self.presence_sent_at_ms = now_ms;
        Some(Op::SetPresence {
            guild_id,
            status: self.own_presence,
        })
    }

    /// The guild the daemon is currently networking, if any, so the GUI can
    /// take it offline when another one goes online on the same port.
    pub fn online_guild(&self) -> Option<GuildId> {
        self.online_hint
    }

    /// Whether an invite for `guild_id` would carry an address a peer can dial.
    ///
    /// A wildcard bind is not dialable and a guild that is offline has no
    /// addresses at all, and the daemon will happily mint an invite from an
    /// empty list, which nobody can join. The invite button reads this.
    pub fn can_invite(&self, guild_id: GuildId) -> bool {
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| connection.guild_id == guild_id && connection.connected)
        else {
            return false;
        };
        let public = self.network_public.trim();
        let public_ok =
            !public.is_empty() && public.starts_with('/') && !is_private_listener(public);
        public_ok
            || connection
                .external_addresses
                .iter()
                .chain(connection.listen_addresses.iter())
                .any(|address| !is_private_listener(address))
    }

    /// Dialable addresses to put in an invite, best first.
    ///
    /// The explicit public or DNS address comes first, because the daemon only
    /// knows this machine's listeners and never the router's forwarded
    /// address. With it empty, the daemon's external and non-wildcard listen
    /// addresses are used, which covers a friend on the same network.
    pub fn invite_addresses(&self) -> Vec<String> {
        let mut addresses = Vec::new();
        let peer = self
            .connection
            .as_ref()
            .and_then(|connection| connection.peer_id.clone())
            .unwrap_or_default();
        let public = self.network_public.trim();
        if !public.is_empty() && public.starts_with('/') && !is_private_listener(public) {
            push_unique(&mut addresses, with_peer(public, &peer));
        }
        let Some(connection) = &self.connection else {
            return addresses;
        };
        for address in connection
            .external_addresses
            .iter()
            .chain(connection.listen_addresses.iter())
        {
            // A wildcard or loopback listener is not reachable from another
            // machine, and putting it in the invite only makes the joiner dial
            // its own loopback before the real address.
            if is_private_listener(address) {
                continue;
            }
            push_unique(&mut addresses, with_peer(address, &peer));
        }
        addresses
    }

    /// Whether a setup step has everything it needs before the flow moves on.
    pub fn wizard_step_valid(&self, step: WizardStep) -> bool {
        match step {
            WizardStep::Identity => !self.wizard_name.trim().is_empty(),
            WizardStep::Commune => match self.wizard_commune {
                WizardCommune::Create => !self.wizard_commune_name.trim().is_empty(),
                WizardCommune::Join => !self.wizard_join_code.trim().is_empty(),
            },
            _ => true,
        }
    }

    /// Connected peers as reported by the daemon for the selected guild.
    pub fn online_peers(&self) -> usize {
        self.connection
            .as_ref()
            .map(|connection| {
                connection
                    .peers
                    .iter()
                    .filter(|peer| peer.connected)
                    .count()
            })
            .unwrap_or(0)
    }

    // --- selection helpers ------------------------------------------------

    pub fn selected_guild_meta(&self) -> Option<&GuildMeta> {
        let id = self.selected_guild?;
        self.guilds.iter().find(|guild| guild.id == id)
    }

    pub fn selected_channel_meta(&self) -> Option<&ChannelView> {
        let id = self.selected_channel?;
        self.channels.iter().find(|channel| channel.id == id)
    }

    pub fn channel_view(&self, channel_id: ChannelId) -> Option<&ChannelView> {
        self.channels
            .iter()
            .find(|channel| channel.id == channel_id)
    }

    /// The live call snapshot for the selected channel, from either a `Voice`
    /// event or the control snapshot's voice entries.
    pub fn selected_voice_call(&self) -> Option<&VoiceSnapshot> {
        self.selected_channel
            .and_then(|channel| self.voice.snapshots.get(&channel))
    }

    /// Whether this window's joined call is the channel currently on screen.
    pub fn voice_joined_here(&self) -> bool {
        self.selected_channel.is_some() && self.voice.channel == self.selected_channel
    }

    /// Channels at the root of the sidebar, in display order.
    pub fn root_channels(&self) -> impl Iterator<Item = &ChannelView> {
        self.channels
            .iter()
            .filter(|channel| channel.parent.is_none())
    }

    /// Channels inside one category, in display order.
    pub fn channels_in(&self, category: ChannelId) -> impl Iterator<Item = &ChannelView> {
        self.channels
            .iter()
            .filter(move |channel| channel.parent == Some(category))
    }

    pub fn channel_kind(&self, channel_id: ChannelId) -> ChannelKind {
        self.channel_view(channel_id)
            .map(|channel| channel.kind)
            .unwrap_or_default()
    }

    /// Open the settings dialog for one channel, pre-filling its fields.
    pub fn begin_channel_settings(&mut self, channel_id: ChannelId) {
        let Some((name, parent, kind)) = self
            .channel_view(channel_id)
            .map(|channel| (channel.name.clone(), channel.parent, channel.kind))
        else {
            return;
        };
        self.channel_target = Some(channel_id);
        self.rename_input = name;
        self.channel_parent = parent;
        self.channel_kind = kind;
        self.dialog = Dialog::ChannelSettings;
    }

    pub fn is_muted_channel(&self, channel_id: ChannelId) -> bool {
        self.notification_settings
            .muted_channels
            .contains(&channel_id)
    }

    pub fn is_muted_guild(&self, guild_id: GuildId) -> bool {
        self.notification_settings.muted_guilds.contains(&guild_id)
    }

    pub fn toggle_mute_channel(&mut self, channel_id: ChannelId) {
        let muted = &mut self.notification_settings.muted_channels;
        if let Some(index) = muted.iter().position(|id| *id == channel_id) {
            muted.remove(index);
        } else {
            muted.push(channel_id);
        }
    }

    pub fn toggle_mute_guild(&mut self, guild_id: GuildId) {
        let muted = &mut self.notification_settings.muted_guilds;
        if let Some(index) = muted.iter().position(|id| *id == guild_id) {
            muted.remove(index);
        } else {
            muted.push(guild_id);
        }
    }

    /// Set a channel's muted flag to an explicit value, so the same intent can
    /// be sent to the daemon without racing another window's copy.
    pub fn set_channel_muted(&mut self, channel_id: ChannelId, muted: bool) {
        let list = &mut self.notification_settings.muted_channels;
        if muted {
            if !list.contains(&channel_id) {
                list.push(channel_id);
            }
        } else if let Some(index) = list.iter().position(|id| *id == channel_id) {
            list.remove(index);
        }
    }

    pub fn set_guild_muted(&mut self, guild_id: GuildId, muted: bool) {
        let list = &mut self.notification_settings.muted_guilds;
        if muted {
            if !list.contains(&guild_id) {
                list.push(guild_id);
            }
        } else if let Some(index) = list.iter().position(|id| *id == guild_id) {
            list.remove(index);
        }
    }

    pub fn member(&self, author: AuthorId) -> Option<&MemberInfo> {
        self.members.iter().find(|member| member.author == author)
    }

    /// A display name for an author, falling back to a short id.
    pub fn display_name(&self, author: AuthorId) -> String {
        self.member(author)
            .map(|member| member.display_name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| short_id(&author.to_string()))
    }

    pub fn unread_for(&self, channel: ChannelId) -> usize {
        self.unread.get(&channel).copied().unwrap_or(0)
    }

    pub fn is_self(&self, author: AuthorId) -> bool {
        self.identity == Some(author)
    }

    // --- profiles ----------------------------------------------------------

    /// Open the profile dialog for one member of `guild_id`.
    ///
    /// No request is sent here; `profile_stale` tells the application to ask
    /// `GetMember` on the next frame.
    pub fn open_profile(&mut self, guild_id: GuildId, author: AuthorId) {
        self.profile_guild = Some(guild_id);
        self.profile_author = Some(author);
        self.profile = None;
        self.profile_loading = false;
        self.profile_stale = true;
        self.profile_editing = false;
        self.profile_saving = false;
        self.profile_about.clear();
        self.profile_pronouns.clear();
        self.profile_links.clear();
        self.dialog = Dialog::Profile;
    }

    /// Drop the profile dialog and any draft.
    ///
    /// Clearing `profile_author` and `profile_guild` is what makes a late
    /// `GetMember` reply harmless: it no longer matches the open dialog.
    pub fn close_profile(&mut self) {
        if self.dialog == Dialog::Profile {
            self.dialog = Dialog::None;
        }
        self.profile_guild = None;
        self.profile_author = None;
        self.profile = None;
        self.profile_loading = false;
        self.profile_stale = false;
        self.profile_editing = false;
        self.profile_saving = false;
        self.dm_opening = None;
        self.profile_about.clear();
        self.profile_pronouns.clear();
        self.profile_links.clear();
    }

    /// Turn the fetched profile into editable drafts.
    pub fn begin_profile_edit(&mut self) {
        let Some(profile) = &self.profile else {
            return;
        };
        self.profile_about = profile.about.clone();
        self.profile_pronouns = profile.pronouns.clone();
        self.profile_links = profile.links.join("\n");
        self.profile_editing = true;
    }

    /// Discard the drafts and go back to the read-only profile.
    pub fn cancel_profile_edit(&mut self) {
        self.profile_editing = false;
        if let Some(profile) = &self.profile {
            self.profile_about = profile.about.clone();
            self.profile_pronouns = profile.pronouns.clone();
            self.profile_links = profile.links.join("\n");
        }
    }

    /// Validate and trim the draft into a save request.
    ///
    /// Blank link lines are dropped, each remaining link must be http(s), and
    /// the byte caps the daemon enforces are checked first. A refused draft
    /// leaves the dialog in edit mode so the reader can fix it.
    pub fn profile_save_request(&self) -> Result<Action, String> {
        let Some(guild_id) = self.profile_guild else {
            return Err("no profile is open".to_string());
        };
        let about = self.profile_about.trim().to_string();
        let pronouns = self.profile_pronouns.trim().to_string();
        if about.len() > MAX_PROFILE_ABOUT_BYTES {
            return Err(format!(
                "about is {} bytes, over the {MAX_PROFILE_ABOUT_BYTES} limit",
                about.len()
            ));
        }
        if pronouns.len() > MAX_PROFILE_PRONOUNS_BYTES {
            return Err(format!(
                "pronouns are {} bytes, over the {MAX_PROFILE_PRONOUNS_BYTES} limit",
                pronouns.len()
            ));
        }
        let mut links = Vec::new();
        for link in self.profile_links.split_whitespace() {
            let link = link.trim();
            if !link.starts_with("http://") && !link.starts_with("https://") {
                return Err(format!("{link} is not an http(s) link"));
            }
            if link.len() > MAX_PROFILE_LINK_BYTES {
                return Err(format!(
                    "one link is {} bytes, over the {MAX_PROFILE_LINK_BYTES} limit",
                    link.len()
                ));
            }
            links.push(link.to_string());
        }
        if links.len() > MAX_PROFILE_LINKS {
            return Err(format!(
                "{} links, the limit is {MAX_PROFILE_LINKS}",
                links.len()
            ));
        }
        Ok(Action::SaveProfile {
            guild_id,
            about,
            pronouns,
            links,
        })
    }

    // --- direct messages ---------------------------------------------------

    /// Whether `guild_id` is one of the direct conversations already listed.
    pub fn is_direct_guild(&self, guild_id: GuildId) -> bool {
        self.direct_messages
            .iter()
            .any(|conversation| conversation.guild.id == guild_id)
    }

    /// The direct conversation currently selected, if it is one.
    pub fn selected_dm(&self) -> Option<&DirectConversation> {
        let guild_id = self.selected_guild?;
        self.direct_messages
            .iter()
            .find(|conversation| conversation.guild.id == guild_id)
    }

    /// A direct conversation's peer name, falling back to a short author id.
    pub fn dm_name(&self, conversation: &DirectConversation) -> String {
        let name = conversation.peer.display_name.trim();
        if name.is_empty() {
            short_id(&conversation.peer.author.to_string())
        } else {
            name.to_string()
        }
    }

    pub fn dm_unread_for(&self, guild_id: GuildId) -> usize {
        self.dm_unread.get(&guild_id).copied().unwrap_or(0)
    }

    /// Clear the badge once the conversation is being read.
    pub fn mark_dm_read(&mut self, guild_id: GuildId) {
        self.dm_unread.insert(guild_id, 0);
    }

    pub fn presence_for(&self, author: AuthorId) -> PresenceStatus {
        if let Some(status) = self.presence.get(&author) {
            return *status;
        }
        // The control snapshot carries the status a peer announced on join, so
        // the panel does not have to wait for that peer's next presence frame,
        // which may never come. The live map still wins when it has an entry,
        // because it is updated by frames that arrive after the snapshot.
        self.control
            .as_ref()
            .and_then(|control| {
                control
                    .roster
                    .iter()
                    .find(|member| member.author == author)
                    .map(|member| member.presence)
            })
            .unwrap_or(PresenceStatus::Offline)
    }

    pub fn typing_in(&self, channel: ChannelId, now_ms: u64) -> Vec<AuthorId> {
        let mut authors: Vec<AuthorId> = self
            .typing
            .iter()
            .filter(|((id, author), until)| {
                *id == channel && **until > now_ms && !self.is_self(*author)
            })
            .map(|((_, author), _)| *author)
            .collect();
        authors.sort();
        authors
    }

    /// The newest message in the selected channel, if any.
    pub fn latest_event_id(&self) -> Option<EventId> {
        self.messages.last().map(|message| message.event_id)
    }

    // --- daemon replies ---------------------------------------------------

    pub fn apply_message(&mut self, message: Message) {
        match message {
            Message::Reply { tag, result } => self.apply_reply(tag, result),
            Message::Event(event) => self.apply_event(event),
            Message::DaemonState(state) => {
                self.connected = state.connected;
                self.reconnect_attempts = state.reconnect_attempts;
                if let Some(welcome) = state.daemon {
                    self.daemon_version = Some(welcome.daemon_version);
                    self.identity = Some(welcome.author_id);
                    self.public_key_hex = Some(welcome.public_key_hex);
                }
                if let Some(error) = state.last_error {
                    self.daemon_error = Some(error);
                } else if state.connected {
                    self.daemon_error = None;
                }
            }
            Message::BridgeError(error) => self.error = Some(error),
            Message::AttachmentStaged { path, result } => match result {
                Ok(staged) => {
                    self.staged_attachments.push(staged);
                    self.attach_path.clear();
                    self.attach_open = false;
                    self.notice = Some(format!(
                        "attached {} ({})",
                        self.staged_attachments
                            .last()
                            .map(|staged| staged.attachment.name.clone())
                            .unwrap_or_default(),
                        ovvc_core::human_size(
                            self.staged_attachments
                                .last()
                                .map(|staged| staged.attachment.size)
                                .unwrap_or_default(),
                        )
                    ));
                }
                Err(error) => {
                    self.error = Some(format!("could not attach {}: {error}", path.display()));
                }
            },
            Message::AttachmentSaved { name, result } => match result {
                Ok(path) => {
                    self.notice = Some(format!("saved {name} to {}", path.display()));
                    if let Err(error) = open_path(&path) {
                        self.error = Some(format!(
                            "saved {}, but could not open it: {error}",
                            path.display()
                        ));
                    }
                }
                Err(error) => self.error = Some(format!("could not fetch {name}: {error}")),
            },
        }
    }

    pub fn apply_reply(&mut self, tag: RequestTag, result: Result<Reply, IpcError>) {
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => {
                self.searching = false;
                self.history_loading = false;
                // A refused connect must stop the connection poll, otherwise
                // the GUI keeps asking a daemon that already said no.
                if matches!(tag, RequestTag::Connect(_)) {
                    self.connect_requested = None;
                }
                // The checkboxes were changed before the daemon answered, so a
                // refused save leaves this window showing a state the daemon
                // does not hold. Ask for the real settings on the next frame.
                if matches!(tag, RequestTag::SetNotificationSettings) {
                    self.settings_stale = true;
                }
                // A failed profile read must stop the loading marker without
                // clearing `profile_stale`, so it does not become a retry loop;
                // the dialog offers an explicit retry instead.
                if matches!(&tag, RequestTag::MemberProfile { .. }) {
                    self.profile_loading = false;
                }
                if matches!(&tag, RequestTag::SetProfile) {
                    self.profile_saving = false;
                }
                if matches!(&tag, RequestTag::OpenDirectMessage { .. }) {
                    self.dm_opening = None;
                }
                // A failed post keeps its files staged; the pending list only
                // exists to clear them after a post the daemon accepted.
                if let RequestTag::SendRichMessage(send_id) = tag {
                    self.pending_attachments.remove(&send_id);
                }
                self.error = Some(error.message);
                return;
            }
        };
        match (tag, reply) {
            (
                RequestTag::Identity,
                Reply::Identity {
                    author_id,
                    public_key_hex,
                    ..
                },
            ) => {
                self.identity = Some(author_id);
                self.public_key_hex = Some(public_key_hex);
            }
            (RequestTag::Status, Reply::Status(status)) => {
                self.status = Some(status);
            }
            (RequestTag::Guilds, Reply::Guilds(guilds)) => {
                // A direct conversation is an ordinary guild in the store, but
                // it must survive a guild list that was taken before it synced.
                let keep = self.selected_guild.filter(|id| {
                    guilds.iter().any(|guild| guild.id == *id)
                        || self
                            .direct_messages
                            .iter()
                            .any(|conversation| conversation.guild.id == *id)
                });
                if keep.is_none() {
                    self.selected_guild = guilds.first().map(|guild| guild.id);
                    self.policy = None;
                    self.voice.clear_all();
                    self.channels.clear();
                    self.members.clear();
                    self.selected_channel = None;
                    self.events.clear();
                    self.messages.clear();
                    self.loaded_channel = None;
                    self.reset_history_window();
                }
                self.guilds = guilds;
                let conversations = self.direct_messages.clone();
                for conversation in conversations {
                    if !self
                        .guilds
                        .iter()
                        .any(|guild| guild.id == conversation.guild.id)
                    {
                        self.guilds.push(conversation.guild);
                    }
                }
            }
            (RequestTag::Channels(guild_id), Reply::Channels(channels)) => {
                let views = channels
                    .into_iter()
                    .map(|channel| ChannelView {
                        id: channel.id,
                        guild_id: channel.guild_id,
                        name: channel.name,
                        created_by: channel.created_by,
                        created_at: channel.created_at,
                        kind: ChannelKind::Text,
                        parent: None,
                    })
                    .collect();
                self.apply_channel_tree(guild_id, Vec::new(), views);
            }
            (RequestTag::ChannelTree(guild_id), Reply::ChannelTree(tree)) => {
                self.apply_channel_tree(guild_id, tree.categories, tree.channels);
            }
            (RequestTag::CreateCategory(guild_id), Reply::Category(category)) => {
                if self.selected_guild == Some(guild_id)
                    && !self
                        .categories
                        .iter()
                        .any(|existing| existing.id == category.id)
                {
                    self.categories.push(category);
                    self.sort_sidebar();
                }
                self.dialog = Dialog::None;
                self.dialog_input.clear();
            }
            (RequestTag::NotificationSettings, Reply::NotificationSettings(settings)) => {
                self.notification_settings = settings;
                self.sync_quiet_fields();
            }
            (RequestTag::Policy(guild_id), Reply::Policy(view)) => {
                if self.selected_guild == Some(guild_id) {
                    self.policy = Some(view);
                }
            }
            (RequestTag::Members(guild_id), Reply::Members(members)) => {
                if self.selected_guild == Some(guild_id) {
                    self.members = members;
                    self.members_stale = false;
                }
            }
            (
                RequestTag::History {
                    guild_id,
                    channel_id,
                },
                Reply::Events(events),
            ) => {
                if self.selected_guild == Some(guild_id)
                    && self.selected_channel == Some(channel_id)
                {
                    self.load_page(events, false, false);
                    self.loaded_channel = Some(channel_id);
                }
            }
            (
                RequestTag::HistoryBefore {
                    guild_id,
                    channel_id,
                    cursor,
                },
                Reply::HistoryPage(page),
            ) => {
                if self.selected_guild == Some(guild_id)
                    && self.selected_channel == Some(channel_id)
                {
                    let prepend = cursor.is_some();
                    let anchor = if prepend {
                        self.messages.first().map(|message| message.event_id)
                    } else {
                        None
                    };
                    self.load_page(page.events, prepend, page.more);
                    self.loaded_channel = Some(channel_id);
                    self.history_loading = false;
                    if anchor.is_some() {
                        self.anchor_to = anchor;
                    }
                }
            }
            (RequestTag::Unread(guild_id), Reply::Unread(counts)) => {
                if self.selected_guild == Some(guild_id) {
                    self.unread = counts;
                    if self.is_direct_guild(guild_id) {
                        let total = self.unread.values().sum();
                        self.dm_unread.insert(guild_id, total);
                    }
                }
            }
            (RequestTag::SendRichMessage(send_id), Reply::Event(_)) => {
                // The daemon accepted that message, so the files it carried can
                // leave the composer. Only this send's entries go, and only if
                // they are still staged, so a file attached while the post was
                // in flight survives.
                if let Some(sent) = self.pending_attachments.remove(&send_id) {
                    self.staged_attachments
                        .retain(|staged| !sent.contains(staged));
                }
            }
            (RequestTag::Search(_), Reply::Search(hits)) => {
                self.searching = false;
                self.search_hits = hits;
            }
            (
                RequestTag::Control {
                    guild_id,
                    session_id,
                }
                | RequestTag::JoinControl {
                    guild_id,
                    session_id,
                },
                Reply::Control(snapshot),
            ) => {
                if self.selected_guild == Some(guild_id) && snapshot.session_id == session_id {
                    self.control = Some(snapshot);
                    self.session_id = Some(session_id);
                }
            }
            (RequestTag::CreateGuild, Reply::GuildCreated(guild)) => {
                if !self.guilds.iter().any(|existing| existing.id == guild.id) {
                    self.guilds.push(guild.clone());
                }
                self.selected_guild = Some(guild.id);
                self.policy = None;
                self.channels.clear();
                self.selected_channel = None;
                self.events.clear();
                self.messages.clear();
                self.loaded_channel = None;
                self.reset_history_window();
                self.dialog = Dialog::None;
            }
            (RequestTag::CreateChannel(guild_id), Reply::Channel(channel)) => {
                if self.selected_guild == Some(guild_id)
                    && !self
                        .channels
                        .iter()
                        .any(|existing| existing.id == channel.id)
                {
                    self.channels.push(ChannelView {
                        id: channel.id,
                        guild_id: channel.guild_id,
                        name: channel.name,
                        created_by: channel.created_by,
                        created_at: channel.created_at,
                        kind: self.channel_kind,
                        parent: self.channel_parent,
                    });
                    self.sort_sidebar();
                }
                self.dialog = Dialog::None;
                self.dialog_input.clear();
            }
            (RequestTag::RenameChannel(_), _) | (RequestTag::MoveChannel(_), _) => {
                self.dialog = Dialog::None;
                self.channel_target = None;
                self.rename_input.clear();
            }
            (RequestTag::JoinGuild, Reply::Guild(Some(guild))) => {
                if !self.guilds.iter().any(|existing| existing.id == guild.id) {
                    self.guilds.push(guild.clone());
                }
                self.selected_guild = Some(guild.id);
                self.policy = None;
                self.channels.clear();
                self.selected_channel = None;
                self.reset_history_window();
                self.dialog = Dialog::None;
                self.join_code.clear();
            }
            (RequestTag::JoinGuild, Reply::Guild(None)) => {
                self.error =
                    Some("the invite named a commune the daemon could not load".to_string());
            }
            (RequestTag::Connection(guild_id), Reply::Connection(connection)) => {
                if self.selected_guild == Some(guild_id) {
                    if connection.connected && connection.guild_id == guild_id {
                        // The daemon has answered for the guild that was asked.
                        self.connect_requested = None;
                    }
                    // A snapshot taken before the node finished starting can
                    // arrive after its Listening event, because the daemon
                    // handles each frame on its own task. While a connect is
                    // still pending, a "not connected" answer must not undo a
                    // connection the daemon already announced; the next poll
                    // settles it.
                    let stale_offline = !connection.connected
                        && self.connect_requested.is_some()
                        && self.connection.as_ref().is_some_and(|current| {
                            current.guild_id == guild_id && current.connected
                        });
                    if !stale_offline {
                        if connection.connected {
                            self.online_hint = Some(connection.guild_id);
                        } else if self.online_hint == Some(connection.guild_id) {
                            self.online_hint = None;
                        }
                        self.connection = Some(connection);
                    }
                }
            }
            (RequestTag::Connect(guild_id), Reply::Unit) => {
                // The daemon accepted the request. The guild only counts as
                // online once a connection snapshot or a Listening event says
                // so, so the strip says what is actually being waited on.
                self.connect_requested = Some(guild_id);
                self.notice = Some(
                    "connecting: waiting for the daemon to report a listen address".to_string(),
                );
            }
            (RequestTag::Disconnect(guild_id), Reply::Unit) => {
                self.connect_requested = None;
                self.mark_offline(guild_id);
                self.notice = Some("offline".to_string());
            }
            (RequestTag::CreateInvite(_), Reply::Invite(invite)) => match invite.encode() {
                Ok(code) => self.invite_code = code,
                Err(error) => self.error = Some(error.to_string()),
            },
            (RequestTag::MemberProfile { guild_id, author }, Reply::Member(profile)) => {
                // A reply for a closed or replaced dialog is dropped: the tag
                // no longer matches what the profile panel is showing.
                if self.profile_guild == Some(guild_id) && self.profile_author == Some(author) {
                    if !self.profile_editing {
                        self.profile_about = profile.about.clone();
                        self.profile_pronouns = profile.pronouns.clone();
                        self.profile_links = profile.links.join("\n");
                    }
                    self.profile = Some(profile);
                    self.profile_loading = false;
                    self.profile_stale = false;
                }
            }
            (RequestTag::SetProfile, Reply::Event(_)) => {
                self.profile_saving = false;
                self.profile_editing = false;
                // The daemon also emits the profile event, but asking now keeps
                // the derived counts and updated_at current without waiting for
                // it to reach this frontend.
                self.profile_stale = true;
                self.notice = Some("profile saved".to_string());
            }
            (RequestTag::OpenDirectMessage { peer }, Reply::GuildCreated(guild)) => {
                let opened_here = self.dm_opening == Some(peer);
                self.dm_opening = None;
                self.direct_messages_stale = true;
                let peer_member = self
                    .members
                    .iter()
                    .find(|member| member.author == peer)
                    .cloned();
                if !self.guilds.iter().any(|existing| existing.id == guild.id) {
                    self.guilds.push(guild.clone());
                }
                if let Some(member) = peer_member
                    && !self
                        .direct_messages
                        .iter()
                        .any(|conversation| conversation.guild.id == guild.id)
                {
                    self.direct_messages.push(DirectConversation {
                        guild: guild.clone(),
                        peer: member,
                    });
                }
                if opened_here {
                    let name = self.display_name(peer);
                    self.close_profile();
                    self.select_guild(guild.id);
                    self.notice = Some(format!("dm with {name}"));
                }
            }
            (RequestTag::DirectMessages, Reply::DirectMessages(conversations)) => {
                self.direct_messages_stale = false;
                let guilds = conversations;
                for conversation in &guilds {
                    if !self
                        .guilds
                        .iter()
                        .any(|guild| guild.id == conversation.guild.id)
                    {
                        self.guilds.push(conversation.guild.clone());
                    }
                }
                let listed: HashSet<GuildId> = guilds
                    .iter()
                    .map(|conversation| conversation.guild.id)
                    .collect();
                self.dm_unread
                    .retain(|guild_id, _| listed.contains(guild_id));
                self.direct_messages = guilds;
            }
            (RequestTag::DmUnread(guild_id), Reply::Unread(counts))
                if self.selected_guild != Some(guild_id) =>
            {
                // The selected guild's ordinary unread reply is authoritative;
                // this one only badges a row in another conversation, so a
                // stale answer cannot clear the open channel's marker.
                self.dm_unread.insert(guild_id, counts.values().sum());
            }
            (
                RequestTag::VoiceJoin {
                    guild_id,
                    channel_id,
                },
                Reply::Voice(snapshot),
            ) => {
                if self.selected_guild == Some(guild_id) {
                    match snapshot {
                        Some(snapshot) => {
                            self.voice.guild = Some(guild_id);
                            self.voice.channel = Some(channel_id);
                            // Fresh call, so the previous service's counters
                            // and any media error would describe a transport
                            // that is already gone.
                            self.voice.audio = None;
                            self.voice.error = None;
                            self.voice.snapshots.insert(channel_id, snapshot);
                        }
                        None => {
                            self.voice.snapshots.remove(&channel_id);
                        }
                    }
                }
            }
            (
                RequestTag::VoiceLeave {
                    guild_id,
                    channel_id,
                },
                Reply::Voice(snapshot),
            ) => {
                // A leave sent on the way out of a guild can answer after the
                // new guild's snapshot has replaced the cache, so ignore it
                // when that guild is no longer on screen.
                if self.selected_guild == Some(guild_id) {
                    if self.voice.guild == Some(guild_id) && self.voice.channel == Some(channel_id)
                    {
                        self.voice.clear_call();
                    }
                    match snapshot {
                        Some(snapshot) => {
                            self.voice.snapshots.insert(channel_id, snapshot);
                        }
                        None => {
                            self.voice.snapshots.remove(&channel_id);
                        }
                    }
                }
            }
            (
                RequestTag::SetVoiceState {
                    guild_id,
                    channel_id,
                },
                Reply::Voice(snapshot),
            ) => {
                // The window may have moved on before the daemon answered; the
                // snapshot belongs to a guild that is no longer on screen.
                if self.selected_guild == Some(guild_id) {
                    match snapshot {
                        Some(snapshot) if snapshot.members.is_empty() => {
                            self.voice.snapshots.remove(&channel_id);
                            if self.voice.channel == Some(channel_id) {
                                self.voice.clear_call();
                            }
                        }
                        Some(snapshot) => {
                            self.voice.snapshots.insert(channel_id, snapshot);
                        }
                        None => {
                            self.voice.snapshots.remove(&channel_id);
                            if self.voice.channel == Some(channel_id) {
                                self.voice.clear_call();
                            }
                        }
                    }
                }
            }
            (
                RequestTag::VoiceAudio {
                    guild_id,
                    channel_id,
                },
                Reply::VoiceAudio(stats),
            ) if self.selected_guild == Some(guild_id)
                && self.voice.channel == Some(channel_id) =>
            {
                self.voice.audio = stats;
            }
            _ => {}
        }
    }

    // --- channel tree -----------------------------------------------------

    fn apply_channel_tree(
        &mut self,
        guild_id: GuildId,
        categories: Vec<CategoryMeta>,
        channels: Vec<ChannelView>,
    ) {
        if self.selected_guild != Some(guild_id) {
            return;
        }
        let keep = self
            .selected_channel
            .filter(|id| channels.iter().any(|channel| channel.id == *id));
        let chosen = keep
            .or_else(|| {
                channels
                    .iter()
                    .find(|channel| channel.name == "general")
                    .map(|channel| channel.id)
            })
            .or_else(|| {
                channels
                    .iter()
                    .find(|channel| channel.kind == ChannelKind::Text)
                    .map(|channel| channel.id)
            })
            .or_else(|| channels.first().map(|channel| channel.id));
        self.categories = categories;
        self.channels = channels;
        self.sort_sidebar();
        if chosen != self.selected_channel {
            self.select_channel(chosen);
        }
        // A guild with no channels is legal only while the creation events are
        // still arriving; keep the composer hidden.
        self.members_stale = true;
    }

    /// Keep root channels first, then categories by age with their channels.
    ///
    /// This mirrors the daemon's `view_sort_key`, so a sidebar built from live
    /// events and one loaded from the store come out in the same order.
    pub fn sort_sidebar(&mut self) {
        let ranks: HashMap<ChannelId, (u64, String)> = self
            .categories
            .iter()
            .map(|category| (category.id, (category.created_at, category.name.clone())))
            .collect();
        self.channels.sort_by(|a, b| {
            let key = |channel: &ChannelView| match channel.parent {
                None => (0u8, 0u64, String::new(), channel.name.clone()),
                Some(parent) => {
                    let (created_at, name) = ranks
                        .get(&parent)
                        .cloned()
                        .unwrap_or((u64::MAX, String::new()));
                    (1, created_at, name, channel.name.clone())
                }
            };
            key(a).cmp(&key(b))
        });
        self.categories
            .sort_by(|a, b| (a.created_at, &a.name).cmp(&(b.created_at, &b.name)));
    }

    fn sync_quiet_fields(&mut self) {
        self.quiet_start = self
            .notification_settings
            .quiet_hours
            .map(|quiet| format_minute(quiet.start_minute))
            .unwrap_or_default();
        self.quiet_end = self
            .notification_settings
            .quiet_hours
            .map(|quiet| format_minute(quiet.end_minute))
            .unwrap_or_default();
    }

    /// Commit the `HH:MM` text fields into quiet hours, or clear them.
    ///
    /// The fields are not rewritten from the settings here. A half-typed value
    /// parses to `None`, and normalising at that point used to wipe the field
    /// on the first keystroke, which made the control impossible to type into.
    ///
    /// Returns whether the settings changed. A half-typed time leaves the
    /// daemon's quiet hours alone, so typing `22:0` on the way to `22:00` does
    /// not switch quiet hours off behind the reader. Clearing both fields is
    /// still an explicit way to disable them.
    pub fn commit_quiet_hours(&mut self) -> bool {
        if self.quiet_start.trim().is_empty() && self.quiet_end.trim().is_empty() {
            let changed = self.notification_settings.quiet_hours.is_some();
            self.notification_settings.quiet_hours = None;
            return changed;
        }
        let start = parse_minute(&self.quiet_start);
        let end = parse_minute(&self.quiet_end);
        match (start, end) {
            (Some(start), Some(end)) if start != end => {
                self.notification_settings.quiet_hours = Some(QuietHours {
                    start_minute: start,
                    end_minute: end,
                });
                true
            }
            // A window that starts and ends at the same minute is no window at
            // all, which is how the fields used to clear themselves.
            (Some(start), Some(end)) if start == end => {
                let changed = self.notification_settings.quiet_hours.is_some();
                self.notification_settings.quiet_hours = None;
                changed
            }
            _ => false,
        }
    }

    // --- live events ------------------------------------------------------

    /// Record a classified diagnostic for the alert strip.
    ///
    /// The daemon names media failures with several prefixes: the start-up
    /// path says WebRTC, while the manager reports `peer <id> connection
    /// failed` and rejects candidates and invalid RTP. Keep one for the VOICE
    /// panel, because the generic alert strip is dismissed too easily to
    /// explain a call that connected the control session but no audio.
    fn apply_diagnostic(&mut self, diagnostic: Diagnostic) {
        if is_voice_media_error(&diagnostic.message) && self.voice.channel.is_some() {
            self.voice.error = Some(diagnostic.message.clone());
        }
        self.error_class = Some(diagnostic.class);
        self.error_log = diagnostic.log_path;
        self.error = Some(diagnostic.message);
    }

    pub fn apply_event(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::Connected {
                daemon_version,
                author_id,
            } => {
                self.connected = true;
                self.daemon_error = None;
                self.daemon_version = Some(daemon_version);
                self.identity = Some(author_id);
            }
            DaemonEvent::Disconnected { reason } => {
                self.connected = false;
                // The snapshot came from a daemon that is no longer answering,
                // so keeping its addresses on screen would claim a connection
                // the GUI cannot see. It is refetched on reconnect.
                self.connection = None;
                self.connect_requested = None;
                self.online_hint = None;
                self.presence_sent_at_ms = 0;
                // A request in flight when the daemon went away is not coming
                // back on this connection, so clear its marker rather than
                // leaving a dialog waiting forever.
                self.profile_loading = false;
                self.profile_saving = false;
                self.dm_opening = None;
                // The call lived in the daemon that just went away.
                self.voice.clear_all();
                self.daemon_error = Some(reason);
            }
            DaemonEvent::Event {
                guild_id,
                event,
                local,
            } => self.apply_signed_event(guild_id, event, local),
            DaemonEvent::Presence {
                guild_id,
                author,
                status,
            } => {
                if self.selected_guild == Some(guild_id) {
                    self.presence.insert(author, status);
                }
            }
            DaemonEvent::Typing {
                guild_id,
                channel_id,
                author,
                until_ms,
            } => {
                if self.selected_guild == Some(guild_id) {
                    self.typing.insert((channel_id, author), until_ms);
                }
            }
            DaemonEvent::TypingEnded {
                guild_id,
                channel_id,
                author,
            } => {
                if self.selected_guild == Some(guild_id) {
                    self.typing.remove(&(channel_id, author));
                }
            }
            DaemonEvent::ControlState { guild_id, snapshot } => {
                let voices = snapshot.voice.clone();
                if self.selected_guild == Some(guild_id) {
                    // Voice calls live in their own per-channel sessions, and
                    // the daemon emits a `ControlState` for those too. Only the
                    // guild session's snapshot may replace the roster and
                    // session id this window tracks.
                    if snapshot.session_id == SessionId::for_guild(guild_id) {
                        self.session_id = Some(snapshot.session_id);
                        self.control = Some(snapshot);
                    }
                    // The voice session's snapshot is the call state for its
                    // channel, so it can fill the panel before the first
                    // per-channel `Voice` event.
                    self.voice.apply_control(&voices);
                }
            }
            DaemonEvent::Listening { guild_id, address } => {
                // `connect_requested` stays set until a connection snapshot
                // confirms the guild is networked, because that snapshot is
                // also what carries the peer id an invite needs.
                if self.selected_guild == Some(guild_id) {
                    // The "connecting" strip has served its purpose.
                    self.notice = None;
                }
                self.update_connection(guild_id, |connection| {
                    connection.connected = true;
                    push_unique(&mut connection.listen_addresses, address);
                });
            }
            DaemonEvent::ExternalAddr { guild_id, address } => {
                self.update_connection(guild_id, |connection| {
                    push_unique(&mut connection.external_addresses, address);
                });
            }
            DaemonEvent::ExternalAddrRemoved { guild_id, address } => {
                self.update_connection(guild_id, |connection| {
                    connection
                        .external_addresses
                        .retain(|item| item != &address);
                });
            }
            DaemonEvent::PeerConnected {
                guild_id,
                peer,
                author,
            } => {
                self.update_connection(guild_id, |connection| {
                    if let Some(existing) =
                        connection.peers.iter_mut().find(|item| item.peer == peer)
                    {
                        existing.connected = true;
                        existing.author = author;
                    } else {
                        connection.peers.push(PeerSnapshot {
                            peer,
                            author,
                            connected: true,
                            addresses: Vec::new(),
                        });
                    }
                });
            }
            DaemonEvent::PeerDisconnected { guild_id, peer, .. } => {
                self.update_connection(guild_id, |connection| {
                    connection.peers.retain(|item| item.peer != peer);
                });
            }
            DaemonEvent::Voice {
                guild_id,
                channel_id,
                snapshot,
                ..
            } => {
                // Another guild's call cannot belong to this window; the
                // selected guild's snapshot fills in when it is selected.
                if self.selected_guild == Some(guild_id) {
                    match snapshot {
                        Some(snapshot) if snapshot.members.is_empty() => {
                            self.voice.snapshots.remove(&channel_id);
                            if self.voice.channel == Some(channel_id) {
                                self.voice.clear_call();
                            }
                        }
                        Some(snapshot) => {
                            let contains_local = self.identity.is_some_and(|author| {
                                snapshot
                                    .members
                                    .iter()
                                    .any(|member| member.author == author)
                            });
                            self.voice.snapshots.insert(channel_id, snapshot);
                            // The daemon timed this member out or another
                            // frontend left the call for us. Either way this
                            // window must stop claiming it is still in it.
                            if self.voice.channel == Some(channel_id) && !contains_local {
                                self.voice.clear_call();
                            }
                        }
                        None => {
                            self.voice.snapshots.remove(&channel_id);
                            if self.voice.channel == Some(channel_id) {
                                self.voice.clear_call();
                            }
                        }
                    }
                }
            }
            DaemonEvent::VoiceAudio {
                guild_id,
                channel_id,
                stats,
            } => {
                // The counters describe the daemon's device for the local
                // member, so they only apply to the call this window joined.
                if self.selected_guild == Some(guild_id) && self.voice.channel == Some(channel_id) {
                    self.voice.audio = Some(stats);
                }
            }
            DaemonEvent::Synced { .. }
            | DaemonEvent::HolePunch { .. }
            | DaemonEvent::Control { .. } => {}
            DaemonEvent::Notification {
                guild_id,
                channel_id,
                author,
                body,
                reason,
                ..
            } => {
                if self.selected_guild == Some(guild_id) {
                    // The matching `Event` already bumped the unread count, so
                    // this only supplies the status-line hint.
                    let who = self.display_name(author);
                    let what = match reason {
                        NotificationReason::Mention => "mentioned you",
                        NotificationReason::Reply => "replied to you",
                    };
                    let channel = self
                        .channels
                        .iter()
                        .find(|channel| channel.id == channel_id)
                        .map(|channel| channel.name.clone())
                        .unwrap_or_else(|| short_id(&channel_id.to_string()));
                    self.notice = Some(format!(
                        "{who} {what} in #{channel}: {}",
                        notice_excerpt(&body)
                    ));
                }
            }
            DaemonEvent::Error { guild_id, message }
                if guild_id.is_none() || guild_id == self.selected_guild =>
            {
                // A legacy daemon has no class. Treat its error as an
                // operational warning rather than a bug and let the alert
                // strip label it accordingly.
                self.apply_diagnostic(Diagnostic::operational(message).with_guild(guild_id));
            }
            DaemonEvent::Diagnostic(diagnostic)
                if diagnostic.guild_id.is_none() || diagnostic.guild_id == self.selected_guild =>
            {
                self.apply_diagnostic(diagnostic);
            }
            // `DaemonEvent` is non-exhaustive; a newer daemon may send a
            // variant this build does not understand.
            _ => {}
        }
    }

    fn apply_signed_event(&mut self, guild_id: GuildId, event: SignedEvent, local: bool) {
        if event.body.guild_id != guild_id {
            return;
        }
        let in_guild = self.selected_guild == Some(guild_id);
        // A profile event is an ordinary signed event, so the profile panel
        // refetches when one lands for the member it is showing.
        if matches!(&event.body.kind, EventKind::MemberProfileUpdated { .. })
            && self.profile_guild == Some(guild_id)
            && self.profile_author == Some(event.body.author)
        {
            self.profile_stale = true;
        }
        // A direct-message announcement, a newly imported DM guild, or a
        // rename in a DM must refresh the separate DM list.
        if matches!(
            &event.body.kind,
            EventKind::DirectMessageOpened { .. } | EventKind::GuildCreated { .. }
        ) || (self.is_direct_guild(guild_id)
            && matches!(
                &event.body.kind,
                EventKind::MemberJoined { .. } | EventKind::MemberRenamed { .. }
            ))
        {
            self.direct_messages_stale = true;
        }
        match &event.body.kind {
            EventKind::GuildCreated { name } => {
                if !self.guilds.iter().any(|guild| guild.id == guild_id) {
                    self.guilds.push(GuildMeta {
                        id: guild_id,
                        name: name.clone(),
                        founder: event.body.author,
                        created_at: event.body.timestamp,
                    });
                }
            }
            EventKind::ChannelCreated { name } => {
                if in_guild
                    && let Some(channel_id) = event.body.channel_id
                    && !self.channels.iter().any(|channel| channel.id == channel_id)
                {
                    self.channels.push(ChannelView {
                        id: channel_id,
                        guild_id,
                        name: name.clone(),
                        created_by: event.body.author,
                        created_at: event.body.timestamp,
                        kind: ChannelKind::Text,
                        parent: None,
                    });
                    self.sort_sidebar();
                }
            }
            EventKind::ChannelCreatedV2 { name, kind, parent } => {
                if in_guild && let Some(channel_id) = event.body.channel_id {
                    match self.channels.iter_mut().find(|c| c.id == channel_id) {
                        Some(channel) => {
                            channel.name = name.clone();
                            channel.kind = *kind;
                            channel.parent = *parent;
                        }
                        None => self.channels.push(ChannelView {
                            id: channel_id,
                            guild_id,
                            name: name.clone(),
                            created_by: event.body.author,
                            created_at: event.body.timestamp,
                            kind: *kind,
                            parent: *parent,
                        }),
                    }
                    self.sort_sidebar();
                }
            }
            EventKind::CategoryCreated { name } => {
                if in_guild
                    && let Some(category_id) = event.body.channel_id
                    && !self.categories.iter().any(|c| c.id == category_id)
                {
                    self.categories.push(CategoryMeta {
                        id: category_id,
                        guild_id,
                        name: name.clone(),
                        created_by: event.body.author,
                        created_at: event.body.timestamp,
                    });
                    self.sort_sidebar();
                }
            }
            EventKind::ChannelRenamed { target, name } if in_guild => {
                if let Some(channel) = self.channels.iter_mut().find(|c| c.id == *target) {
                    channel.name = name.clone();
                    self.sort_sidebar();
                }
            }
            EventKind::ChannelMoved { target, parent } if in_guild => {
                if let Some(channel) = self.channels.iter_mut().find(|c| c.id == *target) {
                    channel.parent = *parent;
                    self.sort_sidebar();
                }
            }
            EventKind::MemberJoined { display_name } if in_guild => {
                if !self
                    .members
                    .iter()
                    .any(|member| member.author == event.body.author)
                {
                    self.members.push(MemberInfo {
                        guild_id,
                        author: event.body.author,
                        display_name: display_name.clone(),
                        joined_at: event.body.timestamp,
                    });
                }
                self.members_stale = true;
            }
            EventKind::MemberRenamed { display_name } if in_guild => {
                if let Some(member) = self
                    .members
                    .iter_mut()
                    .find(|member| member.author == event.body.author)
                {
                    member.display_name = display_name.clone();
                }
            }
            _ => {}
        }

        let kind = &event.body.kind;
        let in_timeline = kind.is_message()
            || matches!(
                kind,
                EventKind::MessageEdited { .. } | EventKind::MessageDeleted { .. }
            )
            || kind.reaction().is_some();
        if !in_timeline {
            return;
        }
        let Some(channel_id) = event.body.channel_id else {
            return;
        };
        let same_channel = in_guild && self.selected_channel == Some(channel_id);
        if same_channel {
            self.push_event(event);
        } else if !local && kind.is_message() {
            *self.unread.entry(channel_id).or_insert(0) += 1;
            // The channel map only belongs to the selected guild, so a DM in
            // another guild keeps its own badge count here.
            if self.is_direct_guild(guild_id) {
                *self.dm_unread.entry(guild_id).or_insert(0) += 1;
            }
        }
    }

    fn mark_offline(&mut self, guild_id: GuildId) {
        self.presence_sent_at_ms = 0;
        if self.online_hint == Some(guild_id) {
            self.online_hint = None;
        }
        if self.connect_requested == Some(guild_id) {
            self.connect_requested = None;
        }
        if let Some(connection) = self.connection.as_mut()
            && connection.guild_id == guild_id
        {
            connection.connected = false;
            connection.listen_addresses.clear();
            connection.external_addresses.clear();
            connection.peers.clear();
        }
    }

    fn update_connection(
        &mut self,
        guild_id: GuildId,
        update: impl FnOnce(&mut ConnectionStateDto),
    ) {
        if self.selected_guild != Some(guild_id) {
            return;
        }
        if self.connection.is_none() {
            self.connection = Some(ConnectionStateDto {
                guild_id,
                connected: false,
                peer_id: None,
                listen_addresses: Vec::new(),
                external_addresses: Vec::new(),
                peers: Vec::new(),
            });
        }
        if let Some(connection) = self.connection.as_mut()
            && connection.guild_id == guild_id
        {
            update(connection);
        }
    }

    // --- channel state ----------------------------------------------------

    pub fn select_channel(&mut self, channel: Option<ChannelId>) {
        if channel == self.selected_channel {
            return;
        }
        let marker = channel
            .map(|id| self.unread.get(&id).copied().unwrap_or(0))
            .filter(|count| *count > 0);
        self.selected_channel = channel;
        self.events.clear();
        self.messages.clear();
        self.loaded_channel = None;
        self.history_more = false;
        self.history_loading = false;
        self.history_oldest = None;
        self.anchor_to = None;
        self.jump_to = None;
        self.unread_marker = marker;
        self.reply_to = None;
        self.editing = None;
        self.search_query.clear();
        self.search_hits.clear();
        self.staged_attachments.clear();
        self.pending_attachments.clear();
        self.attach_open = false;
        self.attach_path.clear();
        if let Some(channel) = channel {
            self.unread.insert(channel, 0);
        }
    }

    /// Drop the find text and its results, returning the transcript to view.
    ///
    /// `select_channel` does this only when the channel changes, so a hit in
    /// the channel already open would otherwise leave the results list up and
    /// the jump unread.
    pub fn clear_search(&mut self) {
        self.search_query.clear();
        self.searched_query = None;
        self.search_hits.clear();
        self.searching = false;
    }

    /// Point the transcript at a search hit, closing the results list first.
    ///
    /// The caller loads the channel; this only sets the target the draw pass
    /// scrolls to and clears it after it has landed.
    pub fn open_search_hit(&mut self, channel_id: ChannelId, event_id: EventId) {
        self.select_channel(Some(channel_id));
        self.clear_search();
        self.jump_to = Some(event_id);
    }

    pub fn select_guild(&mut self, guild_id: GuildId) {
        if self.selected_guild == Some(guild_id) {
            return;
        }
        self.close_profile();
        self.selected_guild = Some(guild_id);
        self.channels.clear();
        self.members.clear();
        self.members_stale = false;
        self.control = None;
        self.session_id = None;
        self.connection = None;
        self.policy = None;
        self.connect_requested = None;
        self.presence_sent_at_ms = 0;
        self.presence.clear();
        self.typing.clear();
        // The call belongs to the guild that just left the screen. The daemon
        // keeps it running until a leave frame arrives, which the caller sends
        // before switching; this window stops claiming it immediately.
        self.voice.clear_all();
        self.select_channel(None);
    }

    /// Install a page of history. An older page is prepended and de-duplicated
    /// against the window already in memory; a fresh load replaces it.
    fn load_page(&mut self, events: Vec<SignedEvent>, prepend: bool, more: bool) {
        if prepend {
            let mut older = events;
            older.retain(|event| {
                let Ok(id) = event.id() else {
                    return false;
                };
                !self
                    .events
                    .iter()
                    .any(|existing| existing.id().ok() == Some(id))
            });
            older.append(&mut self.events);
            self.events = older;
        } else {
            self.events = events;
        }
        self.history_more = more;
        self.update_oldest();
        self.refold();
    }

    fn update_oldest(&mut self) {
        self.history_oldest = self
            .events
            .first()
            .and_then(|event| SyncCursor::after(event).ok());
    }

    fn reset_history_window(&mut self) {
        self.history_more = false;
        self.history_loading = false;
        self.history_oldest = None;
        self.anchor_to = None;
        self.unread_marker = None;
    }

    fn push_event(&mut self, event: SignedEvent) {
        let Ok(id) = event.id() else {
            return;
        };
        if self
            .events
            .iter()
            .any(|existing| existing.id().ok() == Some(id))
        {
            return;
        }
        // Bound the in-memory window; the full history stays on disk and is
        // reloaded with `HistoryBefore` when the reader asks for it.
        if self.events.len() >= 2_000 {
            self.events.remove(0);
        }
        self.events.push(event);
        self.update_oldest();
        self.refold();
    }

    fn refold(&mut self) {
        self.messages = fold_messages(&self.events);
    }

    /// The members mentioned by a composer body, resolved against the roster.
    pub fn mentions_in(&self, body: &str) -> Vec<AuthorId> {
        parse_mentions(body, &self.members)
    }
}

/// Turn raw channel events into renderable messages.
pub fn fold_messages(events: &[SignedEvent]) -> Vec<MessageView> {
    let Ok(entries) = timeline(events) else {
        return Vec::new();
    };
    let mut messages = Vec::new();
    for entry in entries {
        match entry {
            TimelineEntry::Message {
                event,
                body,
                edited,
                reply_to,
                mentions,
                reactions,
            } => {
                let Ok(event_id) = event.id() else {
                    continue;
                };
                messages.push(MessageView {
                    event_id,
                    author: event.body.author,
                    timestamp: event.body.timestamp,
                    body,
                    edited,
                    deleted: false,
                    reply_to: reply_to.map(|reference| ReplyPreview {
                        author: reference.event.body.author,
                        body: reference.body.clone(),
                    }),
                    mentions: mentions.to_vec(),
                    attachments: event.body.kind.attachments().to_vec(),
                    reactions: reactions
                        .into_iter()
                        .map(|reaction| ReactionView {
                            emoji: reaction.emoji,
                            authors: reaction.authors,
                        })
                        .collect(),
                });
            }
            TimelineEntry::Deleted { event } => {
                let Ok(event_id) = event.id() else {
                    continue;
                };
                messages.push(MessageView {
                    event_id,
                    author: event.body.author,
                    timestamp: event.body.timestamp,
                    body: String::new(),
                    edited: false,
                    deleted: true,
                    reply_to: None,
                    mentions: Vec::new(),
                    attachments: Vec::new(),
                    reactions: Vec::new(),
                });
            }
            TimelineEntry::Other(_) => {}
        }
    }
    messages
}

/// Render a minutes-past-midnight value as `HH:MM`.
pub fn format_minute(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

/// Parse `HH:MM` into minutes past midnight, rejecting nonsense.
pub fn parse_minute(text: &str) -> Option<u16> {
    let (hours, minutes) = text.trim().split_once(':')?;
    let hours: u16 = hours.trim().parse().ok()?;
    let minutes: u16 = minutes.trim().parse().ok()?;
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

/// Split a text field into multiaddrs. Whitespace separates several addresses
/// so the network tab needs one line per list rather than a row of fields.
///
/// Callers must run [`address_problem`] first: silently dropping a token such
/// as `192.0.2.5:41000` would turn a listen request into an empty list, and
/// the daemon reads an empty list as "bind an ephemeral wildcard port".
pub fn parse_addresses(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|address| address.starts_with('/'))
        .map(ToString::to_string)
        .collect()
}

/// The first token that is not a multiaddress, phrased for the error strip.
pub fn address_problem(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|token| !token.starts_with('/'))
        .map(|token| {
            format!(
                "{token} is not a multiaddress, write it like /ip4/0.0.0.0/tcp/41000 or leave the field empty"
            )
        })
}

pub fn short_id(id: &str) -> String {
    let visible: String = id.chars().take(8).collect();
    visible
}

/// Bound a notification body so it fits on the status line.
fn notice_excerpt(body: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut chars = body.chars();
    let excerpt: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{excerpt}...")
    } else {
        excerpt
    }
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}

/// Whether an address is a wildcard or loopback listener: fine to bind, but
/// meaningless to a peer on another machine, so it is left out of an invite.
fn is_private_listener(address: &str) -> bool {
    address.contains("/ip4/0.0.0.0/")
        || address.contains("/ip6/::/")
        || address.contains("/ip4/127.")
        || address.contains("/ip6/::1/")
}

/// Attach the local peer id to a plain address so a joiner knows who to dial.
/// An address that already names a peer is left alone.
fn with_peer(address: &str, peer: &str) -> String {
    if address.contains("/p2p/") || peer.is_empty() {
        address.to_string()
    } else {
        format!("{address}/p2p/{peer}")
    }
}

/// Authors the state has seen anywhere, for tests and small summaries.
pub fn known_authors(messages: &[MessageView]) -> HashSet<AuthorId> {
    messages.iter().map(|message| message.author).collect()
}

/// Open a saved file with the desktop's own handler, without a shell.
///
/// `spawn` returns as soon as the handler is started, so a viewer that takes a
/// second to appear cannot stall the frame. The path is passed as one argument,
/// never interpolated into a command line.
pub fn open_path(path: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Whether a daemon error describes a failure of the call's media path.
///
/// The manager's runtime failures do not all say "WebRTC": a peer that dies
/// mid-call reports `peer <id> connection failed`, rejected candidates and
/// invalid RTP use their own prefixes, and a media-key or decryption failure
/// explains why a connected call carries no audio. Those belong in the VOICE
/// panel too.
fn is_voice_media_error(message: &str) -> bool {
    message.contains("WebRTC")
        || message.contains("media key")
        || message.contains("decrypt")
        || message.starts_with("peer ")
        || message.starts_with("candidate from ")
        || (message.starts_with("dropped ") && message.contains("RTP packets"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_control::PresenceStatus;
    use ovvc_core::EventBody;
    use ovvc_core::EventKind;
    use ovvc_core::Identity;

    fn identity() -> Identity {
        Identity::from_seed([7u8; 32])
    }

    fn message_event(guild: GuildId, channel: ChannelId, body: &str, seq: u64) -> SignedEvent {
        let id = identity();
        identity()
            .sign_body(EventBody {
                version: 2,
                guild_id: guild,
                channel_id: Some(channel),
                author: id.author_id(),
                seq,
                lamport: seq,
                timestamp: 1_700_000_000 + seq,
                prev: None,
                kind: EventKind::Message {
                    body: body.to_string(),
                },
            })
            .unwrap()
    }

    fn guild() -> GuildMeta {
        GuildMeta {
            id: GuildId([1; 32]),
            name: "guild".to_string(),
            founder: Identity::from_seed([7u8; 32]).author_id(),
            created_at: 1,
        }
    }

    fn channel() -> ChannelView {
        ChannelView {
            id: ChannelId([2; 16]),
            guild_id: GuildId([1; 32]),
            name: "general".to_string(),
            created_by: Identity::from_seed([7u8; 32]).author_id(),
            created_at: 1,
            kind: ChannelKind::Text,
            parent: None,
        }
    }

    #[test]
    fn changing_guild_outside_select_guild_drops_the_policy_cache() {
        let mut state = AppState::new();
        state.selected_guild = Some(GuildId([1; 32]));
        fn standard_view() -> PolicyView {
            let standard = ovvc_core::GuildPolicy::standard();
            PolicyView {
                guild: standard.clone(),
                client_floor: ovvc_core::ClientPolicyFloor::default(),
                effective: standard,
                raised_by_client: Vec::new(),
            }
        }

        state.policy = Some(standard_view());
        let mut created = guild();
        created.id = GuildId([2; 32]);
        state.apply_reply(
            RequestTag::CreateGuild,
            Ok(Reply::GuildCreated(created.clone())),
        );
        assert_eq!(state.selected_guild, Some(created.id));
        assert!(
            state.policy.is_none(),
            "creating a guild must drop the previous guild's policy"
        );

        state.policy = Some(standard_view());
        let mut joined = guild();
        joined.id = GuildId([3; 32]);
        state.apply_reply(
            RequestTag::JoinGuild,
            Ok(Reply::Guild(Some(joined.clone()))),
        );
        assert_eq!(state.selected_guild, Some(joined.id));
        assert!(
            state.policy.is_none(),
            "joining a guild must drop the previous guild's policy"
        );

        state.policy = Some(standard_view());
        state.apply_reply(RequestTag::Guilds, Ok(Reply::Guilds(Vec::new())));
        assert_eq!(state.selected_guild, None);
        assert!(
            state.policy.is_none(),
            "the guild-list fallback must drop the policy too"
        );
    }

    #[test]
    fn guilds_reply_selects_the_first_guild_and_default_channel() {
        let mut state = AppState::new();
        state.apply_reply(RequestTag::Guilds, Ok(Reply::Guilds(vec![guild()])));
        assert_eq!(state.selected_guild, Some(GuildId([1; 32])));
        state.apply_reply(
            RequestTag::ChannelTree(GuildId([1; 32])),
            Ok(Reply::ChannelTree(ovvc_ipc::ChannelTreeDto {
                categories: Vec::new(),
                channels: vec![channel()],
            })),
        );
        assert_eq!(state.selected_channel, Some(ChannelId([2; 16])));
    }

    #[test]
    fn history_folds_into_messages_and_keeps_order() {
        let mut state = AppState::new();
        state.selected_guild = Some(GuildId([1; 32]));
        state.selected_channel = Some(ChannelId([2; 16]));
        let events = vec![
            message_event(GuildId([1; 32]), ChannelId([2; 16]), "one", 1),
            message_event(GuildId([1; 32]), ChannelId([2; 16]), "two", 2),
        ];
        state.apply_reply(
            RequestTag::History {
                guild_id: GuildId([1; 32]),
                channel_id: ChannelId([2; 16]),
            },
            Ok(Reply::Events(events)),
        );
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0].body, "one");
        assert_eq!(state.messages[1].body, "two");
    }

    #[test]
    fn an_older_page_is_prepended_after_the_anchor() {
        let mut state = AppState::new();
        let guild_id = GuildId([1; 32]);
        let channel_id = ChannelId([2; 16]);
        state.selected_guild = Some(guild_id);
        state.selected_channel = Some(channel_id);

        let older = message_event(guild_id, channel_id, "older", 1);
        let middle = message_event(guild_id, channel_id, "middle", 2);
        let newest = message_event(guild_id, channel_id, "newest", 3);
        state.apply_reply(
            RequestTag::HistoryBefore {
                guild_id,
                channel_id,
                cursor: None,
            },
            Ok(Reply::HistoryPage(ovvc_core::EventPage {
                events: vec![middle.clone(), newest.clone()],
                more: true,
            })),
        );
        assert_eq!(state.messages.len(), 2);
        assert!(state.history_more);
        let anchor = state.messages[0].event_id;
        let cursor = state.history_oldest.expect("the oldest event is a cursor");
        assert_eq!(cursor, SyncCursor::at(&middle).unwrap());

        state.history_loading = true;
        state.apply_reply(
            RequestTag::HistoryBefore {
                guild_id,
                channel_id,
                cursor: Some(cursor),
            },
            Ok(Reply::HistoryPage(ovvc_core::EventPage {
                // The duplicate must not appear twice in the transcript.
                events: vec![older.clone(), middle.clone()],
                more: false,
            })),
        );
        let bodies: Vec<&str> = state
            .messages
            .iter()
            .map(|message| message.body.as_str())
            .collect();
        assert_eq!(bodies, ["older", "middle", "newest"]);
        assert_eq!(state.anchor_to, Some(anchor));
        assert!(!state.history_more);
        assert!(!state.history_loading);
        assert_eq!(state.history_oldest, Some(SyncCursor::at(&older).unwrap()));
    }

    #[test]
    fn opening_a_channel_remembers_the_unread_marker() {
        let mut state = AppState::new();
        let channel_id = ChannelId([2; 16]);
        state.unread.insert(channel_id, 3);
        state.select_channel(Some(channel_id));
        assert_eq!(state.unread_marker, Some(3));
        assert_eq!(state.unread_for(channel_id), 0);
        state.select_channel(None);
        assert_eq!(state.unread_marker, None);
    }

    #[test]
    fn stale_channel_replies_are_ignored() {
        let mut state = AppState::new();
        state.selected_guild = Some(GuildId([1; 32]));
        state.selected_channel = Some(ChannelId([3; 16]));
        state.apply_reply(
            RequestTag::History {
                guild_id: GuildId([1; 32]),
                channel_id: ChannelId([2; 16]),
            },
            Ok(Reply::Events(vec![message_event(
                GuildId([1; 32]),
                ChannelId([2; 16]),
                "old",
                1,
            )])),
        );
        assert!(state.messages.is_empty());
    }

    #[test]
    fn remote_messages_in_other_channels_bump_unread_only() {
        let mut state = AppState::new();
        state.selected_guild = Some(GuildId([1; 32]));
        state.selected_channel = Some(ChannelId([2; 16]));
        let event = message_event(GuildId([1; 32]), ChannelId([3; 16]), "elsewhere", 1);
        state.apply_event(DaemonEvent::Event {
            guild_id: GuildId([1; 32]),
            event: event.clone(),
            local: false,
        });
        assert_eq!(state.unread_for(ChannelId([3; 16])), 1);
        state.apply_event(DaemonEvent::Event {
            guild_id: GuildId([1; 32]),
            event,
            local: true,
        });
        assert_eq!(state.unread_for(ChannelId([3; 16])), 1);
    }

    #[test]
    fn local_messages_in_the_open_channel_are_appended_once() {
        let mut state = AppState::new();
        state.selected_guild = Some(GuildId([1; 32]));
        state.selected_channel = Some(ChannelId([2; 16]));
        let event = message_event(GuildId([1; 32]), ChannelId([2; 16]), "hi", 1);
        for _ in 0..3 {
            state.apply_event(DaemonEvent::Event {
                guild_id: GuildId([1; 32]),
                event: event.clone(),
                local: true,
            });
        }
        assert_eq!(state.messages.len(), 1);
    }

    fn kind_event(kind: EventKind, seq: u64) -> SignedEvent {
        let id = identity();
        identity()
            .sign_body(EventBody {
                version: 2,
                guild_id: GuildId([1; 32]),
                channel_id: Some(ChannelId([2; 16])),
                author: id.author_id(),
                seq,
                lamport: seq,
                timestamp: 1_700_000_000 + seq,
                prev: None,
                kind,
            })
            .unwrap()
    }

    #[test]
    fn live_reactions_edits_and_deletions_fold_into_the_open_channel() {
        let mut state = AppState::new();
        state.selected_guild = Some(GuildId([1; 32]));
        state.selected_channel = Some(ChannelId([2; 16]));
        let message = kind_event(
            EventKind::Message {
                body: "original".to_string(),
            },
            1,
        );
        let target = message.id().unwrap();
        state.apply_event(DaemonEvent::Event {
            guild_id: GuildId([1; 32]),
            event: message,
            local: true,
        });

        state.apply_event(DaemonEvent::Event {
            guild_id: GuildId([1; 32]),
            event: kind_event(
                EventKind::ReactionAdded {
                    target,
                    emoji: "x".to_string(),
                },
                2,
            ),
            local: true,
        });
        assert_eq!(state.messages[0].reactions.len(), 1);

        state.apply_event(DaemonEvent::Event {
            guild_id: GuildId([1; 32]),
            event: kind_event(
                EventKind::MessageEdited {
                    target,
                    body: "edited".to_string(),
                },
                3,
            ),
            local: true,
        });
        assert!(state.messages[0].edited);
        assert_eq!(state.messages[0].body, "edited");

        state.apply_event(DaemonEvent::Event {
            guild_id: GuildId([1; 32]),
            event: kind_event(EventKind::MessageDeleted { target }, 4),
            local: true,
        });
        assert!(state.messages[0].deleted);
    }

    #[test]
    fn reactions_to_unknown_targets_are_ignored() {
        let mut state = AppState::new();
        state.selected_guild = Some(GuildId([1; 32]));
        state.selected_channel = Some(ChannelId([2; 16]));
        state.apply_event(DaemonEvent::Event {
            guild_id: GuildId([1; 32]),
            event: kind_event(
                EventKind::ReactionAdded {
                    target: EventId([0; 32]),
                    emoji: "x".to_string(),
                },
                1,
            ),
            local: false,
        });
        assert!(state.messages.is_empty());
        assert_eq!(state.unread_for(ChannelId([2; 16])), 0);
    }

    #[test]
    fn presence_typing_and_control_update_the_selected_guild_only() {
        let mut state = AppState::new();
        state.selected_guild = Some(GuildId([1; 32]));
        state.selected_channel = Some(ChannelId([2; 16]));
        let author = Identity::from_seed([9u8; 32]).author_id();
        state.apply_event(DaemonEvent::Presence {
            guild_id: GuildId([1; 32]),
            author,
            status: PresenceStatus::Away,
        });
        state.apply_event(DaemonEvent::Typing {
            guild_id: GuildId([1; 32]),
            channel_id: ChannelId([2; 16]),
            author,
            until_ms: 10_000,
        });
        assert_eq!(state.presence_for(author), PresenceStatus::Away);
        assert_eq!(state.typing_in(ChannelId([2; 16]), 5_000), vec![author]);
        assert!(state.typing_in(ChannelId([2; 16]), 11_000).is_empty());

        state.apply_event(DaemonEvent::Presence {
            guild_id: GuildId([9; 32]),
            author,
            status: PresenceStatus::Online,
        });
        assert_eq!(state.presence_for(author), PresenceStatus::Away);
    }

    #[test]
    fn the_member_panel_falls_back_to_the_control_roster() {
        let mut state = AppState::new();
        let guild_id = GuildId([1; 32]);
        let author = Identity::from_seed([9u8; 32]).author_id();
        let base = ovvc_control::ControlSession::new(
            guild_id,
            SessionId::for_guild(guild_id),
            ovvc_control::ElectionConfig::default(),
            [author],
        )
        .snapshot(0);
        state.control = Some(ovvc_control::ControlSnapshot {
            roster: vec![ovvc_control::RosterMember {
                author,
                display_name: "ada".to_string(),
                capabilities: ovvc_control::Capabilities::default(),
                joined_at_ms: 0,
                last_seen_ms: 1,
                presence: PresenceStatus::Away,
            }],
            voice: Vec::new(),
            ..base
        });

        // No live frame has arrived for this author, so the snapshot answers.
        assert_eq!(state.presence_for(author), PresenceStatus::Away);

        // A live frame is newer than the snapshot and wins over it.
        state.selected_guild = Some(guild_id);
        state.apply_event(DaemonEvent::Presence {
            guild_id,
            author,
            status: PresenceStatus::DoNotDisturb,
        });
        assert_eq!(state.presence_for(author), PresenceStatus::DoNotDisturb);

        // A guild switch empties the live map, so the panel falls back to the
        // roster instead of claiming every member is offline.
        state.presence.clear();
        assert_eq!(state.presence_for(author), PresenceStatus::Away);
    }

    #[test]
    fn presence_is_re_announced_while_the_window_is_idle() {
        let mut state = AppState::new();
        let guild_id = GuildId([1; 32]);
        state.selected_guild = Some(guild_id);
        state.connection = Some(ConnectionStateDto {
            guild_id,
            connected: true,
            peer_id: None,
            listen_addresses: Vec::new(),
            external_addresses: Vec::new(),
            peers: Vec::new(),
        });
        state.control = Some(
            ovvc_control::ControlSession::new(
                guild_id,
                SessionId::for_guild(guild_id),
                ovvc_control::ElectionConfig::default(),
                [],
            )
            .snapshot(0),
        );
        state.own_presence = PresenceStatus::Away;

        let sent = |state: &mut AppState, now_ms: u64| {
            matches!(
                state.presence_keepalive(now_ms, guild_id),
                Some(Op::SetPresence {
                    status: PresenceStatus::Away,
                    ..
                })
            )
        };
        assert!(sent(&mut state, 10_000), "the first keepalive is due");
        assert!(
            state.presence_keepalive(11_500, guild_id).is_none(),
            "a keepalive went out before the lease needed one"
        );
        assert!(sent(&mut state, 12_000), "the next keepalive is due");

        // A guild that is not networked has no lease to keep, and neither has
        // a window that has not joined the control session.
        state.connection.as_mut().unwrap().connected = false;
        assert!(state.presence_keepalive(20_000, guild_id).is_none());
        state.connection.as_mut().unwrap().connected = true;
        state.control = None;
        assert!(state.presence_keepalive(20_000, guild_id).is_none());
    }

    #[test]
    fn a_guild_switch_restarts_the_presence_clock() {
        let mut state = AppState::new();
        let guild_id = GuildId([1; 32]);
        state.selected_guild = Some(guild_id);
        state.connection = Some(ConnectionStateDto {
            guild_id,
            connected: true,
            peer_id: None,
            listen_addresses: Vec::new(),
            external_addresses: Vec::new(),
            peers: Vec::new(),
        });
        state.control = Some(
            ovvc_control::ControlSession::new(
                guild_id,
                SessionId::for_guild(guild_id),
                ovvc_control::ElectionConfig::default(),
                [],
            )
            .snapshot(0),
        );
        assert!(state.presence_keepalive(10_000, guild_id).is_some());
        state.select_guild(GuildId([2; 32]));
        assert_eq!(state.presence_sent_at_ms, 0);
    }

    #[test]
    fn a_channel_created_event_appears_in_the_sidebar() {
        let mut state = AppState::new();
        let guild_id = GuildId([1; 32]);
        state.selected_guild = Some(guild_id);
        let event = identity()
            .sign_body(EventBody {
                version: 2,
                guild_id,
                channel_id: Some(ChannelId([4; 16])),
                author: identity().author_id(),
                seq: 1,
                lamport: 1,
                timestamp: 10,
                prev: None,
                kind: EventKind::ChannelCreated {
                    name: "random".to_string(),
                },
            })
            .unwrap();
        state.apply_event(DaemonEvent::Event {
            guild_id,
            event,
            local: false,
        });
        assert_eq!(state.channels.len(), 1);
        assert_eq!(state.channels[0].name, "random");
    }

    #[test]
    fn switching_channels_clears_the_loaded_window() {
        let mut state = AppState::new();
        state.selected_guild = Some(GuildId([1; 32]));
        state.selected_channel = Some(ChannelId([2; 16]));
        state.apply_event(DaemonEvent::Event {
            guild_id: GuildId([1; 32]),
            event: message_event(GuildId([1; 32]), ChannelId([2; 16]), "hi", 1),
            local: true,
        });
        assert_eq!(state.messages.len(), 1);
        state.select_channel(Some(ChannelId([3; 16])));
        assert!(state.messages.is_empty());
        assert_eq!(state.selected_channel, Some(ChannelId([3; 16])));
    }

    #[test]
    fn the_channel_tree_puts_roots_first_and_categories_in_age_order() {
        let mut state = AppState::new();
        let guild_id = GuildId([1; 32]);
        state.selected_guild = Some(guild_id);
        let later = ovvc_core::CategoryMeta {
            id: ChannelId([8; 16]),
            guild_id,
            name: "later".to_string(),
            created_by: Identity::from_seed([7u8; 32]).author_id(),
            created_at: 20,
        };
        let earlier = ovvc_core::CategoryMeta {
            id: ChannelId([9; 16]),
            guild_id,
            name: "projects".to_string(),
            created_by: Identity::from_seed([7u8; 32]).author_id(),
            created_at: 10,
        };
        let view = |id: u8, name: &str, parent: Option<ChannelId>| ChannelView {
            id: ChannelId([id; 16]),
            guild_id,
            name: name.to_string(),
            created_by: Identity::from_seed([7u8; 32]).author_id(),
            created_at: 1,
            kind: ChannelKind::Text,
            parent,
        };
        state.apply_reply(
            RequestTag::ChannelTree(guild_id),
            Ok(Reply::ChannelTree(ovvc_ipc::ChannelTreeDto {
                categories: vec![later.clone(), earlier.clone()],
                channels: vec![
                    view(3, "zulu", Some(earlier.id)),
                    view(2, "general", None),
                    view(4, "alpha", Some(earlier.id)),
                    view(5, "chat", Some(later.id)),
                ],
            })),
        );
        assert_eq!(state.categories[0].name, "projects");
        assert_eq!(state.categories[1].name, "later");
        let names: Vec<&str> = state.channels.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["general", "alpha", "zulu", "chat"]);
        assert_eq!(state.selected_channel, Some(ChannelId([2; 16])));
    }

    #[test]
    fn live_organisation_events_update_the_sidebar() {
        let mut state = AppState::new();
        let guild_id = GuildId([1; 32]);
        state.selected_guild = Some(guild_id);
        let author = identity().author_id();
        let sign = |seq: u64, kind: EventKind| {
            identity()
                .sign_body(EventBody {
                    version: 2,
                    guild_id,
                    channel_id: Some(ChannelId([4; 16])),
                    author,
                    seq,
                    lamport: seq,
                    timestamp: 10 + seq,
                    prev: None,
                    kind,
                })
                .unwrap()
        };
        for event in [
            sign(
                1,
                EventKind::ChannelCreatedV2 {
                    name: "general".to_string(),
                    kind: ChannelKind::Text,
                    parent: None,
                },
            ),
            sign(
                2,
                EventKind::CategoryCreated {
                    name: "projects".to_string(),
                },
            ),
            sign(
                3,
                EventKind::ChannelCreatedV2 {
                    name: "standup".to_string(),
                    kind: ChannelKind::Voice,
                    parent: Some(ChannelId([4; 16])),
                },
            ),
            sign(
                4,
                EventKind::ChannelRenamed {
                    target: ChannelId([4; 16]),
                    name: "daily-standup".to_string(),
                },
            ),
            sign(
                5,
                EventKind::ChannelMoved {
                    target: ChannelId([4; 16]),
                    parent: None,
                },
            ),
        ] {
            state.apply_event(DaemonEvent::Event {
                guild_id,
                event,
                local: true,
            });
        }
        assert_eq!(state.categories.len(), 1);
        assert_eq!(state.channels.len(), 1);
        assert_eq!(state.channels[0].name, "daily-standup");
        assert_eq!(state.channels[0].kind, ChannelKind::Voice);
        assert_eq!(state.channels[0].parent, None);
    }

    #[test]
    fn notification_settings_replies_and_quiet_fields_round_trip() {
        let mut state = AppState::new();
        let settings = NotificationSettings {
            desktop: false,
            dnd: true,
            quiet_hours: Some(QuietHours {
                start_minute: 22 * 60,
                end_minute: 7 * 60,
            }),
            muted_guilds: vec![GuildId([1; 32])],
            muted_channels: vec![ChannelId([2; 16])],
        };
        state.apply_reply(
            RequestTag::NotificationSettings,
            Ok(Reply::NotificationSettings(settings.clone())),
        );
        assert_eq!(state.notification_settings, settings);
        assert_eq!(state.quiet_start, "22:00");
        assert_eq!(state.quiet_end, "07:00");
        assert!(state.is_muted_channel(ChannelId([2; 16])));
        assert!(state.is_muted_guild(GuildId([1; 32])));

        state.quiet_start = "06:30".to_string();
        state.quiet_end.clear();
        assert!(
            !state.commit_quiet_hours(),
            "a half-typed window must not be committed"
        );
        assert_eq!(
            state.notification_settings.quiet_hours,
            Some(QuietHours {
                start_minute: 22 * 60,
                end_minute: 7 * 60,
            }),
            "typing over one field must leave the daemon's quiet hours alone"
        );
        // Clearing both fields is the explicit way to switch quiet hours off.
        state.quiet_start.clear();
        assert!(state.commit_quiet_hours());
        assert_eq!(state.notification_settings.quiet_hours, None);
        assert_eq!(parse_minute("23:59"), Some(23 * 60 + 59));
        assert_eq!(parse_minute("24:00"), None);
        assert_eq!(parse_minute("noon"), None);
    }

    #[test]
    fn a_search_hit_in_the_open_channel_closes_the_results_and_keeps_the_jump() {
        let mut state = AppState::new();
        let guild = GuildId([9; 32]);
        let channel = ChannelId([7; 16]);
        let event = EventId([8; 32]);
        state.selected_guild = Some(guild);
        state.select_channel(Some(channel));
        state.search_query = "needle".to_string();
        state.searched_query = Some("needle".to_string());
        state.searching = true;

        state.open_search_hit(channel, event);

        assert!(
            state.search_query.is_empty(),
            "the results view would otherwise keep drawing and no scroll would happen"
        );
        assert_eq!(state.searched_query, None);
        assert!(!state.searching);
        assert_eq!(state.jump_to, Some(event));
    }

    #[test]
    fn overlapping_rich_sends_only_clear_their_own_files() {
        let staged = |byte: u8| StagedAttachment {
            path: PathBuf::from(format!("/tmp/file-{byte}.txt")),
            attachment: Attachment {
                hash: ovvc_core::hash_bytes(&[byte]),
                size: 1,
                mime: "text/plain".to_string(),
                name: format!("file-{byte}.txt"),
            },
            bytes: vec![byte],
        };
        let first = staged(1);
        let second = staged(2);
        let mut state = AppState::new();
        state.staged_attachments = vec![first.clone(), second.clone()];
        state.pending_attachments.insert(7, vec![first.clone()]);
        state.pending_attachments.insert(8, vec![second.clone()]);
        let reply = || {
            Reply::Event(message_event(
                GuildId([1; 32]),
                ChannelId([2; 16]),
                "sent with a file",
                1,
            ))
        };

        // The second post is confirmed first. The first send is still in
        // flight, so its chips must not be cleared by the other reply.
        state.apply_reply(RequestTag::SendRichMessage(8), Ok(reply()));
        assert_eq!(state.staged_attachments, vec![first.clone()]);
        assert!(state.pending_attachments.contains_key(&7));

        state.apply_reply(RequestTag::SendRichMessage(7), Ok(reply()));
        assert!(state.staged_attachments.is_empty());
        assert!(state.pending_attachments.is_empty());
    }

    #[test]
    fn short_ids_are_bounded() {
        assert_eq!(short_id("0123456789abcdef"), "01234567");
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn connect_replies_and_listening_events_track_the_network_state() {
        let mut state = AppState::new();
        let guild = GuildId([3; 32]);
        state.selected_guild = Some(guild);
        assert!(!state.is_online(guild));

        // The reply only says the daemon accepted the request. Until the daemon
        // reports a connection, the guild is not online.
        state.apply_reply(RequestTag::Connect(guild), Ok(Reply::Unit));
        assert!(!state.is_online(guild));
        assert!(state.notice.is_some());
        assert_eq!(state.online_peers(), 0);

        state.apply_event(DaemonEvent::Listening {
            guild_id: guild,
            address: "/ip4/192.0.2.5/tcp/41000".to_string(),
        });
        assert!(state.is_online(guild));
        assert_eq!(state.notice, None);
        let connection = state
            .connection
            .as_ref()
            .expect("connection follows a listen");
        assert!(connection.connected);
        assert_eq!(
            connection.listen_addresses,
            vec!["/ip4/192.0.2.5/tcp/41000"]
        );

        state.apply_reply(RequestTag::Disconnect(guild), Ok(Reply::Unit));
        assert!(!state.is_online(guild));
        let connection = state.connection.as_ref().expect("connection remains");
        assert!(!connection.connected);
        assert!(connection.listen_addresses.is_empty());
    }

    #[test]
    fn a_stale_offline_snapshot_does_not_undo_a_connecting_guild() {
        let mut state = AppState::new();
        let guild = GuildId([5; 32]);
        state.selected_guild = Some(guild);
        state.apply_reply(RequestTag::Connect(guild), Ok(Reply::Unit));
        state.apply_event(DaemonEvent::Listening {
            guild_id: guild,
            address: "/ip4/127.0.0.1/tcp/41000".to_string(),
        });
        assert!(state.is_online(guild));

        // The daemon answers each frame on its own task, so a snapshot taken
        // before the node started can land after the Listening event.
        state.apply_reply(
            RequestTag::Connection(guild),
            Ok(Reply::Connection(ovvc_ipc::ConnectionStateDto {
                guild_id: guild,
                connected: false,
                peer_id: None,
                listen_addresses: Vec::new(),
                external_addresses: Vec::new(),
                peers: Vec::new(),
            })),
        );
        assert!(state.is_online(guild));

        state.apply_reply(
            RequestTag::Connection(guild),
            Ok(Reply::Connection(ovvc_ipc::ConnectionStateDto {
                guild_id: guild,
                connected: true,
                peer_id: Some("12D3KooExample".to_string()),
                listen_addresses: vec!["/ip4/127.0.0.1/tcp/41000".to_string()],
                external_addresses: Vec::new(),
                peers: Vec::new(),
            })),
        );
        assert!(state.is_online(guild));
        assert_eq!(state.connect_requested, None);
    }

    #[test]
    fn a_disconnect_reply_for_another_guild_keeps_the_shown_guild_online() {
        let mut state = AppState::new();
        let shown = GuildId([1; 32]);
        let other = GuildId([2; 32]);
        state.selected_guild = Some(shown);
        state.connection = Some(ovvc_ipc::ConnectionStateDto {
            guild_id: shown,
            connected: true,
            peer_id: None,
            listen_addresses: vec!["/ip4/127.0.0.1/tcp/41000".to_string()],
            external_addresses: Vec::new(),
            peers: Vec::new(),
        });

        state.apply_reply(RequestTag::Disconnect(other), Ok(Reply::Unit));
        assert!(state.is_online(shown));
    }

    #[test]
    fn a_gui_restart_reads_online_guilds_from_the_daemon_snapshot() {
        let mut state = AppState::new();
        let guild = GuildId([4; 32]);
        state.selected_guild = Some(guild);
        // No connect request was made by this process; the daemon says it is
        // already networking the guild.
        state.apply_reply(
            RequestTag::Connection(guild),
            Ok(Reply::Connection(ovvc_ipc::ConnectionStateDto {
                guild_id: guild,
                connected: true,
                peer_id: Some("12D3KooExample".to_string()),
                listen_addresses: vec!["/ip4/192.0.2.5/tcp/41000".to_string()],
                external_addresses: Vec::new(),
                peers: Vec::new(),
            })),
        );
        assert!(state.is_online(guild));
        assert_eq!(state.online_guild(), Some(guild));
    }

    #[test]
    fn address_fields_keep_only_multiaddrs() {
        assert_eq!(
            parse_addresses("  /ip4/0.0.0.0/tcp/41000   /ip4/192.0.2.5/tcp/41000 "),
            vec!["/ip4/0.0.0.0/tcp/41000", "/ip4/192.0.2.5/tcp/41000"]
        );
        assert!(parse_addresses("192.0.2.5:41000").is_empty());
        assert!(parse_addresses("").is_empty());
    }

    #[test]
    fn address_problems_name_the_bad_token() {
        assert_eq!(address_problem("/ip4/0.0.0.0/tcp/41000"), None);
        assert_eq!(address_problem(""), None);
        let problem = address_problem("192.0.2.5:41000").expect("a problem");
        assert!(problem.contains("192.0.2.5:41000"));
        assert!(address_problem("/ip4/0.0.0.0/tcp/41000 nonsense").is_some());
    }

    #[test]
    fn the_public_address_leads_the_invite_and_wildcards_are_dropped() {
        let mut state = AppState::new();
        let guild = GuildId([3; 32]);
        state.selected_guild = Some(guild);
        state.connection = Some(ovvc_ipc::ConnectionStateDto {
            guild_id: guild,
            connected: true,
            peer_id: Some("12D3KooExample".to_string()),
            listen_addresses: vec![
                "/ip4/0.0.0.0/tcp/41000".to_string(),
                "/ip4/127.0.0.1/tcp/41000".to_string(),
                "/ip4/192.0.2.5/tcp/41000".to_string(),
            ],
            external_addresses: vec!["/ip4/198.51.100.2/tcp/41000".to_string()],
            peers: Vec::new(),
        });
        assert!(state.can_invite(guild));
        assert_eq!(
            state.invite_addresses(),
            vec![
                "/ip4/198.51.100.2/tcp/41000/p2p/12D3KooExample",
                "/ip4/192.0.2.5/tcp/41000/p2p/12D3KooExample",
            ]
        );

        // A router with no UPnP/NAT-PMP cannot be discovered, so the person
        // supplies the forwarded address. It goes first, because it is the one
        // a friend outside the network can dial.
        state.network_public = "/dns4/example.net/tcp/41000".to_string();
        assert_eq!(
            state.invite_addresses().first().map(String::as_str),
            Some("/dns4/example.net/tcp/41000/p2p/12D3KooExample")
        );
    }

    #[test]
    fn the_setup_flow_requires_a_display_name_and_a_commune_choice() {
        let mut state = AppState::new();
        assert!(!state.wizard_step_valid(WizardStep::Identity));
        state.wizard_name = "Conleth".to_string();
        assert!(state.wizard_step_valid(WizardStep::Identity));
        assert!(!state.wizard_step_valid(WizardStep::Commune));

        state.wizard_commune = WizardCommune::Create;
        assert!(!state.wizard_step_valid(WizardStep::Commune));
        state.wizard_commune_name = "late night devs".to_string();
        assert!(state.wizard_step_valid(WizardStep::Commune));

        state.wizard_commune = WizardCommune::Join;
        assert!(!state.wizard_step_valid(WizardStep::Commune));
        state.wizard_join_code = "ovvc://invite".to_string();
        assert!(state.wizard_step_valid(WizardStep::Commune));
    }

    // --- profiles and direct messages -------------------------------------

    fn other_identity() -> Identity {
        Identity::from_seed([9u8; 32])
    }

    fn member_info(identity: &Identity, name: &str) -> MemberInfo {
        MemberInfo {
            guild_id: guild().id,
            author: identity.author_id(),
            display_name: name.to_string(),
            joined_at: 1_700_000_000,
        }
    }

    fn profile_for(author: AuthorId, name: &str) -> MemberProfile {
        MemberProfile {
            guild_id: guild().id,
            author,
            display_name: name.to_string(),
            about: "about text".to_string(),
            pronouns: "they/them".to_string(),
            links: vec!["https://example.com".to_string()],
            joined_at: 1_700_000_000,
            updated_at: 1_700_000_100,
            message_count: 12,
            last_activity: 1_700_000_200,
        }
    }

    fn dm_guild() -> GuildMeta {
        GuildMeta {
            id: GuildId([6; 32]),
            name: "ada, conleth".to_string(),
            founder: identity().author_id(),
            created_at: 2,
        }
    }

    fn profile_updated_event(identity: &Identity, seq: u64) -> SignedEvent {
        identity
            .sign_body(EventBody {
                version: 2,
                guild_id: guild().id,
                channel_id: Some(channel().id),
                author: identity.author_id(),
                seq,
                lamport: seq,
                timestamp: 1_700_000_500,
                prev: None,
                kind: EventKind::MemberProfileUpdated {
                    about: "new about".to_string(),
                    pronouns: String::new(),
                    links: Vec::new(),
                },
            })
            .unwrap()
    }

    #[test]
    fn a_profile_reply_fills_the_dialog_and_a_late_reply_is_dropped() {
        let mut state = AppState::new();
        let me = identity();
        let other = other_identity();
        state.identity = Some(me.author_id());
        state.open_profile(guild().id, other.author_id());
        assert_eq!(state.dialog, Dialog::Profile);
        assert!(state.profile_stale);
        assert_eq!(state.profile_author, Some(other.author_id()));

        state.apply_reply(
            RequestTag::MemberProfile {
                guild_id: guild().id,
                author: other.author_id(),
            },
            Ok(Reply::Member(profile_for(other.author_id(), "Ada"))),
        );
        assert_eq!(
            state
                .profile
                .as_ref()
                .map(|profile| profile.display_name.as_str()),
            Some("Ada")
        );
        assert_eq!(state.profile_about, "about text");
        assert_eq!(state.profile_links, "https://example.com");
        assert!(!state.profile_editing);
        assert!(!state.profile_stale);

        // A profile event for the member on screen asks for a fresh copy.
        state.apply_event(DaemonEvent::Event {
            guild_id: guild().id,
            event: profile_updated_event(&other, 5),
            local: false,
        });
        assert!(state.profile_stale);

        // A reply for somebody else must not replace the open profile.
        state.apply_reply(
            RequestTag::MemberProfile {
                guild_id: guild().id,
                author: me.author_id(),
            },
            Ok(Reply::Member(profile_for(me.author_id(), "Conleth"))),
        );
        assert_eq!(
            state
                .profile
                .as_ref()
                .map(|profile| profile.display_name.as_str()),
            Some("Ada")
        );

        state.close_profile();
        assert_eq!(state.dialog, Dialog::None);
        assert!(state.profile.is_none());
        state.apply_reply(
            RequestTag::MemberProfile {
                guild_id: guild().id,
                author: other.author_id(),
            },
            Ok(Reply::Member(profile_for(other.author_id(), "Ada"))),
        );
        assert!(
            state.profile.is_none(),
            "a reply after the dialog closed must be dropped"
        );
    }

    #[test]
    fn own_profile_edits_are_trimmed_before_saving() {
        let mut state = AppState::new();
        let me = identity();
        state.identity = Some(me.author_id());
        state.open_profile(guild().id, me.author_id());
        state.apply_reply(
            RequestTag::MemberProfile {
                guild_id: guild().id,
                author: me.author_id(),
            },
            Ok(Reply::Member(profile_for(me.author_id(), "Conleth"))),
        );
        state.begin_profile_edit();
        assert!(state.profile_editing);

        state.profile_about = "  hello there  \n".to_string();
        state.profile_pronouns = "  they/them ".to_string();
        state.profile_links = " https://a.example \n\nhttp://b.example  ".to_string();
        let action = state.profile_save_request().expect("valid draft");
        assert_eq!(
            action,
            Action::SaveProfile {
                guild_id: guild().id,
                about: "hello there".to_string(),
                pronouns: "they/them".to_string(),
                links: vec![
                    "https://a.example".to_string(),
                    "http://b.example".to_string()
                ],
            }
        );

        state.profile_saving = true;
        state.apply_reply(
            RequestTag::SetProfile,
            Ok(Reply::Event(message_event(
                guild().id,
                channel().id,
                "profile",
                7,
            ))),
        );
        assert!(!state.profile_editing);
        assert!(!state.profile_saving);
        assert!(state.profile_stale);
        assert_eq!(state.notice.as_deref(), Some("profile saved"));
    }

    #[test]
    fn profile_drafts_reject_bad_links_and_oversized_fields() {
        let mut state = AppState::new();
        let me = identity();
        state.identity = Some(me.author_id());
        state.open_profile(guild().id, me.author_id());

        state.profile_links = "ftp://example.com".to_string();
        let problem = state.profile_save_request().expect_err("ftp is not http");
        assert!(problem.contains("http(s)"), "{problem}");

        state.profile_links = (0..6)
            .map(|index| format!("https://example.com/{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let problem = state.profile_save_request().expect_err("too many links");
        assert!(problem.contains("6 links"), "{problem}");

        state.profile_links.clear();
        state.profile_about = "a".repeat(MAX_PROFILE_ABOUT_BYTES + 1);
        let problem = state.profile_save_request().expect_err("about too long");
        assert!(problem.contains("about"), "{problem}");

        state.profile_about = "ok".to_string();
        state.profile_pronouns = "p".repeat(MAX_PROFILE_PRONOUNS_BYTES + 1);
        let problem = state.profile_save_request().expect_err("pronouns too long");
        assert!(problem.contains("pronouns"), "{problem}");

        state.profile_pronouns = "ok".to_string();
        state.profile_links = format!("https://example.com/{}", "x".repeat(MAX_PROFILE_LINK_BYTES));
        let problem = state.profile_save_request().expect_err("link too long");
        assert!(problem.contains("one link"), "{problem}");
    }

    #[test]
    fn direct_conversations_are_listed_separately_and_badge_unread() {
        let mut state = AppState::new();
        let me = identity();
        let other = other_identity();
        state.identity = Some(me.author_id());
        state.apply_reply(RequestTag::Guilds, Ok(Reply::Guilds(vec![guild()])));
        let dm = dm_guild();
        let peer = MemberInfo {
            guild_id: dm.id,
            author: other.author_id(),
            display_name: "Ada".to_string(),
            joined_at: 1_700_000_000,
        };
        state.apply_reply(
            RequestTag::DirectMessages,
            Ok(Reply::DirectMessages(vec![DirectConversation {
                guild: dm.clone(),
                peer,
            }])),
        );
        assert_eq!(state.direct_messages.len(), 1);
        assert!(state.is_direct_guild(dm.id));
        assert!(
            state.guilds.iter().any(|guild| guild.id == dm.id),
            "the DM guild must still resolve for its header"
        );
        assert_eq!(state.dm_name(&state.direct_messages[0]), "Ada");

        let dm_channel = ChannelId([7; 16]);
        state.apply_reply(
            RequestTag::DmUnread(dm.id),
            Ok(Reply::Unread(HashMap::from([(dm_channel, 3)]))),
        );
        assert_eq!(state.dm_unread_for(dm.id), 3);

        // A live message in an unselected DM bumps its badge.
        state.apply_event(DaemonEvent::Event {
            guild_id: dm.id,
            event: message_event(dm.id, dm_channel, "hello", 1),
            local: false,
        });
        assert_eq!(state.dm_unread_for(dm.id), 4);

        state.select_guild(dm.id);
        assert_eq!(
            state
                .selected_dm()
                .map(|conversation| conversation.guild.id),
            Some(dm.id)
        );
        state.mark_dm_read(dm.id);
        assert_eq!(state.dm_unread_for(dm.id), 0);
    }

    #[test]
    fn opening_a_direct_message_selects_the_returned_guild() {
        let mut state = AppState::new();
        let me = identity();
        let other = other_identity();
        state.identity = Some(me.author_id());
        state.selected_guild = Some(guild().id);
        state.members = vec![member_info(&me, "Conleth"), member_info(&other, "Ada")];
        state.dm_opening = Some(other.author_id());
        let dm = dm_guild();
        state.apply_reply(
            RequestTag::OpenDirectMessage {
                peer: other.author_id(),
            },
            Ok(Reply::GuildCreated(dm.clone())),
        );
        assert_eq!(state.selected_guild, Some(dm.id));
        assert_eq!(state.dialog, Dialog::None);
        assert_eq!(state.dm_opening, None);
        assert!(state.is_direct_guild(dm.id));
        assert_eq!(state.direct_messages[0].peer.display_name, "Ada");
        assert!(state.direct_messages_stale);

        // If the profile closed before the reply, the conversation is still
        // recorded but the window is not yanked away from what it shows.
        let mut late = AppState::new();
        late.identity = Some(me.author_id());
        late.selected_guild = Some(guild().id);
        late.members = vec![member_info(&other, "Ada")];
        let dm_late = GuildMeta {
            id: GuildId([8; 32]),
            ..dm
        };
        late.apply_reply(
            RequestTag::OpenDirectMessage {
                peer: other.author_id(),
            },
            Ok(Reply::GuildCreated(dm_late.clone())),
        );
        assert_eq!(late.selected_guild, Some(guild().id));
        assert!(late.is_direct_guild(dm_late.id));
    }

    #[test]
    fn a_failed_profile_read_does_not_retry_by_itself() {
        let mut state = AppState::new();
        let other = other_identity();
        state.open_profile(guild().id, other.author_id());
        state.profile_loading = true;
        state.profile_stale = false;
        state.apply_reply(
            RequestTag::MemberProfile {
                guild_id: guild().id,
                author: other.author_id(),
            },
            Err(IpcError::not_found("member not found")),
        );
        assert!(!state.profile_loading);
        assert!(
            !state.profile_stale,
            "a failed read must wait for an explicit retry"
        );
        assert_eq!(state.error.as_deref(), Some("member not found"));
        assert!(state.profile.is_none());
    }

    fn voice_call(
        channel_id: ChannelId,
        author: AuthorId,
        state: VoiceMemberState,
    ) -> VoiceSnapshot {
        VoiceSnapshot {
            channel_id,
            call_id: ovvc_media::CallId::for_channel(channel_id),
            members: vec![ovvc_media::VoiceMember {
                author,
                joined_at_ms: 10,
                last_seen_ms: 20,
                state,
                profiles: ovvc_media::ProfileSet::STANDARD,
            }],
            topology: ovvc_media::CallTopology::Mesh,
            hub: None,
            scope: ovvc_core::CallScope::Auto,
            hub_policy: ovvc_core::HubPolicy::default(),
            key_epoch: 0,
            codec: None,
            started_at_ms: 10,
            updated_at_ms: 20,
            master: Some(author),
            backup: None,
            term: 1,
        }
    }

    fn audio_stats(latency_ms: u64) -> VoiceAudioStats {
        VoiceAudioStats {
            backend: "pipewire".to_string(),
            captured: 480,
            packets: 24,
            played: 480,
            concealed: 0,
            lost: 0,
            decode_errors: 0,
            peers: 1,
            xruns: 0,
            latency_ms,
            peak: 300,
            rms: 0.125,
            transport: "webrtc".to_string(),
            rtt_ms: 35,
            packets_lost: 12,
            relayed: true,
            e2ee: true,
            epoch: 1,
            e2ee_errors: 0,
        }
    }

    #[test]
    fn voice_join_and_leave_replies_drive_the_local_call() {
        let mut state = AppState::new();
        let guild_id = guild().id;
        let channel_id = ChannelId([3; 16]);
        let author = identity().author_id();
        state.selected_guild = Some(guild_id);
        state.selected_channel = Some(channel_id);
        state.identity = Some(author);

        let call = voice_call(channel_id, author, VoiceMemberState::default());
        state.apply_reply(
            RequestTag::VoiceJoin {
                guild_id,
                channel_id,
            },
            Ok(Reply::Voice(Some(call))),
        );
        assert!(state.voice.joined());
        assert_eq!(state.voice.channel, Some(channel_id));
        assert!(state.voice_joined_here());
        assert_eq!(
            state.selected_voice_call().map(|call| call.members.len()),
            Some(1)
        );

        state.apply_reply(
            RequestTag::VoiceLeave {
                guild_id,
                channel_id,
            },
            Ok(Reply::Voice(None)),
        );
        assert!(!state.voice.joined());
        assert!(state.voice.channel.is_none());
        assert!(state.selected_voice_call().is_none());
    }

    #[test]
    fn voice_flags_publish_without_clearing_speaking() {
        let mut state = AppState::new();
        let guild_id = guild().id;
        let channel_id = ChannelId([3; 16]);
        let author = identity().author_id();
        state.selected_guild = Some(guild_id);
        state.selected_channel = Some(channel_id);
        state.identity = Some(author);
        state.voice.channel = Some(channel_id);
        state.voice.guild = Some(guild_id);
        state.voice.snapshots.insert(
            channel_id,
            voice_call(
                channel_id,
                author,
                VoiceMemberState {
                    speaking: true,
                    ..VoiceMemberState::default()
                },
            ),
        );

        let previous = state.voice.local_state(author);
        assert!(previous.speaking, "the fixture should be speaking");
        state.voice.muted = true;
        state.voice.push_to_talk = true;
        let published = state.voice.member_state(previous);
        assert!(published.muted);
        assert!(published.push_to_talk);
        assert!(!published.deafened);
        assert!(published.speaking, "muting cleared the speaking flag");

        // The daemon's reply is the authority, so the muted state from the
        // reply replaces the local snapshot.
        let muted = voice_call(
            channel_id,
            author,
            VoiceMemberState {
                speaking: true,
                muted: true,
                push_to_talk: true,
                ..VoiceMemberState::default()
            },
        );
        state.apply_reply(
            RequestTag::SetVoiceState {
                guild_id,
                channel_id,
            },
            Ok(Reply::Voice(Some(muted))),
        );
        let cached = state.voice.snapshots.get(&channel_id).expect("snapshot");
        assert!(cached.members[0].state.muted);
        assert!(cached.members[0].state.push_to_talk);
        assert!(cached.members[0].state.speaking);
    }

    #[test]
    fn a_voice_control_state_fills_the_call_without_replacing_the_guild_snapshot() {
        let mut state = AppState::new();
        let guild_id = guild().id;
        let channel_id = ChannelId([3; 16]);
        let author = identity().author_id();
        state.selected_guild = Some(guild_id);
        state.selected_channel = Some(channel_id);
        state.identity = Some(author);

        let guild_session = SessionId::for_guild(guild_id);
        let base = ovvc_control::ControlSession::new(
            guild_id,
            guild_session,
            ovvc_control::ElectionConfig::default(),
            [author],
        )
        .snapshot(0);
        state.apply_event(DaemonEvent::ControlState {
            guild_id,
            snapshot: base.clone(),
        });
        assert_eq!(state.session_id, Some(guild_session));

        // A voice channel has its own control session. Its snapshot carries
        // the call, but it must not replace the guild roster the window keeps.
        let mut voice_snapshot = base;
        voice_snapshot.session_id = SessionId::for_voice_channel(guild_id, channel_id);
        voice_snapshot.voice = vec![voice_call(channel_id, author, VoiceMemberState::default())];
        state.apply_event(DaemonEvent::ControlState {
            guild_id,
            snapshot: voice_snapshot,
        });
        assert_eq!(state.session_id, Some(guild_session));
        assert!(state.control.is_some());
        assert!(state.voice.snapshots.contains_key(&channel_id));
    }

    #[test]
    fn voice_audio_events_apply_to_the_joined_call_and_clear_with_it() {
        let mut state = AppState::new();
        let guild_id = guild().id;
        let channel_id = ChannelId([3; 16]);
        let other_channel = ChannelId([4; 16]);
        let author = identity().author_id();
        state.selected_guild = Some(guild_id);
        state.selected_channel = Some(channel_id);
        state.identity = Some(author);
        state.voice.guild = Some(guild_id);
        state.voice.channel = Some(channel_id);

        state.apply_event(DaemonEvent::VoiceAudio {
            guild_id,
            channel_id,
            stats: audio_stats(20),
        });
        assert_eq!(
            state.voice.audio.as_ref().map(|stats| stats.latency_ms),
            Some(20)
        );
        assert_eq!(
            state.voice.audio.as_ref().map(|stats| (
                stats.transport.as_str(),
                stats.rtt_ms,
                stats.packets_lost
            )),
            Some(("webrtc", 35, 12))
        );
        assert!(
            state
                .voice
                .audio
                .as_ref()
                .is_some_and(|stats| stats.relayed),
            "the relayed marker was dropped"
        );

        // A sample for another call must not paint this window's device.
        state.apply_event(DaemonEvent::VoiceAudio {
            guild_id,
            channel_id: other_channel,
            stats: audio_stats(80),
        });
        assert_eq!(
            state.voice.audio.as_ref().map(|stats| stats.latency_ms),
            Some(20)
        );

        // The daemon reporting an empty call ends it, and the counters go with
        // the service.
        state.apply_event(DaemonEvent::Voice {
            guild_id,
            channel_id,
            change: ovvc_control::VoiceChange::Expired(author),
            snapshot: Some(VoiceSnapshot {
                members: Vec::new(),
                ..voice_call(channel_id, author, VoiceMemberState::default())
            }),
        });
        assert!(!state.voice.joined());
        assert!(state.voice.audio.is_none());
    }

    #[test]
    fn a_classified_diagnostic_keeps_its_class_and_log_path() {
        let mut state = AppState::new();
        let guild_id = guild().id;
        state.selected_guild = Some(guild_id);

        state.apply_event(DaemonEvent::Diagnostic(
            Diagnostic::bug("an impossible state")
                .with_guild(Some(guild_id))
                .with_log_path("/tmp/ovvc/daemon.log"),
        ));
        assert_eq!(state.error.as_deref(), Some("an impossible state"));
        assert_eq!(state.error_class, Some(ErrorClass::Bug));
        assert_eq!(state.error_log.as_deref(), Some("/tmp/ovvc/daemon.log"));

        // A peer going offline is operational: the strip still shows it, but
        // with a warning class rather than the bug colour.
        state.apply_event(DaemonEvent::Diagnostic(
            Diagnostic::operational("peer 3f2a went offline").with_guild(Some(guild_id)),
        ));
        assert_eq!(state.error_class, Some(ErrorClass::Operational));
        assert_eq!(state.error_log, None);
        assert_eq!(state.error.as_deref(), Some("peer 3f2a went offline"));
    }

    #[test]
    fn a_legacy_daemon_error_is_an_operational_warning_not_a_bug() {
        let mut state = AppState::new();
        let guild_id = guild().id;
        state.selected_guild = Some(guild_id);
        state.apply_event(DaemonEvent::Error {
            guild_id: Some(guild_id),
            message: "peer 3f2a connection failed".to_string(),
        });
        assert_eq!(state.error_class, Some(ErrorClass::Operational));
        assert_eq!(state.error.as_deref(), Some("peer 3f2a connection failed"));
    }

    #[test]
    fn webrtc_errors_stay_with_the_voice_panel_until_the_call_ends() {
        let mut state = AppState::new();
        let guild_id = guild().id;
        let channel_id = ChannelId([3; 16]);
        state.selected_guild = Some(guild_id);
        state.selected_channel = Some(channel_id);

        // Not in a call: there is no panel to annotate, so only the generic
        // alert strip carries the error.
        state.apply_event(DaemonEvent::Error {
            guild_id: Some(guild_id),
            message: "could not connect WebRTC peer 3f2a: timed out".to_string(),
        });
        assert!(state.voice.error.is_none());
        assert!(state.error.is_some());

        state.voice.guild = Some(guild_id);
        state.voice.channel = Some(channel_id);
        state.apply_event(DaemonEvent::Error {
            guild_id: Some(guild_id),
            message: "could not connect WebRTC peer 3f2a: timed out".to_string(),
        });
        assert_eq!(
            state.voice.error.as_deref(),
            Some("could not connect WebRTC peer 3f2a: timed out")
        );

        // Runtime manager failures do not carry the word WebRTC.
        state.apply_event(DaemonEvent::Error {
            guild_id: Some(guild_id),
            message: "peer 3f2a connection failed".to_string(),
        });
        assert_eq!(
            state.voice.error.as_deref(),
            Some("peer 3f2a connection failed")
        );

        state.apply_event(DaemonEvent::Error {
            guild_id: Some(guild_id),
            message: "candidate from 3f2a rejected: address is not permitted".to_string(),
        });
        assert_eq!(
            state.voice.error.as_deref(),
            Some("candidate from 3f2a rejected: address is not permitted")
        );

        // A peer with no usable media key, and a frame that fails to decrypt,
        // both mean the call has no media even though the control session is up.
        state.apply_event(DaemonEvent::Error {
            guild_id: Some(guild_id),
            message: "peer 3f2a has no usable media key at epoch 2; media not sent".to_string(),
        });
        assert_eq!(
            state.voice.error.as_deref(),
            Some("peer 3f2a has no usable media key at epoch 2; media not sent")
        );
        state.apply_event(DaemonEvent::Error {
            guild_id: Some(guild_id),
            message: "could not publish the media key: channel not found".to_string(),
        });
        assert_eq!(
            state.voice.error.as_deref(),
            Some("could not publish the media key: channel not found")
        );
        state.apply_event(DaemonEvent::Error {
            guild_id: Some(guild_id),
            message: "media frame from 3f2a failed to decrypt".to_string(),
        });
        assert_eq!(
            state.voice.error.as_deref(),
            Some("media frame from 3f2a failed to decrypt")
        );

        // An error from another subsystem is not a media-path failure.
        state.voice.error = None;
        state.apply_event(DaemonEvent::Error {
            guild_id: Some(guild_id),
            message: "member not found".to_string(),
        });
        assert!(state.voice.error.is_none());

        state.voice.error = Some("could not start WebRTC media".to_string());
        state.voice.clear_call();
        assert!(state.voice.error.is_none());
    }
}
