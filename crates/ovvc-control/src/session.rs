use std::collections::{BTreeMap, BTreeSet, HashMap};

use ovvc_core::{AuthorId, ChannelId, GuildId, HubMode, MediaPolicy};
use ovvc_media::{
    MediaError, VoiceAction, VoiceChange, VoiceMemberState, VoicePlane, VoiceSnapshot, VoiceUpdate,
};
use serde::{Deserialize, Serialize};

use crate::election::{Election, ElectionConfig, ElectionView};
use crate::error::{ControlError, Result};
use crate::frame::{
    CodecDecision, ControlFrame, ControlKind, PresenceStatus, Role, RosterMember, SessionId,
    Topology,
};
use crate::replay::{RateLimiter, ReplayGuard};

/// A presence/typing entry for the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypingState {
    pub author: AuthorId,
    pub channel_id: ChannelId,
    pub until_ms: u64,
}

/// A read-only view of one control session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlSnapshot {
    pub guild_id: GuildId,
    pub session_id: SessionId,
    pub roster: Vec<RosterMember>,
    pub election: ElectionView,
    pub topology: Topology,
    pub codec: Option<CodecDecision>,
    pub typing: Vec<TypingState>,
    /// One entry per live voice call in this session.
    pub voice: Vec<VoiceSnapshot>,
}

/// What the daemon should do after applying a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEffect {
    ElectionChanged(ElectionView),
    Presence {
        author: AuthorId,
        status: PresenceStatus,
        until_ms: u64,
    },
    Typing {
        author: AuthorId,
        channel_id: ChannelId,
        until_ms: u64,
    },
    TypingEnded {
        author: AuthorId,
        channel_id: ChannelId,
    },
    StateChanged {
        term: u64,
        master: Option<AuthorId>,
        backup: Option<AuthorId>,
    },
    SendPong {
        nonce: u64,
    },
    /// A voice call changed. The snapshot already carries the session's
    /// elected coordinators, so the daemon can forward it as-is.
    Voice(VoiceUpdate),
}

/// The authoritative, in-memory state of one control session.
///
/// This is not a second event log. Nothing here is persisted: if the process
/// exits, the session is gone and peers rebuild it from fresh `Join` and
/// `Claim` frames. That is the point of the control plane.
#[derive(Debug)]
pub struct ControlSession {
    pub guild_id: GuildId,
    pub session_id: SessionId,
    pub roster: BTreeMap<AuthorId, RosterMember>,
    pub election: Election,
    pub topology: Topology,
    pub codec: Option<CodecDecision>,
    pub members: BTreeSet<AuthorId>,
    typing: HashMap<(AuthorId, ChannelId), u64>,
    voice: VoicePlane,
    media_policy: MediaPolicy,
    /// The newest media key epoch announced for each call. The media plane
    /// owns the keyring; the session only records the epoch the members
    /// published so a snapshot can show it without reaching into the daemon.
    voice_epochs: HashMap<ChannelId, u32>,
    replay: ReplayGuard,
    rate: RateLimiter,
}

impl ControlSession {
    pub fn new(
        guild_id: GuildId,
        session_id: SessionId,
        config: ElectionConfig,
        members: impl IntoIterator<Item = AuthorId>,
    ) -> Self {
        Self::with_policy(
            guild_id,
            session_id,
            config,
            members,
            MediaPolicy::standard(),
        )
    }

    /// A session whose voice calls use the commune's media policy.
    pub fn with_policy(
        guild_id: GuildId,
        session_id: SessionId,
        config: ElectionConfig,
        members: impl IntoIterator<Item = AuthorId>,
        media_policy: MediaPolicy,
    ) -> Self {
        Self {
            guild_id,
            session_id,
            roster: BTreeMap::new(),
            election: Election::new(guild_id, session_id, config),
            topology: Topology::default(),
            codec: None,
            members: members.into_iter().collect(),
            typing: HashMap::new(),
            voice: VoicePlane::with_policy(media_policy.call_scope, media_policy.hub),
            media_policy,
            voice_epochs: HashMap::new(),
            replay: ReplayGuard::new(),
            rate: RateLimiter::default(),
        }
    }

    /// The newest media key epoch seen for `channel_id`, zero when none has
    /// been announced.
    pub fn voice_epoch(&self, channel_id: ChannelId) -> u32 {
        self.voice_epochs.get(&channel_id).copied().unwrap_or(0)
    }

    /// The number of members in this session's call, for capacity advertising.
    pub fn voice_call_size(&self) -> usize {
        self.voice
            .calls()
            .map(|call| call.members.len())
            .max()
            .unwrap_or(0)
    }

    /// The current media hub of `channel_id`'s call, `None` for a mesh, a
    /// direct pair, or a call that does not exist.
    pub fn voice_hub(&self, channel_id: ChannelId) -> Option<AuthorId> {
        self.voice
            .calls()
            .find(|call| call.channel_id == channel_id)
            .and_then(|call| call.hub)
    }

    /// Record the epoch the local keyring moved to, for example after a hub
    /// takeover.
    ///
    /// The daemon owns the keyring and may rotate it before the control plane
    /// sees the new `VoiceKey`. Calling this with the new epoch keeps
    /// [`VoiceSnapshot::key_epoch`] honest so the next snapshot does not
    /// advertise the old epoch. Only a higher epoch advances; an equal or
    /// older value is ignored. Returns whether the epoch moved.
    pub fn set_voice_epoch(&mut self, channel_id: ChannelId, epoch: u32) -> bool {
        if epoch == 0 {
            return false;
        }
        let current = self.voice_epochs.get(&channel_id).copied().unwrap_or(0);
        if epoch <= current {
            return false;
        }
        self.voice_epochs.insert(channel_id, epoch);
        self.voice.set_key_epoch(channel_id, epoch);
        true
    }

    /// Every live call's hub, captured before a frame is applied so a hub
    /// change can still be seen after the voice plane has recomputed the
    /// topology.
    fn voice_hubs(&self) -> BTreeMap<ChannelId, Option<AuthorId>> {
        self.voice
            .calls()
            .map(|call| (call.channel_id, call.hub))
            .collect()
    }

    pub fn replace_members(&mut self, members: impl IntoIterator<Item = AuthorId>) {
        self.members = members.into_iter().collect();
        let known: Vec<_> = self.roster.keys().copied().collect();
        for author in known {
            if !self.members.contains(&author) {
                self.roster.remove(&author);
                self.election.remove(author, crate::frame::now_ms());
                self.typing
                    .retain(|(typing_author, _), _| *typing_author != author);
            }
        }

        // A member can be in a call without a roster entry, so reconcile from
        // the voice plane rather than the roster alone. The returned updates
        // are dropped: the membership snapshot is authoritative and the
        // daemon rebroadcasts it wholesale. The sync still moves each call's
        // stored hub and re-keys a star whose hub left.
        let now_ms = crate::frame::now_ms();
        let hubs_before = self.voice_hubs();
        let in_guild = self.members.clone();
        let stale: BTreeSet<AuthorId> = self
            .voice
            .calls()
            .flat_map(|call| call.members.keys().copied())
            .filter(|author| !in_guild.contains(author))
            .collect();
        for author in stale {
            self.voice.remove_author(author, now_ms);
            // A member who left the guild cannot keep standing for the media
            // coordinator role either.
            self.election.remove(author, now_ms);
        }
        let _ = self.sync_voice_hubs(now_ms, &hubs_before);
    }

    pub fn role_for(&self, author: AuthorId) -> Role {
        self.election.role_for(author)
    }

    /// Channels whose live call currently contains `author`.
    pub fn voice_channels_with(&self, author: AuthorId) -> Vec<ChannelId> {
        self.voice
            .calls()
            .filter(|call| call.members.contains_key(&author))
            .map(|call| call.channel_id)
            .collect()
    }

