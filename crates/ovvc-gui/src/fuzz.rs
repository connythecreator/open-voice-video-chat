//! Deterministic fuzzed-interaction tests for the window.
//!
//! Every frame goes through the real [`App::frame_for_test`] and
//! [`crate::ui::draw`], with the daemon bridge swapped for a small fake that
//! answers the same operations from an in-memory world. Interactions are read
//! from the AccessKit tree egui produces, so the fuzzer clicks the widgets that
//! are actually on screen rather than guessed coordinates.
//!
//! The short run is part of `cargo test -p ovvc-gui`. A longer soak is:
//!
//! ```text
//! FUZZ_SEED=7 FUZZ_ITERS=2000 cargo test -p ovvc-gui fuzz:: -- --nocapture
//! ```
//!
//! `FUZZ_SEEDS` sets how many seeds the default run walks (three), and
//! `FUZZ_SEED` runs one seed for reproduction. A failure prints the seed, the
//! frame and the last interactions before it panics.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::rc::Rc;

use egui::accesskit::{Action as AccessAction, Role};
use egui::{Event, Key, Modifiers, PointerButton, Pos2, Rect, pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use ovvc_control::PresenceStatus;
use ovvc_core::{
    Attachment, AuthorId, CategoryMeta, ChannelId, ChannelKind, ChannelMeta, ChannelView,
    DirectConversation, EventBody, EventId, EventKind, EventPage, GuildId, GuildMeta, Identity,
    Invite, MemberInfo, MemberProfile, NotificationSettings, SignedEvent,
};
use ovvc_ipc::{
    ChannelTreeDto, ConnectionStateDto, DaemonEvent, DaemonStatusDto, IpcError, Op, PeerSnapshot,
    Reply,
};

use crate::app::{App, Backend};
use crate::state::{
    AppState, DetailLevel, Dialog, Message, RequestTag, ScreenView, SettingsTab, StagedAttachment,
    WizardCommune, WizardStep,
};

const GUILD_A: GuildId = GuildId([1; 32]);
const GUILD_B: GuildId = GuildId([4; 32]);
const DM_GUILD: GuildId = GuildId([5; 32]);
const GENERAL_A: ChannelId = ChannelId([2; 16]);
const OFFTOPIC_A: ChannelId = ChannelId([3; 16]);
const GENERAL_B: ChannelId = ChannelId([7; 16]);
const DM_CHANNEL: ChannelId = ChannelId([6; 16]);

const SCREEN: [f32; 2] = [1180.0, 760.0];

/// A request repeated in this many consecutive frames is treated as a loop.
///
/// The window polls connection state every 500 ms, so a fast headless run never
/// gets near this without something sending every frame.
const LOOP_FRAMES: usize = 120;

/// A dialog that stays open for longer than this is treated as stuck. The
/// fuzzer presses Escape every 37 frames, so a working overlay is closed well
/// before it.
const STUCK_MODAL_FRAMES: usize = 240;

const MAX_STRIP: usize = 4096;

// --- deterministic randomness ----------------------------------------------

/// SplitMix64, so the harness needs no extra dependency and every seed gives
/// an exact, repeatable sequence.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        assert!(n > 0);
        (self.next_u64() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }
}

// --- action log -------------------------------------------------------------

struct ActionLog {
    entries: VecDeque<String>,
    capacity: usize,
}

impl ActionLog {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity,
        }
    }

    fn push(&mut self, line: String) {
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(line);
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

// --- fake world -------------------------------------------------------------

struct World {
    me: Identity,
    ada: Identity,
    grace: Identity,
    commune_a: GuildMeta,
    commune_b: GuildMeta,
    dm_guild: GuildMeta,
    channels: HashMap<GuildId, Vec<ChannelView>>,
    categories: HashMap<GuildId, Vec<CategoryMeta>>,
    direct: Vec<DirectConversation>,
    settings: NotificationSettings,
    online: bool,
    seq: u64,
    next_guild: u8,
    next_channel: u8,
    next_category: u8,
    /// The profile fields published by this author through `SetProfile`.
    my_profile: (String, String, Vec<String>),
}

impl World {
    fn new() -> Self {
        let me = Identity::from_seed([7u8; 32]);
        let ada = Identity::from_seed([9u8; 32]);
        let grace = Identity::from_seed([11u8; 32]);
        let commune_a = guild(GUILD_A, "The Guild", &me);
        let commune_b = guild(GUILD_B, "Second Commune", &me);
        let dm_guild = guild(DM_GUILD, "Ada", &me);
        let mut channels = HashMap::new();
        channels.insert(
            GUILD_A,
            vec![
                channel(GENERAL_A, GUILD_A, "general", &me),
                channel(OFFTOPIC_A, GUILD_A, "off-topic", &me),
            ],
        );
        channels.insert(
            GUILD_B,
            vec![channel(GENERAL_B, GUILD_B, "general", &me)],
        );
        channels.insert(
            DM_GUILD,
            vec![channel(DM_CHANNEL, DM_GUILD, "dm", &me)],
        );
        let direct = vec![DirectConversation {
            guild: dm_guild.clone(),
            peer: member(DM_GUILD, &ada, "Ada"),
        }];
        Self {
            me,
            ada,
            grace,
            commune_a,
            commune_b,
            dm_guild,
            channels,
            categories: HashMap::new(),
            direct,
            settings: NotificationSettings::default(),
            online: true,
            seq: 1,
            next_guild: 40,
            next_channel: 80,
            next_category: 120,
            my_profile: (
                "writes small tools and reads logs".to_string(),
                "they/them".to_string(),
                vec!["https://example.com/conleth".to_string()],
            ),
        }
    }

    fn guild_list(&self) -> Vec<GuildMeta> {
        let mut list = vec![self.commune_a.clone(), self.commune_b.clone()];
        for guild in self.channels.keys() {
            if *guild != GUILD_A
                && *guild != GUILD_B
                && *guild != DM_GUILD
                && !list.iter().any(|existing| existing.id == *guild)
            {
                list.push(GuildMeta {
                    id: *guild,
                    name: format!("commune {}", guild.0[0]),
                    founder: self.me.author_id(),
                    created_at: 1_700_000_000,
                });
            }
        }
        list
    }

    fn channels_for(&self, guild_id: GuildId) -> Vec<ChannelView> {
        self.channels.get(&guild_id).cloned().unwrap_or_default()
    }

    fn categories_for(&self, guild_id: GuildId) -> Vec<CategoryMeta> {
        self.categories.get(&guild_id).cloned().unwrap_or_default()
    }

    fn members_for(&self, guild_id: GuildId) -> Vec<MemberInfo> {
        match guild_id {
            GUILD_A => vec![
                member(GUILD_A, &self.me, "Conleth"),
                member(GUILD_A, &self.ada, "Ada"),
                member(GUILD_A, &self.grace, "Grace"),
            ],
            GUILD_B => vec![
                member(GUILD_B, &self.me, "Conleth"),
                member(GUILD_B, &self.grace, "Grace"),
            ],
            DM_GUILD => vec![
                member(DM_GUILD, &self.me, "Conleth"),
                member(DM_GUILD, &self.ada, "Ada"),
            ],
            other => vec![member(other, &self.me, "Conleth")],
        }
    }

    fn profile(&self, guild_id: GuildId, author: AuthorId) -> MemberProfile {
        let (display_name, about, pronouns, links, count) = if author == self.me.author_id() {
            (
                "Conleth".to_string(),
                self.my_profile.0.clone(),
                self.my_profile.1.clone(),
                self.my_profile.2.clone(),
                12,
            )
        } else if author == self.ada.author_id() {
            (
                "Ada".to_string(),
                "builds small tools and reads logs".to_string(),
                "she/her".to_string(),
                vec![
                    "https://example.com/ada".to_string(),
                    "https://example.com/notes".to_string(),
                ],
                42,
            )
        } else {
            (
                "Grace".to_string(),
                "keeps the network honest".to_string(),
                String::new(),
                Vec::new(),
                7,
            )
        };
        MemberProfile {
            guild_id,
            author,
            display_name,
            about,
            pronouns,
            links,
            joined_at: 1_700_000_000,
            updated_at: 1_700_000_100,
            message_count: count,
            last_activity: 1_700_000_200,
        }
    }

    fn known_guild(&self) -> GuildId {
        if self.channels.contains_key(&GUILD_A) {
            GUILD_A
        } else {
            GUILD_B
        }
    }

    fn unread(&self, guild_id: GuildId) -> HashMap<ChannelId, usize> {
        let mut counts = HashMap::new();
        for channel in self.channels_for(guild_id) {
            counts.insert(channel.id, 0);
        }
        if guild_id == GUILD_A {
            counts.insert(OFFTOPIC_A, 2);
        }
        counts
    }

    fn history(&self, guild_id: GuildId, channel_id: ChannelId) -> Vec<&'static str> {
        if guild_id == DM_GUILD && channel_id == DM_CHANNEL {
            vec!["hello from the other side"]
        } else {
            vec![
                "morning all, the daemon build is green again",
                "the fuzz harness says hello",
            ]
        }
    }
}

