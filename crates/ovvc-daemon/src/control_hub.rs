//! Per-guild control-plane state for the daemon.
//!
//! Durable state stays in the signed event log. Everything here is ephemeral:
//! if the daemon exits, the sessions vanish and peers rebuild them from fresh
//! `Join` and `Claim` frames. The hub signs its own frames with the identity it
//! already owns, applies them locally first, and hands the caller the frames
//! that still need to go out over the network.

use std::collections::HashMap;

use ovvc_control::{
    Capabilities, ControlError, ControlFrame, ControlKind, ControlSession, ControlSnapshot,
    ElectionConfig, NonceSource, PresenceStatus, Priority, ProfileSet, SessionEffect, SessionId,
    VoiceMemberState,
};
use ovvc_core::{AuthorId, ChannelId, GuildId, Identity, MediaPolicy};
use ovvc_ipc::DaemonEvent;

pub struct HubOutcome {
    pub events: Vec<DaemonEvent>,
    pub outgoing: Vec<ControlFrame>,
    pub snapshot: ControlSnapshot,
}

#[derive(Debug)]
struct SessionState {
    session: ControlSession,
    nonce: NonceSource,
}

pub struct ControlHub {
    identity: Identity,
    /// The commune media policy that every session's voice calls use. Until
    /// signed policy events land, the daemon's configuration is the
    /// provisional per-commune policy, so every peer started with the same
    /// profile derives the same call scope and hub choice.
    media_policy: MediaPolicy,
    election_config: ElectionConfig,
    /// The latest measured local capacity, before the per-call member count is
    /// stamped on it. The daemon replaces this every control tick from real
    /// CPU, memory, transport and failure samples.
    local_priority: Priority,
    sessions: HashMap<(GuildId, SessionId), SessionState>,
}

impl ControlHub {
    /// The shipped policy, used by tests and by a caller with no policy to
    /// apply. Production construction goes through [`with_media_policy`] so
    /// the commune scope reaches every session.
    ///
    /// [`with_media_policy`]: ControlHub::with_media_policy
    #[cfg(test)]
    pub fn new(identity: Identity) -> Self {
        Self::with_media_policy(identity, MediaPolicy::standard())
    }

    /// A hub whose calls use `media_policy` for their scope and hub choice.
    pub fn with_media_policy(identity: Identity, media_policy: MediaPolicy) -> Self {
        let election_config = ElectionConfig::default().with_hub_policy(&media_policy.hub);
        Self {
            identity,
            media_policy,
            election_config,
            local_priority: Priority::local(),
            sessions: HashMap::new(),
        }
    }

    /// Replace the measured local capacity advertised in `Claim` and
    /// `Heartbeat` frames.
    ///
    /// The daemon samples CPU, memory, transport throughput, RTT, loss, active
    /// calls and recent failures, then hands the result here. The per-call
    /// `member_count` is stamped on a copy when a frame is signed, so one
    /// sample serves every call.
    pub fn set_local_priority(&mut self, priority: Priority) {
        self.local_priority = priority;
    }

    /// The capacity sample currently advertised, without a call-size stamp.
    #[cfg(test)]
    pub fn local_priority(&self) -> Priority {
        self.local_priority
    }

    /// The local capacity with the current call size recorded. The count is
    /// informational only, so it never changes the ranking.
    fn local_priority_for_call(&self, call_size: usize) -> Priority {
        priority_for_call(self.local_priority, call_size)
    }

    fn ensure_session(
        &mut self,
        guild_id: GuildId,
        session_id: SessionId,
        members: &[AuthorId],
    ) -> &mut SessionState {
        let key = (guild_id, session_id);
        let media_policy = self.media_policy.clone();
        let election_config = self.election_config;
        let entry = self.sessions.entry(key).or_insert_with(|| SessionState {
            session: ControlSession::with_policy(
                guild_id,
                session_id,
                election_config,
                members.iter().copied(),
                media_policy,
            ),
            nonce: NonceSource::new(),
        });
        entry.session.replace_members(members.iter().copied());
        entry
    }

