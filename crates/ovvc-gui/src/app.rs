//! The eframe application shell: message pump, request orchestration, and the
//! mapping from UI intents to daemon operations.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ovvc_control::SessionId;
#[cfg(test)]
use ovvc_core::{Attachment, AuthorId};
use ovvc_core::{ChannelId, EventId, GuildId};
use ovvc_ipc::{NetworkOptionsDto, Op};

use crate::bridge::Bridge;
use crate::state::{
    Action, AppState, RequestTag, WizardCommune, WizardStep, address_problem, parse_addresses,
};
#[cfg(test)]
use crate::state::{Message, StagedAttachment};
use crate::theme;
use crate::ui;

/// How often the composer may refresh its typing marker. The control frame
/// carries its own six-second deadline, so two seconds is comfortably inside
/// it without flooding the session.
const TYPING_REFRESH: Duration = Duration::from_secs(2);

/// Messages requested per history page, both for the initial load and for
/// "load older" at the top of the transcript.
const HISTORY_PAGE: usize = 200;

/// How long an informational strip stays on screen. Errors are sticky and are
/// dismissed by hand, notices announce something that already happened.
const NOTICE_LIFETIME: Duration = Duration::from_secs(8);

/// How often the GUI re-asks the daemon for connection state while the two
/// disagree. The daemon answers from memory, so this is cheap.
const CONNECTION_POLL: Duration = Duration::from_millis(500);

/// How long a connect may stay unanswered before the window stops waiting for
/// a listen address. The daemon normally reports one in well under a second.
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);

pub struct App {
    pub state: AppState,
    #[cfg(not(test))]
    bridge: Bridge,
    #[cfg(test)]
    bridge: Box<dyn Backend>,
    requested_identity: bool,
    requested_guilds: bool,
    requested_status: bool,
    loaded_guild: Option<GuildId>,
    requested_history: Option<(GuildId, ChannelId)>,
    requested_control: Option<(GuildId, SessionId)>,
    /// The guild whose policy this connection last asked for.
    requested_policy: Option<GuildId>,
    marked_read: HashMap<ChannelId, EventId>,
    last_typing: Option<Instant>,
    typing_sent: bool,
    was_connected: bool,
    requested_settings: bool,
    /// Whether this connection has asked for the direct conversation list.
    requested_direct_messages: bool,
    /// Direct conversations whose unread counts have been asked for once.
    dm_unread_polled: HashSet<GuildId>,
    /// Last time a `ConnectionState` request was sent while the snapshot and
    /// the daemon disagreed, so the poll cannot run every frame.
    connection_polled_at: Option<Instant>,
    /// When the window first saw a pending connect, so it can give up after
    /// `CONNECT_DEADLINE` rather than suppress every later offline snapshot.
    connect_started_at: Option<Instant>,
    /// The notice currently on screen and when it was first shown, so an
    /// informational strip clears itself instead of hanging around.
    notice_seen: Option<String>,
    notice_since: Option<Instant>,
    /// A display name chosen in the getting-started window before a guild was
    /// selected. It is applied as soon as a guild is available.
    pending_display_name: Option<String>,
    /// Set when setup created the first commune: once it is online, create an
    /// invite and open the invite window, so the person can hand a friend the
    /// one thing the friend needs without hunting for a button.
    first_run_invite: bool,
    first_run_connect_sent: bool,
    first_run_invite_sent: bool,
    /// Whether the local push-to-talk key is held, so a transition can be
    /// sent once instead of every frame.
    ptt_held: bool,
    screenshot: Option<Screenshot>,
}

/// A one-shot request for a real framebuffer capture.
///
/// A compositor does not let an unprivileged process read another client's
/// pixels, so the window asks egui for a screenshot of its own surface. This
/// exercises the same wgpu path as the visible window and works on Wayland.
struct Screenshot {
    path: PathBuf,
    requested: bool,
    started: Instant,
}

/// The subset of [`Bridge`] the frame loop uses.
///
/// This exists only for the in-crate interaction fuzzer, which swaps the real
/// socket for a deterministic fake. Keeping it behind `cfg(test)` means the
/// production build still holds a concrete `Bridge` and no fake can be wired
/// in from another crate.
#[cfg(test)]
pub(crate) trait Backend {
    fn send(&self, op: Op, tag: RequestTag);
    fn stage_attachment(&self, path: PathBuf);
    #[allow(clippy::too_many_arguments)]
    fn send_rich(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: String,
        reply_to: Option<EventId>,
        mentions: Vec<AuthorId>,
        attachments: Vec<StagedAttachment>,
        send_id: u64,
    );
    fn save_attachment(&self, guild_id: GuildId, attachment: Attachment);
    fn poll(&mut self) -> Vec<Message>;
}

#[cfg(test)]
impl Backend for Bridge {
    fn send(&self, op: Op, tag: RequestTag) {
        Bridge::send(self, op, tag);
    }

    fn stage_attachment(&self, path: PathBuf) {
        Bridge::stage_attachment(self, path);
    }

    fn send_rich(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: String,
        reply_to: Option<EventId>,
        mentions: Vec<AuthorId>,
        attachments: Vec<StagedAttachment>,
        send_id: u64,
    ) {
        Bridge::send_rich(
            self,
            guild_id,
            channel_id,
            body,
            reply_to,
            mentions,
            attachments,
            send_id,
        );
    }

    fn save_attachment(&self, guild_id: GuildId, attachment: Attachment) {
        Bridge::save_attachment(self, guild_id, attachment);
    }

    fn poll(&mut self) -> Vec<Message> {
        Bridge::poll(self)
    }
}