fn guild(id: GuildId, name: &str, host: &Identity) -> GuildMeta {
    GuildMeta {
        id,
        name: name.to_string(),
        founder: host.author_id(),
        created_at: 1_700_000_000,
    }
}

fn channel(id: ChannelId, guild_id: GuildId, name: &str, creator: &Identity) -> ChannelView {
    ChannelView {
        id,
        guild_id,
        name: name.to_string(),
        created_by: creator.author_id(),
        created_at: 1_700_000_000,
        kind: ChannelKind::Text,
        parent: None,
    }
}

fn member(guild_id: GuildId, identity: &Identity, name: &str) -> MemberInfo {
    MemberInfo {
        guild_id,
        author: identity.author_id(),
        display_name: name.to_string(),
        joined_at: 1_700_000_000,
    }
}

// --- fake backend -----------------------------------------------------------

struct FakeBackend {
    inner: Rc<RefCell<Fake>>,
}

impl Backend for FakeBackend {
    fn send(&self, op: Op, tag: RequestTag) {
        self.inner.borrow_mut().send(op, tag);
    }

    fn stage_attachment(&self, path: PathBuf) {
        self.inner.borrow_mut().stage_attachment(path);
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
        self.inner.borrow_mut().send_rich(
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
        self.inner
            .borrow_mut()
            .save_attachment(guild_id, attachment);
    }

    fn poll(&mut self) -> Vec<Message> {
        self.inner.borrow_mut().poll()
    }
}

struct Fake {
    world: World,
    inbox: VecDeque<Message>,
    delayed: VecDeque<(usize, Message)>,
    frame: usize,
    ops_this_frame: Vec<Op>,
    duplicate: Option<String>,
    looping: Option<String>,
    streaks: HashMap<String, usize>,
    /// Deterministic random refusals, so error paths are exercised. Tests that
    /// want a clean script set this to zero.
    rng: Rng,
    error_chance: u64,
    profile_delay: usize,
    dm_delay: usize,
    sent_bodies: Vec<String>,
    set_profile_calls: usize,
    last_profile_links: Vec<String>,
    staged_calls: usize,
}

impl Fake {
    fn new(seed: u64) -> Self {
        let world = World::new();
        let mut inbox = VecDeque::new();
        inbox.push_back(Message::Event(DaemonEvent::Connected {
            daemon_version: "fuzz-0.1".to_string(),
            author_id: world.me.author_id(),
        }));
        Self {
            world,
            inbox,
            delayed: VecDeque::new(),
            frame: 0,
            ops_this_frame: Vec::new(),
            duplicate: None,
            looping: None,
            streaks: HashMap::new(),
            rng: Rng::new(seed ^ 0x5EED_F0AA),
            error_chance: 2,
            profile_delay: 2,
            dm_delay: 1,
            sent_bodies: Vec::new(),
            set_profile_calls: 0,
            last_profile_links: Vec::new(),
            staged_calls: 0,
        }
    }

    fn poll(&mut self) -> Vec<Message> {
        self.finish_frame();
        self.frame += 1;
        let mut out = Vec::new();
        let mut keep = VecDeque::new();
        while let Some((due, message)) = self.delayed.pop_front() {
            if due <= self.frame {
                out.push(message);
            } else {
                keep.push_back((due, message));
            }
        }
        self.delayed = keep;
        out.extend(self.inbox.drain(..));
        out
    }

    fn finish_frame(&mut self) {
        for first in 0..self.ops_this_frame.len() {
            for second in (first + 1)..self.ops_this_frame.len() {
                if self.ops_this_frame[first] == self.ops_this_frame[second] {
                    self.duplicate = Some(format!(
                        "the same request was sent twice in one frame: {:?}",
                        self.ops_this_frame[first]
                    ));
                }
            }
        }
        let current: HashSet<String> = self
            .ops_this_frame
            .iter()
            .map(|op| format!("{op:?}"))
            .collect();
        for key in &current {
            let streak = self.streaks.entry(key.clone()).or_insert(0);
            *streak += 1;
            if *streak >= LOOP_FRAMES {
                self.looping = Some(format!(
                    "request repeated for {streak} consecutive frames: {key}"
                ));
            }
        }
        self.streaks.retain(|key, _| current.contains(key));
        self.ops_this_frame.clear();
    }

    fn take_failure(&mut self) -> Option<String> {
        self.duplicate.take().or_else(|| self.looping.take())
    }

    fn queue(&mut self, message: Message) {
        self.inbox.push_back(message);
    }

    fn disconnect(&mut self) {
        self.world.online = false;
        self.queue(Message::Event(DaemonEvent::Disconnected {
            reason: "the fuzz bridge went away".to_string(),
        }));
    }

    fn reconnect(&mut self) {
        self.world.online = true;
        self.queue(Message::Event(DaemonEvent::Connected {
            daemon_version: "fuzz-0.1".to_string(),
            author_id: self.world.me.author_id(),
        }));
    }

    fn send(&mut self, op: Op, tag: RequestTag) {
        self.ops_this_frame.push(op.clone());
        if !self.world.online {
            self.queue(Message::Reply {
                tag,
                result: Err(IpcError::internal("fake bridge is offline")),
            });
            return;
        }
        if self.error_chance > 0
            && injectable_error(&op)
            && self.rng.chance(self.error_chance)
        {
            self.queue(Message::Reply {
                tag,
                result: Err(IpcError::internal("fake daemon refused the request")),
            });
            return;
        }
        self.dispatch(op, tag);
    }

