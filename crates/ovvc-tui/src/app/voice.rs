//! Voice-channel commands and rendering helpers for the terminal frontend.
//!
//! The daemon owns the call: this module only parses `/voice`, asks the
//! frontend to join, leave or publish the local flags, and turns the snapshots
//! that come back into the bracketed markers the phosphor layout uses.

use std::collections::HashMap;

use ovvc_client::{AuthorId, ChannelId, ChannelKind, GuildId, VoiceAudioStats};
use ovvc_control::{CallTopology, VoiceChange, VoiceMemberState, VoiceSnapshot};
use ratatui::crossterm::event::KeyCode;

use super::{App, Overlay};

/// Mirrors `ovvc_media::MESH_LIMIT`: calls at or below this size are fully
/// connected under the automatic scope, and above it the control plane may
/// report a star. The media path is still all-pairs until hub forwarding
/// lands, which the call panel says explicitly. It is repeated here so the
/// terminal crate does not grow a media dependency just to render one number.
pub const VOICE_CAPACITY: usize = 6;

/// The key held to talk while `/voice ptt on` is active. Function keys do not
/// collide with typing, so holding it never swallows message text.
pub const PTT_KEY: KeyCode = KeyCode::F(8);

/// The local call state and the snapshots the sidebar and roster render from.
#[derive(Debug, Default)]
pub struct VoiceUi {
    /// Guild and channel of the local participant's call, kept apart from the
    /// snapshot so a best-effort leave still knows what to leave after the
    /// selection has already moved.
    pub guild: Option<GuildId>,
    pub channel: Option<ChannelId>,
    /// The call the user is in, when joined.
    pub snapshot: Option<VoiceSnapshot>,
    /// The daemon's audio counters for the local member's call. The daemon
    /// samples them once a second while its audio service runs, so this is
    /// `None` before the first sample and after the service stops.
    pub audio: Option<VoiceAudioStats>,
    /// The latest snapshot per channel in the selected guild. Sidebar counts
    /// and the roster read this, including calls the user is not in.
    pub snapshots: HashMap<ChannelId, VoiceSnapshot>,
    pub muted: bool,
    pub deafened: bool,
    pub push_to_talk: bool,
}

impl VoiceUi {
    pub fn joined(&self) -> bool {
        self.channel.is_some() && self.snapshot.is_some()
    }

    /// Forget the local call without touching the cached snapshots. A leave
    /// keeps the remaining call so the sidebar still counts the people there.
    pub fn clear_call(&mut self) {
        self.channel = None;
        self.snapshot = None;
        self.guild = None;
        // The counters belong to the daemon's service for this call, so a
        // leave or an ended call must not keep painting the old device or its
        // E2EE epoch.
        self.audio = None;
    }

    /// Forget the call and every cached snapshot, for a guild switch.
    pub fn clear_all(&mut self) {
        self.clear_call();
        self.snapshots.clear();
    }

    /// Change one of the three flags the local participant publishes.
    pub fn set_flag(&mut self, flag: VoiceFlag, on: bool) {
        match flag {
            VoiceFlag::Mute => self.muted = on,
            VoiceFlag::Deafen => self.deafened = on,
            VoiceFlag::PushToTalk => self.push_to_talk = on,
        }
    }

    /// The full member state to publish, keeping flags this frontend does not
    /// control (speaking, video, screen share) from the last snapshot.
    pub fn member_state(&self, previous: VoiceMemberState) -> VoiceMemberState {
        VoiceMemberState {
            muted: self.muted,
            deafened: self.deafened,
            push_to_talk: self.push_to_talk,
            ..previous
        }
    }
}

/// Which local flag a `/voice` command changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceFlag {
    Mute,
    Deafen,
    PushToTalk,
}

impl VoiceFlag {
    pub fn label(self) -> &'static str {
        match self {
            VoiceFlag::Mute => "mute",
            VoiceFlag::Deafen => "deafen",
            VoiceFlag::PushToTalk => "push-to-talk",
        }
    }
}

/// A parsed `/voice` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceCommand {
    Join,
    Leave,
    /// `true` mutes, `false` unmutes.
    Mute(bool),
    Deafen(bool),
    PushToTalk(bool),
    Status,
    Help,
    Unknown(String),
    /// A known subcommand with a missing or unusable argument.
    InvalidUsage(&'static str),
}

pub const PTT_USAGE: &str = "ptt on|off";

/// Parse the text after `/voice`.
pub fn parse_voice_command(rest: &str) -> VoiceCommand {
    let rest = rest.trim();
    if rest.is_empty() {
        return VoiceCommand::Help;
    }
    let normalized = rest.to_ascii_lowercase();
    match normalized.as_str() {
        "join" => VoiceCommand::Join,
        "leave" => VoiceCommand::Leave,
        "mute" => VoiceCommand::Mute(true),
        "unmute" => VoiceCommand::Mute(false),
        "deafen" => VoiceCommand::Deafen(true),
        "undeafen" => VoiceCommand::Deafen(false),
        "ptt on" | "push-to-talk on" | "push to talk on" => VoiceCommand::PushToTalk(true),
        "ptt off" | "push-to-talk off" | "push to talk off" => VoiceCommand::PushToTalk(false),
        "ptt" | "push-to-talk" | "push to talk" => VoiceCommand::InvalidUsage(PTT_USAGE),
        "status" => VoiceCommand::Status,
        "help" => VoiceCommand::Help,
        other
            if other.starts_with("ptt ")
                || other.starts_with("push-to-talk ")
                || other.starts_with("push to talk ") =>
        {
            VoiceCommand::InvalidUsage(PTT_USAGE)
        }
        _ => VoiceCommand::Unknown(rest.to_string()),
    }
}

/// The bracketed markers for one member, in a stable order. ASCII only.
pub fn member_markers(state: VoiceMemberState) -> String {
    let mut markers = String::new();
    if state.speaking {
        markers.push_str("[spk]");
    }
    if state.muted {
        push_marker(&mut markers, "[mute]");
    }
    if state.deafened {
        push_marker(&mut markers, "[deaf]");
    }
    if state.push_to_talk {
        push_marker(&mut markers, "[ptt]");
    }
    markers
}

fn push_marker(markers: &mut String, marker: &str) {
    if !markers.is_empty() {
        markers.push(' ');
    }
    markers.push_str(marker);
}

/// One line of the call roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RosterRow {
    pub author: AuthorId,
    pub markers: String,
    pub master: bool,
    pub speaking: bool,
}

