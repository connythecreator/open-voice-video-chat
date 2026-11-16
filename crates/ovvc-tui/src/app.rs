//! Application state and input handling for the phosphor terminal TUI.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use ovvc_client::{
    AuthorId, CategoryMeta, ChannelId, ChannelKind, ChannelView, ClientEvent, Diagnostic,
    DirectConversation, ErrorClass, EventId, EventKind, EventPage, GuildId, GuildMeta, Invite,
    MAX_ATTACHMENT_BYTES, MAX_PROFILE_ABOUT_BYTES, MAX_PROFILE_LINK_BYTES, MAX_PROFILE_LINKS,
    MAX_PROFILE_PRONOUNS_BYTES, MemberInfo, MemberProfile, Multiaddr, NetworkOptions,
    NotificationSettings, PeerId, QuietHours, SignedEvent, SyncCursor, parse_mentions,
    peer_id_for_author,
};
use ovvc_control::{ControlKind, PresenceStatus};
use ovvc_ipc::PolicyView;
use ovvc_remote::Frontend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::broadcast;

use crate::attach::{attachment_for, split_attach_args};
use crate::history::{
    first_unread, merge_newest_page, oldest_cursor, prepend_page, read_cursor_for,
};
use crate::input::Input;
use crate::profile;
use crate::spinner::Spinner;

mod voice;

pub(crate) const MESSAGE_PAGE: usize = 200;
const SCROLL_STEP: usize = 10;

/// How often the spinner advances while a load is on screen.
pub const SPINNER_FRAME_MS: u64 = 120;
/// A sync that has not reported completion by this deadline stops animating,
/// so one unresponsive peer cannot leave the status bar spinning.
const SYNC_SPINNER_MAX_MS: u64 = 12_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Onboarding,
    Chat,
    Settings,
    Setup,
    NetworkMap,
    Diagnostics,
}

/// The global detail level, shared with the GUI through the same config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum DetailLevel {
    #[default]
    Simple,
    Advanced,
    Expert,
}

impl DetailLevel {
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

/// The four full-page settings tabs. The order matches the design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Composer,
    Sidebar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Name,
    Privacy,
    Choice,
    CreateGuild,
    JoinGuild,
    Joining,
}

pub struct Onboarding {
    pub step: Step,
    pub name: Input,
    pub choice: usize,
    pub text: Input,
    pub error: Option<String>,
}

impl Default for Onboarding {
    fn default() -> Self {
        Self {
            step: Step::Name,
            name: Input::default(),
            choice: 0,
            text: Input::default(),
            error: None,
        }
    }
}

pub struct Overlay {
    pub title: String,
    pub body: Vec<String>,
}

/// A message the composer is currently answering. It mirrors the GUI's reply
/// line above the input: the next send carries the target and the line stays
/// visible until it is sent or cancelled.
#[derive(Clone, Debug)]
pub struct ReplyTarget {
    pub target: EventId,
    pub author: AuthorId,
    pub body: String,
}

pub enum Action {
    Connect(GuildId, NetworkOptions),
    Join(String, NetworkOptions),
    /// Fetch one older page off the input thread so the spinner can paint
    /// and the terminal stays responsive while the daemon answers.
    LoadHistory {
        guild_id: GuildId,
        channel_id: ChannelId,
        cursor: SyncCursor,
    },
}

pub enum Notice {
    Joined(GuildMeta),
    Error(String),
    /// The answer to [`Action::LoadHistory`]. The error is already rendered as
    /// text because the frontend error type does not cross the notice channel.
    HistoryPage {
        guild_id: GuildId,
        channel_id: ChannelId,
        page: std::result::Result<EventPage, String>,
    },
}

/// Network settings captured from command-line flags.
#[derive(Debug, Clone, Default)]
pub struct AppOptions {
    pub guild: Option<String>,
    pub listen: Vec<Multiaddr>,
    pub dial: Vec<Multiaddr>,
    pub relay: Option<Multiaddr>,
    pub upnp: bool,
    pub natpmp: bool,
}

impl AppOptions {
    fn network_requested(&self) -> bool {
        !self.listen.is_empty()
            || !self.dial.is_empty()
            || self.relay.is_some()
            || self.upnp
            || self.natpmp
    }

    fn network_options(&self) -> NetworkOptions {
        NetworkOptions {
            listen: self.listen.clone(),
            dial: self.dial.clone(),
            relay: self.relay.clone(),
            upnp: self.upnp,
            natpmp: self.natpmp,
        }
    }
}

pub struct App {
    pub client: Arc<dyn Frontend>,
    pub data_dir: PathBuf,
    pub profile: String,
    pub screen: Screen,
    pub onboarding: Onboarding,
    pub focus: Focus,
    /// The global detail level, persisted next to the GUI's copy.
    pub detail_level: DetailLevel,
    pub settings_tab: SettingsTab,
    /// The setup step, 1 to 7, and the name being collected.
    pub setup_step: usize,
    pub setup_name: String,
    /// The theme picked in setup. The TUI palette is installed once at start,
    /// so a choice is saved for the next launch.
    pub setup_theme: Option<usize>,

    pub guilds: Vec<GuildMeta>,
    pub guild_index: usize,
    /// Direct conversations, newest first, as the daemon lists them. The
    /// terminal sidebar keeps these apart from the commune tabs.
    pub direct_messages: Vec<DirectConversation>,
    /// Guild id to peer for every direct conversation, cached so drawing and
    /// key handling can tell a DM from a commune without another daemon call.
    direct_peers: HashMap<GuildId, AuthorId>,
    /// The member whose profile the overlay last showed, so a profile event
    /// can refresh the open panel.
    profile_author: Option<AuthorId>,
    direct_dirty: bool,
    /// A DM selected with `/dm` whose guild log has not synced yet. The
    /// composer is paused while this is set, so the next message cannot fall
    /// back to the public channel that was open before the command.
    pub(crate) pending_dm: Option<(GuildId, String)>,
    pub channels: Vec<ChannelView>,
    pub categories: Vec<CategoryMeta>,
    pub channel_index: usize,
    pub messages: Vec<SignedEvent>,
    pub members: Vec<MemberInfo>,
    pub show_members: bool,

    pub input: Input,
    pub scroll_back: usize,
    /// Whether `history_before` reported at least one older event.
    pub history_more: bool,
    /// Set once a backwards page has reached the beginning of the channel.
    /// A later newest-page refresh must not make "load older" reappear.
    history_complete: bool,
    /// The "new messages" divider renders before this event and stays put
    /// while older pages are prepended.
    pub unread_divider: Option<EventId>,
    /// Unread message counts per channel, derived from the channel history.
    pub unread: HashMap<ChannelId, usize>,
    /// Channel ids per guild, used to total a guild's badge.
    pub(crate) channels_by_guild: HashMap<GuildId, Vec<ChannelId>>,
    /// Read cursors advanced in this session, so a marker that could not be
    /// persisted does not resurrect a divider when the channel is reopened.
    session_read: HashMap<ChannelId, SyncCursor>,
    unread_dirty: bool,
    /// The first message visible in the last draw and the screen row it was
    /// on. Consumed by the draw pass after a prepend to keep the reader still.
    pub top_anchor: Option<(EventId, u16)>,
    pub anchor_to: Option<(EventId, u16)>,
    /// A message the user asked to jump to. The draw pass consumes it: only
    /// the draw pass knows how the transcript wrapped at the current width, so
    /// it is the only place that can turn a message into a scroll offset.
    pub jump_to: Option<EventId>,
    /// The latest message that mentioned this user, for Ctrl+M.
    pub mention: Option<EventId>,
    /// Ring the terminal bell once after the next draw.
    pub bell: bool,
    pub online_peers: HashSet<PeerId>,
    pub listen_addrs: Vec<Multiaddr>,
    /// Set by network events and flushed once per screen redraw, so a sync of
    /// hundreds of events costs one reload rather than hundreds.
    guilds_dirty: bool,
    channels_dirty: bool,
    messages_dirty: bool,
    members_dirty: bool,
    pub online_guild: Option<GuildId>,
    /// Whether the daemon socket answered the last reload. The TUI always
    /// talks to a daemon, so this is the equivalent of the GUI's top-bar
    /// connection indicator.
    pub daemon_online: bool,
    /// The daemon's reported version, shown at the right of the status bar
    /// like the GUI's status line.
    pub daemon_version: Option<String>,
    /// Presence frames from the control session, keyed by author.
    pub presence: HashMap<AuthorId, PresenceStatus>,
    pub own_presence: PresenceStatus,
    /// When this window last announced its presence, in milliseconds since the
    /// Unix epoch. The daemon expires a silent member after two three-second
    /// lease windows, so a guild that stays online needs a frame every so
    /// often, and this clock is what stops the tick from flooding it.
    presence_sent_at_ms: u64,
    /// The guild whose control session this window has joined. A TUI user is
    /// only in the roster, and only expires, after a `Join` frame.
    control_joined: Option<GuildId>,
    /// Notification preferences, loaded from the daemon and written back when
    /// `/dnd`, `/mute` or `/quiet` changes them.
    pub notifications: NotificationSettings,
    /// The most recent `/search` results, with `/open <n>` to jump to one.
    pub search_hits: Vec<ovvc_ipc::SearchHit>,
    /// Typing frames, keyed by channel and author, with the expiry in
    /// milliseconds since the Unix epoch.
    pub typing: HashMap<ChannelId, HashMap<AuthorId, u64>>,
    /// Local voice-call state and the snapshots the sidebar renders.
    pub(crate) voice: voice::VoiceUi,
    /// The bracket spinner position. It only advances while something is
    /// actually loading, so an idle terminal is not redrawn every frame.
    pub(crate) spinner: Spinner,
    /// A connect or join request is in flight and the daemon has not reported
    /// a listen address yet.
    pub(crate) connecting: bool,
    /// A peer connected and its history sync has not completed.
    pub(crate) syncing: bool,
    /// When the current sync window started, so a peer that never reports
    /// `Synced` cannot leave the spinner running forever.
    syncing_since_ms: u64,
    /// An older history page is being fetched off the input thread.
    pub(crate) history_loading: bool,
    /// The message and screen row to keep still when that page arrives.
    history_anchor: Option<(EventId, u16)>,
    /// The message the next send will answer, when reply mode is on.
    pub reply: Option<ReplyTarget>,
    pub status: String,
    /// The last error the daemon reported, shown on its own line so it does
    /// not wipe the network status. Esc clears it once the input is empty.
    pub error: Option<String>,
    /// Why the error strip has the colour and label it has. `None` is a local
    /// frontend message that predates the taxonomy or does not know its cause.
    pub error_class: Option<ErrorClass>,
    /// The daemon log a bug-class error names, when the daemon supplied it.
    pub error_log: Option<String>,
    pub overlay: Option<Overlay>,
    pub should_quit: bool,
    /// Whether the push-to-talk key is currently held. Kept separately from
    /// [`voice::VoiceUi::push_to_talk`], which is the roster flag.
    ptt_held: bool,
    /// Whether this terminal has delivered a key-release event. Terminals
    /// without the enhanced keyboard protocol never do, so a second press
    /// falls back to toggling the held state rather than leaving the
    /// microphone hot.
    ptt_release_seen: bool,

    pub(crate) options: AppOptions,
    action: Option<Action>,
}

impl App {
    pub fn new(client: Arc<dyn Frontend>, data_dir: PathBuf, options: AppOptions) -> Result<Self> {
        let existing_name = profile::load(&data_dir);
        let daemon_version = client.daemon_status().ok().map(|status| status.version);
        let screen = if existing_name.is_some() {
            Screen::Chat
        } else {
            Screen::Onboarding
        };

        let mut app = Self {
            client,
            data_dir,
            profile: existing_name.unwrap_or_default(),
            screen,
            onboarding: Onboarding::default(),
            focus: Focus::Composer,
            detail_level: DetailLevel::load(),
            settings_tab: SettingsTab::User,
            setup_step: 1,
            setup_name: String::new(),
            setup_theme: None,
            guilds: Vec::new(),
            guild_index: 0,
            direct_messages: Vec::new(),
            direct_peers: HashMap::new(),
            profile_author: None,
            direct_dirty: false,
            pending_dm: None,
            channels: Vec::new(),
            categories: Vec::new(),
            channel_index: 0,
            messages: Vec::new(),
            members: Vec::new(),
            show_members: true,
            input: Input::default(),
            scroll_back: 0,
            history_more: false,
            history_complete: false,
            unread_divider: None,
            unread: HashMap::new(),
            channels_by_guild: HashMap::new(),
            session_read: HashMap::new(),
            unread_dirty: false,
            top_anchor: None,
            anchor_to: None,
            jump_to: None,
            mention: None,
            bell: false,
            online_peers: HashSet::new(),
            listen_addrs: Vec::new(),
            guilds_dirty: false,
            channels_dirty: false,
            messages_dirty: false,
            members_dirty: false,
            online_guild: None,
            daemon_online: false,
            daemon_version,
            presence: HashMap::new(),
            own_presence: PresenceStatus::Online,
            presence_sent_at_ms: 0,
            control_joined: None,
            notifications: NotificationSettings::default(),
            search_hits: Vec::new(),
            typing: HashMap::new(),
            voice: voice::VoiceUi::default(),
            spinner: Spinner::default(),
            connecting: false,
            syncing: false,
            syncing_since_ms: 0,
            history_loading: false,
            history_anchor: None,
            reply: None,
            status: String::new(),
            error: None,
            error_class: None,
            error_log: None,
            overlay: None,
            should_quit: false,
            ptt_held: false,
            ptt_release_seen: false,
            options,
            action: None,
        };

        let preferred = app
            .options
            .guild
            .as_ref()
            .and_then(|needle| app.client.find_guild(needle).ok())
            .map(|guild| guild.id);
        app.reload_guilds(preferred);
        app.reload_channels();
        app.enter_current_channel();
        app.reload_members();
        app.refresh_unread_counts();
        app.unread_dirty = false;

        // An existing profile with no guilds goes straight to the create/join
        // choice instead of a dead chat screen.
        if app.screen == Screen::Chat && app.guilds.is_empty() {
            app.screen = Screen::Onboarding;
            app.onboarding.step = Step::Choice;
            app.onboarding.name = Input::with_text(&app.profile);
        }

        if app.screen == Screen::Chat
            && app.options.network_requested()
            && app.current_guild().is_some()
        {
            app.queue_connect();
        }

        Ok(app)
    }

    // --- lookups ---------------------------------------------------------

    pub fn current_guild(&self) -> Option<&GuildMeta> {
        self.guilds.get(self.guild_index)
    }

    pub fn current_channel(&self) -> Option<&ChannelView> {
        self.channels.get(self.channel_index)
    }

    pub fn take_action(&mut self) -> Option<Action> {
        self.action.take()
    }

    fn current_ids(&self) -> Option<(GuildId, ChannelId)> {
        Some((self.current_guild()?.id, self.current_channel()?.id))
    }

    /// Whether `guild_id` is a direct conversation rather than a commune.
    pub fn is_direct(&self, guild_id: GuildId) -> bool {
        self.direct_peers.contains_key(&guild_id)
    }

    /// The peer's name for a direct conversation, when `guild_id` is one.
    pub fn direct_peer_name(&self, guild_id: GuildId) -> Option<String> {
        self.direct_messages
            .iter()
            .find(|conversation| conversation.guild.id == guild_id)
            .map(|conversation| conversation.peer.display_name.clone())
    }

    /// The peer's name when the selected guild is a direct conversation.
    pub fn current_direct(&self) -> Option<String> {
        let guild = self.current_guild()?;
        self.direct_peer_name(guild.id)
    }

    /// Indices of the real communes in `guilds`, in tab order.
    ///
    /// Direct conversations live in the same guild list because they are
    /// ordinary guilds underneath, but the commune tabs, Ctrl+G and Alt+1..9
    /// must skip them.
    pub fn commune_indices(&self) -> Vec<usize> {
        self.guilds
            .iter()
            .enumerate()
            .filter(|(_, guild)| !self.is_direct(guild.id))
            .map(|(index, _)| index)
            .collect()
    }

    // --- reloading -------------------------------------------------------

    fn reload_guilds(&mut self, preferred: Option<GuildId>) {
        let previous = preferred.or_else(|| self.current_guild().map(|guild| guild.id));
        match self.client.list_guilds() {
            Ok(guilds) => {
                self.guilds = guilds;
                self.daemon_online = true;
            }
            Err(error) => {
                self.guilds = Vec::new();
                self.daemon_online = false;
                self.status = format!("daemon unavailable: {error}");
            }
        }

        self.guild_index = previous
            .and_then(|id| self.guilds.iter().position(|guild| guild.id == id))
            .unwrap_or(0);
        if self.guild_index >= self.guilds.len() {
            self.guild_index = self.guilds.len().saturating_sub(1);
        }
        self.reload_direct_messages();
    }

    /// Select a `/dm` conversation as soon as its guild log syncs.
    fn resolve_pending_dm(&mut self) {
        let Some((dm_id, name)) = self.pending_dm.clone() else {
            return;
        };
        if let Some(index) = self.guilds.iter().position(|guild| guild.id == dm_id) {
            self.pending_dm = None;
            self.select_guild(index);
            self.status = format!("dm:{name}");
            self.focus = Focus::Composer;
        }
    }