    pub fn snapshot(
        &mut self,
        guild_id: GuildId,
        session_id: SessionId,
        members: &[AuthorId],
        now: u64,
    ) -> ControlSnapshot {
        self.ensure_session(guild_id, session_id, members)
            .session
            .snapshot(now)
    }

    pub fn join(
        &mut self,
        guild_id: GuildId,
        session_id: SessionId,
        members: &[AuthorId],
        display_name: &str,
        now: u64,
    ) -> Result<HubOutcome, ControlError> {
        let frame = self.sign(
            guild_id,
            session_id,
            members,
            ControlKind::Join {
                display_name: display_name.to_string(),
                capabilities: Capabilities::all(),
            },
        )?;
        self.apply_local(frame, members, now)
    }

    pub fn leave(
        &mut self,
        guild_id: GuildId,
        session_id: SessionId,
        members: &[AuthorId],
        now: u64,
    ) -> Result<HubOutcome, ControlError> {
        let frame = self.sign(guild_id, session_id, members, ControlKind::Leave)?;
        let mut outcome = self.apply_local(frame, members, now)?;
        // A guild-level Leave also drops the author from every call in this
        // guild. The voice plane lives in per-channel sessions, so without
        // this pass a person who left the guild would linger in a call until
        // the 20 s member lease expired.
        let author = self.identity.author_id();
        let mut voice_events = self.remove_voice_author(guild_id, author, now);
        outcome.events.append(&mut voice_events);
        Ok(outcome)
    }

    /// Join the call on `channel_id`. The first member to arrive also claims
    /// the voice session, which gives the media master/backup election its
    /// candidates; later joins merge their own claim into the same outcome.
    // The call's own flags are deliberately separate arguments: the IPC op and
    // this method mirror the frame fields one for one.
    #[allow(clippy::too_many_arguments)]
    pub fn voice_join(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        members: &[AuthorId],
        muted: bool,
        deafened: bool,
        push_to_talk: bool,
        now: u64,
    ) -> Result<HubOutcome, ControlError> {
        let session_id = SessionId::for_voice_channel(guild_id, channel_id);
        let frame = self.sign(
            guild_id,
            session_id,
            members,
            ControlKind::VoiceJoin {
                channel_id,
                profiles: ProfileSet::ALL,
                muted,
                deafened,
                push_to_talk,
                video: false,
                screen_share: false,
            },
        )?;
        let joined = self.apply_local(frame, members, now)?;
        // The claim is only useful if the join actually landed this identity
        // in the call; a snapshot check proves that, where a roster check
        // proved nothing.
        let local = self.identity.author_id();
        let in_call = joined.snapshot.voice.iter().any(|call| {
            call.channel_id == channel_id
                && call.members.iter().any(|member| member.author == local)
        });
        if !in_call {
            return Ok(joined);
        }
        let call_size = joined
            .snapshot
            .voice
            .first()
            .map_or(1, |call| call.members.len());
        let priority = self.local_priority_for_call(call_size);
        let claim = self.sign(
            guild_id,
            session_id,
            members,
            ControlKind::Claim { priority },
        )?;
        let claimed = self.apply_local(claim, members, now)?;
        Ok(merge_outcomes(joined, claimed))
    }

    pub fn voice_leave(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        members: &[AuthorId],
        now: u64,
    ) -> Result<HubOutcome, ControlError> {
        let session_id = SessionId::for_voice_channel(guild_id, channel_id);
        let frame = self.sign(
            guild_id,
            session_id,
            members,
            ControlKind::VoiceLeave { channel_id },
        )?;
        self.apply_local(frame, members, now)
    }