/// The roster for the selected voice channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceRoster {
    pub capacity: String,
    pub topology: &'static str,
    /// The media path that is actually running: an all-pairs mesh below the
    /// limit, or the flat star once the hub forwards leaf traffic.
    pub media: String,
    /// The elected hub, backup, trust tier and key epoch.
    pub hub: String,
    pub rows: Vec<RosterRow>,
}

/// `N/6` for the sidebar and the call header.
pub fn capacity_hint(snapshot: &VoiceSnapshot) -> String {
    format!("{}/{}", snapshot.members.len(), VOICE_CAPACITY)
}

/// The signalled call topology.
///
/// The control plane elects a hub and reports a flat star above two members.
/// A two-member call is a direct pair, and a larger call under the automatic
/// scope stays a mesh until it passes the mesh limit.
pub fn topology_label(snapshot: &VoiceSnapshot) -> &'static str {
    match snapshot.topology {
        CallTopology::Mesh => "mesh",
        CallTopology::Star { .. } => "star",
    }
}

/// The media path that is actually running, with the key rule that goes with
/// it. A star forwards leaf traffic through the hub and keys only leaf-to-hub
/// pairs; a mesh keeps one key for every pair.
pub fn media_label(snapshot: &VoiceSnapshot) -> String {
    match &snapshot.topology {
        CallTopology::Mesh => "media mesh · keys all-pairs".to_string(),
        CallTopology::Star { leaves, .. } => {
            let noun = if leaves.len() == 1 { "leaf" } else { "leaves" };
            format!(
                "media star · {} {noun} · keys leaf->hub",
                leaves.len()
            )
        }
    }
}

/// The hub, backup, trust tier and key epoch for the call panel.
pub fn hub_label(snapshot: &VoiceSnapshot) -> String {
    let hub = snapshot
        .hub
        .map(|author| author.short())
        .unwrap_or_else(|| "none".to_string());
    let backup = snapshot
        .backup
        .map(|author| author.short())
        .unwrap_or_else(|| "none".to_string());
    format!(
        "hub {hub} · backup {backup} · {} · keys e{}",
        snapshot.hub_policy.trust.name(),
        snapshot.key_epoch
    )
}

/// Every call member with the bracketed state markers, `[hub]` on the elected
/// media hub, `[master]` on the coordinator of a mesh or direct call, and
/// `[backup]` on the standby.
pub fn roster_entries(snapshot: &VoiceSnapshot) -> Vec<RosterRow> {
    snapshot
        .members
        .iter()
        .map(|member| {
            let mut markers = member_markers(member.state);
            let is_hub = snapshot.hub == Some(member.author);
            let is_master = snapshot.master == Some(member.author);
            let is_backup = snapshot.backup == Some(member.author);
            if is_hub {
                push_marker(&mut markers, "[hub]");
            } else if is_master {
                push_marker(&mut markers, "[master]");
            }
            if is_backup {
                push_marker(&mut markers, "[backup]");
            }
            RosterRow {
                author: member.author,
                markers,
                master: is_hub || is_master,
                speaking: member.state.speaking,
            }
        })
        .collect()
}

/// The status-bar badge for the local call, without a leading or trailing
/// space. The codec is only shown once one has been negotiated.
pub fn status_line(
    snapshot: &VoiceSnapshot,
    audio: Option<&VoiceAudioStats>,
    channel_name: &str,
    muted: bool,
    deafened: bool,
    push_to_talk: bool,
) -> String {
    let mut text = format!("VOICE #{channel_name} {}", capacity_hint(snapshot));
    if muted {
        text.push_str(" MUTED");
    }
    if deafened {
        text.push_str(" DEAF");
    }
    if push_to_talk {
        text.push_str(" PTT");
    }
    if let Some(codec) = &snapshot.codec {
        text.push_str(&format!(" {}/{}k", codec.audio, codec.bitrate_kbps));
    }
    if let Some(audio) = audio {
        // The E2EE badge, epoch and dropped-frame count come before the
        // device and link health. A narrow terminal clips the suffix, and
        // encryption state is the field a user must be able to confirm.
        text.push(' ');
        text.push_str(&e2ee_badge(audio));
        text.push(' ');
        text.push_str(&device_health(audio));
        text.push(' ');
        text.push_str(&media_quality(audio));
    }
    text
}

/// The local audio device health: which backend is running, how many peers it
/// is carrying, its latency and its xrun count.
fn device_health(audio: &VoiceAudioStats) -> String {
    format!(
        "{} {}p {}ms x{}",
        audio.backend, audio.peers, audio.latency_ms, audio.xruns
    )
}

/// The end-to-end encryption badge for the status line.
///
/// A call without a usable key is shown as no media rather than as a live call
/// that silently carries nothing, because the daemon drops every frame it
/// cannot seal or authenticate. Dropped-frame counters are shown beside the
/// badge so a sealed call that is losing frames does not look healthy.
fn e2ee_badge(audio: &VoiceAudioStats) -> String {
    let badge = if audio.e2ee && audio.epoch > 0 {
        format!("E2EE e{}", audio.epoch)
    } else {
        "E2EE none [no media]".to_string()
    };
    if audio.e2ee_errors > 0 {
        format!("{badge} errs={}", audio.e2ee_errors)
    } else {
        badge
    }
}

/// The transport and connection-quality suffix for the status line. RTT and
/// loss are WebRTC-only counters, so the libp2p fallback shows them as
/// unavailable instead of painting zeroes as a measurement.
fn media_quality(audio: &VoiceAudioStats) -> String {
    if audio.transport == "webrtc" {
        let link = if audio.relayed {
            "[relayed]"
        } else {
            "[direct]"
        };
        format!(
            "{} rtt={}ms lost={} {link}",
            audio.transport, audio.rtt_ms, audio.packets_lost
        )
    } else {
        // The libp2p path never measures relay state, so it must not claim
        // the link is direct.
        format!("{} rtt=- lost=- relay=unknown", audio.transport)
    }
}

// --- App wiring ---------------------------------------------------------