    /// Refresh the direct conversations and the guild-to-peer map.
    ///
    /// A failure only clears the cached list; it must not overwrite the
    /// daemon-online flag that `reload_guilds` just set.
    fn reload_direct_messages(&mut self) {
        match self.client.list_direct_messages() {
            Ok(conversations) => {
                self.direct_peers = conversations
                    .iter()
                    .map(|conversation| (conversation.guild.id, conversation.peer.author))
                    .collect();
                self.direct_messages = conversations;
            }
            Err(_) => {
                self.direct_messages.clear();
                self.direct_peers.clear();
            }
        }
    }

    pub(crate) fn reload_channels(&mut self) {
        let previous = self.current_channel().map(|channel| channel.id);
        let guild = self.current_guild().map(|guild| guild.id);
        self.channels = match guild {
            Some(guild) => self.client.list_channel_views(guild).unwrap_or_default(),
            None => Vec::new(),
        };
        self.categories = match guild {
            Some(guild) => self.client.list_categories(guild).unwrap_or_default(),
            None => Vec::new(),
        };
        self.notifications = self.client.notification_settings().unwrap_or_default();
        self.channel_index = previous
            .and_then(|id| self.channels.iter().position(|channel| channel.id == id))
            .unwrap_or(0);
        if self.channel_index >= self.channels.len() {
            self.channel_index = 0;
        }
    }

    /// Install the newest page without throwing away older pages the reader
    /// already paged back to.
    fn reload_messages(&mut self) {
        let Some((guild, channel)) = self.current_ids() else {
            self.messages.clear();
            self.history_more = false;
            return;
        };
        match self
            .client
            .history_before(guild, channel, None, MESSAGE_PAGE)
        {
            Ok(page) => {
                merge_newest_page(&mut self.messages, &page.events);
                if !self.history_complete {
                    self.history_more = page.more;
                    if !page.more {
                        self.history_complete = true;
                    }
                }
            }
            Err(error) => self.status = format!("could not load history: {error}"),
        }
    }

    /// Clear the window and open the currently selected channel: load the
    /// newest page, capture the read marker for the divider, then remember
    /// that the bottom is on screen.
    fn enter_current_channel(&mut self) {
        self.messages.clear();
        self.history_more = false;
        self.history_complete = false;
        self.history_loading = false;
        self.history_anchor = None;
        self.unread_divider = None;
        self.anchor_to = None;
        self.top_anchor = None;
        self.scroll_back = 0;
        self.jump_to = None;
        self.reload_messages();
        self.capture_unread_divider();
        self.mark_seen_if_at_bottom();
        self.unread_dirty = true;
    }

    /// Capture the read marker for the open channel and locate the first
    /// message after it.
    fn capture_unread_divider(&mut self) {
        let Some(channel) = self.current_channel().map(|channel| channel.id) else {
            self.unread_divider = None;
            return;
        };
        let me = self.client.author_id();
        let cursor = read_cursor_for(&self.messages, me, self.session_read.get(&channel).copied());
        self.unread_divider = first_unread(&self.messages, me, cursor);
    }

    /// While the newest message is on screen, record that the channel has been
    /// read. The divider itself is left alone until the channel is reopened, so
    /// the "new messages" marker stays where it was captured.
    ///
    /// The marker is a signed event, so it is only written when the newest
    /// message is one this session has not recorded yet. Scrolling, resizing or
    /// any other redraw that lands at the bottom must not append an event each
    /// time.
    fn mark_seen_if_at_bottom(&mut self) {
        if self.scroll_back != 0 {
            return;
        }
        let Some((guild, channel)) = self.current_ids() else {
            return;
        };
        let newest = self
            .messages
            .iter()
            .rev()
            .find(|event| event.body.kind.is_message());
        if let Some(event) = newest
            && let Ok(cursor) = SyncCursor::at(event)
            && self.session_read.get(&channel) != Some(&cursor)
        {
            self.session_read.insert(channel, cursor);
            if let Ok(event_id) = event.id() {
                let _ = self.client.mark_read(guild, channel, event_id);
            }
        }
        self.unread.remove(&channel);
    }

    /// Load one older page from the oldest event in the window.
    ///
    /// The fetch is handed to the event loop rather than run here: the daemon
    /// call blocks, and drawing the spinner before it starts is what makes the
    /// load visible on a slow link.
    fn load_older(&mut self) {
        let Some((guild, channel)) = self.current_ids() else {
            return;
        };
        if !self.history_more {
            self.status = "no older messages".to_string();
            return;
        }
        if self.history_loading {
            self.status = "still loading older messages".to_string();
            return;
        }
        let Some(cursor) = oldest_cursor(&self.messages) else {
            self.history_more = false;
            return;
        };
        self.history_loading = true;
        self.history_anchor = self.top_anchor;
        self.action = Some(Action::LoadHistory {
            guild_id: guild,
            channel_id: channel,
            cursor,
        });
    }

    /// Merge a page that [`Action::LoadHistory`] fetched on another thread.
    fn history_page(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        page: std::result::Result<EventPage, String>,
    ) {
        self.history_loading = false;
        // A page for a channel the reader has already left must not land in
        // the new transcript.
        if self.current_guild().map(|guild| guild.id) != Some(guild_id)
            || self.current_channel().map(|channel| channel.id) != Some(channel_id)
        {
            self.history_anchor = None;
            return;
        }
        match page {
            Ok(page) => {
                if page.events.is_empty() {
                    self.history_more = false;
                    self.history_complete = true;
                    self.history_anchor = None;
                    return;
                }
                // Remember the top message so the draw pass can put it back on
                // the same screen row after the prepend.
                self.anchor_to = self.history_anchor.take();
                prepend_page(&mut self.messages, &page.events);
                self.history_more = page.more && !self.history_complete;
                if !page.more {
                    self.history_complete = true;
                }
                self.unread_dirty = true;
                self.status = "loaded older messages".to_string();
            }
            Err(error) => {
                self.history_anchor = None;
                self.status = format!("could not load older messages: {error}");
            }
        }
    }

    /// Recompute unread counts for every guild from the signed read markers.
    ///
    /// The daemon answers from its `READ_STATE` table, so a count survives a
    /// restart and is not limited to one page of history the way a client side
    /// scan would be. The open channel reads as zero while its newest message
    /// is on screen, because that is what the reader is looking at.
    fn refresh_unread_counts(&mut self) {
        let current = self.current_channel().map(|channel| channel.id);
        let mut unread = HashMap::new();
        let mut channels_by_guild = HashMap::new();

        for guild in &self.guilds {
            let channels = self.client.list_channel_views(guild.id).unwrap_or_default();
            let ids: Vec<ChannelId> = channels.iter().map(|channel| channel.id).collect();
            let counts = self.client.unread_counts(guild.id).unwrap_or_default();
            for channel in &channels {
                let mut count = counts.get(&channel.id).copied().unwrap_or(0);
                if Some(channel.id) == current && self.scroll_back == 0 {
                    count = 0;
                }
                if count > 0 {
                    unread.insert(channel.id, count);
                }
            }
            channels_by_guild.insert(guild.id, ids);
        }

        self.unread = unread;
        self.channels_by_guild = channels_by_guild;
    }

    /// Total unread messages across one guild's channels.
    pub fn guild_unread(&self, guild_id: GuildId) -> usize {
        self.channels_by_guild
            .get(&guild_id)
            .map(|channels| {
                channels
                    .iter()
                    .map(|channel| self.unread.get(channel).copied().unwrap_or(0))
                    .sum()
            })
            .unwrap_or(0)
    }

    fn reload_members(&mut self) {
        self.members = match self.current_guild() {
            Some(guild) => self.client.list_members(guild.id).unwrap_or_default(),
            None => Vec::new(),
        };
    }

    fn select_guild(&mut self, index: usize) {
        if index >= self.guilds.len() {
            return;
        }
        // Choosing a guild is an explicit answer to a paused `/dm`; do not
        // keep blocking the composer for a conversation the reader left.
        self.pending_dm = None;
        let guild_id = self.guilds[index].id;
        let guild_changed = self.current_guild().map(|guild| guild.id) != Some(guild_id);
        // A call must never point at a channel in another guild, and the
        // channel selection is reset below even when the guild is unchanged.
        self.voice_leave_best_effort();
        if guild_changed {
            self.voice.clear_all();
        }
        let direct = self.direct_peers.contains_key(&guild_id);
        if let Some(online) = self.online_guild.take() {
            self.client.disconnect(online);
            self.listen_addrs.clear();
            self.online_peers.clear();
        }
        self.presence_sent_at_ms = 0;
        self.control_joined = None;
        self.guild_index = index;
        self.scroll_back = 0;
        self.jump_to = None;
        self.mention = None;
        self.reply = None;
        self.presence.clear();
        self.typing.clear();
        self.reload_channels();
        self.enter_current_channel();
        self.reload_members();
        if direct {
            // The daemon opened and saved this conversation's node when the
            // DM was created; reconnecting it as a commune would be wrong.
            let name = self
                .direct_peer_name(guild_id)
                .unwrap_or_else(|| self.guilds[index].name.clone());
            self.status = format!("dm:{name}");
        } else if self.options.network_requested() {
            self.queue_connect();
        }
    }

    fn select_channel(&mut self, index: usize) {
        if index >= self.channels.len() {
            return;
        }
        // An explicit channel change also releases a paused `/dm`.
        self.pending_dm = None;
        if self.voice.channel != Some(self.channels[index].id) {
            self.voice_leave_best_effort();
        }
        self.channel_index = index;
        self.scroll_back = 0;
        self.jump_to = None;
        self.mention = None;
        self.reply = None;
        self.enter_current_channel();
    }

    fn cycle_channel(&mut self, delta: isize) {
        if self.channels.is_empty() {
            return;
        }
        let len = self.channels.len() as isize;
        let next = (self.channel_index as isize + delta).rem_euclid(len);
        self.select_channel(next as usize);
    }

    fn cycle_guild(&mut self, delta: isize) {
        let communes = self.commune_indices();
        if communes.is_empty() {
            return;
        }
        let len = communes.len() as isize;
        let next = match communes.iter().position(|index| *index == self.guild_index) {
            Some(position) => (position as isize + delta).rem_euclid(len) as usize,
            // A direct conversation is not in the cycle, so start at the first
            // commune rather than stepping relative to a guild that has no tab.
            None => 0,
        };
        self.select_guild(communes[next]);
    }

    // --- networking ------------------------------------------------------

    fn queue_connect(&mut self) {
        let Some(guild) = self.current_guild().map(|guild| guild.id) else {
            self.status = "create or join a commune before connecting".to_string();
            return;
        };
        if self.online_guild == Some(guild) {
            self.status = "already online in this commune".to_string();
            return;
        }
        if let Some(other) = self.online_guild.take() {
            self.client.disconnect(other);
        }
        let options = self.options.network_options();
        self.presence_sent_at_ms = 0;
        self.connecting = true;
        self.status = "connecting…".to_string();
        self.action = Some(Action::Connect(guild, options));
    }

    fn disconnect(&mut self) {
        self.voice_leave_best_effort();
        self.connecting = false;
        self.syncing = false;
        if let Some(guild) = self.online_guild.take() {
            self.client.disconnect(guild);
            self.listen_addrs.clear();
            self.online_peers.clear();
            self.presence_sent_at_ms = 0;
            self.control_joined = None;
            self.status = "offline".to_string();
        } else {
            self.status = "not connected".to_string();
        }
    }

    /// How often a networked guild re-announces presence, in milliseconds.
    ///
    /// A control-session lease expires after two three-second windows, so two
    /// seconds leaves room for a missed frame without making the daemon carry
    /// a stream of them.
    pub const PRESENCE_KEEPALIVE_MS: u64 = 2_000;

    /// The guild that needs a presence frame now, if any.
    ///
    /// Presence is a lease in the control session: a member that says nothing
    /// for two three-second windows is marked offline, and that includes the
    /// status this user picked, so a networked guild has to keep saying it.
    pub fn presence_due(&mut self, now_ms: u64) -> Option<GuildId> {
        let guild = self.online_guild?;
        if self.presence_sent_at_ms > 0
            && now_ms.saturating_sub(self.presence_sent_at_ms) < Self::PRESENCE_KEEPALIVE_MS
        {
            return None;
        }
        self.presence_sent_at_ms = now_ms;
        Some(guild)
    }

    /// Re-announce presence for the guild that is online, silently.
    ///
    /// A daemon that has gone away should not rewrite the status line every
    /// two seconds, so a failed frame only means the next tick tries again.
    pub fn presence_tick(&mut self, now_ms: u64) {
        if let Some(guild) = self.presence_due(now_ms) {
            let _ = self.client.set_presence(guild, self.own_presence);
        }
    }

    /// Whether a connect, sync or history load is on screen.
    ///
    /// A sync has a deadline because the daemon reports completion per peer;
    /// a peer that connects and then goes quiet must not animate forever.
    pub(crate) fn spinner_active(&self) -> bool {
        self.connecting
            || self.history_loading
            || self.onboarding.step == Step::Joining
            || (self.syncing
                && now_ms().saturating_sub(self.syncing_since_ms) < SYNC_SPINNER_MAX_MS)
    }

    /// Advance the spinner one frame, but only while it is on screen.
    pub(crate) fn spinner_tick(&mut self) {
        if self.spinner_active() {
            self.spinner.tick();
        }
    }

    /// Join the control session for a guild that has just come online.
    ///
    /// Without the `Join` frame the daemon has no roster entry for this user,
    /// so its presence frame is broadcast but no lease keeps it fresh, and the
    /// member never shows a display name to the other frontends.
    fn join_control_session(&mut self, guild: GuildId) {
        if self.control_joined == Some(guild) || self.profile.is_empty() {
            return;
        }
        let session = ovvc_control::SessionId::for_guild(guild);
        if self
            .client
            .control_join(guild, session, &self.profile)
            .is_ok()
        {
            self.control_joined = Some(guild);
        }
    }

    fn toggle_network(&mut self) {
        if self.online_guild.is_some() {
            self.disconnect();
        } else {
            self.queue_connect();
        }
    }

    /// True when an author's transport identity is currently connected.
    ///
    /// Presence is tracked by peer id rather than by member record: a peer
    /// connects before (or while) its membership event syncs, so looking the
    /// author up in the roster would miss it.
    pub fn is_author_online(&self, author: AuthorId) -> bool {
        peer_id_for_author(author).is_some_and(|peer| self.online_peers.contains(&peer))
    }

    /// The presence the member panel and message headers paint. A control
    /// frame wins when one has arrived; otherwise the transport connection is
    /// the fallback, which is all a peer that has not joined a session sends.
    pub fn presence_for(&self, author: AuthorId) -> PresenceStatus {
        if let Some(status) = self.presence.get(&author) {
            return *status;
        }
        if author == self.client.author_id() {
            return if self.online_guild.is_some() {
                PresenceStatus::Online
            } else {
                PresenceStatus::Offline
            };
        }
        if self.is_author_online(author) {
            PresenceStatus::Online
        } else {
            PresenceStatus::Offline
        }
    }