    fn dispatch(&mut self, op: Op, tag: RequestTag) {
        let me = self.world.me.author_id();
        match op {
            Op::Identity => self.reply(
                tag,
                Reply::Identity {
                    author_id: me,
                    public_key_hex: "f0f0f0f0".to_string(),
                    peer_id: Some("12D3KooWfuzz".to_string()),
                },
            ),
            Op::Status => self.reply(
                tag,
                Reply::Status(DaemonStatusDto {
                    version: "fuzz-0.1".to_string(),
                    pid: 1,
                    data_dir: "/tmp/fuzz".to_string(),
                    socket: "/tmp/fuzz.sock".to_string(),
                    started_at: 1_700_000_000,
                    uptime_secs: 60,
                    author_id: me,
                    connected_guilds: vec![GUILD_A],
                    log_path: "/tmp/fuzz/daemon.log".to_string(),
                }),
            ),
            Op::ListGuilds => {
                let guilds = self.world.guild_list();
                self.reply(tag, Reply::Guilds(guilds));
            }
            Op::ChannelTree { guild_id } => {
                let tree = ChannelTreeDto {
                    categories: self.world.categories_for(guild_id),
                    channels: self.world.channels_for(guild_id),
                };
                self.reply(tag, Reply::ChannelTree(tree));
            }
            Op::ListChannels { guild_id } => {
                let channels = self
                    .world
                    .channels_for(guild_id)
                    .into_iter()
                    .map(|view| ChannelMeta {
                        id: view.id,
                        guild_id: view.guild_id,
                        name: view.name,
                        created_by: view.created_by,
                        created_at: view.created_at,
                    })
                    .collect();
                self.reply(tag, Reply::Channels(channels));
            }
            Op::ListMembers { guild_id } => {
                let members = self.world.members_for(guild_id);
                self.reply(tag, Reply::Members(members));
            }
            Op::UnreadCounts { guild_id } => {
                let counts = self.world.unread(guild_id);
                self.reply(tag, Reply::Unread(counts));
            }
            Op::ConnectionState { guild_id } => {
                let online = self.world.online;
                self.reply(
                    tag,
                    Reply::Connection(ConnectionStateDto {
                        guild_id,
                        connected: online,
                        peer_id: Some("12D3KooWfuzz".to_string()),
                        listen_addresses: vec!["/ip4/127.0.0.1/tcp/41000".to_string()],
                        external_addresses: vec![
                            "/ip4/203.0.113.9/tcp/41000".to_string(),
                        ],
                        peers: vec![PeerSnapshot {
                            peer: "12D3KooWpeer".to_string(),
                            author: Some(self.world.ada.author_id()),
                            connected: online,
                            addresses: vec![
                                "/ip4/198.51.100.2/tcp/41000".to_string(),
                            ],
                        }],
                    }),
                );
            }
            Op::NotificationSettings => {
                let settings = self.world.settings.clone();
                self.reply(tag, Reply::NotificationSettings(settings));
            }
            Op::SetDesktopNotifications { enabled } => {
                self.world.settings.desktop = enabled;
                self.reply(tag, Reply::Unit);
            }
            Op::SetDoNotDisturb { enabled } => {
                self.world.settings.dnd = enabled;
                self.reply(tag, Reply::Unit);
            }
            Op::SetQuietHours { quiet_hours } => {
                self.world.settings.quiet_hours = quiet_hours;
                self.reply(tag, Reply::Unit);
            }
            Op::SetChannelMuted { channel_id, muted } => {
                let list = &mut self.world.settings.muted_channels;
                list.retain(|id| *id != channel_id);
                if muted {
                    list.push(channel_id);
                }
                self.reply(tag, Reply::Unit);
            }
            Op::SetGuildMuted { guild_id, muted } => {
                let list = &mut self.world.settings.muted_guilds;
                list.retain(|id| *id != guild_id);
                if muted {
                    list.push(guild_id);
                }
                self.reply(tag, Reply::Unit);
            }
            Op::ListDirectMessages => {
                let direct = self.world.direct.clone();
                self.reply(tag, Reply::DirectMessages(direct));
            }
            Op::HistoryBefore {
                guild_id,
                channel_id,
                cursor,
                ..
            } => {
                let page = if cursor.is_some() {
                    EventPage {
                        events: Vec::new(),
                        more: false,
                    }
                } else {
                    let ada = self.world.ada.clone();
                    let bodies = self.world.history(guild_id, channel_id);
                    let events = bodies
                        .into_iter()
                        .map(|body| {
                            self.signed(
                                &ada,
                                guild_id,
                                Some(channel_id),
                                EventKind::Message {
                                    body: body.to_string(),
                                },
                            )
                        })
                        .collect();
                    EventPage { events, more: false }
                };
                self.reply(tag, Reply::HistoryPage(page));
            }
            Op::History {
                guild_id,
                channel_id,
                ..
            } => {
                let ada = self.world.ada.clone();
                let events = self
                    .world
                    .history(guild_id, channel_id)
                    .into_iter()
                    .map(|body| {
                        self.signed(
                            &ada,
                            guild_id,
                            Some(channel_id),
                            EventKind::Message {
                                body: body.to_string(),
                            },
                        )
                    })
                    .collect();
                self.reply(tag, Reply::Events(events));
            }
            Op::GetMember { guild_id, author } => {
                let profile = self.world.profile(guild_id, author);
                let delay = self.profile_delay;
                self.reply_later(tag, Reply::Member(profile), delay);
            }
            Op::OpenDirectMessage { .. } => {
                let dm = self.world.dm_guild.clone();
                let delay = self.dm_delay;
                self.reply_later(tag, Reply::GuildCreated(dm), delay);
            }
            Op::SetProfile {
                guild_id,
                about,
                pronouns,
                links,
            } => {
                self.set_profile_calls += 1;
                self.last_profile_links = links.clone();
                self.world.my_profile = (about.clone(), pronouns.clone(), links.clone());
                let me = self.world.me.clone();
                let event = self.signed(
                    &me,
                    guild_id,
                    None,
                    EventKind::MemberProfileUpdated {
                        about,
                        pronouns,
                        links,
                    },
                );
                self.reply(tag, Reply::Event(event));
            }
            Op::SetDisplayName { .. } => self.reply(tag, Reply::Unit),
            Op::CreateGuild { name } => {
                let id = GuildId([self.world.next_guild; 32]);
                self.world.next_guild = self.world.next_guild.wrapping_add(1);
                let meta = guild(id, &name, &self.world.me);
                let channel_id = ChannelId([self.world.next_channel; 16]);
                self.world.next_channel = self.world.next_channel.wrapping_add(1);
                self.world.channels.insert(
                    id,
                    vec![channel(channel_id, id, "general", &self.world.me)],
                );
                self.reply(tag, Reply::GuildCreated(meta));
            }
            Op::CreateChannel { guild_id, name } => {
                let meta = self.new_channel(guild_id, name, ChannelKind::Text, None);
                self.reply(tag, Reply::Channel(meta));
            }
            Op::CreateChannelIn {
                guild_id,
                name,
                kind,
                parent,
            } => {
                let meta = self.new_channel(guild_id, name, kind, parent);
                self.reply(tag, Reply::Channel(meta));
            }
            Op::CreateCategory { guild_id, name } => {
                let id = ChannelId([self.world.next_category; 16]);
                self.world.next_category = self.world.next_category.wrapping_add(1);
                let category = CategoryMeta {
                    id,
                    guild_id,
                    name,
                    created_by: self.world.me.author_id(),
                    created_at: 1_700_000_000,
                };
                self.world
                    .categories
                    .entry(guild_id)
                    .or_default()
                    .push(category.clone());
                self.reply(tag, Reply::Category(category));
            }
            Op::RenameChannel {
                guild_id,
                channel_id,
                name,
            } => {
                if let Some(channels) = self.world.channels.get_mut(&guild_id)
                    && let Some(item) = channels.iter_mut().find(|item| item.id == channel_id)
                {
                    item.name = name;
                }
                self.reply(tag, Reply::Unit);
            }
            Op::MoveChannel {
                guild_id,
                channel_id,
                parent,
            } => {
                if let Some(channels) = self.world.channels.get_mut(&guild_id)
                    && let Some(item) = channels.iter_mut().find(|item| item.id == channel_id)
                {
                    item.parent = parent;
                }
                self.reply(tag, Reply::Unit);
            }
            Op::JoinGuild { .. } => {
                let guild = self.world.commune_b.clone();
                self.reply(tag, Reply::Guild(Some(guild)));
            }
            Op::PostMessage {
                guild_id,
                channel_id,
                body,
            }
            | Op::PostMessageFull {
                guild_id,
                channel_id,
                body,
                ..
            }
            | Op::PostMessageRich {
                guild_id,
                channel_id,
                body,
                ..
            } => {
                self.sent_bodies.push(body.clone());
                let me = self.world.me.clone();
                let event = self.signed(
                    &me,
                    guild_id,
                    Some(channel_id),
                    EventKind::Message { body },
                );
                self.reply(tag, Reply::Event(event));
            }
            Op::CreateInvite {
                guild_id,
                peers,
                expires_in_secs,
            } => {
                let name = self
                    .world
                    .guild_list()
                    .into_iter()
                    .find(|guild| guild.id == guild_id)
                    .map(|guild| guild.name)
                    .unwrap_or_else(|| "created commune".to_string());
                let invite = Invite::new(guild_id, name, &self.world.me, peers, expires_in_secs)
                    .expect("the fake invite is signed by a test identity");
                self.reply(tag, Reply::Invite(invite));
            }
            Op::EditMessage { .. }
            | Op::DeleteMessage { .. }
            | Op::AddReaction { .. }
            | Op::RemoveReaction { .. }
            | Op::MarkRead { .. }
            | Op::SetTyping { .. }
            | Op::SetPresence { .. }
            | Op::ShutdownDaemon => self.reply(tag, Reply::Unit),
            Op::Search { .. } => self.reply(tag, Reply::Search(Vec::new())),
            Op::Connect { .. } => {
                self.world.online = true;
                self.reply(tag, Reply::Unit);
            }
            Op::Disconnect { .. } => {
                self.world.online = false;
                self.reply(tag, Reply::Unit);
            }
            _ => self.reply(tag, Reply::Unit),
        }
    }

    fn new_channel(
        &mut self,
        guild_id: GuildId,
        name: String,
        kind: ChannelKind,
        parent: Option<ChannelId>,
    ) -> ChannelMeta {
        let id = ChannelId([self.world.next_channel; 16]);
        self.world.next_channel = self.world.next_channel.wrapping_add(1);
        let mut view = channel(id, guild_id, &name, &self.world.me);
        view.kind = kind;
        view.parent = parent;
        self.world
            .channels
            .entry(guild_id)
            .or_default()
            .push(view);
        ChannelMeta {
            id,
            guild_id,
            name,
            created_by: self.world.me.author_id(),
            created_at: 1_700_000_000,
        }
    }

    fn signed(
        &mut self,
        author: &Identity,
        guild_id: GuildId,
        channel_id: Option<ChannelId>,
        kind: EventKind,
    ) -> SignedEvent {
        let seq = self.world.seq;
        self.world.seq += 1;
        author
            .sign_body(EventBody {
                version: 2,
                guild_id,
                channel_id,
                author: author.author_id(),
                seq,
                lamport: seq,
                timestamp: 1_700_000_000 + seq * 60,
                prev: None,
                kind,
            })
            .expect("sign a fuzz event")
    }

    fn reply(&mut self, tag: RequestTag, reply: Reply) {
        self.inbox.push_back(Message::Reply {
            tag,
            result: Ok(reply),
        });
    }

    fn reply_later(&mut self, tag: RequestTag, reply: Reply, delay: usize) {
        if delay == 0 {
            self.reply(tag, reply);
        } else {
            let at = self.frame + delay;
            self.delayed.push_back((
                at,
                Message::Reply {
                    tag,
                    result: Ok(reply),
                },
            ));
        }
    }