impl App {
    /// Run a `/voice` line. Every failure lands in `status`; this never panics.
    pub(crate) fn voice_command(&mut self, rest: &str) {
        match parse_voice_command(rest) {
            VoiceCommand::Join => self.voice_join(),
            VoiceCommand::Leave => self.voice_leave(),
            VoiceCommand::Mute(on) => self.voice_set_flag(VoiceFlag::Mute, on),
            VoiceCommand::Deafen(on) => self.voice_set_flag(VoiceFlag::Deafen, on),
            VoiceCommand::PushToTalk(on) => self.voice_set_flag(VoiceFlag::PushToTalk, on),
            VoiceCommand::Status => {
                self.status = self
                    .voice_status_line()
                    .or_else(|| self.voice_channel_status_line())
                    .unwrap_or_else(|| {
                        if self.current_channel().map(|channel| channel.kind)
                            == Some(ChannelKind::Voice)
                        {
                            "voice: nobody is in this channel".to_string()
                        } else {
                            "voice: not in a call; select a voice channel and /voice join"
                                .to_string()
                        }
                    });
            }
            VoiceCommand::Help => {
                self.overlay = Some(voice_help());
            }
            VoiceCommand::Unknown(command) => {
                self.status = format!("unknown voice command: {command} (try /voice help)");
            }
            VoiceCommand::InvalidUsage(usage) => {
                self.status = format!("usage: /voice {usage}");
            }
        }
    }

    fn voice_join(&mut self) {
        let Some(guild) = self.current_guild().map(|guild| guild.id) else {
            self.status = "voice: no commune selected".to_string();
            return;
        };
        let Some((channel_id, name, kind)) = self
            .current_channel()
            .map(|channel| (channel.id, channel.name.clone(), channel.kind))
        else {
            self.status = "voice: no channel selected".to_string();
            return;
        };
        if kind != ChannelKind::Voice {
            self.status = format!("voice: #{name} is not a voice channel");
            return;
        }
        if self.voice.joined() && self.voice.channel == Some(channel_id) {
            self.status = format!("voice: already in #{name}");
            return;
        }
        if self.voice.channel.is_some() {
            self.voice_leave_best_effort();
        }
        let muted = self.voice.muted;
        let deafened = self.voice.deafened;
        let push_to_talk = self.voice.push_to_talk;
        match self
            .client
            .voice_join(guild, channel_id, muted, deafened, push_to_talk)
        {
            Ok(snapshot) => {
                self.voice.guild = Some(guild);
                self.voice.channel = Some(channel_id);
                self.voice.snapshots.insert(channel_id, snapshot.clone());
                self.voice.snapshot = Some(snapshot);
                self.status = format!("voice: joined ~ {name}");
            }
            Err(error) => self.status = format!("voice join failed: {error}"),
        }
    }

    fn voice_leave(&mut self) {
        let Some(channel) = self.voice.channel else {
            self.status = "voice: not in a call".to_string();
            return;
        };
        self.set_ptt_held(false);
        let guild = self
            .voice
            .guild
            .or_else(|| self.current_guild().map(|guild| guild.id));
        let result = guild.map(|guild| self.client.voice_leave(guild, channel));
        self.voice.clear_call();
        match result {
            Some(Ok(Some(snapshot))) => {
                self.voice.snapshots.insert(channel, snapshot);
                self.status = "voice: left".to_string();
            }
            Some(Ok(None)) | None => {
                self.voice.snapshots.remove(&channel);
                self.status = "voice: left".to_string();
            }
            Some(Err(error)) => {
                self.voice.snapshots.remove(&channel);
                self.status = format!("voice leave failed: {error}");
            }
        }
    }

    /// Leave whatever call this window is in, ignoring the daemon result.
    /// Used before the selection changes so a call never points at a channel
    /// in another guild.
    pub(crate) fn voice_leave_best_effort(&mut self) {
        let Some(channel) = self.voice.channel else {
            self.voice.clear_call();
            return;
        };
        self.set_ptt_held(false);
        let guild = self
            .voice
            .guild
            .or_else(|| self.current_guild().map(|guild| guild.id));
        if let Some(guild) = guild
            && let Ok(remaining) = self.client.voice_leave(guild, channel)
        {
            match remaining {
                Some(snapshot) => {
                    self.voice.snapshots.insert(channel, snapshot);
                }
                None => {
                    self.voice.snapshots.remove(&channel);
                }
            }
        } else {
            self.voice.snapshots.remove(&channel);
        }
        self.voice.clear_call();
    }

    /// Update the held state and tell the daemon when a call is live.
    ///
    /// The key is local input rather than roster state, so it goes through
    /// `set_push_to_talk_held` and never through `set_voice_state`.
    pub(crate) fn set_ptt_held(&mut self, held: bool) {
        if self.ptt_held == held {
            return;
        }
        self.ptt_held = held;
        if let (Some(guild), Some(channel)) = (self.voice.guild, self.voice.channel) {
            let _ = self.client.set_push_to_talk_held(guild, channel, held);
        }
    }

    /// Whether the push-to-talk key is currently held, for tests.
    #[cfg(test)]
    pub(crate) fn ptt_held(&self) -> bool {
        self.ptt_held
    }