    /// Authors whose typing indicator has not expired for `channel`.
    pub fn typing_in(&self, channel: ChannelId) -> Vec<AuthorId> {
        let now = now_ms();
        let me = self.client.author_id();
        let mut authors: Vec<AuthorId> = self
            .typing
            .get(&channel)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|(author, until)| **until > now && **author != me)
                    .map(|(author, _)| *author)
                    .collect()
            })
            .unwrap_or_default();
        authors.sort();
        authors
    }

    // --- input events ----------------------------------------------------

    pub fn on_event(&mut self, event: ratatui::crossterm::event::Event) {
        let ratatui::crossterm::event::Event::Key(key) = event else {
            return;
        };
        match key.kind {
            ratatui::crossterm::event::KeyEventKind::Press
            | ratatui::crossterm::event::KeyEventKind::Repeat => self.on_key(key),
            ratatui::crossterm::event::KeyEventKind::Release => {
                // Some terminals only report releases with the enhanced
                // keyboard protocol. Remember that this one does, so a second
                // discrete press is treated as a hold start rather than as a
                // release fallback.
                self.ptt_release_seen = true;
                self.on_key_release(key);
            }
        }
    }

    fn on_key_release(&mut self, key: KeyEvent) {
        if key.code == voice::PTT_KEY {
            self.set_ptt_held(false);
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Push-to-talk consumes its key while the call is live, so the key
        // never leaks into the composer. The roster flag must be on and the
        // person must be in a call for the key to mean anything.
        if key.code == voice::PTT_KEY && self.voice.joined() && self.voice.push_to_talk {
            if self.ptt_held {
                // Key repeat keeps it held. If this terminal never sends a
                // release, a second discrete press toggles it off instead of
                // leaving the microphone hot forever.
                if !self.ptt_release_seen {
                    self.set_ptt_held(false);
                }
            } else {
                self.set_ptt_held(true);
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        match self.screen {
            Screen::Onboarding => self.on_onboarding_key(key),
            Screen::Chat => self.on_chat_key(key),
            Screen::Settings => self.on_settings_key(key),
            Screen::Setup => self.on_setup_key(key),
            Screen::NetworkMap | Screen::Diagnostics => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.screen = Screen::Chat;
                }
            }
        }
    }

    /// Change the global level and persist it. Both frontends read the same
    /// file, so a change here is visible in the GUI after its next start.
    pub(crate) fn set_detail_level(&mut self, level: DetailLevel) {
        self.detail_level = level;
        if let Err(error) = level.save() {
            self.status = format!("could not save the detail level: {error}");
        }
    }

    fn on_settings_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Chat,
            KeyCode::Left | KeyCode::BackTab => {
                let current = SettingsTab::ALL
                    .iter()
                    .position(|tab| *tab == self.settings_tab)
                    .unwrap_or(0);
                self.settings_tab = SettingsTab::ALL[current.saturating_sub(1)];
            }
            KeyCode::Right | KeyCode::Tab => {
                let current = SettingsTab::ALL
                    .iter()
                    .position(|tab| *tab == self.settings_tab)
                    .unwrap_or(0);
                self.settings_tab =
                    SettingsTab::ALL[(current + 1).min(SettingsTab::ALL.len() - 1)];
            }
            KeyCode::Char('[') => {
                let index = self.detail_level.index().saturating_sub(1);
                self.set_detail_level(DetailLevel::from_index(index));
            }
            KeyCode::Char(']') | KeyCode::Char('l') => {
                let index = (self.detail_level.index() + 1).min(2);
                self.set_detail_level(DetailLevel::from_index(index));
            }
            KeyCode::Char('u') => self.settings_tab = SettingsTab::User,
            KeyCode::Char('v') => self.settings_tab = SettingsTab::VoiceVideo,
            KeyCode::Char('n') => self.settings_tab = SettingsTab::Network,
            KeyCode::Char('p') => self.settings_tab = SettingsTab::Preferences,
            KeyCode::Char('d') if self.settings_tab == SettingsTab::Network => {
                self.screen = Screen::Diagnostics;
            }
            _ => {}
        }
    }

    fn on_setup_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Chat;
                self.status = "setup cancelled; nothing was applied".to_string();
            }
            KeyCode::Char('n') | KeyCode::Down | KeyCode::Enter => {
                if self.setup_step == 1 {
                    let name = self.input.text().trim().to_string();
                    if name.is_empty() {
                        self.status = "a display name is required".to_string();
                        return;
                    }
                    self.setup_name = name;
                }
                if self.setup_step == 6 {
                    if !self.setup_name.is_empty() {
                        self.profile = self.setup_name.clone();
                        if let Err(error) = profile::save(&self.data_dir, &self.setup_name) {
                            self.status = format!("could not save profile: {error}");
                        }
                    }
                    if let Some(index) = self.setup_theme
                        && let Some(preset) = ovvc_theme::Preset::all().get(index).copied()
                    {
                        let _ = ovvc_theme::save_preset(preset);
                    }
                    self.screen = Screen::Chat;
                    self.status = "setup finished".to_string();
                } else {
                    self.setup_step = (self.setup_step + 1).min(6);
                }
            }
            KeyCode::Char('p') | KeyCode::Up => {
                self.setup_step = self.setup_step.saturating_sub(1).max(1);
            }
            KeyCode::Char(character)
                if self.setup_step == 3 && ('1'..='4').contains(&character) =>
            {
                self.setup_theme = Some(match character {
                    '1' => 0,
                    '2' => 1,
                    '3' => 2,
                    _ => 3,
                });
            }
            KeyCode::Backspace if self.setup_step == 1 => self.input.backspace(),
            KeyCode::Delete if self.setup_step == 1 => self.input.delete(),
            KeyCode::Left if self.setup_step == 1 => self.input.left(),
            KeyCode::Right if self.setup_step == 1 => self.input.right(),
            KeyCode::Home if self.setup_step == 1 => self.input.home(),
            KeyCode::End if self.setup_step == 1 => self.input.end(),
            KeyCode::Char(character)
                if self.setup_step == 1
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input.insert(character)
            }
            _ => {}
        }
    }

    fn on_onboarding_key(&mut self, key: KeyEvent) {
        let step = self.onboarding.step;

        if step == Step::Joining {
            return;
        }

        if step == Step::Privacy {
            match key.code {
                KeyCode::Enter => self.onboarding.step = Step::Choice,
                // `esc: back` in the footer has to go somewhere: this step's
                // predecessor is the name step.
                KeyCode::Esc => self.onboarding.step = Step::Name,
                _ => {}
            }
            return;
        }

        if step == Step::Choice {
            match key.code {
                KeyCode::Char('1') => {
                    self.onboarding.choice = 0;
                    self.onboarding.step = Step::CreateGuild;
                }
                KeyCode::Char('2') => {
                    self.onboarding.choice = 1;
                    self.onboarding.step = Step::JoinGuild;
                }
                KeyCode::Char('3') => {
                    self.finish_onboarding();
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.onboarding.choice = self.onboarding.choice.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.onboarding.choice = (self.onboarding.choice + 1).min(2);
                }
                KeyCode::Enter => match self.onboarding.choice {
                    0 => self.onboarding.step = Step::CreateGuild,
                    1 => self.onboarding.step = Step::JoinGuild,
                    _ => self.finish_onboarding(),
                },
                KeyCode::Esc => self.onboarding.step = Step::Privacy,
                _ => {}
            }
            return;
        }

        let input = match step {
            Step::Name => &mut self.onboarding.name,
            _ => &mut self.onboarding.text,
        };

        match key.code {
            KeyCode::Enter => match step {
                Step::Name => {
                    let name = self.onboarding.name.text().trim().to_string();
                    if name.is_empty() {
                        self.onboarding.error = Some("a name is required".to_string());
                    } else {
                        if let Err(error) = profile::save(&self.data_dir, &name) {
                            self.onboarding.error =
                                Some(format!("could not save profile: {error}"));
                        }
                        self.profile = name;
                        self.onboarding.error = None;
                        self.onboarding.step = Step::Privacy;
                    }
                }
                Step::CreateGuild => {
                    let name = self.onboarding.text.text().trim().to_string();
                    if name.is_empty() {
                        self.onboarding.error = Some("a commune name is required".to_string());
                        return;
                    }
                    match self.client.create_guild(&name) {
                        Ok(guild) => {
                            let _ = self.client.set_display_name(guild.id, &self.profile);
                            self.reload_guilds(Some(guild.id));
                            self.reload_channels();
                            self.enter_current_channel();
                            self.reload_members();
                            self.refresh_unread_counts();
                            self.status = format!("created {}", guild.name);
                            self.finish_onboarding();
                        }
                        Err(error) => {
                            self.onboarding.error =
                                Some(format!("could not create commune: {error}"));
                        }
                    }
                }
                Step::JoinGuild => {
                    let code = self.onboarding.text.text().trim().to_string();
                    if code.is_empty() {
                        self.onboarding.error = Some("paste an invite code".to_string());
                        return;
                    }
                    self.onboarding.step = Step::Joining;
                    self.onboarding.error = None;
                    self.action = Some(Action::Join(code, self.options.network_options()));
                }
                _ => {}
            },
            KeyCode::Backspace => input.backspace(),
            KeyCode::Delete => input.delete(),
            KeyCode::Left => input.left(),
            KeyCode::Right => input.right(),
            KeyCode::Home => input.home(),
            KeyCode::End => input.end(),
            KeyCode::Esc if step == Step::CreateGuild || step == Step::JoinGuild => {
                self.onboarding.step = Step::Choice;
                self.onboarding.text.clear();
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                input.insert(c)
            }
            _ => {}
        }
    }

    fn on_chat_key(&mut self, key: KeyEvent) {
        if self.overlay.is_some() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                self.overlay = None;
                self.profile_author = None;
            }
            return;
        }

        // Esc dismisses the error strip only when nothing else wants the key,
        // so clearing a draft or cancelling a reply still comes first.
        if key.code == KeyCode::Esc
            && self.error.is_some()
            && self.input.is_empty()
            && self.reply.is_none()
        {
            self.error = None;
            return;
        }

        match key.code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Composer => Focus::Sidebar,
                    Focus::Sidebar => Focus::Composer,
                };
                return;
            }
            KeyCode::F(2) => {
                self.show_members = !self.show_members;
                return;
            }
            KeyCode::F(5) => {
                self.toggle_network();
                return;
            }
            KeyCode::Char(digit @ '1'..='9')
                if key.modifiers.contains(KeyModifiers::ALT)
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let position = digit as usize - '1' as usize;
                if let Some(index) = self.commune_indices().get(position).copied() {
                    self.select_guild(index);
                }
                return;
            }
            KeyCode::PageUp if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.load_older();
                return;
            }
            KeyCode::PageUp => {
                self.scroll_up(SCROLL_STEP);
                return;
            }
            KeyCode::PageDown => {
                self.scroll_down(SCROLL_STEP);
                return;
            }
            KeyCode::Char('m') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.jump_to_latest_mention();
                return;
            }
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // The current-user card's `[T]` is the theme path: settings,
                // preferences, theme.
                self.screen = Screen::Settings;
                self.settings_tab = SettingsTab::Preferences;
                return;
            }
            _ => {}
        }

        // The global level is a text control in the TUI. It is changed with
        // `[` and `]` or `l` when the composer is empty, so it never steals a
        // bracket from a message draft.
        if self.input.is_empty() && !key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('[') => {
                    let index = self.detail_level.index().saturating_sub(1);
                    self.set_detail_level(DetailLevel::from_index(index));
                    return;
                }
                KeyCode::Char(']') => {
                    let index = (self.detail_level.index() + 1).min(2);
                    self.set_detail_level(DetailLevel::from_index(index));
                    return;
                }
                KeyCode::Char('l') => {
                    let index = (self.detail_level.index() + 1) % 3;
                    self.set_detail_level(DetailLevel::from_index(index));
                    return;
                }
                _ => {}
            }
        }

        match self.focus {
            Focus::Sidebar => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.select_channel(self.channel_index.saturating_sub(1))
                }
                KeyCode::Down | KeyCode::Char('j') => self.select_channel(self.channel_index + 1),
                KeyCode::Enter => self.focus = Focus::Composer,
                KeyCode::Esc => self.focus = Focus::Composer,
                _ => {}
            },
            Focus::Composer => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match key.code {
                        KeyCode::Char('n') => self.cycle_channel(1),
                        KeyCode::Char('p') => self.cycle_channel(-1),
                        KeyCode::Char('g') => self.cycle_guild(1),
                        KeyCode::Char('u') => self.input.clear(),
                        _ => {}
                    }
                    return;
                }

                match key.code {
                    KeyCode::Enter => self.submit(),
                    KeyCode::Esc => {
                        if self.reply.take().is_some() {
                            self.status = "reply cancelled".to_string();
                        } else {
                            self.input.clear();
                        }
                    }
                    KeyCode::Backspace => self.input.backspace(),
                    KeyCode::Delete => self.input.delete(),
                    KeyCode::Left => self.input.left(),
                    KeyCode::Right => self.input.right(),
                    KeyCode::Home => self.input.home(),
                    KeyCode::End => self.input.end(),
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => {
                        self.input.insert(c)
                    }
                    _ => {}
                }
            }
        }
    }

    fn scroll_up(&mut self, amount: usize) {
        self.scroll_back = self.scroll_back.saturating_add(amount);
    }

    fn scroll_down(&mut self, amount: usize) {
        self.scroll_back = self.scroll_back.saturating_sub(amount);
    }

    /// Record a message that mentioned this user: ring the bell and offer a
    /// jump when the message is in the channel currently on screen.
    fn notice_mention(
        &mut self,
        author: AuthorId,
        event_id: Option<EventId>,
        in_current_channel: bool,
    ) {
        self.bell = true;
        let name = self
            .members
            .iter()
            .find(|member| member.author == author)
            .map(|member| member.display_name.clone())
            .unwrap_or_else(|| author.short());
        if in_current_channel && let Some(id) = event_id {
            self.mention = Some(id);
            self.status = format!("{name} mentioned you — Ctrl+M to jump");
        } else {
            self.status = format!("{name} mentioned you");
        }
    }

    fn jump_to_latest_mention(&mut self) {
        match self.mention.take() {
            Some(id) => {
                self.jump_to = Some(id);
                self.status = "jumping to the mention".to_string();
            }
            None => self.status = "no mention to jump to".to_string(),
        }
    }

    /// Resolve a message by full id or unique hex prefix in the loaded window.
    fn find_message(&self, needle: &str) -> std::result::Result<EventId, String> {
        let needle = needle.trim();
        if needle.is_empty() {
            return Err("a message id is required".to_string());
        }
        let matches: Vec<EventId> = self
            .messages
            .iter()
            .filter(|event| event.body.kind.is_message())
            .filter_map(|event| event.id().ok())
            .filter(|id| id.to_hex().starts_with(needle))
            .collect();
        match matches.len() {
            0 => Err(format!("no message with id '{needle}' in this channel")),
            1 => Ok(matches[0]),
            _ => Err(format!("message id '{needle}' is ambiguous")),
        }
    }

    // --- submitting ------------------------------------------------------

    fn submit(&mut self) {
        let text = self.input.take();
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if let Some(command) = text.strip_prefix('/') {
            self.command(command);
        } else if let Some((_, name)) = &self.pending_dm {
            // `/dm` selected a conversation that is not in the store yet. The
            // public channel is still the current one, so sending now would
            // leak the message there. Keep the draft and say why.
            self.input = Input::with_text(text);
            self.status = format!("dm:{name} is still syncing; message not sent");
        } else {
            let reply = self.reply.take();
            if !self.send_message(text, reply.as_ref().map(|reply| reply.target)) {
                // Keep the reply line up when the send failed, so the user can
                // fix the text and press enter again.
                self.reply = reply;
            }
        }
    }

    fn send_message(&mut self, body: &str, reply_to: Option<EventId>) -> bool {
        let Some((guild, channel)) = self.current_ids() else {
            self.status = "create or join a commune before sending".to_string();
            return false;
        };
        let mentions = parse_mentions(body, &self.members);
        match self
            .client
            .post_message_full(guild, channel, body, reply_to, &mentions)
        {
            Ok(_) => {
                self.scroll_back = 0;
                self.reload_messages();
                self.mark_seen_if_at_bottom();
                self.reload_members();
                self.unread_dirty = true;
                true
            }
            Err(error) => {
                self.status = format!("send failed: {error}");
                false
            }
        }
    }

    /// Post a reply to the message `needle` names. `rest` is the text after
    /// `/reply <id>`.
    fn send_reply(&mut self, target: &str, rest: &str) {
        let body = rest.trim();
        if body.is_empty() {
            self.status = "usage: /reply <message-id> <text>".to_string();
            return;
        }
        let target = match self.find_message(target) {
            Ok(id) => id,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        let Some((guild, channel)) = self.current_ids() else {
            self.status = "create or join a commune before sending".to_string();
            return;
        };
        let mentions = parse_mentions(body, &self.members);
        match self
            .client
            .post_message_full(guild, channel, body, Some(target), &mentions)
        {
            Ok(_) => {
                self.scroll_back = 0;
                self.reload_messages();
                self.mark_seen_if_at_bottom();
                self.reload_members();
                self.unread_dirty = true;
            }
            Err(error) => self.status = format!("reply failed: {error}"),
        }
    }

    /// `/reply <id>` without text: hold the target above the composer until
    /// the next send, the way the GUI's reply line works.
    fn begin_reply(&mut self, needle: &str) {
        if needle.trim().is_empty() {
            self.status = "usage: /reply <message-id> [text]".to_string();
            return;
        }
        let target = match self.find_message(needle) {
            Ok(id) => id,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        let Some(event) = self
            .messages
            .iter()
            .find(|event| event.id().ok() == Some(target))
        else {
            self.status = "that message is no longer loaded".to_string();
            return;
        };
        let author = event.body.author;
        let body = event
            .body
            .kind
            .message_body()
            .unwrap_or_default()
            .to_string();
        let name = self.display_name_of(author);
        self.reply = Some(ReplyTarget {
            target,
            author,
            body,
        });
        self.status = format!("replying to {name} — enter to send, esc to cancel");
    }

    fn display_name_of(&self, author: AuthorId) -> String {
        if author == self.client.author_id() && !self.profile.is_empty() {
            return self.profile.clone();
        }
        self.members
            .iter()
            .find(|member| member.author == author)
            .map(|member| member.display_name.clone())
            .unwrap_or_else(|| author.short())
    }

    /// `/attach <path> [caption]`: read a file, store it as a blob, then post
    /// the message that announces it.
    ///
    /// Reading and hashing happen on this thread, so a very large file pauses
    /// the interface for the moment it takes to hash it. That is the same pause
    /// the store would cause on any other send, and it keeps the failure path a
    /// single, honest status line.
    fn attach_file(&mut self, rest: &str) {
        let Some((path, caption)) = split_attach_args(rest) else {
            self.status = "usage: /attach <file> [caption]".to_string();
            return;
        };
        let Some((guild, channel)) = self.current_ids() else {
            self.status = "create or join a commune before sending".to_string();
            return;
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status = format!("could not read {}: {error}", path.display());
                return;
            }
        };
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            self.status = format!(
                "{} is {} bytes, the limit is {MAX_ATTACHMENT_BYTES}",
                path.display(),
                bytes.len()
            );
            return;
        }
        let attachment = match attachment_for(&path, &bytes) {
            Ok(attachment) => attachment,
            Err(error) => {
                self.status = format!("cannot attach {}: {error}", path.display());
                return;
            }
        };
        if let Err(error) = self.client.put_blob(&bytes) {
            self.status = format!("could not store the file: {error}");
            return;
        }
        let mentions = parse_mentions(&caption, &self.members);
        let attachments = [attachment];
        match self
            .client
            .post_message_rich(guild, channel, &caption, None, &mentions, &attachments)
        {
            Ok(_) => {
                self.scroll_back = 0;
                self.reload_messages();
                self.mark_seen_if_at_bottom();
                self.reload_members();
                self.unread_dirty = true;
                self.status = format!("attached {}", path.display());
            }
            Err(error) => self.status = format!("send failed: {error}"),
        }
    }

    fn command(&mut self, command: &str) {
        let (name, rest) = match command.split_once(char::is_whitespace) {
            Some((name, rest)) => (name, rest.trim()),
            None => (command, ""),
        };

        // Any command replaces the overlay; only `/profile` keeps a member
        // selected for event-driven refreshes.
        self.profile_author = None;

        match name {
            "help" => {
                self.overlay = Some(Overlay {
                    title: "commands".to_string(),
                    body: vec![
                        "/connect            go online in the current commune".to_string(),
                        "/disconnect         stop networking".to_string(),
                        "/invite             create an invite code to share".to_string(),
                        "/join <code>        verify and join a commune".to_string(),
                        "/new <name>         create a commune".to_string(),
                        "/newchannel [kind] <name> [in <category>]  create a text, announcement, voice or forum channel"
                            .to_string(),
                        "/newcategory <name>  create a channel category".to_string(),
                        "/renamechannel <channel> <name>".to_string(),
                        "/movechannel <channel> <category|root>".to_string(),
                        "/search <text>      search this commune".to_string(),
                        "/open <n>           open the nth search result".to_string(),
                        "/presence <status>  online, away, dnd or offline".to_string(),
                        "/dnd <on|off>       do not disturb".to_string(),
                        "/desktop <on|off>   desktop notifications".to_string(),
                        "/quiet <from> <to>  quiet hours in UTC, or /quiet off".to_string(),
                        "/mute [channel|guild]   mute (default: this channel)".to_string(),
                        "/unmute [channel|guild]".to_string(),
                        "/voice <sub>        voice calls: join, leave, mute, deafen, ptt, status"
                            .to_string(),
                        "/settings [tab]     full-page settings: user, voice, network, preferences"
                            .to_string(),
                        "/setup              the stepped first-run setup flow".to_string(),
                        "/map                network map of swarms and peers".to_string(),
                        "/diagnostics        error classes, daemon log and policy read model"
                            .to_string(),
                        "/level <simple|advanced|expert>  global detail level".to_string(),
                        "/policy             show this commune's policy and client floor"
                            .to_string(),
                        "/profile [member]   show a profile (default: you)".to_string(),
                        "/about <text>       set your about line, one line per command (empty clears)"
                            .to_string(),
                        "/pronouns <text>    set your pronouns (empty clears)".to_string(),
                        "/links <url> [<url>...]  set up to 5 http(s) links (empty clears)"
                            .to_string(),
                        "/dm [member]        list direct messages, or open one with a member"
                            .to_string(),
                        "/guild|/commune <n|name>  switch commune".to_string(),
                        "/channel <name>     switch channel".to_string(),
                        "/reply <id> [text]  reply now, or hold the target for the next send"
                            .to_string(),
                        "/edit <id> <text>   edit a message you wrote".to_string(),
                        "/delete <id>        delete a message you wrote".to_string(),
                        "/react <id> <emoji> add a reaction".to_string(),
                        "/unreact <id> <emoji> remove your reaction".to_string(),
                        "/jump <id>          jump to a message".to_string(),
                        "/history            load an older page".to_string(),
                        "/attach <file> [cap] announce a file, quoting a path with spaces"
                            .to_string(),
                        "/members            toggle the member list".to_string(),
                        "/name <name>        change your display name".to_string(),
                        "/config             show daemon, identity and theme".to_string(),
                        "/status             show network state".to_string(),
                        "/cancel             clear the reply line or input".to_string(),
                        "/whoami             show your identity".to_string(),
                        "/quit               exit".to_string(),
                        String::new(),
                        "Keys: Ctrl+N/P channel · Ctrl+G commune · Alt+1..9 commune tab".to_string(),
                        "      Ctrl+M latest mention · Ctrl+PgUp load older".to_string(),
                        "      PgUp/PgDn scroll · Tab sidebar · F2 members · F5 network"
                            .to_string(),
                        "      Esc cancels a reply · Ctrl+C quit".to_string(),
                        "      mentions: type @name or @id-prefix".to_string(),
                    ],
                });
            }
            "quit" | "exit" => self.should_quit = true,
            "connect" => self.queue_connect(),
            "disconnect" => self.disconnect(),
            "invite" => self.show_invite(),
            "join" => {
                if rest.is_empty() {
                    self.status = "usage: /join <invite code>".to_string();
                } else {
                    self.action = Some(Action::Join(
                        rest.to_string(),
                        self.options.network_options(),
                    ));
                    self.status = "joining…".to_string();
                }
            }
            "new" => {
                if rest.is_empty() {
                    self.status = "usage: /new <commune name>".to_string();
                    return;
                }
                match self.client.create_guild(rest) {
                    Ok(guild) => {
                        let _ = self.client.set_display_name(guild.id, &self.profile);
                        self.reload_guilds(Some(guild.id));
                        self.reload_channels();
                        self.enter_current_channel();
                        self.reload_members();
                        self.refresh_unread_counts();
                        self.status = format!("created {}", guild.name);
                    }
                    Err(error) => self.status = format!("could not create commune: {error}"),
                }
            }
            "newchannel" => {
                let Some(guild) = self.current_guild().map(|guild| guild.id) else {
                    self.status = "no commune selected".to_string();
                    return;
                };
                let (kind, spec) = channel_spec(rest);
                let (name, category) = split_in_category(spec);
                if name.is_empty() {
                    self.status =
                        "usage: /newchannel [text|announcement|voice|forum] <name> [in <category>]"
                            .to_string();
                    return;
                }
                let parent = match category {
                    Some(category) => match self
                        .categories
                        .iter()
                        .find(|known| known.name.eq_ignore_ascii_case(category))
                    {
                        Some(known) => Some(known.id),
                        None => {
                            self.status =
                                format!("no category named {category}; /newcategory makes one");
                            return;
                        }
                    },
                    None => None,
                };
                match self.client.create_channel_in(guild, name, kind, parent) {
                    Ok(channel) => {
                        self.reload_channels();
                        if let Some(index) = self.channels.iter().position(|c| c.id == channel.id) {
                            self.select_channel(index);
                        }
                        self.status = match parent {
                            Some(_) => format!(
                                "created {} {} in {category}",
                                kind.glyph(),
                                channel.name,
                                category = category.unwrap_or_default()
                            ),
                            None => format!("created {} {}", kind.glyph(), channel.name),
                        };
                    }
                    Err(error) => self.status = format!("could not create channel: {error}"),
                }
            }
            "newcategory" | "category" => {
                let Some(guild) = self.current_guild().map(|guild| guild.id) else {
                    self.status = "no commune selected".to_string();
                    return;
                };
                if rest.is_empty() {
                    self.status = "usage: /newcategory <name>".to_string();
                    return;
                }
                match self.client.create_category(guild, rest) {
                    Ok(category) => {
                        self.reload_channels();
                        self.status = format!("created category {}", category.name);
                    }
                    Err(error) => self.status = format!("could not create category: {error}"),
                }
            }
            "renamechannel" => {
                let Some(guild) = self.current_guild().map(|guild| guild.id) else {
                    self.status = "no commune selected".to_string();
                    return;
                };
                let Some((target, name)) = rest.split_once(char::is_whitespace) else {
                    self.status = "usage: /renamechannel <channel> <name>".to_string();
                    return;
                };
                let name = name.trim();
                if name.is_empty() {
                    self.status = "usage: /renamechannel <channel> <name>".to_string();
                    return;
                }
                match self.client.find_channel(guild, target) {
                    Ok(channel) => match self.client.rename_channel(guild, channel.id, name) {
                        Ok(()) => {
                            self.reload_channels();
                            self.status = format!(
                                "renamed to {} {name}",
                                channel_kind_of(&self.channels, channel.id).glyph()
                            );
                        }
                        Err(error) => self.status = format!("rename failed: {error}"),
                    },
                    Err(error) => self.status = format!("{error}"),
                }
            }
            "movechannel" => {
                let Some(guild) = self.current_guild().map(|guild| guild.id) else {
                    self.status = "no commune selected".to_string();
                    return;
                };
                let Some((target, destination)) = rest.split_once(char::is_whitespace) else {
                    self.status = "usage: /movechannel <channel> <category|root>".to_string();
                    return;
                };
                let destination = destination.trim();
                let parent = if destination.eq_ignore_ascii_case("root") {
                    None
                } else {
                    match self
                        .categories
                        .iter()
                        .find(|category| category.name.eq_ignore_ascii_case(destination))
                    {
                        Some(category) => Some(category.id),
                        None => {
                            self.status = format!("no category named {destination}");
                            return;
                        }
                    }
                };
                match self.client.find_channel(guild, target) {
                    Ok(channel) => match self.client.move_channel(guild, channel.id, parent) {
                        Ok(()) => {
                            self.reload_channels();
                            self.status = format!("moved {}", channel.name);
                        }
                        Err(error) => self.status = format!("move failed: {error}"),
                    },
                    Err(error) => self.status = format!("{error}"),
                }
            }
            "search" | "find" => {
                let Some(guild) = self.current_guild().map(|guild| guild.id) else {
                    self.status = "no commune selected".to_string();
                    return;
                };
                if rest.is_empty() {
                    self.status = "usage: /search <text>".to_string();
                    return;
                }
                match self.client.search(guild, rest, 50) {
                    Ok(hits) => {
                        let mut body = Vec::new();
                        for (index, hit) in hits.iter().enumerate() {
                            let channel = self
                                .channels
                                .iter()
                                .find(|channel| channel.id == hit.channel_id)
                                .map(|channel| format!("{}{}", channel.kind.glyph(), channel.name))
                                .unwrap_or_else(|| "?".to_string());
                            body.push(format!(
                                "[{}] {} <{}> {}",
                                index + 1,
                                channel,
                                crate::ui::display_name(self, hit.event.body.author),
                                crate::ui::elide(&ovvc_core::markdown::plain(&hit.body), 64)
                            ));
                        }
                        if body.is_empty() {
                            body.push("no matches".to_string());
                        }
                        self.status = format!("{} matches for {rest}", hits.len());
                        self.search_hits = hits;
                        self.overlay = Some(Overlay {
                            title: format!("search: {rest}"),
                            body,
                        });
                    }
                    Err(error) => self.status = format!("search failed: {error}"),
                }
            }
            "open" => {
                let Ok(index) = rest.parse::<usize>() else {
                    self.status = "usage: /open <number from /search>".to_string();
                    return;
                };
                let Some(hit) = self.search_hits.get(index.saturating_sub(1)).cloned() else {
                    self.status = format!("no search result {index}");
                    return;
                };
                if let Some(position) = self
                    .guilds
                    .iter()
                    .position(|guild| guild.id == hit.guild_id)
                {
                    self.select_guild(position);
                }
                self.reload_channels();
                if let Some(position) = self
                    .channels
                    .iter()
                    .position(|channel| channel.id == hit.channel_id)
                {
                    self.select_channel(position);
                    if let Ok(id) = hit.event.id() {
                        self.jump_to = Some(id);
                    }
                    self.overlay = None;
                }
            }
            "presence" => {
                let Some(status) = presence_from_name(rest) else {
                    self.status = "usage: /presence <online|away|dnd|offline>".to_string();
                    return;
                };
                let Some(guild) = self.current_guild().map(|guild| guild.id) else {
                    self.status = "no commune selected".to_string();
                    return;
                };
                match self.client.set_presence(guild, status) {
                    Ok(()) => {
                        self.own_presence = status;
                        self.status = format!("presence: {rest}");
                    }
                    Err(error) => self.status = format!("presence failed: {error}"),
                }
            }
            "dnd" | "desktop" => {
                let on = match rest {
                    "on" => true,
                    "off" => false,
                    _ => {
                        self.status = format!("usage: /{name} <on|off>");
                        return;
                    }
                };
                let result = if name == "dnd" {
                    self.client.set_do_not_disturb(on)
                } else {
                    self.client.set_desktop_notifications(on)
                };
                match result {
                    Ok(()) => {
                        if name == "dnd" {
                            self.notifications.dnd = on;
                        } else {
                            self.notifications.desktop = on;
                        }
                        self.status = format!("{name}: {}", if on { "on" } else { "off" });
                    }
                    Err(error) => {
                        self.refresh_notifications();
                        self.status = format!("could not save preference: {error}");
                    }
                }
            }
            "quiet" => {
                if rest == "off" {
                    match self.client.set_quiet_hours(None) {
                        Ok(()) => {
                            self.notifications.quiet_hours = None;
                            self.status = "quiet hours off".to_string();
                        }
                        Err(error) => {
                            self.refresh_notifications();
                            self.status = format!("could not save quiet hours: {error}");
                        }
                    }
                    return;
                }
                let mut parts = rest.split_whitespace();
                let (Some(start), Some(end), None) = (parts.next(), parts.next(), parts.next())
                else {
                    self.status = "usage: /quiet <HH:MM> <HH:MM> or /quiet off".to_string();
                    return;
                };
                match (parse_minute(start), parse_minute(end)) {
                    (Some(start), Some(end)) if start != end => {
                        let quiet = QuietHours {
                            start_minute: start,
                            end_minute: end,
                        };
                        match self.client.set_quiet_hours(Some(quiet)) {
                            Ok(()) => {
                                self.notifications.quiet_hours = Some(quiet);
                                self.status = format!("quiet hours {start}..{end} UTC");
                            }
                            Err(error) => {
                                self.refresh_notifications();
                                self.status = format!("could not save quiet hours: {error}");
                            }
                        }
                    }
                    _ => self.status = "usage: /quiet <HH:MM> <HH:MM> (UTC)".to_string(),
                }
            }
            "voice" => self.voice_command(rest),
            "mute" | "unmute" => {
                let Some(guild) = self.current_guild().map(|guild| guild.id) else {
                    self.status = "no commune selected".to_string();
                    return;
                };
                let mute = name == "mute";
                let (result, target) = if rest.eq_ignore_ascii_case("guild") {
                    (self.client.set_guild_muted(guild, mute), "guild")
                } else {
                    let Some(channel) = self.current_channel().map(|channel| channel.id) else {
                        self.status = "no channel selected".to_string();
                        return;
                    };
                    (self.client.set_channel_muted(channel, mute), "channel")
                };
                match result {
                    Ok(()) => {
                        if target == "guild" {
                            toggle_id(&mut self.notifications.muted_guilds, guild, mute);
                        } else if let Some(channel) =
                            self.current_channel().map(|channel| channel.id)
                        {
                            toggle_id(&mut self.notifications.muted_channels, channel, mute);
                        }
                        self.status = format!(
                            "{name}d {}",
                            if target == "guild" { "commune" } else { target }
                        );
                    }
                    Err(error) => {
                        self.refresh_notifications();
                        self.status = format!("could not save mute: {error}");
                    }
                }
            }
            "settings" => {
                self.screen = Screen::Settings;
                self.overlay = None;
                // `/settings preferences` opens a tab directly.
                if let Some(tab) = SettingsTab::ALL
                    .iter()
                    .find(|tab| tab.label().eq_ignore_ascii_case(rest))
                {
                    self.settings_tab = *tab;
                }
            }
            "setup" => {
                self.screen = Screen::Setup;
                self.overlay = None;
                self.setup_step = 1;
                self.setup_name = self.profile.clone();
                self.input = crate::input::Input::default();
                for character in self.setup_name.chars() {
                    self.input.insert(character);
                }
            }
            "map" | "network-map" => {
                self.screen = Screen::NetworkMap;
                self.overlay = None;
            }
            "diagnostics" | "doctor" => {
                self.screen = Screen::Diagnostics;
                self.overlay = None;
            }
            "level" => {
                let Some(level) = DetailLevel::parse(rest) else {
                    self.status = "usage: /level simple|advanced|expert".to_string();
                    return;
                };
                self.set_detail_level(level);
                self.status = format!("detail level: {}", level.label());
            }
            "channel" => {
                let Some(guild) = self.current_guild().map(|guild| guild.id) else {
                    self.status = "no commune selected".to_string();
                    return;
                };
                match self.client.find_channel(guild, rest) {
                    Ok(channel) => {
                        if let Some(index) = self.channels.iter().position(|c| c.id == channel.id) {
                            self.select_channel(index);
                            self.focus = Focus::Composer;
                        }
                    }
                    Err(error) => self.status = format!("{error}"),
                }
            }
            "reply" => match rest.split_once(char::is_whitespace) {
                Some((target, body)) => {
                    self.send_reply(target, body);
                    self.focus = Focus::Composer;
                }
                None => {
                    self.begin_reply(rest);
                    self.focus = Focus::Composer;
                }
            },
            "edit" => {
                let Some((target, body)) = rest.split_once(char::is_whitespace) else {
                    self.status = "usage: /edit <id> <text>".to_string();
                    return;
                };
                let body = body.trim();
                if body.is_empty() {
                    self.status = "usage: /edit <id> <text>".to_string();
                    return;
                }
                let Some(guild) = self.current_guild().map(|guild| guild.id) else {
                    self.status = "no commune selected".to_string();
                    return;
                };
                match self.find_message(target) {
                    Ok(id) => match self.client.edit_message(guild, id, body) {
                        Ok(_) => {
                            self.reload_messages();
                            self.status = "message edited".to_string();
                        }
                        Err(error) => self.status = format!("edit failed: {error}"),
                    },
                    Err(error) => self.status = error,
                }
            }
            "delete" | "del" => {
                let Some(guild) = self.current_guild().map(|guild| guild.id) else {
                    self.status = "no commune selected".to_string();
                    return;
                };
                match self.find_message(rest) {
                    Ok(id) => match self.client.delete_message(guild, id) {
                        Ok(_) => {
                            self.reload_messages();
                            self.status = "message deleted".to_string();
                        }
                        Err(error) => self.status = format!("delete failed: {error}"),
                    },
                    Err(error) => self.status = error,
                }
            }
            "react" | "unreact" => {
                let Some((target, emoji)) = rest.split_once(char::is_whitespace) else {
                    self.status = format!("usage: /{name} <id> <emoji>");
                    return;
                };
                let emoji = emoji.trim();
                if emoji.is_empty() {
                    self.status = format!("usage: /{name} <id> <emoji>");
                    return;
                }
                let Some(guild) = self.current_guild().map(|guild| guild.id) else {
                    self.status = "no commune selected".to_string();
                    return;
                };
                match self.find_message(target) {
                    Ok(id) => {
                        let result = if name == "react" {
                            self.client.add_reaction(guild, id, emoji)
                        } else {
                            self.client.remove_reaction(guild, id, emoji)
                        };
                        match result {
                            Ok(_) => {
                                self.reload_messages();
                                self.status = if name == "react" {
                                    format!("reacted with {emoji}")
                                } else {
                                    format!("removed {emoji}")
                                };
                            }
                            Err(error) => self.status = format!("reaction failed: {error}"),
                        }
                    }
                    Err(error) => self.status = error,
                }
            }
            "jump" => match self.find_message(rest) {
                Ok(id) => {
                    self.jump_to = Some(id);
                    self.status = "jumping".to_string();
                    self.focus = Focus::Composer;
                }
                Err(error) => self.status = error,
            },
            "history" | "older" => self.load_older(),
            "attach" => self.attach_file(rest),
            "members" => self.show_members = !self.show_members,
            "profile" => self.show_profile(rest),
            "about" | "pronouns" | "links" => self.edit_profile_field(name, rest),
            "dm" => self.direct_message_command(rest),
            "name" => {
                let name = rest.trim();
                if name.is_empty() {
                    self.status = "usage: /name <display name>".to_string();
                    return;
                }
                if let Err(error) = profile::save(&self.data_dir, name) {
                    self.status = format!("could not save your name: {error}");
                    return;
                }
                self.profile = name.to_string();
                if let Some(guild) = self.current_guild().map(|guild| guild.id) {
                    let _ = self.client.set_display_name(guild, name);
                    self.reload_members();
                }
                self.status = format!("display name set to {name}");
            }
            "policy" => {
                let Some(guild_id) = self.current_guild().map(|guild| guild.id) else {
                    self.status = "no commune selected".to_string();
                    return;
                };
                match self.client.policy(guild_id) {
                    Ok(view) => {
                        self.overlay = Some(Overlay {
                            title: "policy".to_string(),
                            body: policy_lines(&view),
                        });
                    }
                    Err(error) => self.status = format!("could not read policy: {error}"),
                }
            }
            "config" => {
                let theme = crate::theme::theme_path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "built-in".to_string());
                let guild = self
                    .current_guild()
                    .map(|guild| guild.name.clone())
                    .unwrap_or_else(|| "-".to_string());
                self.overlay = Some(Overlay {
                    title: "config".to_string(),
                    body: vec![
                        format!(
                            "daemon:   {}",
                            if self.daemon_online {
                                "online"
                            } else {
                                "offline"
                            }
                        ),
                        format!("identity: {}", self.client.author_id()),
                        format!("name:     {}", self.profile),
                        format!("commune:  {guild}"),
                        format!("data:     {}", self.data_dir.display()),
                        format!("theme:    {theme}"),
                        String::new(),
                        "set OVVC_THEME to a theme.toml path to restyle both".to_string(),
                        "frontends · change your name with /name <name>".to_string(),
                    ],
                });
            }
            "cancel" => {
                self.reply = None;
                self.input.clear();
                self.status.clear();
            }
            "guild" | "commune" => {
                if rest.is_empty() {
                    self.status = "usage: /guild|/commune <number|name>".to_string();
                    return;
                }
                if let Ok(position) = rest.parse::<usize>() {
                    // Numbers match the commune tabs, which do not count
                    // direct conversations.
                    if let Some(index) = self
                        .commune_indices()
                        .get(position.saturating_sub(1))
                        .copied()
                    {
                        self.select_guild(index);
                    }
                } else {
                    match self.client.find_guild(rest) {
                        Ok(guild) if self.is_direct(guild.id) => {
                            self.status = format!(
                                "{} is a direct message; use /dm {}",
                                guild.name, guild.name
                            )
                        }
                        Ok(guild) => {
                            if let Some(index) = self
                                .guilds
                                .iter()
                                .position(|candidate| candidate.id == guild.id)
                            {
                                self.select_guild(index);
                            }
                        }
                        Err(error) => self.status = format!("{error}"),
                    }
                }
            }
            "whoami" => {
                let peer = self
                    .current_guild()
                    .map(|guild| guild.id)
                    .and_then(|guild| self.client.peer_id(guild));
                self.overlay = Some(Overlay {
                    title: "identity".to_string(),
                    body: vec![
                        format!("name:     {}", self.profile),
                        format!("author:   {}", self.client.author_id()),
                        format!("short:    {}", self.client.author_id().short()),
                        format!(
                            "peer id:  {}",
                            peer.map(|peer| peer.to_string())
                                .unwrap_or_else(|| "offline".to_string())
                        ),
                        format!("data:     {}", self.data_dir.display()),
                    ],
                });
            }
            "status" => {
                let mut body = vec![format!(
                    "network: {}",
                    match self.online_guild {
                        Some(guild) => format!("online in {guild}"),
                        None => "offline".to_string(),
                    }
                )];
                if let Some(guild) = self.current_guild() {
                    body.push(match self.direct_peer_name(guild.id) {
                        Some(name) => format!("dm: {name} ({})", guild.id.short()),
                        None => format!("commune: {} ({})", guild.name, guild.id.short()),
                    });
                }
                if let Some(channel) = self.current_channel() {
                    body.push(format!("#{}", channel.name));
                }
                for address in &self.listen_addrs {
                    body.push(format!("listen:  {address}"));
                }
                if self.listen_addrs.is_empty() {
                    body.push("listen:  (none yet)".to_string());
                }
                body.push(format!("peers:   {}", self.online_peers.len()));
                self.overlay = Some(Overlay {
                    title: "status".to_string(),
                    body,
                });
            }
            "clear" => self.status.clear(),
            "" => {}
            other => self.status = format!("unknown command: /{other} (try /help)"),
        }
    }

    /// `/profile [member]`: show your own profile, or a member picked by
    /// display name or author-id prefix.
    fn show_profile(&mut self, query: &str) {
        let Some(guild) = self.current_guild().map(|guild| guild.id) else {
            self.status = "no commune selected".to_string();
            return;
        };
        let author = if query.trim().is_empty() {
            self.client.author_id()
        } else {
            match resolve_member(&self.members, query) {
                Ok(member) => member.author,
                Err(error) => {
                    self.status = error;
                    return;
                }
            }
        };
        match self.client.member_profile(guild, author) {
            Ok(profile) => {
                self.profile_author = Some(author);
                self.overlay = Some(profile_overlay(&profile, author == self.client.author_id()));
            }
            Err(error) => self.status = format!("could not load profile: {error}"),
        }
    }

    /// Refetch the open profile panel after a `MemberProfileUpdated` event.
    fn refresh_profile_overlay(&mut self) {
        let (Some(author), Some(guild)) = (
            self.profile_author,
            self.current_guild().map(|guild| guild.id),
        ) else {
            return;
        };
        if let Ok(profile) = self.client.member_profile(guild, author) {
            self.overlay = Some(profile_overlay(&profile, author == self.client.author_id()));
        }
    }

    /// `/about`, `/pronouns` and `/links`: read the current profile first so
    /// fields the command does not touch are preserved, validate before
    /// sending, then publish one complete profile event.
    fn edit_profile_field(&mut self, field: &str, rest: &str) {
        let Some(guild) = self.current_guild().map(|guild| guild.id) else {
            self.error = Some("no commune selected".to_string());
            return;
        };
        let me = self.client.author_id();
        let current = match self.client.member_profile(guild, me) {
            Ok(profile) => profile,
            Err(error) => {
                self.error = Some(format!("could not read your profile: {error}"));
                return;
            }
        };
        let (about, pronouns, links) = match field {
            "about" => {
                let about = rest.trim();
                if let Err(error) = check_about(about) {
                    self.error = Some(error);
                    return;
                }
                (about.to_string(), current.pronouns, current.links)
            }
            "pronouns" => {
                let pronouns = rest.trim();
                if let Err(error) = check_pronouns(pronouns) {
                    self.error = Some(error);
                    return;
                }
                (current.about, pronouns.to_string(), current.links)
            }
            "links" => {
                let links = match parse_profile_links(rest) {
                    Ok(links) => links,
                    Err(error) => {
                        self.error = Some(error);
                        return;
                    }
                };
                (current.about, current.pronouns, links)
            }
            _ => return,
        };
        match self.client.set_profile(guild, &about, &pronouns, &links) {
            Ok(_) => {
                self.error = None;
                self.status = match field {
                    "about" if about.is_empty() => "about cleared".to_string(),
                    "about" => "about updated".to_string(),
                    "pronouns" if pronouns.is_empty() => "pronouns cleared".to_string(),
                    "pronouns" => "pronouns updated".to_string(),
                    _ if links.is_empty() => "links cleared".to_string(),
                    _ => format!("{} link(s) saved", links.len()),
                };
            }
            Err(error) => {
                self.error = Some(format!("could not update your profile: {error}"));
            }
        }
    }

    /// `/dm [member]`: without an argument list the conversations; with one,
    /// reuse the conversation if it exists, otherwise ask the daemon to open
    /// it from the current commune.
    fn direct_message_command(&mut self, rest: &str) {
        if rest.trim().is_empty() {
            self.show_direct_messages();
            return;
        }
        let Some(guild) = self.current_guild().cloned() else {
            self.status = "no commune selected".to_string();
            return;
        };
        if self.is_direct(guild.id) {
            self.status = "switch to a commune before starting a direct message".to_string();
            return;
        }
        let member = match resolve_member(&self.members, rest) {
            Ok(member) => member,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        if member.author == self.client.author_id() {
            self.status = "cannot open a direct message with yourself".to_string();
            return;
        }
        // Reusing a known conversation keeps the common path local and avoids
        // asking the daemon to reopen something it already opened.
        let existing = self
            .direct_messages
            .iter()
            .find(|conversation| conversation.peer.author == member.author)
            .map(|conversation| {
                (
                    conversation.guild.id,
                    conversation.peer.display_name.clone(),
                )
            });
        if let Some((dm_id, name)) = existing {
            match self
                .guilds
                .iter()
                .position(|candidate| candidate.id == dm_id)
            {
                Some(index) => {
                    self.pending_dm = None;
                    self.select_guild(index);
                    self.status = format!("dm:{name}");
                }
                None => {
                    // The announcement names a conversation whose guild has
                    // not synced yet. `open_direct_message` creates the guild
                    // when it is missing, which on the recipient side would
                    // make a second founder for the same deterministic id.
                    // Wait for the normal guild-sync event instead, and pause
                    // the composer so a queued message cannot land in the
                    // public channel that is still selected.
                    self.pending_dm = Some((dm_id, name.clone()));
                    self.status = format!(
                        "dm:{name} is still syncing; the composer is paused until it is ready"
                    );
                    self.direct_dirty = true;
                }
            }
            self.focus = Focus::Composer;
            return;
        }

        match self.client.open_direct_message(guild.id, member.author) {
            Ok(dm) => {
                self.pending_dm = None;
                let name = dm.name.clone();
                self.reload_guilds(Some(dm.id));
                if let Some(index) = self.guilds.iter().position(|guild| guild.id == dm.id) {
                    self.select_guild(index);
                }
                let name = self.direct_peer_name(dm.id).unwrap_or(name);
                self.status = format!("dm:{name}");
                self.focus = Focus::Composer;
            }
            Err(error) => self.status = format!("could not open direct message: {error}"),
        }
    }

    fn show_direct_messages(&mut self) {
        self.reload_direct_messages();
        self.direct_dirty = false;
        if self.direct_messages.is_empty() {
            self.status = "no direct messages; /dm <member> starts one".to_string();
            return;
        }
        let body = self
            .direct_messages
            .iter()
            .enumerate()
            .map(|(index, conversation)| {
                format!(
                    "[{}] dm:{} <{}>",
                    index + 1,
                    conversation.peer.display_name,
                    conversation.peer.author.short()
                )
            })
            .collect();
        self.status = format!("{} direct message(s)", self.direct_messages.len());
        self.overlay = Some(Overlay {
            title: "direct messages".to_string(),
            body,
        });
    }

    fn refresh_notifications(&mut self) {
        if let Ok(settings) = self.client.notification_settings() {
            self.notifications = settings;
        }
    }

    fn show_invite(&mut self) {
        let Some(guild) = self.current_guild().map(|guild| guild.id) else {
            self.status = "no commune selected".to_string();
            return;
        };
        let Some(peer) = self.client.peer_id(guild) else {
            self.status = "connect first: /connect".to_string();
            return;
        };
        let peers: Vec<String> = self
            .listen_addrs
            .iter()
            .filter(|address| is_dialable(&address.to_string()))
            .map(|address| format!("{address}/p2p/{peer}"))
            .collect();
        if peers.is_empty() {
            self.status = "no reachable address yet; start with --listen /ip4/<your-lan-ip>/tcp/0"
                .to_string();
            return;
        }
        match self
            .client
            .create_invite(guild, peers.clone(), Some(86_400))
        {
            Ok(invite) => match invite.encode() {
                Ok(code) => {
                    let path = self.data_dir.join("last-invite.txt");
                    // The invite is a bearer credential: write it owner-only,
                    // and do not follow a symlink planted in the data dir.
                    let saved = crate::profile::write_private(&path, &code).is_ok();
                    let mut body = vec![
                        "share this code with someone to invite them:".to_string(),
                        String::new(),
                        code,
                        String::new(),
                        "addresses carried:".to_string(),
                    ];
                    body.extend(peers.into_iter().map(|peer| format!("  {peer}")));
                    if saved {
                        body.push(String::new());
                        body.push(format!("also saved to {}", path.display()));
                    }
                    self.overlay = Some(Overlay {
                        title: "invite".to_string(),
                        body,
                    });
                }
                Err(error) => self.status = format!("could not encode invite: {error}"),
            },
            Err(error) => self.status = format!("could not create invite: {error}"),
        }
    }

    // --- external events -------------------------------------------------

    pub fn on_notice(&mut self, notice: Notice) {
        match notice {
            Notice::Joined(guild) => {
                self.connecting = false;
                self.syncing = false;
                let _ = self.client.set_display_name(guild.id, &self.profile);
                self.reload_guilds(Some(guild.id));
                self.reload_channels();
                self.enter_current_channel();
                self.reload_members();
                self.refresh_unread_counts();
                self.online_guild = Some(guild.id);
                self.presence_sent_at_ms = 0;
                self.join_control_session(guild.id);
                self.status = format!("joined {} ({})", guild.name, guild.id.short());
                self.finish_onboarding();
            }
            Notice::Error(error) => {
                self.connecting = false;
                self.syncing = false;
                self.history_loading = false;
                self.history_anchor = None;
                if self.onboarding.step == Step::Joining {
                    self.onboarding.step = Step::JoinGuild;
                    self.onboarding.error = Some(error.clone());
                }
                self.status = error;
            }
            Notice::HistoryPage {
                guild_id,
                channel_id,
                page,
            } => self.history_page(guild_id, channel_id, page),
        }
    }

    pub fn on_client_event_result(
        &mut self,
        result: std::result::Result<ClientEvent, broadcast::error::RecvError>,
    ) {
        match result {
            Ok(event) => self.on_client_event(event),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                self.status = format!("skipped {skipped} network events");
                self.guilds_dirty = true;
                self.channels_dirty = true;
                self.messages_dirty = true;
                self.members_dirty = true;
            }
            Err(broadcast::error::RecvError::Closed) => {
                self.status = "event stream closed".to_string();
            }
        }
    }

    /// Put a classified daemon diagnostic on the error strip.
    ///
    /// The class decides the label and colour in the draw pass. An operational
    /// condition still gets a line, so a peer that went offline or a relay that
    /// was unavailable is visible, but it is labelled a warning rather than a
    /// bug. A bug carries the daemon log path into the strip.
    fn show_diagnostic(&mut self, diagnostic: Diagnostic) {
        // The daemon refused whatever was in flight; do not keep spinning for
        // an answer that is not coming.
        self.connecting = false;
        self.syncing = false;
        self.error_class = Some(diagnostic.class);
        self.error_log = diagnostic.log_path.clone();
        self.error = Some(diagnostic.message);
    }

    fn on_client_event(&mut self, event: ClientEvent) {
        let current = self.current_guild().map(|guild| guild.id);
        match event {
            ClientEvent::Listening { guild_id, address } => {
                // The connect request has produced a listen address, so the
                // "connecting" spinner has done its job.
                self.connecting = false;
                if !self.listen_addrs.contains(&address) {
                    self.listen_addrs.push(address.clone());
                }
                if Some(guild_id) == current {
                    self.online_guild = Some(guild_id);
                    self.presence_sent_at_ms = 0;
                    self.join_control_session(guild_id);
                    self.status = format!("listening on {address}");
                }
            }
            ClientEvent::ExternalAddr { address, .. } => {
                if !self.listen_addrs.contains(&address) {
                    self.listen_addrs.push(address);
                }
            }
            ClientEvent::ExternalAddrRemoved { address, .. } => {
                self.listen_addrs.retain(|candidate| candidate != &address);
            }
            ClientEvent::PeerConnected { guild_id, peer } => {
                if Some(guild_id) == current {
                    self.online_peers.insert(peer);
                    // A peer that has just connected is about to send its
                    // history; the spinner runs until `Synced` reports it done.
                    self.syncing = true;
                    self.syncing_since_ms = now_ms();
                    if let Some(author) = self
                        .presence
                        .keys()
                        .copied()
                        .find(|author| peer_id_for_author(*author) == Some(peer))
                    {
                        self.presence.insert(author, PresenceStatus::Online);
                    }
                    self.status = format!("peer connected: {}", short_peer(peer));
                }
            }
            ClientEvent::PeerDisconnected { guild_id, peer } => {
                if Some(guild_id) == current {
                    self.online_peers.remove(&peer);
                    if let Some(author) = self
                        .presence
                        .keys()
                        .copied()
                        .find(|author| peer_id_for_author(*author) == Some(peer))
                    {
                        self.presence.insert(author, PresenceStatus::Offline);
                    }
                    self.status = format!("peer disconnected: {}", short_peer(peer));
                    if self.online_peers.is_empty() {
                        // Nobody left to sync from, so stop waiting.
                        self.syncing = false;
                    }
                }
            }
            ClientEvent::HolePunch { peer, success, .. } => {
                self.status = if success {
                    format!("direct connection upgraded with {}", short_peer(peer))
                } else {
                    format!("hole punch with {} failed", short_peer(peer))
                };
            }
            ClientEvent::Synced { guild_id, peer } => {
                if Some(guild_id) == current {
                    // This peer's history is in, so the sync spinner stops.
                    self.syncing = false;
                    self.guilds_dirty = true;
                    self.channels_dirty = true;
                    self.messages_dirty = true;
                    self.members_dirty = true;
                    self.unread_dirty = true;
                    self.status = format!("synced with {}", short_peer(peer));
                }
            }
            ClientEvent::Event { guild_id, event } => {
                // A DM is a guild the user may not be looking at, and the
                // announcement that opens one is written to the context
                // commune, so these two refresh the sidebar even when the
                // event's guild is not on screen.
                match &event.body.kind {
                    EventKind::DirectMessageOpened { .. } | EventKind::GuildCreated { .. } => {
                        self.guilds_dirty = true;
                        self.direct_dirty = true;
                    }
                    _ => {}
                }
                if Some(guild_id) != current {
                    return;
                }
                let event_id = event.id().ok();
                let mentioned = event.body.author != self.client.author_id()
                    && event.body.kind.mentions_author(self.client.author_id());
                let in_current_channel =
                    event.body.channel_id == self.current_channel().map(|channel| channel.id);
                match event.body.kind {
                    EventKind::Message { .. }
                    | EventKind::MessageV2 { .. }
                    | EventKind::MessageV3 { .. }
                    | EventKind::MessageEdited { .. }
                    | EventKind::MessageDeleted { .. } => {
                        if in_current_channel {
                            self.messages_dirty = true;
                        }
                        self.unread_dirty = true;
                        if mentioned {
                            // Mutes, dnd and quiet hours apply to the alert the
                            // TUI raises itself, not only to the daemon's
                            // notification events. The daemon holds the copy
                            // that counts, and another frontend can change it
                            // while this window is open, so read it back first.
                            self.refresh_notifications();
                            let now = ovvc_control::now_ms() / 1000;
                            let suppressed = event.body.channel_id.map_or_else(
                                || self.notifications.suppresses_guild(guild_id, now),
                                |channel_id| {
                                    self.notifications.suppresses(guild_id, channel_id, now)
                                },
                            );
                            if !suppressed {
                                self.notice_mention(
                                    event.body.author,
                                    event_id,
                                    in_current_channel,
                                );
                            }
                        }
                    }
                    EventKind::MemberJoined { .. }
                    | EventKind::MemberRenamed { .. }
                    | EventKind::MemberLeft => {
                        self.members_dirty = true;
                        // A DM peer's name comes from the conversation list,
                        // not only from the current roster.
                        self.direct_dirty = true;
                    }
                    EventKind::ReactionAdded { .. } | EventKind::ReactionRemoved { .. } => {
                        if in_current_channel {
                            self.messages_dirty = true;
                        }
                    }
                    EventKind::ReadUpTo { .. } => {
                        self.unread_dirty = true;
                    }
                    EventKind::ChannelCreated { .. }
                    | EventKind::ChannelCreatedV2 { .. }
                    | EventKind::CategoryCreated { .. }
                    | EventKind::ChannelRenamed { .. }
                    | EventKind::ChannelMoved { .. } => {
                        self.channels_dirty = true;
                        self.unread_dirty = true;
                    }
                    EventKind::GuildCreated { .. } => {
                        self.guilds_dirty = true;
                        self.direct_dirty = true;
                        self.channels_dirty = true;
                        self.members_dirty = true;
                        self.unread_dirty = true;
                    }
                    EventKind::MemberProfileUpdated { .. } => {
                        if self.profile_author == Some(event.body.author) {
                            self.refresh_profile_overlay();
                        }
                    }
                    EventKind::DirectMessageOpened { .. } => {
                        // Already marked before the guild check; an
                        // announcement in this guild also belongs in the list.
                        self.direct_dirty = true;
                    }
                    // An event kind from a newer peer. It is stored in the log,
                    // but this binary has nothing to render or apply for it.
                    EventKind::Unknown { .. } => {}
                }
            }
            ClientEvent::LookingUp { author, .. } => {
                self.status = format!("looking up {} in the DHT", author.short());
            }
            ClientEvent::PeerFound {
                author, addresses, ..
            } => {
                self.status = format!("found {} ({} address(es))", author.short(), addresses.len());
            }
            ClientEvent::LookupFailed { author, reason, .. } => {
                self.status = format!("lookup for {} failed: {reason}", author.short());
            }
            ClientEvent::Error { message, .. } => {
                // A legacy daemon reports an unclassified error. Treat it as a
                // warning rather than a bug: the classes this build knows are
                // carried by `ClientEvent::Diagnostic`.
                self.show_diagnostic(Diagnostic::operational(message));
            }
            ClientEvent::Diagnostic(diagnostic) => self.show_diagnostic(diagnostic),
            ClientEvent::Control { guild_id, frame } => {
                if Some(guild_id) != current {
                    return;
                }
                match frame.kind {
                    ControlKind::Join { .. } => {
                        self.presence.insert(frame.from, PresenceStatus::Online);
                    }
                    ControlKind::Leave => {
                        self.presence.insert(frame.from, PresenceStatus::Offline);
                        for channel in self.typing.values_mut() {
                            channel.remove(&frame.from);
                        }
                    }
                    ControlKind::Presence { status, .. } => {
                        self.presence.insert(frame.from, status);
                    }
                    ControlKind::Typing {
                        channel_id,
                        until_ms,
                    } => {
                        self.typing
                            .entry(channel_id)
                            .or_default()
                            .insert(frame.from, until_ms);
                    }
                    _ => {}
                }
            }
            // Lease expiry and local changes arrive as their own events, with
            // no control frame to carry them.
            ClientEvent::Presence {
                guild_id,
                author,
                status,
            } => {
                if Some(guild_id) == current {
                    self.presence.insert(author, status);
                }
            }
            ClientEvent::Typing {
                guild_id,
                channel_id,
                author,
                until_ms,
            } => {
                if Some(guild_id) == current {
                    self.typing
                        .entry(channel_id)
                        .or_default()
                        .insert(author, until_ms);
                }
            }
            ClientEvent::TypingEnded {
                guild_id,
                channel_id,
                author,
            } if Some(guild_id) == current => {
                if let Some(channel) = self.typing.get_mut(&channel_id) {
                    channel.remove(&author);
                }
            }
            ClientEvent::Voice {
                guild_id,
                channel_id,
                change,
                snapshot,
            } => self.voice_on_event(guild_id, channel_id, change, snapshot),
            ClientEvent::VoiceAudio {
                guild_id,
                channel_id,
                stats,
            } => self.voice_on_audio(guild_id, channel_id, stats),
            // `ClientEvent` is non-exhaustive so a newer daemon can add an
            // event this binary has nothing to render for.
            _ => {}
        }
    }

    /// Apply every reload requested by events received since the last draw.
    pub fn flush_dirty(&mut self) {
        let previous_guild = self.current_guild().map(|guild| guild.id);
        let previous_channel = self.current_channel().map(|channel| channel.id);

        if self.guilds_dirty {
            let preferred = self.current_guild().map(|guild| guild.id);
            self.reload_guilds(preferred);
            self.guilds_dirty = false;
            self.direct_dirty = false;
        } else if self.direct_dirty {
            self.reload_direct_messages();
            self.direct_dirty = false;
        }
        // A DM that was paused when `/dm` ran may have arrived with this
        // refresh. Selecting it here is what makes the composer target the
        // conversation rather than the public channel underneath it.
        self.resolve_pending_dm();

        let guild_changed = self.current_guild().map(|guild| guild.id) != previous_guild;
        if guild_changed {
            // A guild can change under the user through a sync or a DM event.
            self.voice_leave_best_effort();
            self.voice.clear_all();
        }
        if self.channels_dirty || guild_changed {
            self.reload_channels();
            self.channels_dirty = false;
        }

        let channel_changed = self.current_channel().map(|channel| channel.id) != previous_channel;
        if channel_changed {
            // A different channel needs a fresh window and read marker.
            if self.voice.channel != self.current_channel().map(|channel| channel.id) {
                self.voice_leave_best_effort();
            }
            self.enter_current_channel();
            self.messages_dirty = false;
        } else if self.messages_dirty {
            // Merge the newest page so older pages already loaded survive, and
            // advance the session read cursor when the bottom is on screen.
            self.reload_messages();
            self.messages_dirty = false;
            self.mark_seen_if_at_bottom();
        }

        if self.members_dirty {
            self.reload_members();
            self.members_dirty = false;
        }

        if self.unread_dirty {
            self.refresh_unread_counts();
            self.unread_dirty = false;
        }
    }

    fn finish_onboarding(&mut self) {
        self.screen = Screen::Chat;
        self.focus = Focus::Composer;
        self.onboarding.error = None;
        if self.options.network_requested() && self.current_guild().is_some() {
            self.queue_connect();
        }
    }
}