    /// Re-announce the local member's `VoiceJoin` for a call it is already in.
    ///
    /// A member that joined before a late guild joiner cannot know the joiner
    /// is missing its announcement, so it resends its own join when the
    /// joiner's `VoiceJoin` arrives, or when a sync request asks. The media
    /// plane treats a repeat join as a refresh of the existing member, so this
    /// is idempotent. Returns `None` when the local author is not in the call:
    /// a bystander never announces itself into a call it never joined.
    pub fn reannounce_voice_join(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        members: &[AuthorId],
        now: u64,
    ) -> Result<Option<HubOutcome>, ControlError> {
        let session_id = SessionId::for_voice_channel(guild_id, channel_id);
        let local = self.identity.author_id();
        let local_member = {
            let state = self.ensure_session(guild_id, session_id, members);
            state
                .session
                .snapshot(now)
                .voice
                .into_iter()
                .find(|call| call.channel_id == channel_id)
                .and_then(|call| {
                    call.members
                        .into_iter()
                        .find(|member| member.author == local)
                })
        };
        let Some(member) = local_member else {
            return Ok(None);
        };
        let frame = self.sign(
            guild_id,
            session_id,
            members,
            ControlKind::VoiceJoin {
                channel_id,
                profiles: member.profiles,
                muted: member.state.muted,
                deafened: member.state.deafened,
                push_to_talk: member.state.push_to_talk,
                video: member.state.video,
                screen_share: member.state.screen_share,
            },
        )?;
        self.apply_local(frame, members, now).map(Some)
    }

    /// Ask the members of a live call to re-announce their membership and
    /// current epoch key. The session records no state for this frame.
    pub fn voice_sync_request(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        members: &[AuthorId],
        now: u64,
    ) -> Result<HubOutcome, ControlError> {
        let session_id = SessionId::for_voice_channel(guild_id, channel_id);
        let frame = self.sign(
            guild_id,
            session_id,
            members,
            ControlKind::VoiceSyncRequest { channel_id },
        )?;
        self.apply_local(frame, members, now)
    }

    /// Sign one RTC signalling frame on the channel's voice session and apply
    /// it locally, so the hub's own replay guard and membership checks see it
    /// before it goes on the wire.
    ///
    /// The caller has already built an `Rtc*` kind addressed to one peer; the
    /// session layer rejects a target that is not another guild member.
    pub fn rtc_signal(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        members: &[AuthorId],
        kind: ControlKind,
        now: u64,
    ) -> Result<HubOutcome, ControlError> {
        let session_id = SessionId::for_voice_channel(guild_id, channel_id);
        let frame = self.sign(guild_id, session_id, members, kind)?;
        self.apply_local(frame, members, now)
    }

    /// Sign one media-key frame for the channel's voice session and apply it
    /// locally, so the hub's own replay guard and membership checks see it
    /// before it goes on the wire.
    ///
    /// The frame is broadcast to the call: every member installs the key for
    /// its author. A non-member's session accepts it without effect, and the
    /// daemon ignores a key for a call it is not in.
    pub fn publish_voice_key(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        members: &[AuthorId],
        epoch: u32,
        public_key: [u8; 32],
        now: u64,
    ) -> Result<HubOutcome, ControlError> {
        let session_id = SessionId::for_voice_channel(guild_id, channel_id);
        let frame = self.sign(
            guild_id,
            session_id,
            members,
            ControlKind::VoiceKey {
                channel_id,
                epoch,
                public_key,
            },
        )?;
        self.apply_local(frame, members, now)
    }

    pub fn set_voice_state(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        members: &[AuthorId],
        state: VoiceMemberState,
        now: u64,
    ) -> Result<HubOutcome, ControlError> {
        let session_id = SessionId::for_voice_channel(guild_id, channel_id);
        let frame = self.sign(
            guild_id,
            session_id,
            members,
            ControlKind::VoiceState {
                channel_id,
                muted: state.muted,
                deafened: state.deafened,
                push_to_talk: state.push_to_talk,
                speaking: state.speaking,
                video: state.video,
                screen_share: state.screen_share,
            },
        )?;
        self.apply_local(frame, members, now)
    }

    /// The control snapshot for the channel's voice session. Its `voice` list
    /// is empty once the call is removed, which the daemon reports as `None`.
    pub fn voice_snapshot(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        members: &[AuthorId],
        now: u64,
    ) -> ControlSnapshot {
        self.snapshot(
            guild_id,
            SessionId::for_voice_channel(guild_id, channel_id),
            members,
            now,
        )
    }