/// A backend that never answers. Only reachable through `App::new` when the
/// crate is compiled for tests; the fuzz harness always supplies its own
/// backend through `App::with_backend`.
#[cfg(test)]
struct SilentBackend;

#[cfg(test)]
impl Backend for SilentBackend {
    fn send(&self, _op: Op, _tag: RequestTag) {}
    fn stage_attachment(&self, _path: PathBuf) {}
    fn send_rich(
        &self,
        _guild_id: GuildId,
        _channel_id: ChannelId,
        _body: String,
        _reply_to: Option<EventId>,
        _mentions: Vec<AuthorId>,
        _attachments: Vec<StagedAttachment>,
        _send_id: u64,
    ) {
    }
    fn save_attachment(&self, _guild_id: GuildId, _attachment: Attachment) {}
    fn poll(&mut self) -> Vec<Message> {
        Vec::new()
    }
}

impl App {
    pub fn new(
        context: &eframe::CreationContext<'_>,
        socket: PathBuf,
        client_config: Option<ovvc_config::ClientConfig>,
    ) -> Self {
        // The palette is shared with the terminal frontend. The layered
        // `client.theme_file` and `client.theme` win, and otherwise
        // `theme.toml` or `OVVC_THEME` restyles both windows.
        theme::install_with(
            &context.egui_ctx,
            theme::load_configured(client_config.as_ref()),
        );
        let mut state = AppState::new();
        // The level is shared with the terminal frontend, so both start in the
        // same one after a restart.
        state.detail_level = crate::state::DetailLevel::load();
        if !wizard_seen() {
            state.wizard_step = Some(WizardStep::Identity);
        } else {
            state.wizard_done = true;
        }
        #[cfg(not(test))]
        let bridge = Bridge::start(socket, context.egui_ctx.clone());
        #[cfg(test)]
        let bridge = {
            let _ = socket;
            Box::new(SilentBackend) as Box<dyn Backend>
        };
        Self {
            state,
            bridge,
            requested_identity: false,
            requested_guilds: false,
            requested_status: false,
            loaded_guild: None,
            requested_history: None,
            requested_control: None,
            requested_policy: None,
            marked_read: HashMap::new(),
            last_typing: None,
            typing_sent: false,
            was_connected: false,
            requested_settings: false,
            requested_direct_messages: false,
            dm_unread_polled: HashSet::new(),
            connection_polled_at: None,
            connect_started_at: None,
            notice_seen: None,
            notice_since: None,
            pending_display_name: None,
            first_run_invite: false,
            first_run_connect_sent: false,
            first_run_invite_sent: false,
            ptt_held: false,
            screenshot: None,
        }
    }

    /// Assemble an application around a test backend.
    ///
    /// `cfg(test)` only: the fuzzer lives in this crate and needs the same
    /// frame loop as the window without a daemon socket.
    #[cfg(test)]
    pub(crate) fn with_backend(state: AppState, bridge: Box<dyn Backend>) -> Self {
        Self {
            state,
            bridge,
            requested_identity: false,
            requested_guilds: false,
            requested_status: false,
            loaded_guild: None,
            requested_history: None,
            requested_control: None,
            requested_policy: None,
            marked_read: HashMap::new(),
            last_typing: None,
            typing_sent: false,
            was_connected: false,
            requested_settings: false,
            requested_direct_messages: false,
            dm_unread_polled: HashSet::new(),
            connection_polled_at: None,
            connect_started_at: None,
            notice_seen: None,
            notice_since: None,
            pending_display_name: None,
            first_run_invite: false,
            first_run_connect_sent: false,
            first_run_invite_sent: false,
            ptt_held: false,
            screenshot: None,
        }
    }

    /// One frame of the application loop for the in-crate test harness.
    #[cfg(test)]
    pub(crate) fn frame_for_test(&mut self, root: &mut egui::Ui) {
        self.frame(root);
    }

    /// Save one frame of the real window to `path`, then close the app.
    pub fn screenshot_to(&mut self, path: PathBuf) {
        self.screenshot = Some(Screenshot {
            path,
            requested: false,
            started: Instant::now(),
        });
    }

    fn frame(&mut self, root: &mut egui::Ui) {
        for message in self.bridge.poll() {
            self.state.apply_message(message);
        }
        if self.state.connected && !self.was_connected {
            // A fresh daemon connection may have new guilds or a new identity.
            self.reset_requests();
            self.state.control = None;
            self.state.voice.clear_all();
            self.state.session_id = None;
            self.state.connection = None;
        }
        self.was_connected = self.state.connected;

        self.expire_notice();
        self.sync_requests();
        self.sync_push_to_talk(root.ctx());

        let mut actions = Vec::new();
        ui::draw(root, &mut self.state, &mut actions);
        for action in actions {
            self.handle_action(action);
        }
        self.sync_typing();
    }

    /// Send a push-to-talk hold transition when the key or the call state
    /// changes. The key is local device input, so it never becomes a roster
    /// flag.
    fn sync_push_to_talk(&mut self, ctx: &egui::Context) {
        let key_down = ctx.input(|input| input.key_down(egui::Key::F8));
        let wanted = key_down && self.state.voice.push_to_talk && self.state.voice.joined();
        if wanted == self.ptt_held {
            return;
        }
        if let (Some(guild_id), Some(channel_id)) =
            (self.state.voice.guild, self.state.voice.channel)
        {
            self.bridge.send(
                Op::SetPushToTalkHeld {
                    guild_id,
                    channel_id,
                    held: wanted,
                },
                RequestTag::Other,
            );
        }
        self.ptt_held = wanted;
    }