    /// A session is a voice session when it is not the guild-wide one. Claims
    /// to coordinate one only make sense from inside the call.
    fn is_voice_session(&self) -> bool {
        self.session_id != SessionId::for_guild(self.guild_id)
    }

    fn is_in_a_call(&self, author: AuthorId) -> bool {
        self.voice
            .calls()
            .any(|call| call.members.contains_key(&author))
    }

    /// Whether `author` may claim or heartbeat the media coordinator role in
    /// this session. A voice session only allows call members; anything else
    /// (the guild session) has no such restriction.
    pub fn may_coordinate(&self, author: AuthorId) -> bool {
        !self.is_voice_session() || self.is_in_a_call(author)
    }

    /// Verify and apply one frame. The caller has already checked that this
    /// frame belongs to this guild/session and arrived over an authenticated
    /// transport; the signature, membership, replay window and rate limit are
    /// checked here as defence in depth.
    pub fn apply(&mut self, frame: &ControlFrame, now_ms: u64) -> Result<Vec<SessionEffect>> {
        frame.verify()?;
        frame.check_session(self.guild_id, self.session_id)?;
        // A voice frame names its channel, and the channel derives the only
        // session it may travel on. Without this a member could create a call
        // in an arbitrary channel id on the guild session.
        if let Some(channel_id) = frame.kind.voice_channel()
            && SessionId::for_voice_channel(self.guild_id, channel_id) != self.session_id
        {
            return Err(ControlError::WrongSession);
        }
        frame.check_freshness(now_ms)?;
        if !self.members.contains(&frame.from) {
            return Err(ControlError::NotAMember);
        }
        // Being in the guild roster is not enough to coordinate a call: a
        // bystander must not be able to claim the media role for a call they
        // never joined.
        if !self.may_coordinate(frame.from) {
            match &frame.kind {
                ControlKind::Claim { .. }
                | ControlKind::Heartbeat { .. }
                | ControlKind::State { .. }
                | ControlKind::Roster { .. } => {
                    return Err(ControlError::Invalid(
                        "voice coordinator frame from a member outside the call".to_string(),
                    ));
                }
                ControlKind::RtcOffer { .. }
                | ControlKind::RtcAnswer { .. }
                | ControlKind::RtcCandidate { .. }
                | ControlKind::RtcBye { .. } => {
                    return Err(ControlError::Invalid(
                        "rtc signalling from a member outside the call".to_string(),
                    ));
                }
                ControlKind::VoiceKey { .. } => {
                    return Err(ControlError::Invalid(
                        "voice key from a member outside the call".to_string(),
                    ));
                }
                _ => {}
            }
        }
        // An RTC signal is addressed to one peer. The control plane only
        // routes it, so it must at least name another member of this guild:
        // signalling yourself or a non-member is a malformed frame.
        if let Some((_, target)) = frame.kind.rtc_target()
            && (target == frame.from || !self.members.contains(&target))
        {
            return Err(ControlError::Invalid(
                "rtc target must be another guild member".to_string(),
            ));
        }
        self.rate.check(frame.from, now_ms)?;
        self.replay.check_and_record(frame, now_ms)?;

        if let Some(member) = self.roster.get_mut(&frame.from) {
            member.last_seen_ms = now_ms;
        }

        // A membership change can make the voice plane recompute the
        // topology, so the pre-frame hub is captured here to tell a real hub
        // change from a recompute that landed on the same hub.
        let hubs_before = self.voice_hubs();
        let mut effects = Vec::new();
        match &frame.kind {
            ControlKind::Join {
                display_name,
                capabilities,
            } => {
                let member = RosterMember {
                    author: frame.from,
                    display_name: display_name.clone(),
                    capabilities: *capabilities,
                    joined_at_ms: now_ms,
                    last_seen_ms: now_ms,
                    presence: PresenceStatus::Online,
                };
                self.roster.insert(frame.from, member);
                effects.push(SessionEffect::Presence {
                    author: frame.from,
                    status: PresenceStatus::Online,
                    until_ms: 0,
                });
            }
            ControlKind::Leave => {
                self.roster.remove(&frame.from);
                self.typing.retain(|(author, _), _| *author != frame.from);
                if self.election.remove(frame.from, now_ms).is_change() {
                    effects.push(SessionEffect::ElectionChanged(self.election.view()));
                }
                effects.push(SessionEffect::Presence {
                    author: frame.from,
                    status: PresenceStatus::Offline,
                    until_ms: 0,
                });
            }
            ControlKind::Claim { priority } => {
                let change = self.election.claim(frame.from, *priority, now_ms);
                if change.is_change() {
                    effects.push(SessionEffect::ElectionChanged(self.election.view()));
                }
            }
            ControlKind::Heartbeat { role, priority } => {
                let change = self
                    .election
                    .heartbeat(frame.from, *role, *priority, now_ms);
                if change.is_change() {
                    effects.push(SessionEffect::ElectionChanged(self.election.view()));
                }
            }
            ControlKind::State {
                master,
                backup,
                term,
                topology,
                codec,
            } => {
                // A coordinator's word is only worth taking from a coordinator.
                if !self.election.role_for(frame.from).is_coordinator() {
                    return Err(ControlError::Invalid(
                        "state frame from a non-coordinator".to_string(),
                    ));
                }
                if self.election.observe_state(*term, *master, *backup, now_ms) {
                    self.topology = topology.clone();
                    self.codec = codec.clone();
                    effects.push(SessionEffect::ElectionChanged(self.election.view()));
                    effects.push(SessionEffect::StateChanged {
                        term: *term,
                        master: *master,
                        backup: *backup,
                    });
                }
            }
            ControlKind::Roster { members } => {
                if !self.election.role_for(frame.from).is_coordinator() {
                    return Err(ControlError::Invalid(
                        "roster frame from a non-coordinator".to_string(),
                    ));
                }
                for member in members {
                    if self.members.contains(&member.author) {
                        self.roster.insert(member.author, member.clone());
                    }
                }
            }
            ControlKind::Presence { status, until_ms } => {
                if let Some(member) = self.roster.get_mut(&frame.from) {
                    member.presence = *status;
                    member.last_seen_ms = now_ms;
                }
                effects.push(SessionEffect::Presence {
                    author: frame.from,
                    status: *status,
                    until_ms: *until_ms,
                });
            }
            ControlKind::Typing {
                channel_id,
                until_ms,
            } => {
                self.typing.insert((frame.from, *channel_id), *until_ms);
                effects.push(SessionEffect::Typing {
                    author: frame.from,
                    channel_id: *channel_id,
                    until_ms: *until_ms,
                });
            }
            ControlKind::Ping { nonce } => {
                effects.push(SessionEffect::SendPong { nonce: *nonce });
            }
            ControlKind::Pong { .. } => {}
            ControlKind::VoiceJoin {
                channel_id,
                profiles,
                muted,
                deafened,
                push_to_talk,
                video,
                screen_share,
            } => {
                let state = VoiceMemberState {
                    muted: *muted,
                    deafened: *deafened,
                    push_to_talk: *push_to_talk,
                    speaking: false,
                    video: *video,
                    screen_share: *screen_share,
                };
                let update = self.voice.apply(
                    *channel_id,
                    frame.from,
                    VoiceAction::Join {
                        profiles: *profiles,
                        state,
                    },
                    now_ms,
                )?;
                effects.push(SessionEffect::Voice(self.stamp_voice(update)));
            }
            ControlKind::VoiceLeave { channel_id } => {
                match self
                    .voice
                    .apply(*channel_id, frame.from, VoiceAction::Leave, now_ms)
                {
                    Ok(update) => effects.push(SessionEffect::Voice(self.stamp_voice(update))),
                    // Leave is idempotent: a duplicate or racing leave is a
                    // no-op, not an error.
                    Err(MediaError::NotInCall) => {}
                    Err(error) => return Err(error.into()),
                }
                if self.election.remove(frame.from, now_ms).is_change() {
                    effects.push(SessionEffect::ElectionChanged(self.election.view()));
                }
            }
            ControlKind::VoiceState {
                channel_id,
                muted,
                deafened,
                push_to_talk,
                speaking,
                video,
                screen_share,
            } => {
                let state = VoiceMemberState {
                    muted: *muted,
                    deafened: *deafened,
                    push_to_talk: *push_to_talk,
                    speaking: *speaking,
                    video: *video,
                    screen_share: *screen_share,
                };
                match self.voice.apply(
                    *channel_id,
                    frame.from,
                    VoiceAction::Update { state },
                    now_ms,
                ) {
                    Ok(update) => effects.push(SessionEffect::Voice(self.stamp_voice(update))),
                    // A state update can overtake a leave on the wire; the
                    // call is gone, so there is nothing to update.
                    Err(MediaError::NotInCall) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            ControlKind::VoiceHeartbeat { channel_id } => {
                match self.voice.touch(*channel_id, frame.from, now_ms) {
                    Ok(()) | Err(MediaError::NotInCall) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            // RTC signalling is verified and routed, never stored. The session
            // keeps no SDP or ICE state, and a signal does not change the call
            // membership or the election, so there is nothing to emit.
            ControlKind::RtcOffer { .. }
            | ControlKind::RtcAnswer { .. }
            | ControlKind::RtcCandidate { .. }
            | ControlKind::RtcBye { .. } => {}
            // A media key is broadcast to the call, verified and passed on.
            // The media plane owns the keyring; the session records only the
            // newest announced epoch so every snapshot can show it. A rejoin
            // starts from a fresh epoch rather than a remembered key.
            ControlKind::VoiceKey {
                channel_id, epoch, ..
            } => {
                let advanced = {
                    let current = self.voice_epochs.entry(*channel_id).or_insert(0);
                    if *epoch > *current {
                        *current = *epoch;
                        true
                    } else {
                        false
                    }
                };
                if advanced {
                    self.voice.set_key_epoch(*channel_id, *epoch);
                    if let Some(update) = self.voice_media_update(*channel_id) {
                        effects.push(SessionEffect::Voice(update));
                    }
                }
            }
            // A sync request changes no session state, and it is deliberately
            // accepted from a guild member outside this call: that member is
            // the one whose local roster is missing the rest of the call, so
            // refusing it would preserve the divergence it exists to repair.
            ControlKind::VoiceSyncRequest { .. } => {}
        }
        // The election may have moved after a claim, heartbeat or membership
        // change. Move each call onto the new hub and tell frontends when the
        // signalled topology changed.
        let live: BTreeSet<ChannelId> = self.voice.calls().map(|call| call.channel_id).collect();
        self.voice_epochs
            .retain(|channel, _| live.contains(channel));
        for update in self.sync_voice_hubs(now_ms, &hubs_before) {
            effects.push(SessionEffect::Voice(update));
        }
        Ok(effects)
    }

    /// Fill the elected coordinator and media fields on a voice snapshot.
    /// The media plane has no knowledge of the control election, and every
    /// consumer expects a self-contained snapshot.
    fn stamp_voice(&self, mut update: VoiceUpdate) -> VoiceUpdate {
        self.stamp_snapshot(&mut update.snapshot);
        update
    }

    fn stamp_snapshot(&self, snapshot: &mut VoiceSnapshot) {
        let view = self.election.view();
        let elected_hub = view.master.as_ref().map(|candidate| candidate.author);
        let hub = match self.media_policy.hub.mode {
            HubMode::None => None,
            HubMode::Elected | HubMode::Server => elected_hub,
        };
        snapshot.set_coordinators(
            elected_hub,
            view.backup.as_ref().map(|candidate| candidate.author),
            view.term,
        );
        snapshot.apply_hub(
            self.media_policy.call_scope,
            self.media_policy.hub,
            hub,
            self.voice_epoch(snapshot.channel_id),
        );
    }

    /// Move every call's signalled topology onto the current elected hub.
    ///
    /// Returns one `MediaChanged` update per call whose topology or hub
    /// changed, and one per call whose key epoch advanced because the hub
    /// changed. A star's media keys are leaf-to-hub, so a new hub cannot use
    /// the old epoch's ciphers: the control plane advances the advertised
    /// epoch and every member re-keys. A mesh or a direct two-member call has
    /// no hub and is left alone.
    ///
    /// `hubs_before` is the hub map captured before the frame or expiry was
    /// applied. The voice plane may already have recomputed the topology, so
    /// comparing against the post-change call would miss a hub that left.
    fn sync_voice_hubs(
        &mut self,
        now_ms: u64,
        hubs_before: &BTreeMap<ChannelId, Option<AuthorId>>,
    ) -> Vec<VoiceUpdate> {
        let hub = self.election.master().map(|candidate| candidate.author);
        let mut updates = Vec::new();
        let channels: Vec<ChannelId> = self.voice.calls().map(|call| call.channel_id).collect();
        for channel_id in channels {
            let old_hub = hubs_before.get(&channel_id).copied().flatten();
            let changed = self.voice.set_hub(channel_id, hub, now_ms);
            let new_hub = self.voice_hub(channel_id);
            let hub_changed = old_hub != new_hub;
            if hub_changed && (old_hub.is_some() || new_hub.is_some()) {
                // A star's media keys are leaf-to-hub, so a new hub re-keys
                // the call. A mesh or direct pair has neither hub and is not
                // advanced.
                self.advance_voice_epoch(channel_id);
            }
            if (changed || hub_changed)
                && let Some(snapshot) = self.voice.snapshot(channel_id)
            {
                updates.push(VoiceUpdate {
                    channel_id,
                    change: VoiceChange::MediaChanged,
                    snapshot: self.stamp_voice_snapshot(snapshot),
                });
            }
        }
        updates
    }

    /// Advance a call's media key epoch by one after a hub change.
    ///
    /// A call with no announced key yet (epoch zero) is not advanced: there is
    /// nothing to rotate and the first real key will start the sequence. A
    /// `u32` wrap is not reachable in practice, but wrapping to one is safer
    /// than panicking.
    fn advance_voice_epoch(&mut self, channel_id: ChannelId) -> bool {
        let Some(current) = self.voice_epochs.get(&channel_id).copied() else {
            return false;
        };
        if current == 0 {
            return false;
        }
        let next = current.checked_add(1).unwrap_or(1);
        self.set_voice_epoch(channel_id, next)
    }

    fn stamp_voice_snapshot(&self, mut snapshot: VoiceSnapshot) -> VoiceSnapshot {
        self.stamp_snapshot(&mut snapshot);
        snapshot
    }

    /// Emit a media update for `channel_id` if the call is live.
    fn voice_media_update(&self, channel_id: ChannelId) -> Option<VoiceUpdate> {
        let snapshot = self.voice.snapshot(channel_id)?;
        Some(VoiceUpdate {
            channel_id,
            change: VoiceChange::MediaChanged,
            snapshot: self.stamp_voice_snapshot(snapshot),
        })
    }

    /// Expire leases and typing markers. Returns the visible changes.
    pub fn expire(&mut self, now_ms: u64) -> Vec<SessionEffect> {
        let hubs_before = self.voice_hubs();
        let mut effects = Vec::new();
        if self.election.tick(now_ms).is_change() {
            effects.push(SessionEffect::ElectionChanged(self.election.view()));
        }
        let expired: Vec<_> = self
            .typing
            .iter()
            .filter(|(_, until)| **until <= now_ms)
            .map(|((author, channel), _)| (*author, *channel))
            .collect();
        for (author, channel) in expired {
            self.typing.remove(&(author, channel));
            effects.push(SessionEffect::TypingEnded {
                author,
                channel_id: channel,
            });
        }
        // A member that has been silent for two lease windows is shown
        // offline, but stays in the roster: presence is ephemeral, membership
        // is durable and comes from the signed log.
        let stale_after = now_ms.saturating_sub(2 * self.election.config().lease_ms);
        for member in self.roster.values_mut() {
            if member.presence != PresenceStatus::Offline
                && member.last_seen_ms > 0
                && member.last_seen_ms < stale_after
            {
                member.presence = PresenceStatus::Offline;
                effects.push(SessionEffect::Presence {
                    author: member.author,
                    status: PresenceStatus::Offline,
                    until_ms: 0,
                });
            }
        }
        for update in self.voice.expire(now_ms) {
            // A member who timed out of the call must not stay a candidate for
            // its coordinator role.
            if let VoiceChange::Expired(author) = &update.change
                && self.election.remove(*author, now_ms).is_change()
            {
                effects.push(SessionEffect::ElectionChanged(self.election.view()));
            }
            effects.push(SessionEffect::Voice(self.stamp_voice(update)));
        }
        let live: BTreeSet<ChannelId> = self.voice.calls().map(|call| call.channel_id).collect();
        self.voice_epochs
            .retain(|channel, _| live.contains(channel));
        for update in self.sync_voice_hubs(now_ms, &hubs_before) {
            effects.push(SessionEffect::Voice(update));
        }
        effects
    }

    /// Remove an author from every voice call in this session. The daemon
    /// calls this when a guild-level leave arrives on the guild session, which
    /// is a different session from the per-channel voice one.
    pub fn remove_voice_author(&mut self, author: AuthorId, now_ms: u64) -> Vec<SessionEffect> {
        let hubs_before = self.voice_hubs();
        let updates = self.voice.remove_author(author, now_ms);
        let mut effects = Vec::new();
        if !updates.is_empty() && self.election.remove(author, now_ms).is_change() {
            effects.push(SessionEffect::ElectionChanged(self.election.view()));
        }
        effects.extend(
            updates
                .into_iter()
                .map(|update| SessionEffect::Voice(self.stamp_voice(update))),
        );
        let live: BTreeSet<ChannelId> = self.voice.calls().map(|call| call.channel_id).collect();
        self.voice_epochs
            .retain(|channel, _| live.contains(channel));
        for update in self.sync_voice_hubs(now_ms, &hubs_before) {
            effects.push(SessionEffect::Voice(update));
        }
        effects
    }

    pub fn snapshot(&self, now_ms: u64) -> ControlSnapshot {
        let election = self.election.view();
        let voice = self
            .voice
            .calls()
            .map(|call| self.stamp_voice_snapshot(call.snapshot()))
            .collect();
        ControlSnapshot {
            guild_id: self.guild_id,
            session_id: self.session_id,
            roster: self.roster.values().cloned().collect(),
            election,
            topology: self.topology.clone(),
            codec: self.codec.clone(),
            typing: self
                .typing
                .iter()
                .filter(|(_, until)| **until > now_ms)
                .map(|((author, channel_id), until_ms)| TypingState {
                    author: *author,
                    channel_id: *channel_id,
                    until_ms: *until_ms,
                })
                .collect(),
            voice,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Priority;
    use crate::frame::{Capabilities, ControlFrame, ControlKind};
    use ovvc_core::{GuildId, Identity};

    fn setup() -> (ControlSession, Identity, Identity) {
        let guild = GuildId([4; 32]);
        let alice = Identity::generate();
        let bob = Identity::generate();
        let session = ControlSession::new(
            guild,
            SessionId::for_guild(guild),
            ElectionConfig::default(),
            [alice.author_id(), bob.author_id()],
        );
        (session, alice, bob)
    }

    fn join(identity: &Identity, nonce: u64) -> ControlFrame {
        let guild = GuildId([4; 32]);
        ControlFrame::sign(
            identity,
            guild,
            SessionId::for_guild(guild),
            0,
            ControlKind::Join {
                display_name: format!("name-{nonce}"),
                capabilities: Capabilities::voice(),
            },
            nonce,
        )
        .unwrap()
    }

    #[test]
    fn join_typing_and_expiry_are_visible() {
        let (mut session, alice, _) = setup();
        let now = crate::frame::now_ms();
        session.apply(&join(&alice, 1), now).unwrap();
        assert!(session.roster.contains_key(&alice.author_id()));

        let channel = ChannelId([9; 16]);
        let typing = ControlFrame::sign(
            &alice,
            GuildId([4; 32]),
            SessionId::for_guild(GuildId([4; 32])),
            0,
            ControlKind::Typing {
                channel_id: channel,
                until_ms: now + 5000,
            },
            2,
        )
        .unwrap();
        let effects = session.apply(&typing, now).unwrap();
        assert!(matches!(
            effects.first(),
            Some(SessionEffect::Typing { .. })
        ));
        assert_eq!(session.snapshot(now).typing.len(), 1);
        let expired = session.expire(now + 6000);
        assert!(matches!(
            expired.first(),
            Some(SessionEffect::TypingEnded { .. })
        ));
        assert!(session.snapshot(now + 6000).typing.is_empty());
    }

    #[test]
    fn non_members_cannot_join() {
        let (mut session, _, _) = setup();
        let mallory = Identity::generate();
        let now = crate::frame::now_ms();
        assert!(matches!(
            session.apply(&join(&mallory, 1), now),
            Err(ControlError::NotAMember)
        ));
    }

    #[test]
    fn replayed_frame_is_rejected() {
        let (mut session, alice, _) = setup();
        let now = crate::frame::now_ms();
        let frame = join(&alice, 1);
        session.apply(&frame, now).unwrap();
        assert!(matches!(
            session.apply(&frame, now),
            Err(ControlError::Replay)
        ));
    }

    #[test]
    fn election_claims_reach_the_snapshot() {
        let (mut session, alice, bob) = setup();
        let now = crate::frame::now_ms();
        session.apply(&join(&alice, 1), now).unwrap();
        session.apply(&join(&bob, 2), now).unwrap();
        let claim = ControlFrame::sign(
            &alice,
            GuildId([4; 32]),
            SessionId::for_guild(GuildId([4; 32])),
            0,
            ControlKind::Claim {
                priority: Priority {
                    trust_tier: 7,
                    ..Priority::local()
                },
            },
            3,
        )
        .unwrap();
        session.apply(&claim, now).unwrap();
        let snapshot = session.snapshot(now);
        assert_eq!(snapshot.election.master.unwrap().author, alice.author_id());
        assert_eq!(session.role_for(alice.author_id()), Role::Master);
    }

    use ovvc_media::{CallTopology, ProfileSet, VOICE_MEMBER_TIMEOUT_MS, VoiceChange};

    fn voice_guild() -> GuildId {
        GuildId([4; 32])
    }

    fn voice_channel() -> ChannelId {
        ChannelId([9; 16])
    }

    fn voice_join(channel: ChannelId) -> ControlKind {
        ControlKind::VoiceJoin {
            channel_id: channel,
            profiles: ProfileSet::ALL,
            muted: false,
            deafened: false,
            push_to_talk: false,
            video: false,
            screen_share: false,
        }
    }

    fn voice_state(channel: ChannelId, speaking: bool) -> ControlKind {
        ControlKind::VoiceState {
            channel_id: channel,
            muted: false,
            deafened: false,
            push_to_talk: false,
            speaking,
            video: false,
            screen_share: false,
        }
    }

    fn sign_voice(
        identity: &Identity,
        guild: GuildId,
        session_id: SessionId,
        kind: ControlKind,
        nonce: u64,
        at: u64,
    ) -> ControlFrame {
        ControlFrame::sign_at(identity, guild, session_id, 0, kind, nonce, at).unwrap()
    }

    fn voice_members(count: usize) -> Vec<Identity> {
        (0..count).map(|_| Identity::generate()).collect()
    }

    fn voice_session(
        identities: &[Identity],
        channel: ChannelId,
    ) -> (ControlSession, GuildId, SessionId) {
        let guild = voice_guild();
        let session_id = SessionId::for_voice_channel(guild, channel);
        let session = ControlSession::new(
            guild,
            session_id,
            ElectionConfig::default(),
            identities.iter().map(Identity::author_id),
        );
        (session, guild, session_id)
    }

    fn voice_session_with_policy(
        identities: &[Identity],
        channel: ChannelId,
        media_policy: MediaPolicy,
    ) -> (ControlSession, GuildId, SessionId) {
        let guild = voice_guild();
        let session_id = SessionId::for_voice_channel(guild, channel);
        let session = ControlSession::with_policy(
            guild,
            session_id,
            ElectionConfig::default(),
            identities.iter().map(Identity::author_id),
            media_policy,
        );
        (session, guild, session_id)
    }

    #[test]
    fn a_star_scope_exposes_the_elected_hub_and_backup() {
        let channel = voice_channel();
        let identities = voice_members(3);
        let mut media = MediaPolicy::standard();
        media.call_scope = ovvc_core::CallScope::Star;
        media.hub = ovvc_core::HubPolicy::elected();
        let (mut session, guild, session_id) =
            voice_session_with_policy(&identities, channel, media);
        let now = 1_000;

        for (index, identity) in identities.iter().enumerate() {
            let nonce = index as u64 * 10 + 1;
            session
                .apply(
                    &sign_voice(identity, guild, session_id, voice_join(channel), nonce, now),
                    now,
                )
                .unwrap();
            session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        ControlKind::Claim {
                            priority: Priority::local(),
                        },
                        nonce + 1,
                        now,
                    ),
                    now,
                )
                .unwrap();
        }

        let snapshot = session.snapshot(now);
        let call = &snapshot.voice[0];
        assert_eq!(call.scope, ovvc_core::CallScope::Star);
        assert_eq!(call.hub_policy.mode, ovvc_core::HubMode::Elected);
        assert_eq!(call.members.len(), 3);
        let master = snapshot.election.master.as_ref().expect("a master").author;
        assert_eq!(call.hub, Some(master), "the elected master is the hub");
        assert_eq!(call.topology.hub(), Some(master));
        assert!(
            call.topology.is_star(),
            "three members under a star scope must forward"
        );
        let backup = snapshot.election.backup.as_ref().expect("a backup").author;
        assert_ne!(backup, master);
        for member in &call.members {
            if member.author != master {
                assert_eq!(
                    call.topology.parent_of(member.author),
                    Some(master),
                    "every leaf talks only to the hub"
                );
            }
        }
    }

    #[test]
    fn a_hub_change_advances_the_media_key_epoch() {
        let channel = voice_channel();
        let identities = voice_members(4);
        let mut media = MediaPolicy::standard();
        media.call_scope = ovvc_core::CallScope::Star;
        media.hub = ovvc_core::HubPolicy::elected();
        let (mut session, guild, session_id) =
            voice_session_with_policy(&identities, channel, media);
        let now = 3_000;

        for (index, identity) in identities.iter().enumerate() {
            let nonce = index as u64 * 10 + 1;
            session
                .apply(
                    &sign_voice(identity, guild, session_id, voice_join(channel), nonce, now),
                    now,
                )
                .unwrap();
            session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        ControlKind::Claim {
                            priority: Priority::local(),
                        },
                        nonce + 1,
                        now,
                    ),
                    now,
                )
                .unwrap();
        }
        session
            .apply(
                &sign_voice(
                    &identities[0],
                    guild,
                    session_id,
                    voice_key(channel, 1),
                    100,
                    now,
                ),
                now,
            )
            .unwrap();

        let before = session.snapshot(now);
        let call = &before.voice[0];
        let old_hub = call.hub.expect("a star hub");
        assert_eq!(call.key_epoch, 1);
        assert_eq!(call.topology.leaves().len(), 3);
        assert!(call.topology.is_star());

        // The hub leaves. Four members become three, so the call is still a
        // star, but the hub is a different member and the old leaf-to-hub
        // ciphers cannot be used.
        let hub_index = identities
            .iter()
            .position(|identity| identity.author_id() == old_hub)
            .expect("the hub is a call member");
        session
            .apply(
                &sign_voice(
                    &identities[hub_index],
                    guild,
                    session_id,
                    ControlKind::VoiceLeave { channel_id: channel },
                    200,
                    now,
                ),
                now,
            )
            .unwrap();

        let after = session.snapshot(now);
        let call = &after.voice[0];
        assert_eq!(call.members.len(), 3);
        assert!(call.topology.is_star(), "three members stay a flat star");
        let new_hub = call.hub.expect("a new hub");
        assert_ne!(new_hub, old_hub, "the election promoted another member");
        assert_eq!(session.voice_hub(channel), Some(new_hub));
        assert_eq!(
            call.key_epoch, 2,
            "a hub change advances the epoch every member advertises"
        );
        for member in &call.members {
            if member.author != new_hub {
                assert_eq!(
                    call.topology.parent_of(member.author),
                    Some(new_hub),
                    "every leaf is re-keyed to the new hub"
                );
            }
        }
    }

    #[test]
    fn a_direct_pair_keeps_its_epoch_when_a_coordinator_leaves() {
        let channel = voice_channel();
        let identities = voice_members(2);
        let mut media = MediaPolicy::standard();
        // Even a star scope keeps a direct pair; there is no hub to re-key.
        media.call_scope = ovvc_core::CallScope::Star;
        media.hub = ovvc_core::HubPolicy::elected();
        let (mut session, guild, session_id) =
            voice_session_with_policy(&identities, channel, media);
        let now = 4_000;

        for (index, identity) in identities.iter().enumerate() {
            let nonce = index as u64 * 10 + 1;
            session
                .apply(
                    &sign_voice(identity, guild, session_id, voice_join(channel), nonce, now),
                    now,
                )
                .unwrap();
            session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        ControlKind::Claim {
                            priority: Priority::local(),
                        },
                        nonce + 1,
                        now,
                    ),
                    now,
                )
                .unwrap();
        }
        session
            .apply(
                &sign_voice(
                    &identities[0],
                    guild,
                    session_id,
                    voice_key(channel, 3),
                    100,
                    now,
                ),
                now,
            )
            .unwrap();

        let before = session.snapshot(now);
        assert_eq!(before.voice[0].topology, CallTopology::Mesh);
        assert_eq!(before.voice[0].hub, None);
        let master = before.election.master.as_ref().expect("a master").author;
        let master_index = identities
            .iter()
            .position(|identity| identity.author_id() == master)
            .expect("the master is a call member");

        session
            .apply(
                &sign_voice(
                    &identities[master_index],
                    guild,
                    session_id,
                    ControlKind::VoiceLeave { channel_id: channel },
                    200,
                    now,
                ),
                now,
            )
            .unwrap();

        let after = session.snapshot(now);
        assert_eq!(after.voice[0].topology, CallTopology::Mesh);
        assert_eq!(after.voice[0].hub, None);
        assert_eq!(
            after.voice[0].key_epoch, 3,
            "a mesh or direct pair has no hub to re-key"
        );
    }