    /// Remove `author` from every voice call in `guild_id` and turn the
    /// resulting media changes into frontend events.
    fn remove_voice_author(
        &mut self,
        guild_id: GuildId,
        author: AuthorId,
        now: u64,
    ) -> Vec<DaemonEvent> {
        let identity = self.identity.clone();
        let keys: Vec<(GuildId, SessionId)> = self
            .sessions
            .keys()
            .copied()
            .filter(|(session_guild, _)| *session_guild == guild_id)
            .collect();
        let mut events = Vec::new();
        for key in keys {
            let state = self
                .sessions
                .get_mut(&key)
                .expect("session key came from this map");
            let effects = state.session.remove_voice_author(author, now);
            if effects.is_empty() {
                continue;
            }
            let (mut produced, _) = effects_for(&identity, state, key.0, key.1, effects, now);
            events.append(&mut produced);
        }
        events
    }

    pub fn set_presence(
        &mut self,
        guild_id: GuildId,
        session_id: SessionId,
        members: &[AuthorId],
        status: PresenceStatus,
        now: u64,
    ) -> Result<HubOutcome, ControlError> {
        // Presence is refreshed by every later frame; a deadline keeps a
        // crashed frontend from looking online forever.
        let until_ms = if status == PresenceStatus::Offline {
            0
        } else {
            now.saturating_add(60_000)
        };
        let frame = self.sign(
            guild_id,
            session_id,
            members,
            ControlKind::Presence { status, until_ms },
        )?;
        self.apply_local(frame, members, now)
    }

    pub fn set_typing(
        &mut self,
        guild_id: GuildId,
        session_id: SessionId,
        members: &[AuthorId],
        channel_id: ovvc_core::ChannelId,
        typing: bool,
        now: u64,
    ) -> Result<HubOutcome, ControlError> {
        // The frame carries an absolute expiry so a lost `false` cannot leave
        // the indicator stuck on another peer.
        let until_ms = if typing { now.saturating_add(6_000) } else { 0 };
        let frame = self.sign(
            guild_id,
            session_id,
            members,
            ControlKind::Typing {
                channel_id,
                until_ms,
            },
        )?;
        self.apply_local(frame, members, now)
    }

    /// Apply a frame received from a peer.
    pub fn on_remote(
        &mut self,
        frame: &ControlFrame,
        members: &[AuthorId],
        now: u64,
    ) -> Result<HubOutcome, ControlError> {
        let identity = self.identity.clone();
        let state = self.ensure_session(frame.guild_id, frame.session_id, members);
        let effects = state.session.apply(frame, now)?;
        let snapshot = state.session.snapshot(now);
        let (events, outgoing) = effects_for(
            &identity,
            state,
            frame.guild_id,
            frame.session_id,
            effects,
            now,
        );
        Ok(HubOutcome {
            events,
            outgoing,
            snapshot,
        })
    }

    /// Expire typing markers, leases and stale presence in every session.
    pub fn expire(&mut self, now: u64) -> Vec<DaemonEvent> {
        let identity = self.identity.clone();
        let mut events = Vec::new();
        for ((guild_id, session_id), state) in &mut self.sessions {
            let effects = state.session.expire(now);
            let (mut produced, _) =
                effects_for(&identity, state, *guild_id, *session_id, effects, now);
            events.append(&mut produced);
        }
        events
    }