    fn stage_attachment(&mut self, path: PathBuf) {
        self.staged_calls += 1;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file")
            .to_string();
        let bytes = b"fuzz attachment".to_vec();
        if self.error_chance > 0 && self.rng.chance(10) {
            self.queue(Message::AttachmentStaged {
                path,
                result: Err("the fake disk refused the read".to_string()),
            });
            return;
        }
        let attachment = Attachment {
            hash: ovvc_core::hash_bytes(&bytes),
            size: bytes.len() as u64,
            mime: "text/plain".to_string(),
            name,
        };
        self.queue(Message::AttachmentStaged {
            path: path.clone(),
            result: Ok(StagedAttachment {
                path,
                attachment,
                bytes,
            }),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn send_rich(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: String,
        reply_to: Option<EventId>,
        mentions: Vec<AuthorId>,
        attachments: Vec<StagedAttachment>,
        send_id: u64,
    ) {
        let op = Op::PostMessageRich {
            guild_id,
            channel_id,
            body,
            reply_to,
            mentions,
            attachments: attachments
                .into_iter()
                .map(|staged| staged.attachment)
                .collect(),
        };
        self.send(op, RequestTag::SendRichMessage(send_id));
    }

    fn save_attachment(&mut self, _guild_id: GuildId, attachment: Attachment) {
        // Never hand back a real path: the state layer opens it, and a headless
        // test must not launch an external viewer.
        self.queue(Message::AttachmentSaved {
            name: attachment.name,
            result: Err("the fake cache has no blob".to_string()),
        });
    }
}

fn injectable_error(op: &Op) -> bool {
    matches!(
        op,
        Op::SetDisplayName { .. }
            | Op::SetProfile { .. }
            | Op::OpenDirectMessage { .. }
            | Op::PostMessage { .. }
            | Op::PostMessageFull { .. }
            | Op::PostMessageRich { .. }
            | Op::EditMessage { .. }
            | Op::DeleteMessage { .. }
            | Op::AddReaction { .. }
            | Op::RemoveReaction { .. }
            | Op::MarkRead { .. }
            | Op::SetDesktopNotifications { .. }
            | Op::SetDoNotDisturb { .. }
            | Op::SetQuietHours { .. }
            | Op::SetChannelMuted { .. }
            | Op::SetGuildMuted { .. }
            | Op::SetTyping { .. }
            | Op::SetPresence { .. }
            | Op::CreateGuild { .. }
            | Op::CreateChannel { .. }
            | Op::CreateChannelIn { .. }
            | Op::CreateCategory { .. }
            | Op::RenameChannel { .. }
            | Op::MoveChannel { .. }
            | Op::JoinGuild { .. }
            | Op::Connect { .. }
            | Op::Disconnect { .. }
    )
}

// --- harness ----------------------------------------------------------------

#[derive(Clone, Debug)]
struct Target {
    rect: Rect,
    role: Role,
    label: String,
    focused: bool,
    value: Option<String>,
}

impl Target {
    fn is_text(&self) -> bool {
        matches!(self.role, Role::TextInput | Role::MultilineTextInput)
    }

    fn describe(&self) -> String {
        let label = if self.label.is_empty() {
            self.value.clone().unwrap_or_else(|| "<unlabelled>".to_string())
        } else {
            self.label.clone()
        };
        format!("{:?} {label:?} at {:?}", self.role, self.rect.center())
    }
}

struct FuzzHarness {
    harness: Harness<'static, App>,
    fake: Rc<RefCell<Fake>>,
    screen: Rect,
    log: ActionLog,
    seed: u64,
    frames: usize,
    modal_frames: usize,
    clicks: usize,
    texts: usize,
    keys: usize,
    escapes: usize,
}

fn app_frame(ui: &mut egui::Ui, app: &mut App) {
    app.frame_for_test(ui);
}

impl FuzzHarness {
    fn new(seed: u64) -> Self {
        let mut state = AppState::new();
        state.wizard_done = true;
        Self::with_state(seed, state)
    }

    fn with_state(seed: u64, state: AppState) -> Self {
        let fake = Rc::new(RefCell::new(Fake::new(seed)));
        let app = App::with_backend(
            state,
            Box::new(FakeBackend {
                inner: fake.clone(),
            }),
        );
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(SCREEN[0], SCREEN[1]));
        let harness: Harness<'static, App> = Harness::builder()
            .with_size(screen.size())
            .with_context_setup(crate::theme::install)
            .with_max_steps(4)
            .build_ui_state(app_frame, app);
        let mut this = Self {
            harness,
            fake,
            screen,
            log: ActionLog::new(260),
            seed,
            frames: 0,
            modal_frames: 0,
            clicks: 0,
            texts: 0,
            keys: 0,
            escapes: 0,
        };
        for _ in 0..40 {
            let ready = {
                let state = &this.harness.state().state;
                state.connected
                    && state.selected_guild.is_some()
                    && !state.channels.is_empty()
                    && state.loaded_channel.is_some()
            };
            if ready {
                return this;
            }
            this.step();
        }
        this.fail("the fake backend never reached a loaded channel".to_string());
    }

    fn state(&self) -> &AppState {
        &self.harness.state().state
    }

    fn fail(&self, problem: String) -> ! {
        eprintln!("\n=== fuzz failure ===");
        eprintln!("seed: {}", self.seed);
        eprintln!("frame: {}", self.frames);
        eprintln!(
            "reproduce: FUZZ_SEED={} FUZZ_ITERS={} cargo test -p ovvc-gui fuzz:: -- --nocapture",
            self.seed,
            self.frames.max(1)
        );
        eprintln!("state: {}", state_summary(self.state()));
        eprintln!("last interactions:");
        for (index, line) in self.log.entries.iter().enumerate() {
            eprintln!("  [{index:03}] {line}");
        }
        panic!("fuzz seed {} frame {}: {problem}", self.seed, self.frames);
    }

    fn step(&mut self) {
        self.harness.input_mut().screen_rect = Some(self.screen);
        let outcome = catch_unwind(AssertUnwindSafe(|| self.harness.step()));
        if let Err(payload) = outcome {
            self.fail(format!("panicked while rendering: {}", panic_message(payload)));
        }
        self.frames += 1;
        if self.harness.output().shapes.is_empty() {
            self.fail("the frame painted nothing".to_string());
        }
        let backend_failure = self.fake.borrow_mut().take_failure();
        if let Some(problem) = backend_failure {
            self.fail(problem);
        }
        let problem = {
            let state = &self.harness.state().state;
            check_invariants(state, &mut self.modal_frames)
        };
        if let Some(problem) = problem {
            self.fail(problem);
        }
    }

    fn targets(&self) -> Vec<Target> {
        let screen = self.screen;
        self.harness
            .query_all_by(|node| {
                if node.is_disabled() {
                    return false;
                }
                if node.bounding_box().is_none() {
                    return false;
                }
                node.data().supports_action(AccessAction::Click)
                    || matches!(
                        node.role(),
                        Role::TextInput | Role::MultilineTextInput
                    )
            })
            .filter_map(|node| {
                let rect = node.rect();
                if rect.width() < 1.0 || rect.height() < 1.0 {
                    return None;
                }
                if !screen.contains(rect.center()) {
                    return None;
                }
                let access = node.accesskit_node();
                Some(Target {
                    rect,
                    role: access.role(),
                    label: access.label().unwrap_or_default(),
                    focused: access.is_focused(),
                    value: access.value(),
                })
            })
            .collect()
    }

    fn click_targets(&self) -> Vec<Target> {
        let mut targets: Vec<Target> = self
            .targets()
            .into_iter()
            .filter(|target| {
                target.role != Role::TextInput && target.role != Role::MultilineTextInput
            })
            .collect();
        targets.sort_by(|a, b| {
            a.rect
                .top()
                .total_cmp(&b.rect.top())
                .then(a.rect.left().total_cmp(&b.rect.left()))
        });
        targets
    }

    fn text_targets(&self) -> Vec<Target> {
        let mut targets: Vec<Target> = self
            .targets()
            .into_iter()
            .filter(Target::is_text)
            .collect();
        targets.sort_by(|a, b| {
            a.rect
                .top()
                .total_cmp(&b.rect.top())
                .then(a.rect.left().total_cmp(&b.rect.left()))
        });
        targets
    }

    fn click_at(&mut self, pos: Pos2) {
        let events = self.harness.input_mut();
        events.events.push(Event::PointerMoved(pos));
        for pressed in [true, false] {
            events.events.push(Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::default(),
            });
        }
    }

    fn click_target(&mut self, target: &Target) {
        self.click_at(target.rect.center());
        self.clicks += 1;
    }

    /// Press, move and release over three frames, which is what egui needs to
    /// see a window title bar dragged rather than a click.
    fn drag(&mut self, from: Pos2, to: Pos2) {
        {
            let events = self.harness.input_mut();
            events.events.push(Event::PointerMoved(from));
            events.events.push(Event::PointerButton {
                pos: from,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::default(),
            });
        }
        self.step();
        self.harness.input_mut().events.push(Event::PointerMoved(to));
        self.step();
        self.harness.input_mut().events.push(Event::PointerButton {
            pos: to,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::default(),
        });
        self.step();
    }

    fn click_label(&mut self, label: &str) -> bool {
        let targets = self.targets();
        // An exact match first, so clicking `Ada` picks the member button and
        // not the sidebar's `direct message with Ada` row.
        let target = targets
            .iter()
            .find(|target| target.label == label)
            .or_else(|| targets.iter().find(|target| target.label.contains(label)))
            .cloned();
        match target {
            Some(target) => {
                self.click_target(&target);
                self.log.push(format!(
                    "frame {}: click {label:?} at {:?}",
                    self.frames,
                    target.rect.center()
                ));
                true
            }
            None => false,
        }
    }

    fn click_label_or_fail(&mut self, label: &str) {
        if !self.click_label(label) {
            let labels: Vec<String> = self
                .targets()
                .into_iter()
                .filter(|target| !target.label.is_empty())
                .map(|target| target.label)
                .collect();
            self.fail(format!(
                "no clickable target containing {label:?}; visible labels: {labels:?}"
            ));
        }
        // One frame consumes the click and queues the action, the next draws
        // whatever the action opened.
        self.step();
        self.step();
    }

    fn press(&mut self, key: Key) {
        let events = self.harness.input_mut();
        for pressed in [true, false] {
            events.events.push(Event::Key {
                key,
                pressed,
                modifiers: Modifiers::default(),
                repeat: false,
                physical_key: None,
            });
        }
        self.keys += 1;
    }

    fn press_escape(&mut self) {
        self.press(Key::Escape);
        self.escapes += 1;
    }

    fn type_text(&mut self, text: &str) {
        self.harness
            .input_mut()
            .events
            .push(Event::Text(text.to_string()));
        self.texts += 1;
    }

    fn select_all_and_delete(&mut self) {
        let events = self.harness.input_mut();
        events
            .events
            .push(Event::ModifiersChanged(Modifiers::COMMAND));
        for pressed in [true, false] {
            events.events.push(Event::Key {
                key: Key::A,
                pressed,
                modifiers: Modifiers::COMMAND,
                repeat: false,
                physical_key: None,
            });
        }
        events
            .events
            .push(Event::ModifiersChanged(Modifiers::default()));
        for pressed in [true, false] {
            events.events.push(Event::Key {
                key: Key::Backspace,
                pressed,
                modifiers: Modifiers::default(),
                repeat: false,
                physical_key: None,
            });
        }
        self.keys += 1;
    }

    fn wait_for(&mut self, what: &str, mut ready: impl FnMut(&AppState) -> bool) {
        for _ in 0..60 {
            // Step first, so the tree the next interaction reads has drawn
            // whatever changed at the end of the previous frame.
            self.step();
            if ready(self.state()) {
                return;
            }
        }
        self.fail(format!(
            "timed out waiting for {what}; state: {}",
            state_summary(self.state())
        ));
    }

    fn open_profile(&mut self, member_name: &str) {
        self.click_label_or_fail(member_name);
        self.wait_for("the profile dialog", |state| {
            state.dialog == Dialog::Profile && state.profile_author.is_some()
        });
    }

    /// Every AccessKit label and value in the frame. This is the text an
    /// assistive technology, and the harness, can read off the widgets.
    fn visible_texts(&self) -> Vec<String> {
        let mut texts = Vec::new();
        for node in self.harness.query_all_by(|_node| true) {
            let access = node.accesskit_node();
            if let Some(label) = access.label()
                && !label.is_empty()
            {
                texts.push(label);
            }
            if let Some(value) = access.value()
                && !value.is_empty()
            {
                texts.push(value);
            }
        }
        texts
    }

    /// The first node, clickable or not, with this exact AccessKit label.
    fn node_with_label(&self, label: &str) -> Option<Target> {
        self.harness
            .query_all_by(|_node| true)
            .filter(|node| {
                let access = node.accesskit_node();
                // AccessKit keeps the text of a Role::Label node in `value`.
                let text = if access.role() == Role::Label {
                    access.value()
                } else {
                    access.label()
                };
                access.bounding_box().is_some() && text.as_deref() == Some(label)
            })
            .map(|node| {
                let access = node.accesskit_node();
                Target {
                    rect: node.rect(),
                    role: access.role(),
                    label: label.to_string(),
                    focused: access.is_focused(),
                    value: access.value(),
                }
            })
            .next()
    }

    /// Fail when the frame exposes the word "guild" as visible text.
    ///
    /// The daemon's `guild mismatch` protocol error is the one known exception;
    /// it is a wire-level message, not a label for the commune concept.
    fn assert_no_guild(&self, context: &str) {
        let texts = self.visible_texts();
        assert!(!texts.is_empty(), "{context}: the frame exposed no text");
        let hits: Vec<String> = texts
            .into_iter()
            .filter(|text| text.to_lowercase().contains("guild"))
            .filter(|text| !text.trim().eq_ignore_ascii_case("guild mismatch"))
            .collect();
        assert!(
            hits.is_empty(),
            "{context}: user-visible text mentions guild: {hits:?}"
        );
    }

    /// Run the random interaction loop for `frames` bounded steps.
    fn run_random(&mut self, rng: &mut Rng, frames: usize) -> (usize, usize, usize) {
        let mut reconnect_at: Option<usize> = None;
        for frame in 0..frames {
            if frame == 150 {
                self.fake.borrow_mut().disconnect();
                self.log.push(format!("frame {frame}: fake bridge disconnect"));
                reconnect_at = Some(frame + 8);
            }
            if reconnect_at == Some(frame) {
                self.fake.borrow_mut().reconnect();
                self.log.push(format!("frame {frame}: fake bridge reconnect"));
            }

            if frame % 37 == 0 {
                let before = self.state().dialog;
                self.log
                    .push(format!("frame {frame}: press Escape (dialog {before:?})"));
                self.press_escape();
                self.step();
                let after = self.state().dialog;
                if before != Dialog::None && after != Dialog::None {
                    self.fail(format!(
                        "Escape did not close the {:?} dialog",
                        before
                    ));
                }
                continue;
            }

            let roll = rng.below(100);
            match roll {
                0..=44 => {
                    let targets = self.click_targets();
                    if targets.is_empty() {
                        self.log.push(format!("frame {frame}: no click target"));
                    } else {
                        let index = rng.below(targets.len());
                        let target = targets[index].clone();
                        self.log
                            .push(format!("frame {frame}: click {}", target.describe()));
                        self.click_target(&target);
                    }
                }
                45..=64 => {
                    let focused = self
                        .targets()
                        .into_iter()
                        .find(|target| target.focused && target.is_text());
                    match focused {
                        Some(target) => {
                            let words = if target.role == Role::MultilineTextInput {
                                MULTILINE_WORDS
                            } else {
                                SINGLELINE_WORDS
                            };
                            let word = words[rng.below(words.len())];
                            self.log
                                .push(format!("frame {frame}: type {word:?} into {:?}", target.role));
                            self.type_text(word);
                        }
                        None => {
                            let inputs = self.text_targets();
                            if let Some(target) = inputs.get(rng.below(inputs.len().max(1))).cloned()
                            {
                                self.log
                                    .push(format!("frame {frame}: focus {}", target.describe()));
                                self.click_target(&target);
                            }
                        }
                    }
                }
                65..=77 => {
                    let key = KEYS[rng.below(KEYS.len())];
                    self.log.push(format!("frame {frame}: press {key:?}"));
                    self.press(key);
                }
                78..=87 => {
                    let pos = pos2(
                        rng.below(self.screen.width() as usize) as f32,
                        rng.below(self.screen.height() as usize) as f32,
                    );
                    self.log
                        .push(format!("frame {frame}: click empty space at {pos:?}"));
                    self.click_at(pos);
                    self.clicks += 1;
                }
                88..=95 => {
                    self.inject_event(rng);
                }
                _ => {
                    // An idle frame: notices expire and relative times move.
                }
            }
            self.step();
        }
        (self.clicks, self.texts, self.keys)
    }

    fn inject_event(&mut self, rng: &mut Rng) {
        let mut fake = self.fake.borrow_mut();
        let ada = fake.world.ada.author_id();
        let guild = fake.world.known_guild();
        match rng.below(6) {
            0 => {
                fake.queue(Message::Event(DaemonEvent::Typing {
                    guild_id: guild,
                    channel_id: GENERAL_A,
                    author: ada,
                    until_ms: ovvc_control::now_ms() + 6_000,
                }));
                self.log
                    .push(format!("frame {}: backend typing event", self.frames));
            }
            1 => {
                fake.queue(Message::Event(DaemonEvent::Presence {
                    guild_id: guild,
                    author: ada,
                    status: PresenceStatus::Away,
                }));
                self.log
                    .push(format!("frame {}: backend presence event", self.frames));
            }
            2 => {
                let author = fake.world.me.clone();
                let event = fake.signed(
                    &author,
                    guild,
                    Some(GENERAL_A),
                    EventKind::Message {
                        body: "an event pushed by the fuzzer".to_string(),
                    },
                );
                fake.queue(Message::Event(DaemonEvent::Event {
                    guild_id: guild,
                    event,
                    local: false,
                }));
                self.log
                    .push(format!("frame {}: backend message event", self.frames));
            }
            3 => {
                fake.queue(Message::Event(DaemonEvent::Error {
                    guild_id: None,
                    message: "the fake daemon reported an error".to_string(),
                }));
                self.log
                    .push(format!("frame {}: backend error event", self.frames));
            }
            4 => {
                fake.queue(Message::Event(DaemonEvent::PeerConnected {
                    guild_id: guild,
                    peer: "12D3KooWnew".to_string(),
                    author: Some(ada),
                }));
                self.log
                    .push(format!("frame {}: backend peer event", self.frames));
            }
            _ => {
                fake.queue(Message::BridgeError(
                    "the fake bridge hiccuped".to_string(),
                ));
                self.log
                    .push(format!("frame {}: backend bridge error", self.frames));
            }
        }
    }
}

const SINGLELINE_WORDS: &[&str] = &[
    "hello",
    "https://example.com/fuzz",
    "not a link",
    "Ada",
    "/tmp/fuzz.txt",
    "general",
];

const MULTILINE_WORDS: &[&str] = &[
    "hello from the fuzz harness",
    "https://example.com/fuzz",
    "not a link",
    "**bold** and @Ada",
    "```\nfn main() {}\n```",
    "one line\nthen another",
];

const KEYS: &[Key] = &[
    Key::Enter,
    Key::Escape,
    Key::Tab,
    Key::Backspace,
    Key::ArrowUp,
    Key::ArrowDown,
    Key::ArrowLeft,
    Key::ArrowRight,
    Key::Home,
    Key::End,
    Key::Delete,
];

fn state_summary(state: &AppState) -> String {
    format!(
        "connected={} guild={:?} channel={:?} dialog={:?} profile={:?}/{:?} \
         editing={} saving={} dm_opening={:?} error={:?} notice={:?}",
        state.connected,
        state.selected_guild,
        state.selected_channel,
        state.dialog,
        state.profile_guild,
        state.profile_author,
        state.profile_editing,
        state.profile_saving,
        state.dm_opening,
        state
            .error
            .as_ref()
            .map(|text| text.chars().take(160).collect::<String>()),
        state
            .notice
            .as_ref()
            .map(|text| text.chars().take(160).collect::<String>()),
    )
}

fn check_invariants(state: &AppState, modal_frames: &mut usize) -> Option<String> {
    if let Some(guild_id) = state.selected_guild
        && !state.guilds.iter().any(|guild| guild.id == guild_id)
    {
        return Some(format!(
            "selected guild {guild_id:?} is not in the guild list ({:?})",
            state
                .guilds
                .iter()
                .map(|guild| guild.id)
                .collect::<Vec<_>>()
        ));
    }
    if let Some(channel_id) = state.selected_channel {
        let Some(guild_id) = state.selected_guild else {
            return Some(format!(
                "channel {channel_id:?} is selected with no guild"
            ));
        };
        match state
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)
        {
            Some(channel) if channel.guild_id == guild_id => {}
            Some(channel) => {
                return Some(format!(
                    "selected channel {channel_id:?} belongs to {:?}, not {guild_id:?}",
                    channel.guild_id
                ));
            }
            None => {
                return Some(format!(
                    "selected channel {channel_id:?} is not in the sidebar"
                ));
            }
        }
    }
    if state.dialog == Dialog::Profile
        && (state.profile_guild.is_none() || state.profile_author.is_none())
    {
        return Some(format!(
            "the profile dialog is open without a target ({:?}/{:?})",
            state.profile_guild, state.profile_author
        ));
    }
    if state.dialog != Dialog::Profile
        && (state.profile_guild.is_some() || state.profile_author.is_some())
    {
        return Some(format!(
            "a profile target outlived its dialog ({:?}/{:?})",
            state.profile_guild, state.profile_author
        ));
    }
    if let Some(error) = &state.error
        && error.len() > MAX_STRIP
    {
        return Some(format!(
            "the error strip grew to {} bytes",
            error.len()
        ));
    }
    if let Some(notice) = &state.notice
        && notice.len() > MAX_STRIP
    {
        return Some(format!(
            "the notice strip grew to {} bytes",
            notice.len()
        ));
    }
    if state.dialog != Dialog::None {
        *modal_frames += 1;
        if *modal_frames > STUCK_MODAL_FRAMES {
            return Some(format!(
                "the {:?} dialog stayed open for {} frames",
                state.dialog, *modal_frames
            ));
        }
    } else {
        *modal_frames = 0;
    }
    None
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
}

// --- random fuzz ------------------------------------------------------------

#[test]
fn fuzzed_interactions() {
    let frames = env_usize("FUZZ_ITERS", 500);
    let seeds: Vec<u64> = match env_u64("FUZZ_SEED") {
        Some(seed) => vec![seed],
        None => {
            let count = env_usize("FUZZ_SEEDS", 3);
            (0..count as u64).collect()
        }
    };
    let mut total_frames = 0;
    let mut total_clicks = 0;
    let mut total_texts = 0;
    let mut total_keys = 0;
    for seed in seeds {
        let mut run = FuzzHarness::new(seed);
        let mut rng = Rng::new(seed);
        let (clicks, texts, keys) = run.run_random(&mut rng, frames);
        eprintln!(
            "fuzz seed={seed} frames={} clicks={clicks} texts={texts} keys={keys} \
             escapes={}",
            run.frames, run.escapes
        );
        total_frames += run.frames;
        total_clicks += clicks;
        total_texts += texts;
        total_keys += keys;
    }
    eprintln!(
        "fuzz total frames={total_frames} clicks={total_clicks} texts={total_texts} \
         keys={total_keys}"
    );
}

// --- scripted edge cases ----------------------------------------------------

#[test]
fn escape_closes_every_dialog() {
    let mut run = FuzzHarness::new(41);
    run.fake.borrow_mut().error_chance = 0;
    let dialogs = [
        Dialog::CreateGuild,
        Dialog::CreateChannel,
        Dialog::CreateCategory,
        Dialog::Join,
        Dialog::Invite,
        Dialog::ChannelSettings,
    ];
    for dialog in dialogs {
        run.harness.state_mut().state.dialog = dialog;
        run.step();
        run.log
            .push(format!("open {dialog:?} directly and press Escape"));
        run.press_escape();
        run.step();
        assert_eq!(
            run.state().dialog,
            Dialog::None,
            "Escape did not close {dialog:?}"
        );
    }

    // The profile dialog goes through `close_profile`, so the target is
    // cleared as well as the window.
    run.harness
        .state_mut()
        .state
        .open_profile(GUILD_A, run.fake.borrow().world.me.author_id());
    run.step();
    run.press_escape();
    run.step();
    assert_eq!(run.state().dialog, Dialog::None);
    assert!(run.state().profile_author.is_none());
    assert!(run.state().profile_guild.is_none());
}

#[test]
fn profile_edit_rejects_an_invalid_link_and_saves_a_valid_one() {
    let mut run = FuzzHarness::new(42);
    run.fake.borrow_mut().error_chance = 0;
    run.fake.borrow_mut().profile_delay = 0;
    run.open_profile("Conleth (you)");
    run.wait_for("the fetched profile", |state| state.profile.is_some());

    run.click_label_or_fail("[ edit profile ]");
    assert!(run.state().profile_editing);

    // The links field is the second multiline input in the profile window.
    let links = run
        .text_targets()
        .into_iter()
        .filter(|target| target.role == Role::MultilineTextInput)
        .nth(1)
        .expect("the profile links field");
    run.click_target(&links);
    run.log.push("focus the links field".to_string());
    run.step();

    run.select_all_and_delete();
    run.step();
    run.type_text("not a link");
    run.step();
    run.click_label_or_fail("[ save ]");
    assert!(run.state().profile_editing, "an invalid draft must stay editable");
    assert_eq!(run.fake.borrow().set_profile_calls, 0);
    assert!(
        run.state()
            .error
            .as_deref()
            .is_some_and(|error| error.contains("http(s)")),
        "state: {}",
        state_summary(run.state())
    );

    // Clear the bad line and save a valid one.
    run.click_target(&links);
    run.step();
    run.select_all_and_delete();
    run.step();
    run.type_text("https://example.com/fuzz");
    run.step();
    run.click_label_or_fail("[ save ]");
    assert_eq!(run.fake.borrow().set_profile_calls, 1);
    assert_eq!(
        run.fake.borrow().last_profile_links,
        vec!["https://example.com/fuzz".to_string()]
    );
    run.wait_for("the save reply", |state| {
        !state.profile_editing && !state.profile_saving
    });
    assert_eq!(run.state().notice.as_deref(), Some("profile saved"));
    run.wait_for("the refetched profile", |state| {
        state
            .profile
            .as_ref()
            .is_some_and(|profile| profile.links == vec!["https://example.com/fuzz".to_string()])
    });

    run.press_escape();
    run.step();
    assert_eq!(run.state().dialog, Dialog::None);
    assert!(run.state().profile_author.is_none());
}

#[test]
fn a_late_profile_reply_after_escape_is_dropped() {
    let mut run = FuzzHarness::new(43);
    run.fake.borrow_mut().error_chance = 0;
    run.fake.borrow_mut().profile_delay = 8;
    run.open_profile("Ada");
    // The click frame marks the profile stale; the next frame asks for it.
    run.step();
    assert!(run.state().profile_loading);
    run.press_escape();
    run.step();
    assert_eq!(run.state().dialog, Dialog::None);
    assert!(run.state().profile_author.is_none());
    for _ in 0..12 {
        run.step();
    }
    assert_eq!(run.state().dialog, Dialog::None);
    assert!(run.state().profile.is_none());
    assert!(run.state().profile_author.is_none());
}

#[test]
fn switching_commune_closes_the_profile_and_its_draft() {
    let mut run = FuzzHarness::new(44);
    run.fake.borrow_mut().error_chance = 0;
    run.fake.borrow_mut().profile_delay = 0;
    run.open_profile("Conleth (you)");
    run.wait_for("the fetched profile", |state| state.profile.is_some());
    run.click_label_or_fail("[ edit profile ]");
    run.type_text("a draft that will be thrown away");
    run.step();
    assert!(run.state().profile_editing);

    // The profile window opens over the commune tabs, so drag it down by its
    // title bar before reaching for the tab behind it. The close button marks
    // the title bar's right edge.
    let close = run
        .targets()
        .into_iter()
        .find(|target| target.label == "Close window")
        .expect("the profile window close button");
    let title = pos2(close.rect.left() - 200.0, close.rect.center().y);
    let after = pos2(title.x, title.y + 380.0);
    run.log.push(format!("drag the profile window {title:?} -> {after:?}"));
    run.drag(title, after);
    run.click_label_or_fail("[2] Second Commune");
    assert_eq!(run.state().selected_guild, Some(GUILD_B));
    assert_eq!(run.state().dialog, Dialog::None);
    assert!(run.state().profile_author.is_none());
    assert!(!run.state().profile_editing);
    run.wait_for("the second commune's channels", |state| {
        state.channels.iter().any(|channel| channel.id == GENERAL_B)
    });
    run.wait_for("the second commune's members", |state| {
        !state.members.is_empty()
    });
}

#[test]
fn opens_a_dm_from_a_profile_selects_it_and_sends_a_message() {
    let mut run = FuzzHarness::new(45);
    run.fake.borrow_mut().error_chance = 0;
    run.fake.borrow_mut().profile_delay = 0;
    run.fake.borrow_mut().dm_delay = 1;
    run.open_profile("Ada");
    run.wait_for("the fetched profile", |state| state.profile.is_some());
    run.click_label_or_fail("[ message ]");
    run.wait_for("the direct conversation", |state| {
        state.selected_guild == Some(DM_GUILD)
    });
    assert_eq!(run.state().dialog, Dialog::None);

    // Go back to the commune, then select the conversation from the sidebar.
    run.click_label_or_fail("[1] The Guild");
    run.wait_for("the commune channel", |state| {
        state.selected_guild == Some(GUILD_A) && state.selected_channel.is_some()
    });
    run.click_label_or_fail("direct message with Ada");
    assert_eq!(run.state().selected_guild, Some(DM_GUILD));
    run.wait_for("the DM channel", |state| {
        state.selected_channel == Some(DM_CHANNEL)
    });

    let composer = run
        .text_targets()
        .into_iter()
        .find(|target| target.role == Role::MultilineTextInput)
        .expect("the composer");
    run.click_target(&composer);
    run.step();
    run.type_text("hello from the fuzz harness");
    run.step();
    run.press(Key::Enter);
    run.step();
    assert!(
        run.fake
            .borrow()
            .sent_bodies
            .iter()
            .any(|body| body == "hello from the fuzz harness"),
        "sent bodies: {:?}",
        run.fake.borrow().sent_bodies
    );
    assert!(run.state().composer.is_empty());
}

#[test]
fn a_late_dm_reply_after_escape_does_not_open_the_conversation() {
    let mut run = FuzzHarness::new(46);
    run.fake.borrow_mut().error_chance = 0;
    run.fake.borrow_mut().profile_delay = 0;
    run.fake.borrow_mut().dm_delay = 8;
    run.open_profile("Ada");
    run.wait_for("the fetched profile", |state| state.profile.is_some());
    let before = run.state().selected_guild;
    run.click_label_or_fail("[ message ]");
    run.press_escape();
    run.step();
    assert_eq!(run.state().dialog, Dialog::None);
    assert!(run.state().dm_opening.is_none());
    for _ in 0..12 {
        run.step();
    }
    assert_eq!(run.state().dialog, Dialog::None);
    assert_eq!(run.state().selected_guild, before);
}

#[test]
fn the_window_close_button_drops_a_late_profile_reply() {
    let mut run = FuzzHarness::new(49);
    run.fake.borrow_mut().error_chance = 0;
    run.fake.borrow_mut().profile_delay = 8;
    run.open_profile("Ada");
    // The read is in flight when the title bar's X is pressed.
    assert!(run.state().profile_loading || run.state().profile_stale);
    run.click_label_or_fail("Close window");
    assert_eq!(run.state().dialog, Dialog::None);
    assert!(run.state().profile_author.is_none());
    assert!(run.state().profile_guild.is_none());
    for _ in 0..12 {
        run.step();
    }
    assert_eq!(run.state().dialog, Dialog::None);
    assert!(run.state().profile.is_none());
}

#[test]
fn reselecting_the_open_conversation_asks_for_one_page_at_a_time() {
    let mut run = FuzzHarness::new(50);
    run.fake.borrow_mut().error_chance = 0;
    run.fake.borrow_mut().dm_delay = 0;
    run.click_label_or_fail("direct message with Ada");
    run.wait_for("the direct conversation's channel", |state| {
        state.selected_guild == Some(DM_GUILD) && state.selected_channel == Some(DM_CHANNEL)
    });

    // Selecting the commune that is already on screen, then the channel that
    // is already open, used to reset the history bookkeeping after the frame's
    // sync and send the same page twice in one frame. The clicks are injected
    // into consecutive frames with no settle frame between them, which is the
    // order the random loop produced.
    let dm_row = run
        .targets()
        .into_iter()
        .find(|target| target.label == "direct message with Ada")
        .expect("the DM row");
    run.click_target(&dm_row);
    run.step();

    let dm_channel = run
        .targets()
        .into_iter()
        .find(|target| target.label == "# dm (text)")
        .expect("the open DM channel");
    run.click_target(&dm_channel);
    run.step();
    // The next frame's poll checks what the previous frame asked for.
    run.step();

    assert_eq!(run.state().selected_guild, Some(DM_GUILD));
    assert_eq!(run.state().selected_channel, Some(DM_CHANNEL));
}

#[test]
fn reconnect_after_an_offline_settings_write_asks_for_settings_once() {
    let mut run = FuzzHarness::new(47);
    run.fake.borrow_mut().error_chance = 0;
    run.click_label_or_fail("[T]");
    assert_eq!(run.state().screen, ScreenView::Settings);
    assert_eq!(run.state().settings_tab, SettingsTab::Preferences);
    run.click_label_or_fail("[U] user");
    assert_eq!(run.state().settings_tab, SettingsTab::User);
    // Notifications are an advanced row, so the harness asks for that level
    // before it exercises the setting write.
    run.harness.state_mut().state.detail_level = DetailLevel::Advanced;
    run.step();

    // Take the bridge down, then change a setting. The refused write leaves
    // `settings_stale` set because `sync_requests` returns early while
    // disconnected. On reconnect that must still produce one settings read.
    run.fake.borrow_mut().disconnect();
    run.step();
    assert!(!run.state().connected);
    run.click_label_or_fail("do not disturb");
    run.step();
    assert!(run.state().settings_stale);
    assert!(
        run.state()
            .error
            .as_deref()
            .is_some_and(|error| error.contains("offline")),
        "state: {}",
        state_summary(run.state())
    );

    run.fake.borrow_mut().reconnect();
    run.step();
    run.step();
    assert!(run.state().connected);
    assert!(!run.state().settings_stale);
}

// --- new terminal surfaces ---------------------------------------------------

#[test]
fn clicking_a_member_opens_their_profile_with_the_published_fields() {
    let mut run = FuzzHarness::new(61);
    run.fake.borrow_mut().error_chance = 0;
    run.fake.borrow_mut().profile_delay = 0;
    let ada = run.fake.borrow().world.ada.author_id();

    run.open_profile("Ada");
    run.wait_for("Ada's profile", |state| state.profile.is_some());

    assert_eq!(run.state().dialog, Dialog::Profile);
    assert_eq!(run.state().profile_guild, Some(GUILD_A));
    assert_eq!(run.state().profile_author, Some(ada));

    let texts = run.visible_texts();
    for needle in [
        "Ada",
        "builds small tools and reads logs",
        "she/her",
        "https://example.com/ada",
        "https://example.com/notes",
    ] {
        assert!(
            texts.iter().any(|text| text.contains(needle)),
            "the profile dialog did not show {needle:?}; text: {texts:?}"
        );
    }
    assert!(
        !texts.iter().any(|text| text == "Ada (you)"),
        "another member was marked as you: {texts:?}"
    );
}

#[test]
fn your_own_profile_is_marked_you_and_shows_the_published_fields() {
    let mut run = FuzzHarness::new(62);
    run.fake.borrow_mut().error_chance = 0;
    run.fake.borrow_mut().profile_delay = 0;

    run.open_profile("Conleth (you)");
    run.wait_for("your profile", |state| state.profile.is_some());

    let texts = run.visible_texts();
    for needle in [
        "Conleth (you)",
        "writes small tools and reads logs",
        "they/them",
        "https://example.com/conleth",
    ] {
        assert!(
            texts.iter().any(|text| text.contains(needle)),
            "your profile did not show {needle:?}; text: {texts:?}"
        );
    }
    assert!(
        texts.iter().any(|text| text == "[ edit profile ]"),
        "your profile did not offer the edit action: {texts:?}"
    );
}

#[test]
fn the_sidebar_lists_direct_messages_under_their_own_header_and_selects_one() {
    let mut run = FuzzHarness::new(63);
    run.fake.borrow_mut().error_chance = 0;
    run.fake.borrow_mut().dm_delay = 0;
    run.wait_for("the direct message list", |state| {
        !state.direct_messages.is_empty()
    });

    let texts = run.visible_texts();
    let header = run.node_with_label("direct messages").unwrap_or_else(|| {
        panic!("the direct messages section header; text: {texts:?}")
    });
    let row = run
        .node_with_label("direct message with Ada")
        .expect("the Ada DM row");
    let channel = run
        .node_with_label("# general (text)")
        .expect("the general channel row");
    let tab = run
        .targets()
        .into_iter()
        .find(|target| target.label.starts_with("[1] The Guild"))
        .expect("the first commune tab");

    assert!(
        header.rect.right() <= 220.0,
        "the direct messages header was not in the sidebar: {:?}",
        header.rect
    );
    assert!(
        tab.rect.bottom() <= header.rect.top(),
        "the DM list was drawn among the commune tabs: tab {:?}, header {:?}",
        tab.rect,
        header.rect
    );
    assert!(
        header.rect.top() <= row.rect.top(),
        "the DM row was drawn above its own header: header {:?}, row {:?}",
        header.rect,
        row.rect
    );
    assert!(
        row.rect.bottom() <= channel.rect.top(),
        "the DM row was mixed into the channel rows: row {:?}, channel {:?}",
        row.rect,
        channel.rect
    );

    run.click_label_or_fail("direct message with Ada");
    assert_eq!(run.state().selected_guild, Some(DM_GUILD));
    let ada = run.fake.borrow().world.ada.author_id();
    assert_eq!(
        run.state().selected_dm().map(|conversation| conversation.peer.author),
        Some(ada)
    );
    run.wait_for("the DM channel", |state| {
        state.selected_channel == Some(DM_CHANNEL)
    });
}

#[test]
fn the_member_list_in_a_direct_message_opens_the_peers_profile() {
    let mut run = FuzzHarness::new(64);
    run.fake.borrow_mut().error_chance = 0;
    run.fake.borrow_mut().profile_delay = 0;
    run.fake.borrow_mut().dm_delay = 0;
    let ada = run.fake.borrow().world.ada.author_id();

    run.click_label_or_fail("direct message with Ada");
    run.wait_for("the DM roster", |state| {
        state.selected_guild == Some(DM_GUILD)
            && state.members.iter().any(|member| member.author == ada)
    });

    run.click_label_or_fail("Ada");
    run.wait_for("the peer profile", |state| {
        state.dialog == Dialog::Profile && state.profile.is_some()
    });
    assert_eq!(run.state().profile_guild, Some(DM_GUILD));
    assert_eq!(run.state().profile_author, Some(ada));

    let texts = run.visible_texts();
    for needle in [
        "Ada",
        "builds small tools and reads logs",
        "she/her",
        "https://example.com/ada",
    ] {
        assert!(
            texts.iter().any(|text| text.contains(needle)),
            "the DM peer profile did not show {needle:?}; text: {texts:?}"
        );
    }
    assert!(
        texts.iter().any(|text| text == "[ message ]"),
        "the DM peer profile did not offer the message action: {texts:?}"
    );
}

#[test]
fn the_window_says_commune_and_never_says_guild() {
    let mut run = FuzzHarness::new(65);
    run.fake.borrow_mut().error_chance = 0;
    run.fake.borrow_mut().profile_delay = 0;
    run.fake.borrow_mut().dm_delay = 0;
    // The fixture commune is called "The Guild"; that is user content, not a
    // label. Rename it so the scan only judges the window's own wording.
    run.fake.borrow_mut().world.commune_a.name = "The Commune".to_string();
    run.harness.state_mut().state.guilds[0].name = "The Commune".to_string();
    run.step();

    run.assert_no_guild("the commune view");
    let texts = run.visible_texts();
    assert!(
        texts.iter().any(|text| text.starts_with("commune:")),
        "the status bar did not name the commune: {texts:?}"
    );

    for (dialog, context) in [
        (Dialog::CreateGuild, "the create-commune dialog"),
        (Dialog::Join, "the join dialog"),
        (Dialog::Invite, "the invite dialog"),
        (Dialog::ChannelSettings, "the channel settings dialog"),
    ] {
        let state = &mut run.harness.state_mut().state;
        state.dialog = dialog;
        if dialog == Dialog::ChannelSettings {
            state.begin_channel_settings(GENERAL_A);
        }
        // A window needs one frame to be created and another to lay out.
        run.step();
        run.step();
        run.assert_no_guild(context);
        if dialog == Dialog::CreateGuild {
            let texts = run.visible_texts();
            assert!(
                texts.iter().any(|text| text.contains("create commune")),
                "the create dialog did not use commune: {texts:?}"
            );
        }
        run.harness.state_mut().state.dialog = Dialog::None;
        run.step();
    }

    run.open_profile("Ada");
    run.wait_for("Ada's profile", |state| state.profile.is_some());
    run.assert_no_guild("the profile dialog");
    run.press_escape();
    run.step();
    assert!(run.state().profile_author.is_none());

    run.click_label_or_fail("direct message with Ada");
    run.wait_for("the DM view", |state| {
        state.selected_guild == Some(DM_GUILD)
    });
    run.assert_no_guild("the direct message view");

    run.click_label_or_fail("Conleth (you)");
    run.wait_for("your profile in a DM", |state| state.profile.is_some());
    run.harness.state_mut().state.begin_profile_edit();
    run.step();
    run.assert_no_guild("the profile editor");

    run.harness.state_mut().state.wizard_step = Some(WizardStep::Identity);
    run.step();
    run.step();
    run.assert_no_guild("the getting-started window");
    run.harness.state_mut().state.wizard_step = None;

    // The one allowed hit is the daemon's wire-level protocol error. Prove the
    // allowance only covers that exact line.
    run.harness.state_mut().state.error = Some("guild mismatch".to_string());
    run.step();
    let texts = run.visible_texts();
    assert!(
        texts
            .iter()
            .any(|text| text.eq_ignore_ascii_case("guild mismatch")),
        "the protocol error was not rendered: {texts:?}"
    );
    run.assert_no_guild("the protocol error strip");
}

#[test]
fn disconnect_and_reconnect_keeps_the_window_usable() {
    let mut run = FuzzHarness::new(48);
    run.fake.borrow_mut().error_chance = 0;
    run.fake.borrow_mut().profile_delay = 0;
    run.fake.borrow_mut().disconnect();
    run.step();
    assert!(!run.state().connected);
    for _ in 0..5 {
        run.step();
    }
    run.fake.borrow_mut().reconnect();
    run.step();
    assert!(run.state().connected);
    run.wait_for("the reloaded channel", |state| {
        state.selected_guild.is_some() && state.loaded_channel.is_some()
    });

    let composer = run
        .text_targets()
        .into_iter()
        .find(|target| target.role == Role::MultilineTextInput)
        .expect("the composer after reconnect");
    run.click_target(&composer);
    run.step();
    run.type_text("still here after reconnect");
    run.step();
    run.press(Key::Enter);
    run.step();
    assert!(
        run.fake
            .borrow()
            .sent_bodies
            .iter()
            .any(|body| body == "still here after reconnect")
    );
}

#[test]
fn finishing_setup_creates_a_commune_and_opens_the_invite() {
    let mut run = FuzzHarness::new(71);
    run.fake.borrow_mut().error_chance = 0;
    {
        let state = &mut run.harness.state_mut().state;
        state.wizard_step = Some(WizardStep::Review);
        state.wizard_name = "Conleth".to_string();
        state.wizard_commune = WizardCommune::Create;
        state.wizard_commune_name = "first commune".to_string();
        state.wizard_theme_index = Some(0);
        // A clean first run has no other guild to take offline. Leaving an
        // online hint here sends Disconnect first, which the fake treats as
        // the bridge going away.
        state.online_hint = None;
    }
    run.step();
    run.click_label_or_fail("[ finish ]");

    // Finish closes setup, creates the commune, brings it online on the fixed
    // listen address and opens the invite without another click.
    run.wait_for("the first-run invite", |state| {
        !state.invite_code.is_empty() && state.dialog == Dialog::Invite
    });
    // The dialog opens in `handle_action` after the frame was drawn, so draw
    // once more before reading the accesskit tree.
    run.step();
    assert_eq!(run.state().dialog, Dialog::Invite);
    assert!(
        run.state()
            .guilds
            .iter()
            .any(|guild| guild.name == "first commune"),
        "the setup commune was not created: {:?}",
        run.state().guilds
    );
    let texts = run.visible_texts();
    assert!(
        texts
            .iter()
            .any(|text| text.contains("this code is the whole invitation")),
        "the invite window did not explain the code: {texts:?}"
    );
}