/// Resolve a member by display name, or by full author id or unique prefix.
///
/// Names win over id prefixes so a two-word display name does not have to be
/// escaped, and ambiguity is reported instead of picking the first match.
fn resolve_member(members: &[MemberInfo], query: &str) -> std::result::Result<MemberInfo, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("no member given".to_string());
    }
    let by_name: Vec<&MemberInfo> = members
        .iter()
        .filter(|member| member.display_name.eq_ignore_ascii_case(query))
        .collect();
    match by_name.len() {
        1 => return Ok(by_name[0].clone()),
        0 => {}
        count => return Err(format!("{query} matches {count} members by name")),
    }
    let prefix = query.to_ascii_lowercase();
    let by_id: Vec<&MemberInfo> = members
        .iter()
        .filter(|member| {
            member
                .author
                .to_string()
                .to_ascii_lowercase()
                .starts_with(&prefix)
        })
        .collect();
    match by_id.len() {
        1 => Ok(by_id[0].clone()),
        0 => Err(format!("no member matches {query}")),
        count => Err(format!(
            "{query} matches {count} members; give more of the author id"
        )),
    }
}

fn check_about(about: &str) -> std::result::Result<(), String> {
    if about.len() > MAX_PROFILE_ABOUT_BYTES {
        return Err(format!(
            "about is {} bytes; the limit is {MAX_PROFILE_ABOUT_BYTES}",
            about.len()
        ));
    }
    Ok(())
}