    /// Sign and apply the periodic heartbeats this daemon owes, returning the
    /// frames the caller must put on the wire.
    ///
    /// A `VoiceHeartbeat` keeps every call the local identity is in from being
    /// timed out by the media plane. A coordinator `Heartbeat` renews the
    /// election lease, so a live master is not replaced by its backup every
    /// `lease_ms + takeover_grace_ms`.
    pub fn heartbeat(&mut self, now: u64) -> Result<Vec<ControlFrame>, ControlError> {
        let identity = self.identity.clone();
        let author = identity.author_id();
        let keys: Vec<(GuildId, SessionId)> = self.sessions.keys().copied().collect();
        let mut outgoing = Vec::new();
        for key in keys {
            let state = self
                .sessions
                .get_mut(&key)
                .expect("session key came from this map");
            for channel_id in state.session.voice_channels_with(author) {
                let frame = ControlFrame::sign(
                    &identity,
                    key.0,
                    key.1,
                    state.session.election.term(),
                    ControlKind::VoiceHeartbeat { channel_id },
                    state.nonce.next(),
                )?;
                // Applying it locally refreshes this daemon's own member
                // lease as well.
                state.session.apply(&frame, now)?;
                outgoing.push(frame);
            }
            // Every call member refreshes its priority, not only the master
            // and backup. A plain member's one-shot `Claim` at join can be
            // lost or arrive before a later peer is connected, and then the
            // election would compute the backup from a partial roster. The
            // lease is still renewed only by the master's heartbeat.
            let role = state.session.role_for(author);
            if state.session.voice_call_size() > 0 && state.session.may_coordinate(author) {
                // A free function rather than a `&self` helper: `state`
                // already holds the session map, and borrowing all of `self`
                // here would conflict. The priority field is copied first.
                let priority = priority_for_call(self.local_priority, state.session.voice_call_size());
                let frame = ControlFrame::sign(
                    &identity,
                    key.0,
                    key.1,
                    state.session.election.term(),
                    ControlKind::Heartbeat { role, priority },
                    state.nonce.next(),
                )?;
                state.session.apply(&frame, now)?;
                outgoing.push(frame);
            }
        }
        Ok(outgoing)
    }

    fn sign(
        &mut self,
        guild_id: GuildId,
        session_id: SessionId,
        members: &[AuthorId],
        kind: ControlKind,
    ) -> Result<ControlFrame, ControlError> {
        let identity = self.identity.clone();
        let state = self.ensure_session(guild_id, session_id, members);
        ControlFrame::sign(
            &identity,
            guild_id,
            session_id,
            state.session.election.term(),
            kind,
            state.nonce.next(),
        )
    }

    fn apply_local(
        &mut self,
        frame: ControlFrame,
        members: &[AuthorId],
        now: u64,
    ) -> Result<HubOutcome, ControlError> {
        let identity = self.identity.clone();
        let guild_id = frame.guild_id;
        let session_id = frame.session_id;
        let state = self.ensure_session(guild_id, session_id, members);
        let effects = state.session.apply(&frame, now)?;
        let snapshot = state.session.snapshot(now);
        let (mut events, mut outgoing) =
            effects_for(&identity, state, guild_id, session_id, effects, now);
        // The frame itself goes to every other frontend attached to this
        // daemon, then over the network by the caller.
        events.push(DaemonEvent::Control {
            guild_id,
            session_id,
            frame: frame.clone(),
        });
        outgoing.push(frame);
        Ok(HubOutcome {
            events,
            outgoing,
            snapshot,
        })
    }
}

fn priority_for_call(mut priority: Priority, call_size: usize) -> Priority {
    priority.member_count = if call_size > u8::MAX as usize {
        u8::MAX
    } else {
        call_size as u8
    };
    priority
}

fn merge_outcomes(mut first: HubOutcome, second: HubOutcome) -> HubOutcome {
    first.events.extend(second.events);
    first.outgoing.extend(second.outgoing);
    first.snapshot = second.snapshot;
    first
}