    fn capture_screenshot(&mut self, ui: &mut egui::Ui) {
        let Some(pending) = self.screenshot.as_mut() else {
            return;
        };
        if pending.requested {
            // The screenshot callback closes the window once the pixels are
            // in hand. This is only a safety net for a compositor that never
            // delivers them, so the process cannot hang forever.
            if pending.started.elapsed() > Duration::from_secs(20) {
                eprintln!("ovvc-gui: screenshot timed out");
                self.screenshot = None;
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ui.ctx().request_repaint_after(Duration::from_millis(200));
            return;
        }

        // Give the daemon a moment to fill the guild list and transcript, but
        // never hold the screenshot hostage to an offline daemon.
        let ready = self.state.connected
            && !self.state.guilds.is_empty()
            && (!self.state.messages.is_empty()
                || pending.started.elapsed() > Duration::from_secs(2));
        if !ready && pending.started.elapsed() < Duration::from_secs(8) {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
            return;
        }

        pending.requested = true;
        let path = pending.path.clone();
        let ctx = ui.ctx().clone();
        let repaint = ctx.clone();
        ctx.clone().request_screenshot(move |image| {
            match save_png(&image, &path) {
                Ok(()) => eprintln!("ovvc-gui: screenshot written to {}", path.display()),
                Err(error) => eprintln!("ovvc-gui: could not write {}: {error}", path.display()),
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        });
        repaint.request_repaint();
    }

    fn reset_requests(&mut self) {
        self.requested_identity = false;
        self.requested_guilds = false;
        self.requested_status = false;
        self.requested_settings = false;
        self.requested_direct_messages = false;
        self.dm_unread_polled.clear();
        self.loaded_guild = None;
        self.requested_history = None;
        self.requested_control = None;
        self.requested_policy = None;
        self.connection_polled_at = None;
    }

    /// Clear an informational strip once it has been on screen long enough.
    /// Errors stay until the user dismisses them.
    fn expire_notice(&mut self) {
        if self.state.notice != self.notice_seen {
            self.notice_seen = self.state.notice.clone();
            self.notice_since = Some(Instant::now());
        }
        if let Some(since) = self.notice_since
            && since.elapsed() >= NOTICE_LIFETIME
            && self.state.notice.is_some()
        {
            self.state.notice = None;
            self.notice_seen = None;
            self.notice_since = None;
        }
    }

    fn sync_requests(&mut self) {
        if !self.state.connected {
            return;
        }
        if !self.requested_identity {
            self.requested_identity = true;
            self.bridge.send(Op::Identity, RequestTag::Identity);
        }
        if !self.requested_guilds {
            self.requested_guilds = true;
            self.bridge.send(Op::ListGuilds, RequestTag::Guilds);
        }
        if !self.requested_status {
            self.requested_status = true;
            self.bridge.send(Op::Status, RequestTag::Status);
        }
        // One read covers both the first request and a save the daemon
        // refused, so a reconnect cannot put two identical settings requests
        // in the same frame.
        if !self.requested_settings || self.state.settings_stale {
            self.requested_settings = true;
            self.state.settings_stale = false;
            self.bridge
                .send(Op::NotificationSettings, RequestTag::NotificationSettings);
        }

        // Direct conversations are ordinary guilds in the store, but they get
        // their own sidebar section, so the window keeps a separate list. It is
        // fetched once per connection and again when an announcement or a DM
        // membership change arrives.
        if !self.requested_direct_messages || self.state.direct_messages_stale {
            self.requested_direct_messages = true;
            self.state.direct_messages_stale = false;
            self.bridge
                .send(Op::ListDirectMessages, RequestTag::DirectMessages);
        }
        let dm_guilds: Vec<GuildId> = self
            .state
            .direct_messages
            .iter()
            .map(|conversation| conversation.guild.id)
            .collect();
        for guild_id in dm_guilds {
            // The selected DM already gets its counts through `load_guild`'s
            // ordinary UnreadCounts request.
            if Some(guild_id) == self.state.selected_guild {
                continue;
            }
            if self.dm_unread_polled.insert(guild_id) {
                self.bridge.send(
                    Op::UnreadCounts { guild_id },
                    RequestTag::DmUnread(guild_id),
                );
            }
        }

        // The profile panel refetches only when it was opened or a profile
        // event landed, so a refused GetMember does not become a request loop.
        if self.state.dialog == crate::state::Dialog::Profile
            && self.state.profile_stale
            && !self.state.profile_loading
            && let (Some(guild_id), Some(author)) =
                (self.state.profile_guild, self.state.profile_author)
        {
            self.state.profile_loading = true;
            self.state.profile_stale = false;
            self.bridge.send(
                Op::GetMember { guild_id, author },
                RequestTag::MemberProfile { guild_id, author },
            );
        }

        let Some(guild_id) = self.state.selected_guild else {
            return;
        };
        if let Some(name) = self.pending_display_name.take() {
            self.state.display_name = name.clone();
            self.bridge.send(
                Op::SetDisplayName {
                    guild_id,
                    display_name: name,
                },
                RequestTag::SetDisplayName,
            );
        }
        if self.loaded_guild != Some(guild_id) {
            self.loaded_guild = Some(guild_id);
            // A selection can change twice before this runs (a guild joined and
            // then switched back), so drop the remembered request and let
            // `load_guild` ask again rather than leaving the policy tab on "not
            // loaded yet" with a stale id.
            self.requested_policy = None;
            self.load_guild(guild_id);
        }
        if self.state.members_stale {
            self.state.members_stale = false;
            self.bridge
                .send(Op::ListMembers { guild_id }, RequestTag::Members(guild_id));
        }
        // Setup created the first commune. Bring it online on the fixed listen
        // address and open the invite once the daemon reports a dialable one,
        // so the next thing the person sees is the code to send a friend.
        if self.first_run_invite {
            if !self.state.invite_code.is_empty() {
                self.first_run_invite = false;
                self.state.dialog = crate::state::Dialog::Invite;
            } else if self.state.is_online(guild_id) {
                if !self.first_run_invite_sent && self.state.can_invite(guild_id) {
                    self.first_run_invite_sent = true;
                    let peers = self.state.invite_addresses();
                    if peers.is_empty() {
                        self.first_run_invite = false;
                    } else {
                        self.bridge.send(
                            Op::CreateInvite {
                                guild_id,
                                peers,
                                expires_in_secs: None,
                            },
                            RequestTag::CreateInvite(guild_id),
                        );
                    }
                }
            } else if !self.first_run_connect_sent
                && self.state.connect_requested.is_none()
                && self.state.error.is_none()
            {
                self.first_run_connect_sent = true;
                self.take_the_other_guild_offline(Some(guild_id));
                self.state.connect_requested = Some(guild_id);
                self.bridge.send(
                    Op::Connect {
                        guild_id,
                        options: self.network_options(),
                    },
                    RequestTag::Connect(guild_id),
                );
            }
        }
        // Nothing tells an attached frontend that another client took the guild
        // offline: `Op::Disconnect` returns a bare Unit and emits no event. So
        // the snapshot is re-asked on a fixed pace, one small IPC frame every
        // half second, and an external disconnect shows up within that window.
        // It also covers a guild that is still starting its listener.
        if self
            .connection_polled_at
            .is_none_or(|at| at.elapsed() >= CONNECTION_POLL)
        {
            self.connection_polled_at = Some(Instant::now());
            self.bridge.send(
                Op::ConnectionState { guild_id },
                RequestTag::Connection(guild_id),
            );
        }

        // A connect that never reports a listen address must not leave the
        // window waiting forever: clear the pending flag after a deadline so a
        // later offline snapshot is allowed to win.
        match self.state.connect_requested {
            Some(pending) if pending == guild_id => {
                let started = *self.connect_started_at.get_or_insert_with(Instant::now);
                if started.elapsed() >= CONNECT_DEADLINE {
                    self.state.connect_requested = None;
                    self.connect_started_at = None;
                    self.state.error = Some(
                        "the daemon did not report a listen address within ten seconds".to_string(),
                    );
                }
            }
            _ => self.connect_started_at = None,
        }

        let session_id = SessionId::for_guild(guild_id);
        if self.requested_control != Some((guild_id, session_id)) {
            self.requested_control = Some((guild_id, session_id));
            let display_name = self
                .state
                .identity
                .map(|author| self.state.display_name(author))
                .unwrap_or_else(|| "Member".to_string());
            self.bridge.send(
                Op::ControlJoin {
                    guild_id,
                    session_id,
                    display_name,
                },
                RequestTag::JoinControl {
                    guild_id,
                    session_id,
                },
            );
        }

        // Presence is a lease in the control session, so an idle window has to
        // re-announce the chosen status or the local member, and every peer's
        // view of it, turns offline six seconds after the last frame.
        if let Some(op) = self
            .state
            .presence_keepalive(ovvc_control::now_ms(), guild_id)
        {
            self.bridge.send(op, RequestTag::SetPresence);
        }

        let Some(channel_id) = self.state.selected_channel else {
            return;
        };
        if self.requested_history != Some((guild_id, channel_id)) {
            self.request_history(guild_id, channel_id);
        }
        if self.state.loaded_channel == Some(channel_id)
            && let Some(event_id) = self.state.latest_event_id()
            && self.marked_read.get(&channel_id) != Some(&event_id)
        {
            self.marked_read.insert(channel_id, event_id);
            self.state.unread.insert(channel_id, 0);
            self.state.mark_dm_read(guild_id);
            self.bridge.send(
                Op::MarkRead {
                    guild_id,
                    channel_id,
                    event_id,
                },
                RequestTag::MarkRead,
            );
        }
    }

    fn load_guild(&mut self, guild_id: GuildId) {
        // Asking for the connection snapshot here counts as this frame's poll,
        // otherwise the poll block below asks for the same snapshot again in
        // the same frame the guild was first loaded.
        self.connection_polled_at = Some(Instant::now());
        self.bridge.send(
            Op::ChannelTree { guild_id },
            RequestTag::ChannelTree(guild_id),
        );
        self.bridge
            .send(Op::ListMembers { guild_id }, RequestTag::Members(guild_id));
        self.bridge
            .send(Op::UnreadCounts { guild_id }, RequestTag::Unread(guild_id));
        self.bridge.send(
            Op::ConnectionState { guild_id },
            RequestTag::Connection(guild_id),
        );
        if self.requested_policy != Some(guild_id) {
            self.requested_policy = Some(guild_id);
            self.bridge.send(
                Op::Policy { guild_id },
                RequestTag::Policy(guild_id),
            );
        }
    }

    fn request_history(&mut self, guild_id: GuildId, channel_id: ChannelId) {
        // Record it here rather than at each call site: a click handled after
        // `sync_requests` has already run must not leave the flag cleared, or
        // the next frame's sync sends the same page a second time.
        self.requested_history = Some((guild_id, channel_id));
        self.bridge.send(
            Op::HistoryBefore {
                guild_id,
                channel_id,
                cursor: None,
                limit: HISTORY_PAGE,
            },
            RequestTag::HistoryBefore {
                guild_id,
                channel_id,
                cursor: None,
            },
        );
    }

    fn sync_typing(&mut self) {
        let Some(guild_id) = self.state.selected_guild else {
            return;
        };
        let Some(channel_id) = self.state.selected_channel else {
            return;
        };
        let active = !self.state.composer.trim().is_empty() && self.state.editing.is_none();
        let refresh = self
            .last_typing
            .map(|last| last.elapsed() >= TYPING_REFRESH)
            .unwrap_or(true);
        if active && (!self.typing_sent || refresh) {
            self.last_typing = Some(Instant::now());
            self.typing_sent = true;
            self.bridge.send(
                Op::SetTyping {
                    guild_id,
                    channel_id,
                    typing: true,
                },
                RequestTag::Other,
            );
        } else if !active && self.typing_sent {
            self.typing_sent = false;
            self.last_typing = None;
            self.bridge.send(
                Op::SetTyping {
                    guild_id,
                    channel_id,
                    typing: false,
                },
                RequestTag::Other,
            );
        }
    }

    /// Network options as typed in the network tab, shared by [ go online ]
    /// and the join dialog so both use the same listen addresses.
    fn network_options(&self) -> NetworkOptionsDto {
        NetworkOptionsDto {
            listen: parse_addresses(&self.state.network_listen),
            dial: parse_addresses(&self.state.network_dial),
            relay: {
                let relay = self.state.network_relay.trim();
                (!relay.is_empty()).then(|| relay.to_string())
            },
            upnp: false,
            natpmp: false,
        }
    }

    /// Refuse the action when a field holds something that is not a
    /// multiaddress. `parse_addresses` drops such a token, and an empty listen
    /// list makes the daemon bind an ephemeral wildcard port, so a typo would
    /// silently listen somewhere the user cannot invite anyone to.
    fn network_address_problem(&self) -> Option<String> {
        address_problem(&self.state.network_listen)
            .or_else(|| address_problem(&self.state.network_dial))
            .or_else(|| single_address_problem(&self.state.network_relay, "relay address"))
            .or_else(|| {
                single_address_problem(&self.state.network_public, "public invite address")
            })
    }

    /// Take any guild other than `keep` offline, so only one guild node binds
    /// the configured listen port at a time. The window itself shows one guild,
    /// so the daemon never needs two of them networking together here.
    fn take_the_other_guild_offline(&mut self, keep: Option<GuildId>) {
        let Some(online) = self.state.online_guild() else {
            return;
        };
        if Some(online) == keep {
            return;
        }
        self.bridge.send(
            Op::Disconnect { guild_id: online },
            RequestTag::Disconnect(online),
        );
    }

    /// Tell the daemon to hang up the local call, then forget it here.
    ///
    /// A guild switch or a reconnect would otherwise leave the daemon holding
    /// a call and an audio service for a guild the window no longer shows.
    fn leave_voice_call(&mut self) {
        let (Some(guild_id), Some(channel_id)) = (self.state.voice.guild, self.state.voice.channel)
        else {
            self.state.voice.clear_call();
            return;
        };
        self.bridge.send(
            Op::VoiceLeave {
                guild_id,
                channel_id,
            },
            RequestTag::VoiceLeave {
                guild_id,
                channel_id,
            },
        );
        self.state.voice.clear_call();
    }

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::Refresh => {
                self.requested_identity = false;
                self.requested_guilds = false;
                self.requested_status = false;
                self.requested_direct_messages = false;
                self.dm_unread_polled.clear();
                self.state.control = None;
                self.state.voice.clear_all();
                self.state.connection = None;
                self.state.connect_requested = None;
                self.state.error = None;
                self.state.notice = None;
                self.connection_polled_at = None;
            }
            Action::SelectGuild(guild_id) => {
                // Clicking the commune already on screen is not a reload; the
                // state layer already treats it as a no-op, and reloading here
                // would reset the history bookkeeping mid-frame.
                if self.state.selected_guild == Some(guild_id)
                    && self.loaded_guild == Some(guild_id)
                {
                    return;
                }
                self.leave_voice_call();
                self.state.select_guild(guild_id);
                self.loaded_guild = Some(guild_id);
                self.requested_history = None;
                self.requested_control = None;
                self.load_guild(guild_id);
            }
            Action::SelectChannel {
                guild_id,
                channel_id,
            } => {
                // The same channel is already open and its page is on the way
                // or on screen, so the click asks for nothing new.
                if self.state.selected_guild == Some(guild_id)
                    && self.state.selected_channel == Some(channel_id)
                    && self.requested_history == Some((guild_id, channel_id))
                {
                    return;
                }
                self.state.select_channel(Some(channel_id));
                self.request_history(guild_id, channel_id);
            }
            Action::OpenSearchHit {
                guild_id,
                channel_id,
                event_id,
            } => {
                // Search is guild scoped, so this normally cannot differ, but
                // going through the same switch as a guild click keeps the
                // loaded-guild bookkeeping in one shape if it ever can.
                if self.state.selected_guild != Some(guild_id) {
                    self.leave_voice_call();
                    self.state.select_guild(guild_id);
                    self.loaded_guild = Some(guild_id);
                    self.requested_control = None;
                    self.load_guild(guild_id);
                }
                self.state.open_search_hit(channel_id, event_id);
                self.request_history(guild_id, channel_id);
            }
            Action::CreateGuild { name } => {
                // The reply selects the new guild, so hang up the old call now
                // rather than leaving its audio service running in the daemon.
                self.leave_voice_call();
                self.bridge
                    .send(Op::CreateGuild { name }, RequestTag::CreateGuild);
            }
            Action::CreateChannel { guild_id, name } => {
                self.bridge.send(
                    Op::CreateChannel { guild_id, name },
                    RequestTag::CreateChannel(guild_id),
                );
            }
            Action::CreateChannelIn {
                guild_id,
                name,
                kind,
                parent,
            } => {
                self.bridge.send(
                    Op::CreateChannelIn {
                        guild_id,
                        name,
                        kind,
                        parent,
                    },
                    RequestTag::CreateChannel(guild_id),
                );
            }
            Action::CreateCategory { guild_id, name } => {
                self.bridge.send(
                    Op::CreateCategory { guild_id, name },
                    RequestTag::CreateCategory(guild_id),
                );
            }
            Action::RenameChannel {
                guild_id,
                channel_id,
                name,
            } => {
                if let Some(channel) = self
                    .state
                    .channels
                    .iter_mut()
                    .find(|channel| channel.id == channel_id)
                {
                    channel.name = name.clone();
                }
                self.state.sort_sidebar();
                self.bridge.send(
                    Op::RenameChannel {
                        guild_id,
                        channel_id,
                        name,
                    },
                    RequestTag::RenameChannel(guild_id),
                );
            }
            Action::MoveChannel {
                guild_id,
                channel_id,
                parent,
            } => {
                if let Some(channel) = self
                    .state
                    .channels
                    .iter_mut()
                    .find(|channel| channel.id == channel_id)
                {
                    channel.parent = parent;
                }
                self.state.sort_sidebar();
                self.bridge.send(
                    Op::MoveChannel {
                        guild_id,
                        channel_id,
                        parent,
                    },
                    RequestTag::MoveChannel(guild_id),
                );
            }
            Action::SetPresence { guild_id, status } => {
                self.state.own_presence = status;
                self.bridge.send(
                    Op::SetPresence { guild_id, status },
                    RequestTag::SetPresence,
                );
            }
            Action::SetDesktopNotifications(enabled) => {
                self.state.notification_settings.desktop = enabled;
                self.bridge.send(
                    Op::SetDesktopNotifications { enabled },
                    RequestTag::SetNotificationSettings,
                );
            }
            Action::SetDoNotDisturb(enabled) => {
                self.state.notification_settings.dnd = enabled;
                self.bridge.send(
                    Op::SetDoNotDisturb { enabled },
                    RequestTag::SetNotificationSettings,
                );
            }
            Action::SetQuietHours(quiet_hours) => {
                self.state.notification_settings.quiet_hours = quiet_hours;
                self.bridge.send(
                    Op::SetQuietHours { quiet_hours },
                    RequestTag::SetNotificationSettings,
                );
            }
            Action::RefreshNotificationSettings => {
                self.bridge
                    .send(Op::NotificationSettings, RequestTag::NotificationSettings);
            }
            Action::SetChannelMuted { channel_id, muted } => {
                self.state.set_channel_muted(channel_id, muted);
                self.bridge.send(
                    Op::SetChannelMuted { channel_id, muted },
                    RequestTag::SetNotificationSettings,
                );
            }
            Action::SetGuildMuted { guild_id, muted } => {
                self.state.set_guild_muted(guild_id, muted);
                self.bridge.send(
                    Op::SetGuildMuted { guild_id, muted },
                    RequestTag::SetNotificationSettings,
                );
            }
            Action::FinishWizard => {
                self.state.wizard_done = true;
                self.state.wizard_step = None;
                self.state.screen = crate::state::ScreenView::Chat;
                let name = self.state.wizard_name.trim().to_string();
                if !name.is_empty() {
                    self.state.display_name = name.clone();
                    self.pending_display_name = Some(name);
                }
                match self.state.wizard_commune {
                    WizardCommune::Create => {
                        let commune = self.state.wizard_commune_name.trim().to_string();
                        if !commune.is_empty() {
                            // The first commune is created here rather than
                            // left for the person to find. Once the reply
                            // selects it, sync_requests brings it online on the
                            // fixed port and opens the invite.
                            self.first_run_invite = true;
                            self.first_run_connect_sent = false;
                            self.first_run_invite_sent = false;
                            self.bridge.send(
                                Op::CreateGuild { name: commune },
                                RequestTag::CreateGuild,
                            );
                        }
                    }
                    WizardCommune::Join => {
                        let code = self.state.wizard_join_code.trim().to_string();
                        if !code.is_empty() {
                            if let Some(problem) = self.network_address_problem() {
                                self.state.error = Some(problem);
                            } else {
                                self.leave_voice_call();
                                self.take_the_other_guild_offline(None);
                                self.bridge.send(
                                    Op::JoinGuild {
                                        invite: code,
                                        options: self.network_options(),
                                    },
                                    RequestTag::JoinGuild,
                                );
                            }
                        }
                    }
                }
                mark_wizard_seen();
            }
            Action::CancelSetup => {
                // Cancel leaves every draft untouched. It only records that the
                // flow was shown, so a launch does not reopen it.
                self.state.wizard_done = true;
                self.state.wizard_step = None;
                self.state.screen = crate::state::ScreenView::Chat;
                mark_wizard_seen();
            }
            Action::SetDetailLevel(level) => {
                self.state.detail_level = level;
                // A read-only config directory must not stop the level from
                // changing for this session.
                if let Err(error) = level.save() {
                    eprintln!("ovvc-gui: could not save the detail level: {error}");
                }
            }
            Action::SendMessage {
                guild_id,
                channel_id,
                body,
                reply_to,
                mentions,
            } => {
                self.bridge.send(
                    Op::PostMessageFull {
                        guild_id,
                        channel_id,
                        body,
                        reply_to,
                        mentions,
                    },
                    RequestTag::SendMessage,
                );
            }
            Action::SendRichMessage {
                send_id,
                guild_id,
                channel_id,
                body,
                reply_to,
                mentions,
                attachments,
            } => {
                self.bridge.send_rich(
                    guild_id,
                    channel_id,
                    body,
                    reply_to,
                    mentions,
                    attachments,
                    send_id,
                );
            }
            Action::StageAttachment { path } => {
                self.bridge.stage_attachment(path);
            }
            Action::ClearStagedAttachment { index } => {
                if index < self.state.staged_attachments.len() {
                    self.state.staged_attachments.remove(index);
                }
            }
            Action::OpenAttachment {
                guild_id,
                attachment,
            } => {
                self.state.notice = Some(format!("fetching {}...", attachment.name));
                self.bridge.save_attachment(guild_id, attachment);
            }
            Action::BeginEdit { event_id, body } => {
                self.state.editing = Some((event_id, body.clone()));
                self.state.composer = body;
                self.state.reply_to = None;
            }
            Action::Reply { event_id } => {
                if let Some(message) = self
                    .state
                    .messages
                    .iter()
                    .find(|message| message.event_id == event_id)
                {
                    self.state.reply_to = Some((event_id, message.author, message.body.clone()));
                    self.state.editing = None;
                }
            }
            Action::EditMessage {
                guild_id,
                target,
                body,
            } => {
                self.bridge.send(
                    Op::EditMessage {
                        guild_id,
                        target,
                        body,
                    },
                    RequestTag::EditMessage,
                );
            }
            Action::DeleteMessage { guild_id, target } => {
                self.bridge.send(
                    Op::DeleteMessage { guild_id, target },
                    RequestTag::DeleteMessage,
                );
            }
            Action::ToggleReaction {
                guild_id,
                target,
                emoji,
                mine,
            } => {
                let op = if mine {
                    Op::RemoveReaction {
                        guild_id,
                        target,
                        emoji,
                    }
                } else {
                    Op::AddReaction {
                        guild_id,
                        target,
                        emoji,
                    }
                };
                self.bridge.send(op, RequestTag::Reaction);
            }
            Action::MarkRead {
                guild_id,
                channel_id,
                event_id,
            } => {
                self.marked_read.insert(channel_id, event_id);
                self.state.unread.insert(channel_id, 0);
                self.state.mark_dm_read(guild_id);
                self.bridge.send(
                    Op::MarkRead {
                        guild_id,
                        channel_id,
                        event_id,
                    },
                    RequestTag::MarkRead,
                );
            }
            Action::Search { guild_id, query } => {
                self.state.searching = true;
                self.state.searched_query = Some(query.clone());
                self.bridge.send(
                    Op::Search {
                        guild_id,
                        query,
                        limit: 50,
                    },
                    RequestTag::Search(guild_id),
                );
            }
            Action::LoadOlder {
                guild_id,
                channel_id,
            } => {
                if self.state.history_loading || !self.state.history_more {
                    return;
                }
                let Some(cursor) = self.state.history_oldest else {
                    return;
                };
                self.state.history_loading = true;
                self.bridge.send(
                    Op::HistoryBefore {
                        guild_id,
                        channel_id,
                        cursor: Some(cursor),
                        limit: HISTORY_PAGE,
                    },
                    RequestTag::HistoryBefore {
                        guild_id,
                        channel_id,
                        cursor: Some(cursor),
                    },
                );
            }
            Action::JoinControl {
                guild_id,
                session_id,
                display_name,
            } => {
                self.bridge.send(
                    Op::ControlJoin {
                        guild_id,
                        session_id,
                        display_name,
                    },
                    RequestTag::JoinControl {
                        guild_id,
                        session_id,
                    },
                );
            }
            Action::VoiceJoin {
                guild_id,
                channel_id,
                muted,
                deafened,
                push_to_talk,
            } => {
                // The daemon's voice sessions are per channel, so joining a
                // second channel would leave the first call running with its
                // own audio service. Hang up first when the selection moved.
                if let Some(previous) = self.state.voice.channel
                    && previous != channel_id
                {
                    let previous_guild = self.state.voice.guild.unwrap_or(guild_id);
                    self.bridge.send(
                        Op::VoiceLeave {
                            guild_id: previous_guild,
                            channel_id: previous,
                        },
                        RequestTag::VoiceLeave {
                            guild_id: previous_guild,
                            channel_id: previous,
                        },
                    );
                    self.state.voice.clear_call();
                }
                self.state.voice.muted = muted;
                self.state.voice.deafened = deafened;
                self.state.voice.push_to_talk = push_to_talk;
                self.bridge.send(
                    Op::VoiceJoin {
                        guild_id,
                        channel_id,
                        muted,
                        deafened,
                        push_to_talk,
                    },
                    RequestTag::VoiceJoin {
                        guild_id,
                        channel_id,
                    },
                );
            }
            Action::VoiceLeave {
                guild_id,
                channel_id,
            } => {
                self.bridge.send(
                    Op::VoiceLeave {
                        guild_id,
                        channel_id,
                    },
                    RequestTag::VoiceLeave {
                        guild_id,
                        channel_id,
                    },
                );
            }
            Action::SetVoiceState {
                guild_id,
                channel_id,
                muted,
                deafened,
                push_to_talk,
            } => {
                self.state.voice.muted = muted;
                self.state.voice.deafened = deafened;
                self.state.voice.push_to_talk = push_to_talk;
                // Before a join there is nothing to publish: the flags are
                // remembered and sent with `VoiceJoin`.
                if self.state.voice.channel == Some(channel_id) {
                    let author = self.state.identity.unwrap_or(ovvc_core::AuthorId([0; 32]));
                    let previous = self.state.voice.local_state(author);
                    let state = self.state.voice.member_state(previous);
                    self.bridge.send(
                        Op::SetVoiceState {
                            guild_id,
                            channel_id,
                            muted: state.muted,
                            deafened: state.deafened,
                            push_to_talk: state.push_to_talk,
                            speaking: state.speaking,
                            video: state.video,
                            screen_share: state.screen_share,
                        },
                        RequestTag::SetVoiceState {
                            guild_id,
                            channel_id,
                        },
                    );
                }
            }
            Action::SetDisplayName {
                guild_id,
                display_name,
            } => {
                if let Some(identity) = self.state.identity
                    && let Some(member) = self
                        .state
                        .members
                        .iter_mut()
                        .find(|member| member.author == identity)
                {
                    member.display_name = display_name.clone();
                }
                self.bridge.send(
                    Op::SetDisplayName {
                        guild_id,
                        display_name,
                    },
                    RequestTag::SetDisplayName,
                );
            }
            Action::OpenProfile { guild_id, author } => {
                self.state.open_profile(guild_id, author);
            }
            Action::SaveProfile {
                guild_id,
                about,
                pronouns,
                links,
            } => {
                self.state.profile_saving = true;
                self.bridge.send(
                    Op::SetProfile {
                        guild_id,
                        about,
                        pronouns,
                        links,
                    },
                    RequestTag::SetProfile,
                );
            }
            Action::OpenDirectMessage { context, peer } => {
                self.state.dm_opening = Some(peer);
                self.bridge.send(
                    Op::OpenDirectMessage { context, peer },
                    RequestTag::OpenDirectMessage { peer },
                );
            }
            Action::CreateInvite { guild_id } => {
                let addresses = self.state.invite_addresses();
                if addresses.is_empty() {
                    // An invite with no peers decodes on the other machine and
                    // then fails to join, so it is better to say so here.
                    self.state.error = Some(
                        "no dialable address yet: take the commune online before creating an invite"
                            .to_string(),
                    );
                    return;
                }
                self.bridge.send(
                    Op::CreateInvite {
                        guild_id,
                        peers: addresses,
                        expires_in_secs: None,
                    },
                    RequestTag::CreateInvite(guild_id),
                );
            }
            Action::JoinGuild { code } => {
                // Joining dials the invite and listens on the network tab's
                // addresses so the new member is reachable for the first sync.
                // The daemon does not reconnect guilds at startup, so this is
                // about the join itself, not about a later restart.
                if let Some(problem) = self.network_address_problem() {
                    self.state.error = Some(problem);
                    return;
                }
                self.leave_voice_call();
                self.take_the_other_guild_offline(None);
                self.bridge.send(
                    Op::JoinGuild {
                        invite: code,
                        options: self.network_options(),
                    },
                    RequestTag::JoinGuild,
                );
            }
            Action::RefreshMembers(guild_id) => {
                self.bridge
                    .send(Op::ListMembers { guild_id }, RequestTag::Members(guild_id));
            }
            Action::Connect { guild_id } => {
                if let Some(problem) = self.network_address_problem() {
                    self.state.error = Some(problem);
                    return;
                }
                // One guild at a time: two nodes on the same listen port use
                // SO_REUSEPORT and steal each other's connections, so the
                // guild that is online goes offline before the new one binds.
                self.take_the_other_guild_offline(Some(guild_id));
                self.state.connect_requested = Some(guild_id);
                self.bridge.send(
                    Op::Connect {
                        guild_id,
                        options: self.network_options(),
                    },
                    RequestTag::Connect(guild_id),
                );
            }
            Action::Disconnect { guild_id } => {
                self.bridge.send(
                    Op::Disconnect { guild_id },
                    RequestTag::Disconnect(guild_id),
                );
            }
            Action::ShutdownDaemon => {
                self.bridge.send(Op::ShutdownDaemon, RequestTag::Shutdown);
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
        self.capture_screenshot(ui);
        if self.state.connected {
            // Events wake the UI immediately; this keeps relative timestamps
            // and typing deadlines moving while the window is otherwise idle.
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
    }
}

/// Where the GUI remembers that the getting-started window has been shown.
fn wizard_marker_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("org", "ovvc", "ovvc")
        .map(|dirs| dirs.config_dir().join("onboarding-seen"))
}

fn wizard_seen() -> bool {
    wizard_marker_path().is_some_and(|path| path.exists())
}

/// Write the marker, failing soft: a read-only config directory should not
/// stop the user from finishing onboarding.
fn mark_wizard_seen() {
    let Some(path) = wizard_marker_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = std::fs::write(&path, b"1\n") {
        eprintln!("ovvc-gui: could not record onboarding state: {error}");
    }
}

/// A relay or public address is one address, unlike the listen and dial lists.
/// An accidental second token would otherwise be dropped and the daemon would
/// run with a different address than the field shows.
fn single_address_problem(text: &str, label: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.split_whitespace().count() > 1 {
        return Some(format!("{label}: enter one multiaddress, not a list"));
    }
    address_problem(trimmed).map(|problem| format!("{label}: {problem}"))
}

fn save_png(image: &egui::ColorImage, path: &std::path::Path) -> Result<(), String> {
    let width = u32::try_from(image.width()).map_err(|_| "image is too wide".to_string())?;
    let height = u32::try_from(image.height()).map_err(|_| "image is too tall".to_string())?;
    let mut bytes = Vec::with_capacity(image.pixels.len() * 4);
    for pixel in &image.pixels {
        bytes.extend_from_slice(&pixel.to_array());
    }
    image::save_buffer(path, &bytes, width, height, image::ColorType::Rgba8)
        .map_err(|error| error.to_string())
}