fn check_pronouns(pronouns: &str) -> std::result::Result<(), String> {
    if pronouns.len() > MAX_PROFILE_PRONOUNS_BYTES {
        return Err(format!(
            "pronouns are {} bytes; the limit is {MAX_PROFILE_PRONOUNS_BYTES}",
            pronouns.len()
        ));
    }
    Ok(())
}

fn check_links(links: &[String]) -> std::result::Result<(), String> {
    if links.len() > MAX_PROFILE_LINKS {
        return Err(format!(
            "{} links given; a profile carries at most {MAX_PROFILE_LINKS}",
            links.len()
        ));
    }
    for link in links {
        if link.len() > MAX_PROFILE_LINK_BYTES {
            return Err(format!(
                "link is {} bytes; the limit is {MAX_PROFILE_LINK_BYTES}",
                link.len()
            ));
        }
        if !(link.starts_with("http://") || link.starts_with("https://")) {
            return Err(format!("{link} is not an http(s) link"));
        }
    }
    Ok(())
}

/// Split `/links` arguments on whitespace, validating the whole set before
/// any of it is sent.
fn parse_profile_links(rest: &str) -> std::result::Result<Vec<String>, String> {
    let links: Vec<String> = rest.split_whitespace().map(str::to_string).collect();
    check_links(&links)?;
    Ok(links)
}