fn effects_for(
    identity: &Identity,
    state: &SessionState,
    guild_id: GuildId,
    session_id: SessionId,
    effects: Vec<SessionEffect>,
    now: u64,
) -> (Vec<DaemonEvent>, Vec<ControlFrame>) {
    let mut events = Vec::new();
    let mut outgoing = Vec::new();
    let mut snapshot_needed = false;
    for effect in effects {
        match effect {
            SessionEffect::ElectionChanged(_) | SessionEffect::StateChanged { .. } => {
                snapshot_needed = true;
            }
            SessionEffect::Presence { author, status, .. } => {
                events.push(DaemonEvent::Presence {
                    guild_id,
                    author,
                    status,
                });
            }
            SessionEffect::Typing {
                author,
                channel_id,
                until_ms,
            } => {
                events.push(DaemonEvent::Typing {
                    guild_id,
                    channel_id,
                    author,
                    until_ms,
                });
            }
            SessionEffect::TypingEnded { author, channel_id } => {
                events.push(DaemonEvent::TypingEnded {
                    guild_id,
                    channel_id,
                    author,
                });
            }
            SessionEffect::Voice(update) => {
                events.push(DaemonEvent::Voice {
                    guild_id,
                    channel_id: update.channel_id,
                    change: update.change,
                    snapshot: Some(update.snapshot),
                });
            }
            SessionEffect::SendPong { nonce } => {
                if let Ok(frame) = ControlFrame::sign(
                    identity,
                    guild_id,
                    session_id,
                    state.session.election.term(),
                    ControlKind::Pong { nonce },
                    state.nonce.next(),
                ) {
                    outgoing.push(frame);
                }
            }
        }
    }
    if snapshot_needed {
        events.push(DaemonEvent::ControlState {
            guild_id,
            snapshot: state.session.snapshot(now),
        });
    }
    (events, outgoing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_control::now_ms;
    use ovvc_control::{ControlFrame, ControlKind, Priority};
    use ovvc_core::{CallScope, HubMode, HubPolicy, Identity};

    fn media_policy(scope: CallScope) -> MediaPolicy {
        let mut policy = MediaPolicy::standard();
        policy.call_scope = scope;
        if scope == CallScope::Mesh {
            policy.hub = HubPolicy::none();
        }
        policy
    }

    fn voice_frame(
        identity: &Identity,
        guild: GuildId,
        session_id: SessionId,
        kind: ControlKind,
        nonce: u64,
        now: u64,
    ) -> ControlFrame {
        ControlFrame::sign_at(identity, guild, session_id, 0, kind, nonce, now).unwrap()
    }

    #[test]
    fn a_measured_local_priority_reaches_the_heartbeat() {
        let local = Identity::generate();
        let guild = GuildId([8; 32]);
        let channel = ChannelId([4; 16]);
        let members = vec![local.author_id()];
        let mut hub = ControlHub::with_media_policy(local.clone(), media_policy(CallScope::Star));
        let start = now_ms();
        hub.voice_join(guild, channel, &members, false, false, false, start)
            .expect("local join");

        let mut measured = Priority::local();
        measured.cpu_load_pct = 77;
        measured.mem_used_pct = 66;
        measured.recent_failures = 2;
        measured.rtt_ms = 123;
        hub.set_local_priority(measured);
        assert_eq!(hub.local_priority().cpu_load_pct, 77);

        let frames = hub.heartbeat(start + 1_000).expect("heartbeats sign");
        let heartbeat = frames
            .iter()
            .find_map(|frame| match &frame.kind {
                ControlKind::Heartbeat { priority, .. } => Some(*priority),
                _ => None,
            })
            .expect("a voice heartbeat");
        assert_eq!(heartbeat.cpu_load_pct, 77, "the sample reached the frame");
        assert_eq!(heartbeat.mem_used_pct, 66);
        assert_eq!(heartbeat.recent_failures, 2);
        assert_eq!(heartbeat.rtt_ms, 123);
        assert_eq!(heartbeat.member_count, 1, "the call size is stamped on");
    }

    #[test]
    fn a_star_policy_elects_a_hub_and_a_backup_for_three_members() {
        let local = Identity::generate();
        let bob = Identity::generate();
        let carol = Identity::generate();
        let guild = GuildId([7; 32]);
        let channel = ChannelId([3; 16]);
        let session_id = SessionId::for_voice_channel(guild, channel);
        let members = vec![local.author_id(), bob.author_id(), carol.author_id()];
        let mut hub = ControlHub::with_media_policy(local.clone(), media_policy(CallScope::Star));
        let start = now_ms();

        // The local identity joins and claims through the same path a real
        // frontend uses.
        hub.voice_join(guild, channel, &members, false, false, false, start)
            .expect("local join");

        // Two remote members join and claim, each over a signed frame.
        for (index, remote) in [&bob, &carol].into_iter().enumerate() {
            let join = voice_frame(
                remote,
                guild,
                session_id,
                ControlKind::VoiceJoin {
                    channel_id: channel,
                    profiles: ovvc_control::ProfileSet::ALL,
                    muted: false,
                    deafened: false,
                    push_to_talk: false,
                    video: false,
                    screen_share: false,
                },
                index as u64 * 10 + 1,
                start,
            );
            hub.on_remote(&join, &members, start).expect("remote join");
            let claim = voice_frame(
                remote,
                guild,
                session_id,
                ControlKind::Claim {
                    priority: Priority::local_for_call(3),
                },
                index as u64 * 10 + 2,
                start,
            );
            hub.on_remote(&claim, &members, start)
                .expect("remote claim");
        }

        let snapshot = hub.voice_snapshot(guild, channel, &members, start);
        let call = &snapshot.voice[0];
        let master = snapshot.election.master.as_ref().expect("a master").author;
        let backup = snapshot.election.backup.as_ref().expect("a backup").author;
        assert_eq!(call.members.len(), 3);
        assert_eq!(call.scope, CallScope::Star);
        assert!(call.topology.is_star());
        assert_eq!(call.hub, Some(master));
        assert_eq!(call.hub_policy.mode, HubMode::Elected);
        assert_ne!(master, backup);
        for member in &call.members {
            if member.author != master {
                assert_eq!(call.topology.parent_of(member.author), Some(master));
            }
        }
    }

    #[test]
    fn a_mesh_policy_keeps_the_direct_path_and_reports_no_hub() {
        let local = Identity::generate();
        let guild = GuildId([7; 32]);
        let channel = ChannelId([3; 16]);
        let members = vec![local.author_id()];
        let mut hub = ControlHub::with_media_policy(local, media_policy(CallScope::Mesh));
        let snapshot = hub
            .voice_join(guild, channel, &members, false, false, false, now_ms())
            .expect("join")
            .snapshot;
        let call = &snapshot.voice[0];
        assert_eq!(call.scope, CallScope::Mesh);
        assert!(!call.topology.is_star());
        assert_eq!(call.hub, None);
        assert_eq!(call.hub_policy, HubPolicy::none());
    }

    #[test]
    fn heartbeats_keep_a_call_and_its_coordinator_alive() {
        let identity = Identity::generate();
        let guild = GuildId([7; 32]);
        let channel = ChannelId([3; 16]);
        let session_id = SessionId::for_voice_channel(guild, channel);
        let members = vec![identity.author_id()];
        let mut hub = ControlHub::new(identity.clone());
        let start = now_ms();

        let joined = hub
            .voice_join(guild, channel, &members, false, false, false, start)
            .expect("the local identity joins its own call");
        assert_eq!(joined.snapshot.voice.len(), 1);
        let term = joined.snapshot.election.term;
        assert_eq!(
            joined.snapshot.election.master.as_ref().map(|c| c.author),
            Some(identity.author_id())
        );

        // Advance well past both the 3 s lease and the 20 s member timeout,
        // running the same heartbeat-then-expire pair the daemon tick uses.
        for step in 1..=25u64 {
            let now = start + step * 1_000;
            let frames = hub.heartbeat(now).expect("heartbeats sign");
            assert!(
                frames
                    .iter()
                    .any(|frame| matches!(frame.kind, ControlKind::VoiceHeartbeat { .. })),
                "no voice heartbeat at step {step}"
            );
            assert!(
                frames
                    .iter()
                    .any(|frame| matches!(frame.kind, ControlKind::Heartbeat { .. })),
                "the coordinator stopped heartbeating at step {step}"
            );
            let events = hub.expire(now);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, DaemonEvent::Voice { .. })),
                "the call changed at step {step}"
            );
            let snapshot = hub.snapshot(guild, session_id, &members, now);
            assert_eq!(
                snapshot.voice.len(),
                1,
                "the call was dropped at step {step}"
            );
            assert_eq!(
                snapshot.voice[0].members.len(),
                1,
                "the member expired at step {step}"
            );
            assert_eq!(
                snapshot.election.master.as_ref().map(|c| c.author),
                Some(identity.author_id()),
                "the master rotated at step {step}"
            );
            assert_eq!(
                snapshot.election.term, term,
                "the election term churned at step {step}"
            );
        }
    }
}