    #[test]
    fn a_published_epoch_is_never_moved_backwards() {
        let channel = voice_channel();
        let identities = voice_members(2);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let now = 5_000;
        session
            .apply(
                &sign_voice(
                    &identities[0],
                    guild,
                    session_id,
                    voice_join(channel),
                    1,
                    now,
                ),
                now,
            )
            .unwrap();
        assert!(session.set_voice_epoch(channel, 4));
        assert!(!session.set_voice_epoch(channel, 4));
        assert!(!session.set_voice_epoch(channel, 2));
        assert_eq!(session.voice_epoch(channel), 4);
        assert_eq!(
            session.snapshot(now).voice[0].key_epoch,
            4,
            "the snapshot carries the epoch the daemon recorded"
        );
    }

    #[test]
    fn two_members_join_one_voice_session() {
        let channel = voice_channel();
        let identities = voice_members(2);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let now = 1_000;

        for (index, identity) in identities.iter().enumerate() {
            let effects = session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        voice_join(channel),
                        index as u64 + 1,
                        now,
                    ),
                    now,
                )
                .unwrap();
            assert!(matches!(effects.as_slice(), [SessionEffect::Voice(_)]));
        }

        let snapshot = session.snapshot(now);
        assert_eq!(snapshot.voice.len(), 1);
        let call = &snapshot.voice[0];
        assert_eq!(call.channel_id, channel);
        assert_eq!(call.members.len(), 2);
        assert_eq!(call.topology, CallTopology::Mesh);
        let codec = call
            .codec
            .as_ref()
            .expect("a common profile was negotiated");
        assert_eq!(codec.audio, "opus");
    }

    #[test]
    fn voice_state_updates_and_speaking_transitions_are_visible() {
        let channel = voice_channel();
        let identities = voice_members(2);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let now = 2_000;
        for (index, identity) in identities.iter().enumerate() {
            session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        voice_join(channel),
                        index as u64 + 1,
                        now,
                    ),
                    now,
                )
                .unwrap();
        }

        let alice = &identities[0];
        let effects = session
            .apply(
                &sign_voice(
                    alice,
                    guild,
                    session_id,
                    voice_state(channel, true),
                    10,
                    now,
                ),
                now,
            )
            .unwrap();
        match effects.as_slice() {
            [SessionEffect::Voice(update)] => {
                assert_eq!(
                    update.change,
                    VoiceChange::SpeakingChanged {
                        author: alice.author_id(),
                        speaking: true,
                    }
                );
            }
            other => panic!("expected one voice effect, got {other:?}"),
        }

        let snapshot = session.snapshot(now);
        let member = snapshot.voice[0]
            .members
            .iter()
            .find(|member| member.author == alice.author_id())
            .expect("alice is in the call");
        assert!(member.state.speaking);
    }

    #[test]
    fn voice_leave_removes_members_and_the_last_leave_empties_the_call() {
        let channel = voice_channel();
        let identities = voice_members(2);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let now = 3_000;
        for (index, identity) in identities.iter().enumerate() {
            session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        voice_join(channel),
                        index as u64 + 1,
                        now,
                    ),
                    now,
                )
                .unwrap();
        }

        let alice = &identities[0];
        let bob = &identities[1];
        let effects = session
            .apply(
                &sign_voice(
                    alice,
                    guild,
                    session_id,
                    ControlKind::VoiceLeave {
                        channel_id: channel,
                    },
                    10,
                    now,
                ),
                now,
            )
            .unwrap();
        match effects.as_slice() {
            [SessionEffect::Voice(update)] => {
                assert_eq!(update.change, VoiceChange::Left(alice.author_id()));
                assert_eq!(update.snapshot.members.len(), 1);
            }
            other => panic!("expected one voice effect, got {other:?}"),
        }
        assert_eq!(session.snapshot(now).voice.len(), 1);

        let effects = session
            .apply(
                &sign_voice(
                    bob,
                    guild,
                    session_id,
                    ControlKind::VoiceLeave {
                        channel_id: channel,
                    },
                    11,
                    now,
                ),
                now,
            )
            .unwrap();
        match effects.as_slice() {
            [SessionEffect::Voice(update)] => {
                assert_eq!(update.change, VoiceChange::Left(bob.author_id()));
                assert!(update.snapshot.members.is_empty());
            }
            other => panic!("expected one voice effect, got {other:?}"),
        }
        assert!(session.snapshot(now).voice.is_empty());
    }

    #[test]
    fn a_non_members_voice_join_is_rejected() {
        let channel = voice_channel();
        let identities = voice_members(1);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let mallory = Identity::generate();
        let now = crate::frame::now_ms();
        assert!(matches!(
            session.apply(
                &sign_voice(&mallory, guild, session_id, voice_join(channel), 1, now),
                now
            ),
            Err(ControlError::NotAMember)
        ));
    }

    #[test]
    fn expire_drops_a_silent_voice_member() {
        let channel = voice_channel();
        let identities = voice_members(1);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let now = 4_000;
        let alice = &identities[0];
        session
            .apply(
                &sign_voice(alice, guild, session_id, voice_join(channel), 1, now),
                now,
            )
            .unwrap();

        let later = now + VOICE_MEMBER_TIMEOUT_MS + 1;
        let effects = session.expire(later);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            SessionEffect::Voice(update)
                if update.change == VoiceChange::Expired(alice.author_id())
        )));
        assert!(session.snapshot(later).voice.is_empty());
    }

    #[test]
    fn seven_members_forward_through_one_flat_star() {
        let channel = voice_channel();
        let identities = voice_members(7);
        let authors: Vec<AuthorId> = identities.iter().map(Identity::author_id).collect();
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let now = 5_000;
        for (index, identity) in identities.iter().enumerate() {
            session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        voice_join(channel),
                        index as u64 + 1,
                        now,
                    ),
                    now,
                )
                .unwrap();
        }

        let snapshot = session.snapshot(now);
        let CallTopology::Star { hub, leaves } = &snapshot.voice[0].topology else {
            panic!("seven members must be a flat star");
        };
        assert!(authors.contains(hub));
        assert_eq!(leaves.len(), authors.len() - 1);
        for leaf in leaves {
            assert_ne!(leaf, hub, "the hub is not its own leaf");
            assert!(
                authors.contains(leaf),
                "every leaf is a call member"
            );
            assert_eq!(
                snapshot.voice[0].topology.parent_of(*leaf),
                Some(*hub),
                "every leaf is one hop from the hub"
            );
        }
    }

    #[test]
    fn voice_session_ids_are_deterministic_and_scoped() {
        let guild = GuildId([4; 32]);
        let other_guild = GuildId([5; 32]);
        let channel = ChannelId([9; 16]);
        let other_channel = ChannelId([8; 16]);

        let id = SessionId::for_voice_channel(guild, channel);
        assert_eq!(id, SessionId::for_voice_channel(guild, channel));
        assert_ne!(id, SessionId::for_voice_channel(guild, other_channel));
        assert_ne!(id, SessionId::for_voice_channel(other_guild, channel));
        assert_eq!(id.to_hex().len(), 32);
    }

    #[test]
    fn version_one_frames_are_rejected_after_the_bump() {
        let guild = voice_guild();
        let identity = Identity::generate();
        let frame = ControlFrame::sign(
            &identity,
            guild,
            SessionId::for_guild(guild),
            0,
            ControlKind::Leave,
            1,
        )
        .unwrap();
        let mut old = frame.clone();
        old.version = 1;
        assert!(matches!(
            old.verify(),
            Err(ControlError::UnsupportedVersion(1))
        ));

        // The version 3 bump also fences out version 2, which lacks the RTC
        // signalling kinds.
        old.version = 2;
        assert!(matches!(
            old.verify(),
            Err(ControlError::UnsupportedVersion(2))
        ));

        // The version 4 bump fences out version 3, which lacks `VoiceKey`.
        old.version = 3;
        assert!(matches!(
            old.verify(),
            Err(ControlError::UnsupportedVersion(3))
        ));

        // The version 5 bump fences out version 4, which lacks
        // `VoiceSyncRequest`.
        old.version = 4;
        assert!(matches!(
            old.verify(),
            Err(ControlError::UnsupportedVersion(4))
        ));

        // The version 6 bump fences out version 5, whose `Priority` lacks the
        // capacity, health and reachability fields.
        old.version = 5;
        assert!(matches!(
            old.verify(),
            Err(ControlError::UnsupportedVersion(5))
        ));
    }

    #[test]
    fn voice_frames_must_use_their_channel_session() {
        let channel = voice_channel();
        let other_channel = ChannelId([8; 16]);
        let identities = voice_members(1);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let now = 7_000;

        // A frame naming a different channel than the session derives is
        // rejected even though the signature and guild are valid.
        assert!(matches!(
            session.apply(
                &sign_voice(
                    &identities[0],
                    guild,
                    session_id,
                    voice_join(other_channel),
                    1,
                    now,
                ),
                now
            ),
            Err(ControlError::WrongSession)
        ));

        // A voice frame can never travel on the guild-wide session.
        let mut guild_session = ControlSession::new(
            guild,
            SessionId::for_guild(guild),
            ElectionConfig::default(),
            identities.iter().map(Identity::author_id),
        );
        assert!(matches!(
            guild_session.apply(
                &sign_voice(
                    &identities[0],
                    guild,
                    SessionId::for_guild(guild),
                    voice_join(channel),
                    2,
                    now,
                ),
                now
            ),
            Err(ControlError::WrongSession)
        ));
    }

    #[test]
    fn a_bystander_cannot_claim_the_voice_coordinator_role() {
        let channel = voice_channel();
        let identities = voice_members(2);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let now = 8_000;
        let alice = &identities[0];
        let bob = &identities[1];

        // Only Alice joins the call; Bob is a guild member, not a participant.
        session
            .apply(
                &sign_voice(alice, guild, session_id, voice_join(channel), 1, now),
                now,
            )
            .unwrap();
        session
            .apply(
                &sign_voice(
                    alice,
                    guild,
                    session_id,
                    ControlKind::Claim {
                        priority: Priority::local(),
                    },
                    2,
                    now,
                ),
                now,
            )
            .unwrap();
        assert_eq!(session.role_for(alice.author_id()), Role::Master);

        assert!(matches!(
            session.apply(
                &sign_voice(
                    bob,
                    guild,
                    session_id,
                    ControlKind::Claim {
                        priority: Priority {
                            trust_tier: 255,
                            ..Priority::local()
                        },
                    },
                    3,
                    now,
                ),
                now
            ),
            Err(ControlError::Invalid(_))
        ));
        assert!(matches!(
            session.apply(
                &sign_voice(
                    bob,
                    guild,
                    session_id,
                    ControlKind::Heartbeat {
                        role: Role::Master,
                        priority: Priority::local(),
                    },
                    4,
                    now,
                ),
                now
            ),
            Err(ControlError::Invalid(_))
        ));
        assert_eq!(session.role_for(bob.author_id()), Role::Member);
        assert_eq!(session.role_for(alice.author_id()), Role::Master);
    }

    #[test]
    fn an_expired_member_leaves_the_election() {
        let channel = voice_channel();
        let identities = voice_members(2);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let now = 9_000;
        for (index, identity) in identities.iter().enumerate() {
            session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        voice_join(channel),
                        index as u64 + 1,
                        now,
                    ),
                    now,
                )
                .unwrap();
            session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        ControlKind::Claim {
                            priority: Priority::local(),
                        },
                        100 + index as u64,
                        now,
                    ),
                    now,
                )
                .unwrap();
        }
        assert_eq!(session.snapshot(now).election.candidates.len(), 2);

        let later = now + VOICE_MEMBER_TIMEOUT_MS + 1;
        session.expire(later);

        let snapshot = session.snapshot(later);
        assert!(snapshot.voice.is_empty(), "the call should be empty");
        assert!(
            snapshot.election.candidates.is_empty(),
            "timed-out members are still coordinator candidates"
        );
        assert!(snapshot.election.master.is_none());
        assert!(snapshot.election.backup.is_none());
    }

    #[test]
    fn leaving_the_guild_drops_the_voice_claim() {
        let channel = voice_channel();
        let identities = voice_members(2);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let now = 9_500;
        for (index, identity) in identities.iter().enumerate() {
            session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        voice_join(channel),
                        index as u64 + 1,
                        now,
                    ),
                    now,
                )
                .unwrap();
            session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        ControlKind::Claim {
                            priority: Priority::local(),
                        },
                        100 + index as u64,
                        now,
                    ),
                    now,
                )
                .unwrap();
        }
        let departing = identities[1].author_id();

        // The guild roster no longer contains the second member.
        session.replace_members([identities[0].author_id()]);

        let snapshot = session.snapshot(now);
        assert!(
            snapshot
                .voice
                .iter()
                .flat_map(|call| call.members.iter().map(|member| member.author))
                .all(|author| author != departing),
            "the departing member is still in the call"
        );
        assert!(
            snapshot
                .election
                .candidates
                .iter()
                .all(|candidate| candidate.author != departing),
            "the departing member is still a coordinator candidate"
        );
    }

    #[test]
    fn removing_a_voice_author_drops_its_claim() {
        let channel = voice_channel();
        let identities = voice_members(2);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let now = 10_000;
        for (index, identity) in identities.iter().enumerate() {
            session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        voice_join(channel),
                        index as u64 + 1,
                        now,
                    ),
                    now,
                )
                .unwrap();
            session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        ControlKind::Claim {
                            priority: Priority::local(),
                        },
                        100 + index as u64,
                        now,
                    ),
                    now,
                )
                .unwrap();
        }

        let alice = identities[0].author_id();
        let effects = session.remove_voice_author(alice, now + 1);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            SessionEffect::Voice(update)
                if update.change == VoiceChange::Expired(alice)
        )));

        let snapshot = session.snapshot(now + 1);
        assert!(
            snapshot
                .election
                .candidates
                .iter()
                .all(|candidate| candidate.author != alice),
            "a departed member is still a candidate"
        );
        assert_ne!(
            snapshot
                .election
                .master
                .as_ref()
                .map(|candidate| candidate.author),
            Some(alice)
        );
        assert_ne!(
            snapshot
                .election
                .backup
                .as_ref()
                .map(|candidate| candidate.author),
            Some(alice)
        );
    }

    fn call_members_in_a_voice_session(
        count: usize,
        now: u64,
    ) -> (ControlSession, GuildId, SessionId, ChannelId, Vec<Identity>) {
        let channel = voice_channel();
        let identities = voice_members(count);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        for (index, identity) in identities.iter().enumerate() {
            session
                .apply(
                    &sign_voice(
                        identity,
                        guild,
                        session_id,
                        voice_join(channel),
                        index as u64 + 1,
                        now,
                    ),
                    now,
                )
                .unwrap();
        }
        (session, guild, session_id, channel, identities)
    }

    #[test]
    fn call_members_can_exchange_rtc_signals_without_changing_session_state() {
        let now = 11_000;
        let (mut session, guild, session_id, channel, identities) =
            call_members_in_a_voice_session(2, now);
        let alice = &identities[0];
        let bob = identities[1].author_id();
        let before = session.snapshot(now);

        let kinds = [
            ControlKind::RtcOffer {
                channel_id: channel,
                to: bob,
                sdp: "v=0\r\na=fingerprint:sha-256 AA:BB\r\n".to_string(),
            },
            ControlKind::RtcAnswer {
                channel_id: channel,
                to: bob,
                sdp: "v=0\r\n".to_string(),
            },
            ControlKind::RtcCandidate {
                channel_id: channel,
                to: bob,
                candidate: "candidate:1 1 udp 1 127.0.0.1 5000 typ host".to_string(),
                mid: Some("audio".to_string()),
            },
            ControlKind::RtcBye {
                channel_id: channel,
                to: bob,
            },
        ];
        for (index, kind) in kinds.into_iter().enumerate() {
            let effects = session
                .apply(
                    &sign_voice(alice, guild, session_id, kind, 200 + index as u64, now),
                    now,
                )
                .unwrap();
            assert!(
                effects.is_empty(),
                "an RTC signal must not change session state"
            );
        }

        assert_eq!(
            session.snapshot(now),
            before,
            "the session keeps no SDP or ICE state"
        );
    }

    #[test]
    fn rtc_signalling_requires_the_sender_to_be_in_the_call() {
        let now = 12_000;
        // Three guild members; only the first joins the call.
        let channel = voice_channel();
        let identities = voice_members(3);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let alice = &identities[0];
        let bob = &identities[1];
        session
            .apply(
                &sign_voice(alice, guild, session_id, voice_join(channel), 1, now),
                now,
            )
            .unwrap();

        let offer = ControlKind::RtcOffer {
            channel_id: channel,
            to: alice.author_id(),
            sdp: "v=0\r\n".to_string(),
        };
        // Bob is a guild member but is not in the call.
        assert!(matches!(
            session.apply(
                &sign_voice(bob, guild, session_id, offer.clone(), 2, now),
                now
            ),
            Err(ControlError::Invalid(_))
        ));

        // Mallory is not even a guild member.
        let mallory = Identity::generate();
        assert!(matches!(
            session.apply(&sign_voice(&mallory, guild, session_id, offer, 3, now), now),
            Err(ControlError::NotAMember)
        ));
    }

    #[test]
    fn an_rtc_signal_must_target_another_guild_member() {
        let now = 13_000;
        let (mut session, guild, session_id, channel, identities) =
            call_members_in_a_voice_session(2, now);
        let alice = &identities[0];
        let bob = identities[1].author_id();
        let outside = Identity::generate().author_id();

        let to_stranger = ControlKind::RtcOffer {
            channel_id: channel,
            to: outside,
            sdp: "v=0\r\n".to_string(),
        };
        assert!(matches!(
            session.apply(
                &sign_voice(alice, guild, session_id, to_stranger, 300, now),
                now
            ),
            Err(ControlError::Invalid(_))
        ));

        let to_self = ControlKind::RtcCandidate {
            channel_id: channel,
            to: alice.author_id(),
            candidate: "candidate:1 1 udp 1 127.0.0.1 5000 typ host".to_string(),
            mid: None,
        };
        assert!(matches!(
            session.apply(
                &sign_voice(alice, guild, session_id, to_self, 301, now),
                now
            ),
            Err(ControlError::Invalid(_))
        ));

        // The same signal to the other call member is accepted.
        let to_bob = ControlKind::RtcBye {
            channel_id: channel,
            to: bob,
        };
        session
            .apply(&sign_voice(alice, guild, session_id, to_bob, 302, now), now)
            .unwrap();
    }

    fn voice_key(channel: ChannelId, epoch: u32) -> ControlKind {
        ControlKind::VoiceKey {
            channel_id: channel,
            epoch,
            public_key: [9; 32],
        }
    }

    #[test]
    fn a_call_member_can_publish_a_media_key_without_storing_it() {
        let now = 14_000;
        let (mut session, guild, session_id, channel, identities) =
            call_members_in_a_voice_session(2, now);
        let alice = &identities[0];
        assert_eq!(session.voice_epoch(channel), 0);

        let effects = session
            .apply(
                &sign_voice(alice, guild, session_id, voice_key(channel, 1), 400, now),
                now,
            )
            .unwrap();
        // The key itself is not stored, but the announced epoch is recorded so
        // a snapshot can show it, and the media change is a visible update.
        assert_eq!(effects.len(), 1, "the epoch change is a media update");
        let SessionEffect::Voice(update) = &effects[0] else {
            panic!("expected a voice update, got {:?}", effects[0]);
        };
        assert_eq!(update.change, VoiceChange::MediaChanged);
        assert_eq!(update.snapshot.key_epoch, 1);
        assert_eq!(session.voice_epoch(channel), 1);

        let older = session
            .apply(
                &sign_voice(alice, guild, session_id, voice_key(channel, 1), 401, now),
                now,
            )
            .unwrap();
        assert!(older.is_empty(), "an older epoch is not a new update");
    }

    #[test]
    fn a_media_key_requires_the_sender_to_be_in_the_call() {
        let now = 15_000;
        let channel = voice_channel();
        let identities = voice_members(3);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let alice = &identities[0];
        let bob = &identities[1];
        session
            .apply(
                &sign_voice(alice, guild, session_id, voice_join(channel), 1, now),
                now,
            )
            .unwrap();

        // Bob is a guild member but never joined the call.
        assert!(matches!(
            session.apply(
                &sign_voice(bob, guild, session_id, voice_key(channel, 1), 2, now),
                now
            ),
            Err(ControlError::Invalid(_))
        ));

        // Mallory is not even a guild member.
        let mallory = Identity::generate();
        assert!(matches!(
            session.apply(
                &sign_voice(&mallory, guild, session_id, voice_key(channel, 1), 3, now),
                now
            ),
            Err(ControlError::NotAMember)
        ));
    }

    #[test]
    fn a_media_key_from_another_guild_or_session_is_rejected() {
        let now = 16_000;
        let (mut session, guild, session_id, channel, identities) =
            call_members_in_a_voice_session(1, now);
        let alice = &identities[0];

        // A frame built for another guild's call session.
        let other_guild = GuildId([5; 32]);
        let other_guild_session = SessionId::for_voice_channel(other_guild, channel);
        assert!(matches!(
            session.apply(
                &sign_voice(
                    alice,
                    other_guild,
                    other_guild_session,
                    voice_key(channel, 1),
                    500,
                    now,
                ),
                now
            ),
            Err(ControlError::WrongSession)
        ));

        // The same guild, but a different call session.
        let other_channel = ChannelId([8; 16]);
        let other_channel_session = SessionId::for_voice_channel(guild, other_channel);
        assert!(matches!(
            session.apply(
                &sign_voice(
                    alice,
                    guild,
                    other_channel_session,
                    voice_key(other_channel, 1),
                    501,
                    now,
                ),
                now
            ),
            Err(ControlError::WrongSession)
        ));

        // The valid frame on this session is still accepted.
        session
            .apply(
                &sign_voice(alice, guild, session_id, voice_key(channel, 1), 502, now),
                now,
            )
            .unwrap();
    }

    #[test]
    fn a_sync_request_from_outside_the_call_changes_no_state() {
        let now = 17_000;
        // Three guild members; only Alice is in the call.
        let channel = voice_channel();
        let identities = voice_members(3);
        let (mut session, guild, session_id) = voice_session(&identities, channel);
        let alice = &identities[0];
        let bob = &identities[1];
        session
            .apply(
                &sign_voice(alice, guild, session_id, voice_join(channel), 1, now),
                now,
            )
            .unwrap();
        let before = session.snapshot(now);

        // Bob is not in the call locally, which is exactly the late joiner's
        // view. His request must be accepted and must not alter the call.
        let effects = session
            .apply(
                &sign_voice(
                    bob,
                    guild,
                    session_id,
                    ControlKind::VoiceSyncRequest {
                        channel_id: channel,
                    },
                    2,
                    now,
                ),
                now,
            )
            .unwrap();
        assert!(effects.is_empty(), "a sync request changes no state");
        assert_eq!(session.snapshot(now), before);

        // Mallory is not a guild member and is still refused.
        let mallory = Identity::generate();
        assert!(matches!(
            session.apply(
                &sign_voice(
                    &mallory,
                    guild,
                    session_id,
                    ControlKind::VoiceSyncRequest {
                        channel_id: channel,
                    },
                    3,
                    now,
                ),
                now
            ),
            Err(ControlError::NotAMember)
        ));
    }
}