fn dash_if_empty(text: &str) -> &str {
    if text.trim().is_empty() { "-" } else { text }
}

/// The profile overlay body. Field labels stay lowercase to match the rest of
/// the terminal frontend, and the self marker is explicit so an empty display
/// name cannot make your own page look like someone else's.
fn profile_overlay(profile: &MemberProfile, is_self: bool) -> Overlay {
    let mut body = vec![
        format!("name      {}", profile.display_name),
        format!("author    {}", profile.author.short()),
        format!("joined    {}", format_day(profile.joined_at)),
        format!("messages  {}", profile.message_count),
        format!("active    {}", format_day(profile.last_activity)),
        format!("pronouns  {}", dash_if_empty(&profile.pronouns)),
        format!("you       {}", if is_self { "yes" } else { "no" }),
        String::new(),
        "about".to_string(),
    ];
    if profile.about.trim().is_empty() {
        body.push("  (not set)".to_string());
    } else {
        body.extend(profile.about.lines().map(|line| format!("  {line}")));
    }
    body.push(String::new());
    body.push("links".to_string());
    if profile.links.is_empty() {
        body.push("  (none)".to_string());
    } else {
        body.extend(profile.links.iter().map(|link| format!("  {link}")));
    }
    Overlay {
        title: format!("profile: {}", profile.display_name),
        body,
    }
}

fn format_day(timestamp: u64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(timestamp as i64) {
        Ok(time) => format!(
            "{:04}-{:02}-{:02}",
            time.year(),
            u8::from(time.month()),
            time.day()
        ),
        Err(_) => "---- -- --".to_string(),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

fn short_peer(peer: PeerId) -> String {
    let text = peer.to_string();
    text.chars().take(8).collect()
}

fn channel_spec(rest: &str) -> (ChannelKind, &str) {
    let rest = rest.trim();
    if let Some((first, tail)) = rest.split_once(char::is_whitespace) {
        let kind = match first {
            "#" => Some(ChannelKind::Text),
            "!" => Some(ChannelKind::Announcement),
            "~" => Some(ChannelKind::Voice),
            "?" => Some(ChannelKind::Forum),
            other => ChannelKind::from_name(other),
        };
        let tail = tail.trim();
        if let Some(kind) = kind
            && !tail.is_empty()
        {
            return (kind, tail);
        }
    }
    (ChannelKind::Text, rest)
}

/// Split a trailing `in <category>` from a channel name.
///
/// The split is taken from the right, so a channel called `stand in` keeps its
/// full name unless a real category follows it, and the category is matched by
/// name later because the command line has no ids to offer.
fn split_in_category(spec: &str) -> (&str, Option<&str>) {
    let spec = spec.trim();
    // The category lookup ignores case, so the separator does too. Lower
    // casing ASCII keeps the byte offsets, which lets the split use the
    // original text and keep the caller's capitals in the names.
    let lowered = spec.to_ascii_lowercase();
    match lowered.rfind(" in ") {
        Some(index) => {
            let name = &spec[..index];
            let category = &spec[index + " in ".len()..];
            if name.trim().is_empty() || category.trim().is_empty() {
                (spec, None)
            } else {
                (name.trim(), Some(category.trim()))
            }
        }
        None => (spec, None),
    }
}

fn channel_kind_of(channels: &[ChannelView], channel_id: ChannelId) -> ChannelKind {
    channels
        .iter()
        .find(|channel| channel.id == channel_id)
        .map(|channel| channel.kind)
        .unwrap_or_default()
}

fn presence_from_name(name: &str) -> Option<PresenceStatus> {
    match name.trim().to_lowercase().as_str() {
        "online" => Some(PresenceStatus::Online),
        "away" => Some(PresenceStatus::Away),
        "dnd" | "busy" | "do-not-disturb" => Some(PresenceStatus::DoNotDisturb),
        "offline" | "invisible" => Some(PresenceStatus::Offline),
        _ => None,
    }
}

/// The policy overlay body: one row per axis with the raised marker.
fn policy_lines(view: &PolicyView) -> Vec<String> {
    let guild = &view.guild;
    let floor = &view.client_floor;
    let effective = &view.effective;
    let mut lines = vec![
        format!(
            "commune policy (read-only): {} ({})",
            guild.profile.name(),
            guild.topology.name()
        ),
        String::new(),
        format!(
            "{:<16} {:<21} {:<21} {:<21} {}",
            "axis", "guild", "client floor", "effective", "raised"
        ),
    ];
    let rows = [
        (
            "discovery",
            format!("{:?}", guild.discovery),
            floor
                .discovery
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.discovery),
        ),
        (
            "transport",
            format!("{:?}", guild.transport),
            floor
                .transport
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.transport),
        ),
        (
            "padding",
            format!("{:?}", guild.padding),
            floor
                .padding
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.padding),
        ),
        (
            "author_identity",
            format!("{:?}", guild.author_identity),
            floor
                .author_identity
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.author_identity),
        ),
        (
            "timestamps",
            format!("{:?}", guild.timestamps),
            floor
                .timestamps
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.timestamps),
        ),
        (
            "hardware_keys",
            format!("{:?}", guild.hardware_keys),
            floor
                .hardware_keys
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.hardware_keys),
        ),
        (
            "plugins",
            format!("{:?}", guild.plugins),
            floor
                .plugins
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.plugins),
        ),
    ];
    for (axis, published, client, effective) in rows {
        let raised = if view.raised_by_client.iter().any(|name| name == axis) {
            "*"
        } else {
            ""
        };
        lines.push(format!(
            "{:<16} {:<21} {:<21} {:<21} {}",
            axis, published, client, effective, raised
        ));
    }
    lines.push(String::new());
    let raised = if view.raised_by_client.is_empty() {
        "none".to_string()
    } else {
        view.raised_by_client.join(", ")
    };
    lines.push(format!("raised by client: {raised}"));
    lines
}