    fn voice_set_flag(&mut self, flag: VoiceFlag, on: bool) {
        self.voice.set_flag(flag, on);
        if flag == VoiceFlag::PushToTalk && !on {
            // Turning the gate off also releases the key, so re-enabling it
            // cannot inherit a stale hold.
            self.set_ptt_held(false);
        }
        let word = if on { "on" } else { "off" };
        if !self.voice.joined() {
            self.status = format!("voice: {} {word}", flag.label());
            return;
        }
        let (Some(guild), Some(channel)) = (self.voice.guild, self.voice.channel) else {
            self.status = format!("voice: {} {word}", flag.label());
            return;
        };
        let previous = self
            .voice
            .snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .members
                    .iter()
                    .find(|member| member.author == self.client.author_id())
                    .map(|member| member.state)
            })
            .unwrap_or_default();
        let state = self.voice.member_state(previous);
        match self.client.set_voice_state(guild, channel, state) {
            Ok(snapshot) => {
                self.voice.snapshots.insert(channel, snapshot.clone());
                self.voice.snapshot = Some(snapshot);
                self.status = format!("voice: {} {word}", flag.label());
            }
            Err(error) => self.status = format!("voice state failed: {error}"),
        }
    }

    /// Apply a `ClientEvent::Voice`. Events for another guild are ignored.
    pub(crate) fn voice_on_event(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        change: VoiceChange,
        snapshot: Option<VoiceSnapshot>,
    ) {
        if self.current_guild().map(|guild| guild.id) != Some(guild_id) {
            return;
        }
        let channel_name = self
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .map(|channel| channel.name.clone())
            .unwrap_or_else(|| channel_id.short());
        match snapshot {
            // The last member leaving (or expiring) still arrives as a
            // snapshot, but it is empty: the call is gone, not a live call of
            // zero people.
            Some(snapshot) if snapshot.members.is_empty() => {
                self.voice.snapshots.remove(&channel_id);
                if self.voice.channel == Some(channel_id) {
                    self.set_ptt_held(false);
                    self.voice.clear_call();
                }
            }
            Some(snapshot) => {
                let contains_local = snapshot
                    .members
                    .iter()
                    .any(|member| member.author == self.client.author_id());
                self.voice.snapshots.insert(channel_id, snapshot.clone());
                if self.voice.channel == Some(channel_id) {
                    // If the daemon no longer lists us, we were timed out and
                    // must stop claiming to be in the call.
                    if contains_local {
                        self.voice.snapshot = Some(snapshot);
                        self.voice.guild = Some(guild_id);
                    } else {
                        self.set_ptt_held(false);
                        self.voice.clear_call();
                    }
                }
            }
            None => {
                self.voice.snapshots.remove(&channel_id);
                if self.voice.channel == Some(channel_id) {
                    self.set_ptt_held(false);
                    self.voice.clear_call();
                }
            }
        }
        match change {
            VoiceChange::Joined(author) => {
                self.status = format!(
                    "voice: {} joined ~ {channel_name}",
                    self.voice_member_name(author)
                );
            }
            VoiceChange::Left(author) => {
                self.status = format!(
                    "voice: {} left ~ {channel_name}",
                    self.voice_member_name(author)
                );
            }
            VoiceChange::SpeakingChanged { author, speaking } => {
                if author != self.client.author_id() {
                    let name = self.voice_member_name(author);
                    self.status = if speaking {
                        format!("voice: {name} speaking")
                    } else {
                        format!("voice: {name} stopped speaking")
                    };
                }
            }
            VoiceChange::Expired(author) => {
                self.status = format!(
                    "voice: {} timed out of ~ {channel_name}",
                    self.voice_member_name(author)
                );
            }
            VoiceChange::Updated(_) => {}
            VoiceChange::MediaChanged => {
                if let Some(snapshot) = self.voice.snapshot.as_ref() {
                    let hub = snapshot
                        .hub
                        .map(|author| self.voice_member_name(author))
                        .unwrap_or_else(|| "none".to_string());
                    let backup = snapshot
                        .backup
                        .map(|author| self.voice_member_name(author))
                        .unwrap_or_else(|| "none".to_string());
                    let trust = snapshot.hub_policy.trust.name();
                    let epoch = snapshot.key_epoch;
                    self.status =
                        format!("voice: hub {hub} · backup {backup} · {trust} keys e{epoch}");
                }
            }
        }
    }

    /// Apply a `ClientEvent::VoiceAudio`. The counters describe the daemon's
    /// local audio service, so they are only kept while this window is in that
    /// channel's call.
    pub(crate) fn voice_on_audio(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        stats: VoiceAudioStats,
    ) {
        if self.current_guild().map(|guild| guild.id) != Some(guild_id) {
            return;
        }
        if self.voice.channel == Some(channel_id) {
            self.voice.audio = Some(stats);
        }
    }

    fn voice_member_name(&self, author: AuthorId) -> String {
        if author == self.client.author_id() {
            if self.profile.is_empty() {
                "you".to_string()
            } else {
                self.profile.clone()
            }
        } else {
            self.members
                .iter()
                .find(|member| member.author == author)
                .map(|member| member.display_name.clone())
                .unwrap_or_else(|| author.short())
        }
    }

    /// `N/6` for a voice channel that has a live snapshot.
    pub(crate) fn voice_sidebar_hint(&self, channel_id: ChannelId) -> Option<String> {
        if self
            .channels
            .iter()
            .any(|channel| channel.id == channel_id && channel.kind != ChannelKind::Voice)
        {
            return None;
        }
        self.voice.snapshots.get(&channel_id).map(capacity_hint)
    }

    /// The call roster when the selected channel is a voice channel with a
    /// snapshot; otherwise the ordinary member panel keeps its guild roster.
    pub(crate) fn voice_roster(&self) -> Option<VoiceRoster> {
        let channel = self.current_channel()?;
        if channel.kind != ChannelKind::Voice {
            return None;
        }
        let snapshot = self.voice.snapshots.get(&channel.id)?;
        Some(VoiceRoster {
            capacity: capacity_hint(snapshot),
            topology: topology_label(snapshot),
            media: media_label(snapshot),
            hub: hub_label(snapshot),
            rows: roster_entries(snapshot),
        })
    }

    /// A one-line summary of the selected voice channel's call when this
    /// window is not in it, so `/voice status` can name the call instead of
    /// claiming the channel is empty.
    pub(crate) fn voice_channel_status_line(&self) -> Option<String> {
        let channel = self.current_channel()?;
        if channel.kind != ChannelKind::Voice {
            return None;
        }
        let snapshot = self.voice.snapshots.get(&channel.id)?;
        let mut text = format!(
            "VOICE #{} {} {} (not joined)",
            channel.name,
            capacity_hint(snapshot),
            topology_label(snapshot)
        );
        text.push_str(&format!(" {}", hub_label(snapshot)));
        Some(text)
    }

    /// The status-bar badge for the local call, when the user is joined.
    pub(crate) fn voice_status_line(&self) -> Option<String> {
        let channel_id = self.voice.channel?;
        let snapshot = self.voice.snapshot.as_ref()?;
        let channel = self
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .map(|channel| channel.name.clone())
            .unwrap_or_else(|| channel_id.short());
        Some(status_line(
            snapshot,
            self.voice.audio.as_ref(),
            &channel,
            self.voice.muted,
            self.voice.deafened,
            self.voice.push_to_talk,
        ))
    }
}