fn toggle_id<T: PartialEq + Copy>(list: &mut Vec<T>, id: T, mute: bool) {
    if mute {
        if !list.contains(&id) {
            list.push(id);
        }
    } else {
        list.retain(|item| *item != id);
    }
}

fn parse_minute(text: &str) -> Option<u16> {
    let (hours, minutes) = text.trim().split_once(':')?;
    let hours: u16 = hours.trim().parse().ok()?;
    let minutes: u16 = minutes.trim().parse().ok()?;
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

fn is_dialable(address: &str) -> bool {
    !address.starts_with("/ip4/0.0.0.0/") && !address.starts_with("/ip6/::/")
}

pub fn parse_invite(code: &str) -> Result<Invite> {
    let invite = Invite::decode(code).map_err(|error| anyhow::anyhow!("{error}"))?;
    invite
        .verify()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(invite)
}

#[cfg(test)]
mod tests {
    use ovvc_client::{Client, TimelineEntry, direct_guild_id, timeline};
    use ovvc_core::Identity;

    use super::*;

    fn app_with_message() -> (tempfile::TempDir, Arc<Client>, App, EventId) {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(Client::open_local(dir.path()).expect("client"));
        let guild = client.create_guild("terminal").expect("guild");
        let channel = client.default_channel(guild.id).expect("channel");
        let message = client
            .post_message(guild.id, channel.id, "hello")
            .expect("message");
        // An existing profile takes the app straight to the chat screen.
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");
        let mut app = App::new(
            client.clone(),
            dir.path().to_path_buf(),
            AppOptions::default(),
        )
        .expect("app");
        app.reload_messages();
        (dir, client, app, message.id().expect("id"))
    }

    #[test]
    fn edit_delete_and_reactions_reach_the_folded_timeline() {
        let (_dir, client, mut app, id) = app_with_message();
        let full = id.to_hex();
        let needle = &full[..8];

        app.command(&format!("edit {needle} goodbye"));
        let folded = timeline(&app.messages).expect("timeline");
        let (body, edited) = folded
            .iter()
            .find_map(|entry| match entry {
                TimelineEntry::Message {
                    event,
                    body,
                    edited,
                    ..
                } if event.id().ok() == Some(id) => Some((body.clone(), *edited)),
                _ => None,
            })
            .expect("the edited message is still in the window");
        assert_eq!(body, "goodbye");
        assert!(edited, "the edit marker was not folded in");

        app.command(&format!("react {needle} 👍"));
        let reacted =
            timeline(&app.messages)
                .expect("timeline")
                .into_iter()
                .any(|entry| match entry {
                    TimelineEntry::Message {
                        event, reactions, ..
                    } if event.id().ok() == Some(id) => reactions.iter().any(|reaction| {
                        reaction.emoji == "👍" && reaction.authors.contains(&client.author_id())
                    }),
                    _ => false,
                });
        assert!(reacted, "the reaction was not folded into the message");

        app.command(&format!("delete {needle}"));
        let deleted = timeline(&app.messages)
            .expect("timeline")
            .iter()
            .any(|entry| match entry {
                TimelineEntry::Deleted { event } => event.id().ok() == Some(id),
                _ => false,
            });
        assert!(deleted, "the deleted message still renders as content");
    }

    #[test]
    fn channels_get_kinds_categories_and_renames() {
        let (_dir, _client, mut app, _id) = app_with_message();
        app.command("newcategory projects");
        app.command("newchannel voice standup");
        let standup = app
            .channels
            .iter()
            .find(|channel| channel.name == "standup")
            .cloned()
            .expect("the voice channel exists");
        assert_eq!(standup.kind, ChannelKind::Voice);
        assert_eq!(
            app.current_channel().map(|channel| channel.kind),
            Some(ChannelKind::Voice)
        );

        app.command("movechannel standup projects");
        let moved = app
            .channels
            .iter()
            .find(|channel| channel.id == standup.id)
            .cloned()
            .expect("still there");
        let category = app.categories.first().expect("category");
        assert_eq!(moved.parent, Some(category.id));

        app.command("renamechannel standup daily");
        assert!(
            app.channels
                .iter()
                .any(|channel| channel.name == "daily" && channel.kind == ChannelKind::Voice)
        );
    }

    #[test]
    fn search_opens_a_hit_and_notification_commands_are_saved() {
        let (_dir, _client, mut app, id) = app_with_message();
        app.command("search hello");
        assert_eq!(app.search_hits.len(), 1);
        app.command("open 1");
        assert_eq!(app.jump_to, Some(id));

        app.command("dnd on");
        assert!(app.notifications.dnd);
        app.command("quiet 22:00 07:00");
        assert_eq!(
            app.notifications.quiet_hours,
            Some(QuietHours {
                start_minute: 22 * 60,
                end_minute: 7 * 60,
            })
        );
        let channel = app
            .current_channel()
            .map(|channel| channel.id)
            .expect("channel");
        app.command("mute");
        assert!(app.notifications.muted_channels.contains(&channel));
        app.command("unmute");
        assert!(!app.notifications.muted_channels.contains(&channel));
        app.command("quiet off");
        assert!(app.notifications.quiet_hours.is_none());
    }

    #[test]
    fn newchannel_creates_and_selects_the_channel() {
        let (_dir, _client, mut app, _id) = app_with_message();
        app.command("newchannel Random");
        assert!(app.channels.iter().any(|channel| channel.name == "Random"));
        assert_eq!(
            app.current_channel().map(|channel| channel.name.as_str()),
            Some("Random")
        );
    }

    #[test]
    fn newchannel_can_place_the_channel_in_a_category() {
        let (_dir, _client, mut app, _id) = app_with_message();
        app.command("newcategory projects");
        let category = app.categories.first().cloned().expect("category");
        app.command("newchannel voice standup in projects");
        let channel = app
            .channels
            .iter()
            .find(|channel| channel.name == "standup")
            .cloned()
            .expect("the channel exists");
        assert_eq!(channel.kind, ChannelKind::Voice);
        assert_eq!(channel.parent, Some(category.id));
        assert!(app.status.contains("in projects"), "{}", app.status);

        let before = app.channels.len();
        app.command("newchannel lounge in missing");
        assert_eq!(app.channels.len(), before);
        assert!(app.status.contains("no category named missing"));
    }

    #[test]
    fn a_trailing_in_category_is_split_from_the_right() {
        assert_eq!(
            split_in_category("standup in projects"),
            ("standup", Some("projects"))
        );
        assert_eq!(split_in_category("stand in"), ("stand in", None));
        assert_eq!(split_in_category("  general  "), ("general", None));
        assert_eq!(split_in_category("a in b in c"), ("a in b", Some("c")));
        // The category lookup ignores case, so the separator has to as well,
        // and the names keep the capitals the reader typed.
        assert_eq!(
            split_in_category("Standup IN Projects"),
            ("Standup", Some("Projects"))
        );
        assert_eq!(split_in_category("in projects"), ("in projects", None));
        assert_eq!(split_in_category("x in "), ("x in", None));
        assert_eq!(split_in_category(""), ("", None));
    }

    #[test]
    fn a_classified_diagnostic_keeps_its_class_and_log_path() {
        let (_dir, _client, mut app, _id) = app_with_message();
        app.on_client_event(ClientEvent::Diagnostic(
            Diagnostic::bug("an impossible state").with_log_path("/tmp/ovvc/daemon.log"),
        ));
        assert_eq!(app.error_class, Some(ErrorClass::Bug));
        assert_eq!(app.error_log.as_deref(), Some("/tmp/ovvc/daemon.log"));
        assert_eq!(app.error.as_deref(), Some("an impossible state"));
        assert!(!app.connecting);

        // A peer going offline is an operational condition, not a bug. It
        // still reaches the strip, with the class that decides the warning
        // label.
        app.on_client_event(ClientEvent::Diagnostic(Diagnostic::operational(
            "peer 3f2a went offline",
        )));
        assert_eq!(app.error_class, Some(ErrorClass::Operational));
        assert_eq!(app.error_log, None);
        assert_eq!(app.error.as_deref(), Some("peer 3f2a went offline"));
    }

    #[test]
    fn a_daemon_error_gets_its_own_line_and_esc_dismisses_it() {
        let (_dir, _client, mut app, _id) = app_with_message();
        app.status = "listening on /ip4/127.0.0.1/tcp/40000".to_string();
        app.on_client_event(ClientEvent::Error {
            guild_id: app.current_guild().map(|guild| guild.id),
            message: "publishing peer announcement failed: the quorum failed".to_string(),
        });
        assert_eq!(
            app.status, "listening on /ip4/127.0.0.1/tcp/40000",
            "a daemon error must not overwrite the network status"
        );
        assert_eq!(
            app.error.as_deref(),
            Some("publishing peer announcement failed: the quorum failed")
        );
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.error.is_none(), "esc should dismiss the error strip");
    }

    #[test]
    fn presence_and_typing_events_reach_the_panels() {
        let (_dir, _client, mut app, _id) = app_with_message();
        let guild = app.current_guild().expect("guild").id;
        let channel = app.current_channel().expect("channel").id;
        let other = ovvc_core::Identity::from_seed([7u8; 32]).author_id();

        app.on_client_event(ClientEvent::Presence {
            guild_id: guild,
            author: other,
            status: PresenceStatus::Away,
        });
        assert_eq!(
            app.presence_for(other),
            PresenceStatus::Away,
            "a presence event must paint the member panel"
        );

        app.on_client_event(ClientEvent::Typing {
            guild_id: guild,
            channel_id: channel,
            author: other,
            until_ms: now_ms() + 10_000,
        });
        assert!(
            app.typing_in(channel).contains(&other),
            "a typing event must paint the typing line"
        );

        app.on_client_event(ClientEvent::TypingEnded {
            guild_id: guild,
            channel_id: channel,
            author: other,
        });
        assert!(!app.typing_in(channel).contains(&other));
    }

    /// A mentioning message signed by a second author. It goes through the same
    /// `on_client_event` path as one that arrived over the network, without
    /// needing a second daemon.
    fn mention_event(app: &App, lamport: u64, channel: Option<ChannelId>) -> ClientEvent {
        let guild_id = app.current_guild().expect("guild").id;
        let other = ovvc_core::Identity::from_seed([9u8; 32]);
        let event = other
            .sign_body(ovvc_core::model::EventBody {
                version: ovvc_core::model::EVENT_VERSION,
                guild_id,
                channel_id: channel,
                author: other.author_id(),
                seq: lamport,
                lamport,
                timestamp: 0,
                prev: None,
                kind: EventKind::MessageV2 {
                    body: "hey".to_string(),
                    reply_to: None,
                    mentions: vec![app.client.author_id()],
                },
            })
            .expect("sign");
        ClientEvent::Event { guild_id, event }
    }

    #[test]
    fn a_mute_or_quiet_hours_keeps_the_tui_bell_quiet() {
        let (_dir, client, mut app, _id) = app_with_message();
        let guild = app.current_guild().expect("guild").id;
        let channel = app.current_channel().expect("channel").id;

        let ring = |app: &mut App, lamport: u64| {
            app.bell = false;
            app.status.clear();
            app.on_client_event(mention_event(app, lamport, Some(channel)));
            (app.bell, app.status.clone())
        };

        let (bell, status) = ring(&mut app, 10);
        assert!(bell, "an unmuted mention should ring the bell");
        assert!(status.contains("mentioned you"), "{status}");

        client.set_do_not_disturb(true);
        app.refresh_notifications();
        let (bell, status) = ring(&mut app, 11);
        assert!(!bell, "dnd kept the bell quiet but it still rang");
        assert!(!status.contains("mentioned"), "{status}");

        client.set_do_not_disturb(false);
        client.set_guild_muted(guild, true);
        app.refresh_notifications();
        assert!(!ring(&mut app, 12).0, "a muted guild still rang the bell");

        client.set_guild_muted(guild, false);
        client.set_channel_muted(channel, true);
        app.refresh_notifications();
        assert!(!ring(&mut app, 13).0, "a muted channel still rang the bell");

        // Quiet hours are evaluated in UTC; put the window around the clock.
        client.set_channel_muted(channel, false);
        client.set_quiet_hours(Some(QuietHours {
            start_minute: 0,
            end_minute: 1440,
        }));
        app.refresh_notifications();
        assert!(!ring(&mut app, 14).0, "quiet hours still rang the bell");

        // An older wire event carries no channel: the guild-wide reasons still
        // have to apply even though no channel mute can match.
        client.set_quiet_hours(None);
        client.set_do_not_disturb(true);
        app.refresh_notifications();
        app.bell = false;
        app.on_client_event(mention_event(&app, 15, None));
        assert!(!app.bell, "dnd did not silence a channel-less mention");
    }

    #[test]
    fn presence_is_re_announced_while_online_and_stops_when_offline() {
        let (_dir, _client, mut app, _id) = app_with_message();
        let guild = app.current_guild().expect("guild").id;

        assert_eq!(
            app.presence_due(1_000),
            None,
            "a guild that is not networked must not announce presence"
        );

        app.online_guild = Some(guild);
        assert_eq!(
            app.presence_due(10_000),
            Some(guild),
            "the first frame after coming online was not due"
        );
        assert_eq!(
            app.presence_due(11_999),
            None,
            "a second frame inside the window would flood the daemon"
        );
        assert_eq!(
            app.presence_due(12_000),
            Some(guild),
            "the lease was not refreshed after the window elapsed"
        );

        app.disconnect();
        assert_eq!(
            app.presence_due(30_000),
            None,
            "presence kept announcing after the guild went offline"
        );
    }

    #[test]
    fn a_mute_set_in_another_window_silences_this_bell() {
        let (_dir, client, mut app, _id) = app_with_message();
        let channel = app.current_channel().expect("channel").id;

        assert!(
            !app.notifications.muted_channels.contains(&channel),
            "the app did not start from the daemon's defaults"
        );

        // A GUI window (or the CLI) writes the mute through the daemon while
        // this TUI is already running, so the TUI's cached copy is stale.
        client.set_channel_muted(channel, true);

        app.bell = false;
        app.on_client_event(mention_event(&app, 20, Some(channel)));
        assert!(
            !app.bell,
            "a channel muted in another window still rang this bell"
        );
    }

    /// Put a second person on the guild roster without networking.
    fn add_member(client: &Client, guild: GuildId, name: &str, seed: u8) -> Identity {
        let identity = Identity::from_seed([seed; 32]);
        client
            .storage()
            .lock()
            .expect("storage")
            .append_member_joined(guild, name, &identity)
            .expect("roster row")
            .expect("a new member");
        identity
    }

    /// Create a two-member DM guild the way the daemon would, so the listing
    /// and selection paths can be tested without a network or a runtime.
    fn add_direct(client: &Client, guild: GuildId, peer_name: &str, seed: u8) -> GuildId {
        let identity = add_member(client, guild, peer_name, seed);
        let dm = direct_guild_id(&client.author_id(), &identity.author_id());
        {
            let storage = client.storage();
            let mut storage = storage.lock().expect("storage");
            storage
                .create_guild_with_id(dm, peer_name, "ada", client.identity())
                .expect("dm guild");
            storage
                .append_member_joined(dm, peer_name, &identity)
                .expect("peer joined")
                .expect("a new member");
        }
        dm
    }

    fn overlay_text(app: &App) -> String {
        app.overlay
            .as_ref()
            .expect("an overlay is open")
            .body
            .join("\n")
    }

    #[test]
    fn profile_overlay_shows_self_and_another_member() {
        let (_dir, client, mut app, _id) = app_with_message();
        let guild = app.current_guild().expect("guild").id;
        client
            .set_profile(
                guild,
                "about me",
                "they/them",
                &["https://example.com".into()],
            )
            .expect("own profile");
        let bob = add_member(&client, guild, "bob", 7).author_id();
        app.reload_members();

        app.command("profile");
        let body = overlay_text(&app);
        assert!(body.contains("you       yes"), "{body}");
        assert!(body.contains("about me"), "{body}");
        assert!(body.contains("they/them"), "{body}");
        assert!(body.contains("https://example.com"), "{body}");
        assert!(body.contains("messages"), "{body}");

        // A name picks the other member, and the id-prefix path resolves the
        // same person.
        app.command("profile bob");
        let body = overlay_text(&app);
        assert!(body.contains("bob"), "{body}");
        assert!(body.contains("you       no"), "{body}");
        assert!(!body.contains("about me"), "{body}");

        let prefix = &bob.short()[..4];
        app.command(&format!("profile {prefix}"));
        let body = overlay_text(&app);
        assert!(body.contains("you       no"), "{body}");
        assert!(
            body.contains(&format!("author    {}", bob.short())),
            "{body}"
        );

        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.overlay.is_none(), "esc did not dismiss the profile");
        assert!(
            app.profile_author.is_none(),
            "a dismissed profile was still armed for event refreshes"
        );
    }

    #[test]
    fn profile_events_refresh_an_open_overlay() {
        let (_dir, client, mut app, _id) = app_with_message();
        let guild = app.current_guild().expect("guild").id;
        app.command("profile");
        assert!(overlay_text(&app).contains("(not set)"));

        let event = client
            .set_profile(guild, "written elsewhere", "", &[])
            .expect("profile");
        app.on_client_event(ClientEvent::Event {
            guild_id: guild,
            event,
        });
        let body = overlay_text(&app);
        assert!(
            body.contains("written elsewhere"),
            "the open profile did not refetch on its event: {body}"
        );
    }

    #[test]
    fn profile_edit_commands_validate_before_sending() {
        let (_dir, client, mut app, _id) = app_with_message();
        let guild = app.current_guild().expect("guild").id;
        let me = client.author_id();

        app.command("about hello there");
        assert_eq!(
            client.member_profile(guild, me).expect("profile").about,
            "hello there"
        );
        app.command("pronouns she/her");
        assert_eq!(
            client.member_profile(guild, me).expect("profile").pronouns,
            "she/her"
        );
        app.command("links https://a.example https://b.example");
        assert_eq!(
            client.member_profile(guild, me).expect("profile").links,
            vec!["https://a.example", "https://b.example"]
        );
        assert!(app.error.is_none(), "unexpected error: {:?}", app.error);

        // Clearing one field leaves the others alone.
        app.command("about");
        let profile = client.member_profile(guild, me).expect("profile");
        assert!(profile.about.is_empty());
        assert_eq!(profile.pronouns, "she/her");
        assert_eq!(profile.links.len(), 2);

        // An over-long about line and too many links are rejected before any
        // event is written, so the stored profile is neither half-updated nor
        // truncated.
        let long = "x".repeat(MAX_PROFILE_ABOUT_BYTES + 1);
        app.command(&format!("about {long}"));
        assert!(
            app.error
                .as_deref()
                .is_some_and(|error| error.contains("limit"))
        );
        let profile = client.member_profile(guild, me).expect("profile");
        assert!(profile.about.is_empty(), "a rejected about was sent");

        app.command("links https://1 https://2 https://3 https://4 https://5 https://6");
        assert!(
            app.error
                .as_deref()
                .is_some_and(|error| error.contains("at most"))
        );
        let profile = client.member_profile(guild, me).expect("profile");
        assert_eq!(profile.links.len(), 2, "a rejected link list was sent");

        app.command("links ftp://example.com");
        assert!(
            app.error
                .as_deref()
                .is_some_and(|error| error.contains("http"))
        );
        assert_eq!(
            client
                .member_profile(guild, me)
                .expect("profile")
                .links
                .len(),
            2
        );

        // A later success clears the error strip.
        app.command("about fine");
        assert!(app.error.is_none());
        assert_eq!(app.status, "about updated");
    }

    #[test]
    fn direct_messages_are_a_separate_selectable_group() {
        let (_dir, client, mut app, _id) = app_with_message();
        let commune = app.current_guild().expect("guild").id;
        let dm = add_direct(&client, commune, "bob", 9);
        app.reload_guilds(Some(commune));
        app.reload_members();

        assert_eq!(app.direct_messages.len(), 1);
        assert_eq!(
            app.commune_indices(),
            vec![
                app.guilds
                    .iter()
                    .position(|guild| guild.id == commune)
                    .expect("commune")
            ],
            "the DM must not appear in the commune tabs"
        );
        assert!(app.is_direct(dm));

        app.command("dm");
        let body = overlay_text(&app);
        assert!(body.starts_with("[1] dm:bob "), "{body}");
        assert!(body.contains(&format!("<{}>", app.direct_messages[0].peer.author.short())));

        // Reusing a known conversation selects it without needing the async
        // daemon open path.
        app.command("dm bob");
        assert_eq!(app.current_guild().map(|guild| guild.id), Some(dm));
        assert_eq!(app.current_direct().as_deref(), Some("bob"));
        assert_eq!(app.status, "dm:bob");

        // `/commune` is a visible alias for `/guild`, and both count only
        // communes.
        app.command("commune 1");
        assert_eq!(app.current_guild().map(|guild| guild.id), Some(commune));
        app.command("guild 1");
        assert_eq!(app.current_guild().map(|guild| guild.id), Some(commune));

        // Cycling communes from a DM goes to the first commune, never to the
        // direct conversation.
        app.select_guild(
            app.guilds
                .iter()
                .position(|guild| guild.id == dm)
                .expect("dm"),
        );
        app.cycle_guild(1);
        assert_eq!(app.current_guild().map(|guild| guild.id), Some(commune));
    }

    /// The TUI is synchronous, but opening a new direct conversation needs the
    /// async client to start a node. The frontend bridge must drive that on the
    /// multi-thread runtime the real binary already runs under. Under a plain
    /// `#[test]` there is no runtime and the bridge correctly returns an error,
    /// so this test lives inside one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn opening_a_new_direct_message_drives_the_tuis_runtime() {
        let (_dir, client, mut app, _id) = app_with_message();
        let commune = app.current_guild().expect("guild").id;
        let bob = add_member(&client, commune, "bob", 61);
        app.reload_members();

        app.command("dm bob");

        let dm = direct_guild_id(&client.author_id(), &bob.author_id());
        assert!(
            client.get_guild(dm).expect("lookup").is_some(),
            "the DM guild was not created: {}",
            app.status
        );
        assert_eq!(app.status, "dm:bob");
        assert_eq!(app.current_guild().map(|guild| guild.id), Some(dm));
        client.shutdown();
    }

    #[test]
    fn direct_message_waits_for_an_unsynced_conversation() {
        let (_dir, client, mut app, _id) = app_with_message();
        let commune = app.current_guild().expect("guild").id;
        let bob = add_member(&client, commune, "bob", 11);
        app.reload_members();
        let dm = direct_guild_id(&client.author_id(), &bob.author_id());

        // The announcement has reached the sidebar, but the opener's guild
        // log has not synced: there is a conversation and no local guild yet.
        let peer = app
            .members
            .iter()
            .find(|member| member.author == bob.author_id())
            .expect("bob on the roster")
            .clone();
        app.direct_messages.push(DirectConversation {
            guild: GuildMeta {
                id: dm,
                name: "bob".to_string(),
                founder: bob.author_id(),
                created_at: 1,
            },
            peer,
        });

        app.command("dm bob");
        assert!(
            app.status.contains("still syncing"),
            "an unsynced conversation should wait, not open: {}",
            app.status
        );
        assert_eq!(
            app.current_guild().map(|guild| guild.id),
            Some(commune),
            "an unsynced DM must not become the selected guild"
        );
        assert!(
            client
                .list_guilds()
                .expect("guilds")
                .iter()
                .all(|guild| guild.id != dm),
            "the recipient created a second founder for the DM id"
        );

        // The public channel is still selected while the DM syncs. A message
        // typed now must be held back rather than posted there.
        let public = app.current_channel().expect("public channel").id;
        app.input = Input::with_text("dm from a tui");
        app.submit();
        assert!(
            app.status.contains("still syncing"),
            "an unsynced DM must block the composer: {}",
            app.status
        );
        assert_eq!(
            app.input.text(),
            "dm from a tui",
            "the blocked draft must stay in the composer"
        );
        let posted = client.history(commune, public, 20).expect("history");
        assert!(
            !posted.iter().any(|event| matches!(
                &event.body.kind,
                EventKind::Message { body, .. } if body == "dm from a tui"
            )),
            "a message meant for the DM leaked into the public channel"
        );
    }

    /// Once the DM guild syncs, the paused `/dm` must become the selected
    /// conversation, and the next message must go there rather than to the
    /// public channel that was open when `/dm` ran.
    #[test]
    fn a_synced_dm_becomes_the_next_send_target() {
        let (_dir, client, mut app, _id) = app_with_message();
        let commune = app.current_guild().expect("guild").id;
        let public = app.current_channel().expect("channel").id;
        let dm = add_direct(&client, commune, "bob", 13);
        app.reload_members();

        // The sidebar knows the conversation, but the guild has not been
        // reloaded into the app yet, so `/dm` takes the syncing path.
        let peer = app
            .members
            .iter()
            .find(|member| member.display_name == "bob")
            .expect("bob on the roster")
            .clone();
        app.direct_messages.push(DirectConversation {
            guild: GuildMeta {
                id: dm,
                name: "bob".to_string(),
                founder: peer.author,
                created_at: 1,
            },
            peer,
        });

        app.command("dm bob");
        assert!(app.pending_dm.is_some(), "the DM guild is not loaded yet");

        // The guild-sync event arrives.
        app.guilds_dirty = true;
        app.flush_dirty();
        assert_eq!(
            app.current_guild().map(|guild| guild.id),
            Some(dm),
            "the synced DM must become the selected guild"
        );
        assert!(app.pending_dm.is_none(), "the pause must be released");
        let dm_channel = app.current_channel().expect("dm channel").id;

        app.input = Input::with_text("dm from a tui");
        app.submit();

        let dm_events = client.history(dm, dm_channel, 20).expect("dm history");
        assert!(
            dm_events.iter().any(|event| matches!(
                &event.body.kind,
                EventKind::Message { body, .. } if body == "dm from a tui"
            )),
            "the message did not reach the DM channel"
        );
        let public_events = client.history(commune, public, 20).expect("public history");
        assert!(
            !public_events.iter().any(|event| matches!(
                &event.body.kind,
                EventKind::Message { body, .. } if body == "dm from a tui"
            )),
            "the message leaked into the public channel"
        );
    }

    #[test]
    fn help_lists_the_profile_dm_and_commune_commands() {
        let (_dir, _client, mut app, _id) = app_with_message();
        app.command("help");
        let body = overlay_text(&app);
        for needle in [
            "/profile",
            "/about",
            "/pronouns",
            "/links",
            "/dm",
            "/guild|/commune",
        ] {
            assert!(body.contains(needle), "help does not mention {needle}");
        }
    }

    #[test]
    fn local_overlays_identity_and_navigation_commands_work() {
        let (_dir, client, mut app, _id) = app_with_message();

        app.command("help");
        assert_eq!(app.overlay.as_ref().unwrap().title, "commands");

        app.command("settings");
        assert_eq!(app.screen, Screen::Settings);
        assert_eq!(app.settings_tab, SettingsTab::User);
        app.command("settings preferences");
        assert_eq!(app.settings_tab, SettingsTab::Preferences);
        app.screen = Screen::Chat;

        app.command("config");
        let body = overlay_text(&app);
        assert_eq!(app.overlay.as_ref().unwrap().title, "config");
        assert!(body.contains(&client.author_id().to_string()));
        assert!(body.contains("theme:"));

        app.command("whoami");
        let body = overlay_text(&app);
        assert_eq!(app.overlay.as_ref().unwrap().title, "identity");
        assert!(body.contains("peer id:  offline"));

        app.command("status");
        let body = overlay_text(&app);
        assert_eq!(app.overlay.as_ref().unwrap().title, "status");
        assert!(body.contains("network: offline"));
        assert!(body.contains("#general"));

        let initial_members = app.show_members;
        app.command("members");
        assert_eq!(app.show_members, !initial_members);
        app.command("members");
        assert_eq!(app.show_members, initial_members);

        app.input = crate::input::Input::with_text("draft");
        app.status = "notice".to_string();
        app.command("cancel");
        assert!(app.input.is_empty());
        assert!(app.status.is_empty());

        app.command("commune");
        assert!(app.status.contains("usage"));
        app.command("channel");
        assert!(app.status.contains("usage") || app.status.contains("no commune"));

        app.command("name");
        assert!(app.status.contains("usage: /name"));

        app.command("mystery");
        assert!(app.status.contains("unknown command"));
    }

    #[test]
    fn attach_unreact_and_argument_errors_are_reported() {
        let (dir, client, mut app, id) = app_with_message();
        let full = id.to_hex();
        let needle = &full[..8];

        app.command(&format!("react {needle} 👍"));
        app.command(&format!("unreact {needle} 👍"));
        let still_reacted = timeline(&app.messages)
            .expect("timeline")
            .into_iter()
            .any(|entry| match entry {
                TimelineEntry::Message {
                    event, reactions, ..
                } if event.id().ok() == Some(id) => reactions
                    .iter()
                    .any(|reaction| reaction.authors.contains(&client.author_id())),
                _ => false,
            });
        assert!(!still_reacted, "the reaction was not removed");

        let file = dir.path().join("note.txt");
        std::fs::write(&file, b"attached from the terminal").unwrap();
        app.command(&format!("attach {} a note", file.display()));
        let attached = app
            .messages
            .iter()
            .any(|event| !event.body.kind.attachments().is_empty());
        assert!(attached, "the attachment event is missing: {}", app.status);

        app.command("attach /nonexistent/ovvc-note");
        assert!(app.status.contains("could not read"), "{}", app.status);

        app.command("jump not-an-id");
        assert!(!app.status.is_empty());

        app.command("newcategory");
        assert!(app.status.contains("usage: /newcategory"));

        app.command("quiet 99:99 00:00");
        assert!(app.status.contains("usage") || app.status.contains("UTC"));

        app.command("dnd maybe");
        assert!(app.status.contains("usage"));

        app.command("presence sideways");
        assert!(app.status.contains("usage"));

        app.command("open 99");
        assert!(app.status.contains("no search result") || app.status.contains("usage"));
    }

    #[test]
    fn the_policy_table_keeps_its_columns_aligned_for_long_names() {
        use ovvc_core::{ClientPolicyFloor, GuildPolicy, IdentityPolicy};
        let guild = GuildPolicy {
            author_identity: IdentityPolicy::AnonymousCredentials,
            ..GuildPolicy::standard()
        };
        let view = PolicyView {
            guild: guild.clone(),
            client_floor: ClientPolicyFloor::default(),
            effective: guild,
            raised_by_client: vec!["author_identity".to_string()],
        };

        let lines = policy_lines(&view);
        let header_raised = lines[2].find("raised").expect("header");
        let row = lines
            .iter()
            .find(|line| line.starts_with("author_identity"))
            .expect("author identity row");
        let marker = row.find('*').expect("the raised marker");
        assert_eq!(
            marker, header_raised,
            "the raised marker must sit under the header for a 20 character value"
        );
    }

    #[test]
    fn the_spinner_only_advances_while_a_load_is_on_screen() {
        let (_dir, _client, mut app, _id) = app_with_message();
        assert!(!app.spinner_active());
        let idle = app.spinner.glyph();
        for _ in 0..20 {
            app.spinner_tick();
        }
        assert_eq!(
            app.spinner.glyph(),
            idle,
            "an idle terminal advanced the spinner"
        );

        app.connecting = true;
        assert!(app.spinner_active());
        app.spinner_tick();
        assert_ne!(app.spinner.glyph(), idle);

        app.connecting = false;
        app.history_loading = true;
        assert!(app.spinner_active(), "a history load must animate");
        app.history_loading = false;

        app.syncing = true;
        app.syncing_since_ms = now_ms();
        assert!(app.spinner_active(), "a live sync must animate");
        app.syncing_since_ms = now_ms().saturating_sub(SYNC_SPINNER_MAX_MS + 1);
        assert!(
            !app.spinner_active(),
            "a sync with no completion kept spinning forever"
        );
    }

    #[test]
    fn a_failed_join_stops_the_onboarding_spinner() {
        let (_dir, _client, mut app, _id) = app_with_message();
        app.screen = Screen::Onboarding;
        app.onboarding.step = Step::Joining;
        assert!(app.spinner_active(), "joining must animate");

        app.on_notice(Notice::Error("join failed: unreachable".to_string()));
        assert_eq!(app.onboarding.step, Step::JoinGuild);
        assert!(!app.spinner_active());
        assert!(app.onboarding.error.is_some());
    }

    #[test]
    fn loading_older_history_is_queued_so_the_spinner_can_paint() {
        let (_dir, _client, mut app, _id) = app_with_message();
        app.history_more = true;

        app.command("history");
        assert!(app.history_loading);
        assert!(app.spinner_active());
        assert!(
            matches!(app.action, Some(Action::LoadHistory { .. })),
            "the fetch was not handed to the event loop"
        );

        // A second request while the first is in flight must not stack.
        app.command("history");
        assert!(app.status.contains("still loading"), "{}", app.status);

        let page = EventPage {
            events: app.messages.clone(),
            more: false,
        };
        let (guild, channel) = app.current_ids().expect("current channel");
        app.on_notice(Notice::HistoryPage {
            guild_id: guild,
            channel_id: channel,
            page: Ok(page),
        });
        assert!(!app.history_loading);
        assert!(!app.spinner_active());
        assert_eq!(app.status, "loaded older messages");
    }

    #[test]
    fn a_history_page_for_another_channel_is_discarded() {
        let (_dir, _client, mut app, _id) = app_with_message();
        app.history_loading = true;
        app.on_notice(Notice::HistoryPage {
            guild_id: GuildId([4; 32]),
            channel_id: ChannelId([9; 16]),
            page: Err("socket closed".to_string()),
        });
        assert!(!app.history_loading);
        assert_ne!(app.status, "could not load older messages: socket closed");
    }
}