fn voice_help() -> Overlay {
    Overlay {
        title: "voice".to_string(),
        body: vec![
            "/voice join          join the selected voice channel".to_string(),
            "/voice leave         hang up".to_string(),
            "/voice mute|unmute   microphone".to_string(),
            "/voice deafen|undeafen  incoming audio".to_string(),
            "/voice ptt on|off    push-to-talk; hold F8 while on".to_string(),
            "/voice status        members, topology, coordinators, E2EE and audio".to_string(),
            "/voice help          this list".to_string(),
            String::new(),
            "Markers: [spk] speaking · [mute] muted · [deaf] deafened".to_string(),
            "         [ptt] push-to-talk · [hub] elected media hub".to_string(),
            "         [master] coordinator of a mesh · [backup] standby hub".to_string(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_client::Client;
    use ovvc_control::{CallTopology, NegotiatedCodec, ProfileSet, VoiceMember, VoiceProfile};
    use ovvc_media::CallId;

    fn author(byte: u8) -> AuthorId {
        AuthorId([byte; 32])
    }

    fn codec() -> NegotiatedCodec {
        NegotiatedCodec {
            profile: VoiceProfile::Standard,
            audio: "opus".to_string(),
            bitrate_kbps: 64,
            frame_ms: 20,
            sample_rate: 48_000,
            ptt_only: false,
            video: None,
        }
    }

    fn snapshot_with(codec: Option<NegotiatedCodec>) -> VoiceSnapshot {
        let alice = author(1);
        let bob = author(2);
        let channel = ChannelId([9; 16]);
        VoiceSnapshot {
            channel_id: channel,
            call_id: CallId::for_channel(channel),
            members: vec![
                VoiceMember {
                    author: alice,
                    joined_at_ms: 10,
                    last_seen_ms: 20,
                    state: VoiceMemberState {
                        speaking: true,
                        ..VoiceMemberState::default()
                    },
                    profiles: ProfileSet::STANDARD,
                },
                VoiceMember {
                    author: bob,
                    joined_at_ms: 11,
                    last_seen_ms: 21,
                    state: VoiceMemberState {
                        muted: true,
                        ..VoiceMemberState::default()
                    },
                    profiles: ProfileSet::STANDARD,
                },
            ],
            topology: CallTopology::Star {
                hub: alice,
                leaves: vec![bob],
            },
            hub: Some(alice),
            scope: ovvc_core::CallScope::Star,
            hub_policy: ovvc_core::HubPolicy::elected(),
            key_epoch: 1,
            codec,
            started_at_ms: 10,
            updated_at_ms: 20,
            master: Some(alice),
            backup: Some(bob),
            term: 1,
        }
    }

    #[test]
    fn parses_every_voice_subcommand() {
        assert_eq!(parse_voice_command(""), VoiceCommand::Help);
        assert_eq!(parse_voice_command("   "), VoiceCommand::Help);
        assert_eq!(parse_voice_command("help"), VoiceCommand::Help);
        assert_eq!(parse_voice_command("join"), VoiceCommand::Join);
        assert_eq!(parse_voice_command(" JOIN "), VoiceCommand::Join);
        assert_eq!(parse_voice_command("leave"), VoiceCommand::Leave);
        assert_eq!(parse_voice_command("mute"), VoiceCommand::Mute(true));
        assert_eq!(parse_voice_command("unmute"), VoiceCommand::Mute(false));
        assert_eq!(parse_voice_command("deafen"), VoiceCommand::Deafen(true));
        assert_eq!(parse_voice_command("undeafen"), VoiceCommand::Deafen(false));
        assert_eq!(
            parse_voice_command("ptt on"),
            VoiceCommand::PushToTalk(true)
        );
        assert_eq!(
            parse_voice_command("ptt off"),
            VoiceCommand::PushToTalk(false)
        );
        assert_eq!(
            parse_voice_command("push-to-talk on"),
            VoiceCommand::PushToTalk(true)
        );
        assert_eq!(
            parse_voice_command("push-to-talk off"),
            VoiceCommand::PushToTalk(false)
        );
        assert_eq!(
            parse_voice_command("push to talk on"),
            VoiceCommand::PushToTalk(true)
        );
        assert_eq!(parse_voice_command("status"), VoiceCommand::Status);
    }

    #[test]
    fn reports_unknown_and_malformed_voice_commands() {
        assert_eq!(
            parse_voice_command("shout"),
            VoiceCommand::Unknown("shout".to_string())
        );
        assert_eq!(
            parse_voice_command("ptt"),
            VoiceCommand::InvalidUsage(PTT_USAGE)
        );
        assert_eq!(
            parse_voice_command("ptt maybe"),
            VoiceCommand::InvalidUsage(PTT_USAGE)
        );
        assert_eq!(
            parse_voice_command("push-to-talk sideways"),
            VoiceCommand::InvalidUsage(PTT_USAGE)
        );
    }

    #[test]
    fn local_flags_toggle_independently() {
        let mut voice = VoiceUi::default();
        assert!(!voice.muted);
        assert!(!voice.deafened);
        assert!(!voice.push_to_talk);
        assert!(voice.channel.is_none());
        assert!(voice.snapshot.is_none());
        assert!(voice.snapshots.is_empty());

        voice.set_flag(VoiceFlag::Mute, true);
        assert!(voice.muted);
        voice.set_flag(VoiceFlag::Deafen, true);
        assert!(voice.deafened);
        assert!(voice.muted, "deafening cleared the mute flag");
        voice.set_flag(VoiceFlag::PushToTalk, true);
        assert!(voice.push_to_talk);
        assert!(voice.muted && voice.deafened);

        voice.set_flag(VoiceFlag::Mute, false);
        assert!(!voice.muted);
        assert!(voice.deafened && voice.push_to_talk);
        voice.set_flag(VoiceFlag::Deafen, false);
        voice.set_flag(VoiceFlag::PushToTalk, false);
        assert!(!voice.deafened);
        assert!(!voice.push_to_talk);
    }

    #[test]
    fn publishing_state_preserves_flags_the_terminal_does_not_own() {
        let mut voice = VoiceUi::default();
        voice.set_flag(VoiceFlag::Mute, true);
        voice.set_flag(VoiceFlag::PushToTalk, true);
        let previous = VoiceMemberState {
            speaking: true,
            video: true,
            screen_share: true,
            ..VoiceMemberState::default()
        };
        let state = voice.member_state(previous);
        assert!(state.muted);
        assert!(state.push_to_talk);
        assert!(!state.deafened);
        assert!(state.speaking);
        assert!(state.video);
        assert!(state.screen_share);
    }

    #[test]
    fn markers_are_bracketed_and_ordered() {
        assert_eq!(member_markers(VoiceMemberState::default()), "");
        assert_eq!(
            member_markers(VoiceMemberState {
                speaking: true,
                ..VoiceMemberState::default()
            }),
            "[spk]"
        );
        assert_eq!(
            member_markers(VoiceMemberState {
                muted: true,
                ..VoiceMemberState::default()
            }),
            "[mute]"
        );
        assert_eq!(
            member_markers(VoiceMemberState {
                deafened: true,
                ..VoiceMemberState::default()
            }),
            "[deaf]"
        );
        assert_eq!(
            member_markers(VoiceMemberState {
                push_to_talk: true,
                ..VoiceMemberState::default()
            }),
            "[ptt]"
        );
        assert_eq!(
            member_markers(VoiceMemberState {
                speaking: true,
                muted: true,
                deafened: true,
                push_to_talk: true,
                video: true,
                screen_share: true,
            }),
            "[spk] [mute] [deaf] [ptt]"
        );
    }

    #[test]
    fn a_star_snapshot_names_the_forwarding_path_and_key_rule() {
        let snapshot = snapshot_with(None);
        assert_eq!(capacity_hint(&snapshot), "2/6");
        // The control plane reports a flat star above two members, and the
        // media path forwards leaf traffic through the elected hub.
        assert_eq!(topology_label(&snapshot), "star");
        assert_eq!(snapshot.hub, Some(author(1)));
        assert!(matches!(snapshot.topology, CallTopology::Star { .. }));
        assert_eq!(
            media_label(&snapshot),
            "media star · 1 leaf · keys leaf->hub",
            "the panel must report the star media path, not a pending mesh"
        );
        assert!(!media_label(&snapshot).contains("forwarding pending"));
        let hub = hub_label(&snapshot);
        assert!(hub.contains("hub"));
        assert!(hub.contains("backup"));

        let rows = roster_entries(&snapshot);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].author, author(1));
        assert_eq!(rows[0].markers, "[spk] [hub]");
        assert!(rows[0].master);
        assert!(rows[0].speaking);
        assert_eq!(rows[1].author, author(2));
        assert_eq!(rows[1].markers, "[mute] [backup]");
        assert!(!rows[1].master);
        assert!(!rows[1].speaking);
    }

    #[test]
    fn status_line_carries_members_state_and_codec() {
        let plain = snapshot_with(None);
        assert_eq!(
            status_line(&plain, None, "standup", false, false, false),
            "VOICE #standup 2/6"
        );
        assert_eq!(
            status_line(&plain, None, "standup", true, false, true),
            "VOICE #standup 2/6 MUTED PTT"
        );
        assert_eq!(
            status_line(&plain, None, "standup", true, true, true),
            "VOICE #standup 2/6 MUTED DEAF PTT"
        );
        let negotiated = snapshot_with(Some(codec()));
        assert_eq!(
            status_line(&negotiated, None, "standup", false, true, false),
            "VOICE #standup 2/6 DEAF opus/64k"
        );
        let audio = VoiceAudioStats {
            backend: "pipewire".to_string(),
            captured: 0,
            packets: 0,
            played: 0,
            concealed: 0,
            lost: 0,
            decode_errors: 0,
            peers: 2,
            xruns: 0,
            latency_ms: 20,
            peak: 0,
            rms: 0.0,
            transport: "webrtc".to_string(),
            rtt_ms: 35,
            packets_lost: 12,
            relayed: true,
            e2ee: true,
            epoch: 1,
            e2ee_errors: 0,
        };
        assert_eq!(
            status_line(&negotiated, Some(&audio), "standup", true, false, false),
            "VOICE #standup 2/6 MUTED opus/64k E2EE e1 \
             pipewire 2p 20ms x0 webrtc rtt=35ms lost=12 [relayed]"
        );

        // The interim path has no RTT or loss counter, so the line names the
        // transport and shows the values as unavailable rather than zero.
        let fallback = VoiceAudioStats {
            transport: "libp2p".to_string(),
            rtt_ms: 0,
            packets_lost: 0,
            relayed: false,
            ..audio.clone()
        };
        assert_eq!(
            status_line(&negotiated, Some(&fallback), "standup", false, false, false),
            "VOICE #standup 2/6 opus/64k E2EE e1 \
             pipewire 2p 20ms x0 libp2p rtt=- lost=- relay=unknown"
        );
    }

    #[test]
    fn a_call_without_a_media_key_is_shown_as_no_media() {
        let audio = VoiceAudioStats {
            backend: "pipewire".to_string(),
            captured: 0,
            packets: 0,
            played: 0,
            concealed: 0,
            lost: 0,
            decode_errors: 0,
            peers: 1,
            xruns: 0,
            latency_ms: 20,
            peak: 0,
            rms: 0.0,
            transport: "webrtc".to_string(),
            rtt_ms: 35,
            packets_lost: 0,
            relayed: false,
            e2ee: false,
            epoch: 0,
            e2ee_errors: 0,
        };
        let line = status_line(
            &snapshot_with(None),
            Some(&audio),
            "standup",
            false,
            false,
            false,
        );
        assert!(line.contains("E2EE none [no media]"), "{line}");
        assert!(!line.contains("E2EE e0"), "{line}");

        // A stale epoch with a peer that has no key still has no media.
        let stale = VoiceAudioStats {
            e2ee: false,
            epoch: 3,
            ..audio
        };
        let line = status_line(
            &snapshot_with(None),
            Some(&stale),
            "standup",
            false,
            false,
            false,
        );
        assert!(line.contains("E2EE none [no media]"), "{line}");
        assert!(!line.contains("E2EE e3"), "{line}");
    }

    #[test]
    fn dropped_media_frames_are_shown_beside_the_e2ee_badge() {
        let audio = VoiceAudioStats {
            backend: "pipewire".to_string(),
            captured: 0,
            packets: 0,
            played: 0,
            concealed: 0,
            lost: 0,
            decode_errors: 0,
            peers: 2,
            xruns: 0,
            latency_ms: 20,
            peak: 0,
            rms: 0.0,
            transport: "libp2p".to_string(),
            rtt_ms: 0,
            packets_lost: 0,
            relayed: false,
            e2ee: true,
            epoch: 4,
            e2ee_errors: 7,
        };
        let line = status_line(
            &snapshot_with(None),
            Some(&audio),
            "standup",
            false,
            false,
            false,
        );
        assert!(line.contains("E2EE e4 errs=7"), "{line}");
    }

    fn app() -> (tempfile::TempDir, std::sync::Arc<Client>, App) {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = std::sync::Arc::new(Client::open_local(dir.path()).expect("client"));
        let guild = client.create_guild("terminal").expect("guild");
        client
            .create_channel_in(guild.id, "standup", ChannelKind::Voice, None)
            .expect("voice channel");
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");
        let app = App::new(
            client.clone(),
            dir.path().to_path_buf(),
            crate::app::AppOptions::default(),
        )
        .expect("app");
        (dir, client, app)
    }

    /// Draw the app into a fixed-size test terminal and return the text.
    fn screen_at(app: &mut App, width: u16, height: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| crate::ui::draw(frame, app))
            .expect("drawing the test frame");
        let cells: Vec<&str> = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        cells
            .chunks(width as usize)
            .map(|row| row.concat())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A joined call with audio counters, for status-bar rendering tests.
    fn joined_voice_app(e2ee_errors: u64) -> (tempfile::TempDir, std::sync::Arc<Client>, App) {
        let (dir, client, mut app) = app();
        app.command("channel standup");
        let channel = app.current_channel().expect("voice channel").id;
        let snapshot = snapshot_with(Some(codec()));
        app.voice.guild = app.current_guild().map(|guild| guild.id);
        app.voice.channel = Some(channel);
        app.voice.snapshot = Some(snapshot.clone());
        app.voice.snapshots.insert(channel, snapshot);
        app.voice.muted = true;
        app.voice.push_to_talk = true;
        app.voice.audio = Some(VoiceAudioStats {
            backend: "pipewire".to_string(),
            captured: 0,
            packets: 0,
            played: 0,
            concealed: 0,
            lost: 0,
            decode_errors: 0,
            peers: 2,
            xruns: 0,
            latency_ms: 20,
            peak: 0,
            rms: 0.0,
            transport: "webrtc".to_string(),
            rtt_ms: 35,
            packets_lost: 12,
            relayed: true,
            e2ee: true,
            epoch: 1,
            e2ee_errors,
        });
        (dir, client, app)
    }

    #[test]
    fn push_to_talk_key_press_and_release_drive_the_held_state() {
        use ratatui::crossterm::event::{Event, KeyEvent, KeyEventKind, KeyModifiers};

        let (_dir, _client, mut app) = app();
        app.voice.channel = Some(ChannelId([9; 16]));
        app.voice.snapshot = Some(snapshot_with(None));
        app.voice.push_to_talk = true;

        app.on_event(Event::Key(KeyEvent::new_with_kind(
            PTT_KEY,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        )));
        assert!(app.ptt_held(), "F8 press must hold the microphone open");

        app.on_event(Event::Key(KeyEvent::new_with_kind(
            PTT_KEY,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        )));
        assert!(!app.ptt_held(), "F8 release must close the microphone");
    }

    #[test]
    fn without_release_events_a_second_press_releases_push_to_talk() {
        use ratatui::crossterm::event::{Event, KeyEvent, KeyEventKind, KeyModifiers};

        let (_dir, _client, mut app) = app();
        app.voice.channel = Some(ChannelId([9; 16]));
        app.voice.snapshot = Some(snapshot_with(None));
        app.voice.push_to_talk = true;

        let press = || {
            Event::Key(KeyEvent::new_with_kind(
                PTT_KEY,
                KeyModifiers::NONE,
                KeyEventKind::Press,
            ))
        };
        app.on_event(press());
        assert!(app.ptt_held());
        // A terminal with no enhanced key reporting sends no release, so the
        // next discrete press must be the way out rather than a hot mic.
        app.on_event(press());
        assert!(!app.ptt_held());
    }

    #[test]
    fn command_arm_updates_flags_and_rejects_text_channels() {
        let (_dir, _client, mut app) = app();
        app.command("voice mute");
        assert!(app.voice.muted);
        app.command("voice ptt on");
        assert!(app.voice.push_to_talk);
        app.command("voice unmute");
        assert!(!app.voice.muted);

        // The default channel is text, so joining must be refused before any
        // daemon call.
        app.command("voice join");
        assert!(app.status.contains("not a voice channel"), "{}", app.status);

        // With a voice channel selected the in-process client has no control
        // session, so the failure is reported rather than panicking.
        app.command("channel standup");
        app.command("voice join");
        assert!(app.status.contains("voice join failed"), "{}", app.status);

        app.command("voice");
        assert_eq!(
            app.overlay.as_ref().map(|overlay| overlay.title.as_str()),
            Some("voice")
        );
    }

    #[test]
    fn selected_voice_channel_renders_the_call_roster_and_status() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (_dir, _client, mut app) = app();
        app.command("channel standup");
        let channel = app.current_channel().expect("voice channel").id;
        let snapshot = snapshot_with(Some(codec()));
        app.voice.guild = app.current_guild().map(|guild| guild.id);
        app.voice.channel = Some(channel);
        app.voice.snapshot = Some(snapshot.clone());
        app.voice.snapshots.insert(channel, snapshot);
        app.voice.muted = true;
        app.voice.push_to_talk = true;
        app.voice.audio = Some(VoiceAudioStats {
            backend: "pipewire".to_string(),
            captured: 0,
            packets: 0,
            played: 0,
            concealed: 0,
            lost: 0,
            decode_errors: 0,
            peers: 2,
            xruns: 0,
            latency_ms: 20,
            peak: 0,
            rms: 0.0,
            transport: "webrtc".to_string(),
            rtt_ms: 35,
            packets_lost: 0,
            relayed: false,
            e2ee: true,
            epoch: 1,
            e2ee_errors: 0,
        });

        // Wide enough for the status bar to carry the call, media quality and
        // E2EE badge without clipping the badge at the end.
        // The expert level is what owns the topology and role detail now.
        app.detail_level = crate::app::DetailLevel::Expert;
        let backend = TestBackend::new(220, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .expect("drawing the test frame");
        let cells: Vec<&str> = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let screen = cells
            .chunks(220)
            .map(|row| row.concat())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("~ standup [2/6]"), "{screen}");
        assert!(screen.contains("call 2/6"), "{screen}");
        assert!(screen.contains("star"), "{screen}");
        assert!(screen.contains("media star"), "{screen}");
        assert!(screen.contains("keys leaf->hub"), "{screen}");
        assert!(!screen.contains("forwarding pending"), "{screen}");
        assert!(!screen.contains("keys all-pairs"), "{screen}");
        assert!(screen.contains("[spk] [hub]"), "{screen}");
        assert!(screen.contains("[mute] [backup]"), "{screen}");
        assert!(screen.contains("hub "), "{screen}");
        assert!(screen.contains("keys e1"), "{screen}");
        assert!(
            screen.contains("VOICE #standup 2/6 MUTED PTT opus/64k"),
            "{screen}"
        );
        // The status bar carries the media key epoch beside the call state.
        assert!(screen.contains("E2EE e1"), "{screen}");
    }

    #[test]
    fn clearing_the_call_keeps_or_drops_cached_snapshots() {
        let mut voice = VoiceUi {
            guild: Some(GuildId([4; 32])),
            channel: Some(ChannelId([9; 16])),
            snapshot: Some(snapshot_with(None)),
            audio: Some(VoiceAudioStats {
                backend: "pipewire".to_string(),
                captured: 0,
                packets: 0,
                played: 0,
                concealed: 0,
                lost: 0,
                decode_errors: 0,
                peers: 1,
                xruns: 0,
                latency_ms: 20,
                peak: 0,
                rms: 0.0,
                transport: "webrtc".to_string(),
                rtt_ms: 35,
                packets_lost: 12,
                relayed: false,
                e2ee: true,
                epoch: 1,
                e2ee_errors: 0,
            }),
            ..VoiceUi::default()
        };
        voice
            .snapshots
            .insert(ChannelId([9; 16]), snapshot_with(None));
        voice
            .snapshots
            .insert(ChannelId([10; 16]), snapshot_with(None));

        voice.clear_call();
        assert!(voice.channel.is_none() && voice.snapshot.is_none());
        assert!(
            voice.audio.is_none(),
            "the transport fields outlived the call"
        );
        assert_eq!(voice.snapshots.len(), 2, "clear_call dropped snapshots");

        voice.clear_all();
        assert!(voice.snapshots.is_empty());
    }

    fn snapshot_of(channel: ChannelId, authors: &[AuthorId]) -> VoiceSnapshot {
        VoiceSnapshot {
            channel_id: channel,
            call_id: CallId::for_channel(channel),
            members: authors
                .iter()
                .map(|author| VoiceMember {
                    author: *author,
                    joined_at_ms: 10,
                    last_seen_ms: 20,
                    state: VoiceMemberState::default(),
                    profiles: ProfileSet::STANDARD,
                })
                .collect(),
            topology: CallTopology::Mesh,
            hub: None,
            scope: ovvc_core::CallScope::Auto,
            hub_policy: ovvc_core::HubPolicy::default(),
            key_epoch: 0,
            codec: None,
            started_at_ms: 10,
            updated_at_ms: 20,
            master: authors.first().copied(),
            backup: authors.get(1).copied(),
            term: 1,
        }
    }

    #[test]
    fn an_empty_snapshot_ends_the_call() {
        let (_dir, client, mut app) = app();
        let guild = app.current_guild().expect("guild").id;
        let channel = app.current_channel().expect("channel").id;
        let local = client.author_id();
        let live = snapshot_of(channel, &[local]);
        app.voice.guild = Some(guild);
        app.voice.channel = Some(channel);
        app.voice.snapshot = Some(live.clone());
        app.voice.snapshots.insert(channel, live);

        app.voice_on_event(
            guild,
            channel,
            VoiceChange::Expired(local),
            Some(snapshot_of(channel, &[])),
        );

        assert!(
            !app.voice.joined(),
            "the finished call still shows as joined"
        );
        assert!(app.voice.channel.is_none());
        assert!(app.voice.snapshot.is_none());
        assert!(
            !app.voice.snapshots.contains_key(&channel),
            "the empty snapshot is still cached"
        );
    }

    #[test]
    fn losing_the_local_member_clears_the_local_call() {
        let (_dir, client, mut app) = app();
        let guild = app.current_guild().expect("guild").id;
        let channel = app.current_channel().expect("channel").id;
        let local = client.author_id();
        let other = author(2);
        let live = snapshot_of(channel, &[local, other]);
        app.voice.guild = Some(guild);
        app.voice.channel = Some(channel);
        app.voice.snapshot = Some(live.clone());
        app.voice.snapshots.insert(channel, live);

        // The daemon timed the local member out; the call itself lives on
        // with the other member.
        app.voice_on_event(
            guild,
            channel,
            VoiceChange::Expired(local),
            Some(snapshot_of(channel, &[other])),
        );

        assert!(
            !app.voice.joined(),
            "a timed-out member still shows as joined"
        );
        assert!(app.voice.channel.is_none());
        assert!(app.voice.snapshot.is_none());
        let cached = app
            .voice
            .snapshots
            .get(&channel)
            .expect("the remaining call is still cached");
        assert_eq!(cached.members.len(), 1);
        assert_eq!(cached.members[0].author, other);
    }

    #[test]
    fn e2ee_epoch_and_error_counts_survive_80_120_140_columns() {
        for width in [80u16, 120, 140] {
            let (_dir, _client, mut app) = joined_voice_app(7);
            let screen = screen_at(&mut app, width, 30);
            assert!(
                screen.contains("E2EE e1"),
                "the E2EE badge was clipped at {width} columns: {screen}"
            );
            assert!(
                screen.contains("errs=7"),
                "the error count was clipped at {width} columns: {screen}"
            );
        }
    }

    #[test]
    fn a_call_above_six_members_is_labelled_star() {
        let (_dir, _client, mut app) = joined_voice_app(0);
        let channel = app.voice.channel.expect("channel");
        let members: Vec<AuthorId> = (1..=7).map(author).collect();
        let mut snapshot = snapshot_of(channel, &members);
        // The control plane elects a hub and derives a flat star above
        // MESH_LIMIT, and the media path forwards each leaf through it.
        snapshot.topology = CallTopology::Star {
            hub: members[0],
            leaves: members[1..].to_vec(),
        };
        snapshot.hub = Some(members[0]);
        app.voice.snapshot = Some(snapshot.clone());
        app.voice.snapshots.insert(channel, snapshot);

        // The topology is expert detail; simple and advanced show only the
        // link and the roster flags.
        app.detail_level = crate::app::DetailLevel::Expert;
        let screen = screen_at(&mut app, 120, 30);
        assert!(screen.contains("call 7/6"), "{screen}");
        assert!(
            screen.contains("star"),
            "a seven-member call must say star: {screen}"
        );
        assert!(
            screen.contains("media star"),
            "the panel must report the star media path: {screen}"
        );
        assert!(
            screen.contains("6 leaves"),
            "the panel must report the leaf count: {screen}"
        );
        assert!(
            screen.contains("keys leaf->hub"),
            "the panel must state the star key rule: {screen}"
        );
        assert!(!screen.contains("forwarding pending"), "{screen}");
        assert!(screen.contains("[hub]"), "{screen}");
    }
}
