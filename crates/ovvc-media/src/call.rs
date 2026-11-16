//! Voice-call membership, topology and codec state.
//!
//! This is the transport-free state machine for a voice channel. A daemon
//! feeds it local actions and remote hints (join, leave, state update,
//! heartbeat, timeout) and turns the returned [`VoiceUpdate`] into control
//! frames. No audio and no network live here.

use std::collections::BTreeMap;
use std::fmt;

use ovvc_core::{AuthorId, CallScope, ChannelId, HubPolicy};
use serde::{Deserialize, Serialize};

use crate::codec::{NegotiatedCodec, ProfileSet, negotiate};
use crate::error::{MediaError, Result};

/// The default mesh limit: calls at or below this size are fully connected.
/// A larger `Auto` call is a flat star. The limit is a parameter of
/// [`topology_for_with_limit`], so a caller can choose a different one.
pub const MESH_LIMIT: usize = 6;

/// A member that has not sent a heartbeat for longer than this is dropped.
pub const VOICE_MEMBER_TIMEOUT_MS: u64 = 20_000;

const CALL_ID_DOMAIN: &[u8] = b"ovvc/voice-call/v1\0";

/// A deterministic identifier for the call in a channel.
///
/// It is derived from the channel id so every member computes the same id
/// without another round trip. A channel has at most one call at a time.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CallId(pub [u8; 16]);

impl CallId {
    pub fn for_channel(channel: ChannelId) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(CALL_ID_DOMAIN);
        hasher.update(&channel.0);
        let digest = hasher.finalize();
        let mut out = [0u8; 16];
        out.copy_from_slice(&digest.as_bytes()[..16]);
        Self(out)
    }

    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(32);
        for byte in self.0 {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
        out
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl fmt::Debug for CallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CallId")
            .field(&self.to_hex())
            .finish()
    }
}

/// The per-member flags a client publishes to the call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceMemberState {
    pub muted: bool,
    pub deafened: bool,
    pub push_to_talk: bool,
    pub speaking: bool,
    pub video: bool,
    pub screen_share: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceMember {
    pub author: AuthorId,
    pub joined_at_ms: u64,
    pub last_seen_ms: u64,
    pub state: VoiceMemberState,
    pub profiles: ProfileSet,
}

/// What a member did to the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoiceAction {
    Join {
        profiles: ProfileSet,
        state: VoiceMemberState,
    },
    Leave,
    Update {
        state: VoiceMemberState,
    },
}

/// How audio reaches every member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallTopology {
    /// Every member connects directly to every other member.
    Mesh,
    /// A flat star. Every leaf sends its uplink to `hub` and receives the
    /// hub's downlink; no leaf talks media to another leaf. `leaves` is
    /// sorted and never contains `hub`.
    ///
    /// This is the only forwarding shape. There is deliberately no tree or
    /// multi-level variant: the media key rule only keys leaf-to-hub pairs,
    /// so a deeper tree could not be keyed. A parent list that describes a
    /// deeper tree is flattened by [`CallTopology::flatten_star`].
    Star {
        hub: AuthorId,
        leaves: Vec<AuthorId>,
    },
}

/// The topology for a call of `members` under `scope`, using [`MESH_LIMIT`].
///
/// See [`topology_for_with_limit`] for the rules and the two-member
/// direct-pair case.
pub fn topology_for(members: &[AuthorId], scope: CallScope, hub: Option<AuthorId>) -> CallTopology {
    topology_for_with_limit(members, scope, hub, MESH_LIMIT)
}

/// The topology for a call of `members` under `scope` with an explicit mesh
/// limit.
///
/// Two members are always a direct pair: one connection is the whole call, so
/// a hub would add a hop and no forwarding, whatever the scope says. Above
/// that, `Mesh` stays fully connected, `Star` uses a flat star as soon as
/// there are three members, and `Auto` keeps the mesh up to `mesh_limit`
/// before switching to the star.
///
/// The hub is the elected master when it is a member, otherwise the lowest
/// author id, so every peer derives the same fallback before an election has
/// settled.
pub fn topology_for_with_limit(
    members: &[AuthorId],
    scope: CallScope,
    hub: Option<AuthorId>,
    mesh_limit: usize,
) -> CallTopology {
    if members.len() <= 2 {
        return CallTopology::Mesh;
    }
    let star = match scope {
        CallScope::Mesh => false,
        CallScope::Star => true,
        CallScope::Auto => members.len() > mesh_limit,
    };
    if !star {
        return CallTopology::Mesh;
    }
    let Some(root) = hub
        .filter(|candidate| members.contains(candidate))
        .or_else(|| members.iter().min().copied())
    else {
        return CallTopology::Mesh;
    };
    CallTopology::flat_star(root, members)
}

impl CallTopology {
    /// Build the flat star for `members` around `hub`.
    ///
    /// Every other member becomes a leaf, sorted by author id. A call of two
    /// or fewer members has no hub to add, so it is the direct-pair mesh. A
    /// `hub` that is not in `members` is also rejected into a mesh: a hub the
    /// key rule cannot name is not a star.
    pub fn flat_star(hub: AuthorId, members: &[AuthorId]) -> Self {
        if members.len() <= 2 || !members.contains(&hub) {
            return CallTopology::Mesh;
        }
        let mut leaves: Vec<AuthorId> = members
            .iter()
            .copied()
            .filter(|member| *member != hub)
            .collect();
        leaves.sort();
        CallTopology::Star { hub, leaves }
    }

    /// Flatten a parent list into the flat star around `hub`.
    ///
    /// Callers that still hold a `(child, parent)` list can use this instead
    /// of publishing a multi-level tree that [`crate::CallTopology`] and the
    /// key rule cannot represent. Every child is attached directly to `hub`;
    /// a second-level parent is discarded. Returns the topology and whether
    /// the input was deeper than one hop, so a caller can log or reject it.
    pub fn flatten_star(hub: AuthorId, parents: &[(AuthorId, AuthorId)]) -> (Self, bool) {
        let mut leaves: Vec<AuthorId> = parents
            .iter()
            .map(|(child, _)| *child)
            .filter(|child| *child != hub)
            .collect();
        leaves.sort();
        leaves.dedup();
        let deep = parents
            .iter()
            .any(|(_, parent)| *parent != hub);
        let members: Vec<AuthorId> = std::iter::once(hub).chain(leaves.iter().copied()).collect();
        (CallTopology::flat_star(hub, &members), deep)
    }

    /// The hub of a star, or `None` for a mesh.
    pub fn hub(&self) -> Option<AuthorId> {
        match self {
            CallTopology::Mesh => None,
            CallTopology::Star { hub, .. } => Some(*hub),
        }
    }

    /// The leaves of a star, or an empty slice for a mesh.
    pub fn leaves(&self) -> &[AuthorId] {
        match self {
            CallTopology::Mesh => &[],
            CallTopology::Star { leaves, .. } => leaves,
        }
    }

    /// Whether every non-hub member talks only to the hub.
    pub fn is_star(&self) -> bool {
        self.hub().is_some()
    }

    pub fn parent_of(&self, author: AuthorId) -> Option<AuthorId> {
        match self {
            CallTopology::Mesh => None,
            CallTopology::Star { hub, leaves } => {
                leaves.contains(&author).then_some(*hub)
            }
        }
    }

    pub fn children_of(&self, author: AuthorId) -> Vec<AuthorId> {
        match self {
            CallTopology::Mesh => Vec::new(),
            CallTopology::Star { hub, leaves } => {
                if *hub == author {
                    leaves.clone()
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// The members this member exchanges audio with directly.
    pub fn neighbours(&self, author: AuthorId) -> Vec<AuthorId> {
        match self {
            CallTopology::Mesh => Vec::new(),
            CallTopology::Star { hub, leaves } => {
                if *hub == author {
                    leaves.clone()
                } else if leaves.contains(&author) {
                    vec![*hub]
                } else {
                    Vec::new()
                }
            }
        }
    }
}

/// A point-in-time view of a call, suitable for sending in a control frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceSnapshot {
    pub channel_id: ChannelId,
    pub call_id: CallId,
    pub members: Vec<VoiceMember>,
    pub topology: CallTopology,
    /// The elected media hub, which is the master for a star call. `None` for
    /// a mesh or a direct two-member call.
    pub hub: Option<AuthorId>,
    /// The commune's call scope, so a frontend can explain why a call is a
    /// mesh, a star or a direct pair.
    pub scope: CallScope,
    /// The hub's trust and permissions from the commune media policy.
    pub hub_policy: HubPolicy,
    /// The media key epoch the call is sealing frames under, zero when no key
    /// has been announced yet.
    pub key_epoch: u32,
    pub codec: Option<NegotiatedCodec>,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
    pub master: Option<AuthorId>,
    pub backup: Option<AuthorId>,
    pub term: u64,
}

impl VoiceSnapshot {
    pub fn set_coordinators(
        &mut self,
        master: Option<AuthorId>,
        backup: Option<AuthorId>,
        term: u64,
    ) {
        self.master = master;
        self.backup = backup;
        self.term = term;
    }

    /// Recompute the signalled topology for this snapshot and record the hub,
    /// scope, policy and key epoch that go with it.
    ///
    /// This is the point where the control plane's election meets the media
    /// plane's member list. The snapshot is self-contained: a frontend never
    /// has to pair it with a separate election view to know who the hub is.
    pub fn apply_hub(
        &mut self,
        scope: CallScope,
        hub_policy: HubPolicy,
        hub: Option<AuthorId>,
        key_epoch: u32,
    ) {
        self.apply_hub_with_limit(scope, hub_policy, hub, key_epoch, MESH_LIMIT);
    }

    /// [`apply_hub`](Self::apply_hub) with an explicit mesh limit for `Auto`.
    pub fn apply_hub_with_limit(
        &mut self,
        scope: CallScope,
        hub_policy: HubPolicy,
        hub: Option<AuthorId>,
        key_epoch: u32,
        mesh_limit: usize,
    ) {
        let members: Vec<AuthorId> = self.members.iter().map(|member| member.author).collect();
        self.topology = topology_for_with_limit(&members, scope, hub, mesh_limit);
        self.hub = self.topology.hub();
        self.scope = scope;
        self.hub_policy = hub_policy;
        self.key_epoch = key_epoch;
    }

    /// Whether the hub can read the media it forwards.
    pub fn hub_can_decrypt(&self) -> bool {
        self.hub.is_some() && self.hub_policy.can_decrypt()
    }

    /// Whether the hub may mix audio per listener.
    pub fn hub_can_mix(&self) -> bool {
        self.hub.is_some() && self.hub_policy.can_mix()
    }
}

/// Counters for the local member's audio engine in one call.
///
/// The daemon samples these from its audio service once a second, so a
/// frontend can show capture, playback and device health without owning a
/// device itself. The counters describe the engine; the transport fields say
/// which media path carried the call and, for WebRTC, how healthy it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceAudioStats {
    /// The backend that was actually opened: `"null"` or `"pipewire"`.
    pub backend: String,
    /// Frames read from the capture device since the call started.
    pub captured: u64,
    /// Packets encoded and handed to the transport.
    pub packets: u64,
    /// Frames written to the playback device.
    pub played: u64,
    /// Frames the peer jitter buffers concealed.
    pub concealed: u64,
    /// Packets the peer jitter buffers considered lost.
    pub lost: u64,
    /// Peer payloads the decoders rejected.
    pub decode_errors: u64,
    /// Peer streams being mixed, which falls as call members leave.
    pub peers: usize,
    /// Device underruns or overruns.
    pub xruns: u64,
    /// Estimated output latency in milliseconds.
    pub latency_ms: u64,
    /// Largest absolute mixed sample since the previous sample.
    pub peak: i16,
    /// Root-mean-square level of the mixed samples since the previous sample.
    pub rms: f32,
    /// Transport that carried this call: `"webrtc"` or `"libp2p"`.
    pub transport: String,
    /// Worst round-trip time across the WebRTC peers, in milliseconds. Zero
    /// on the libp2p transport and until ICE has measured a candidate pair.
    pub rtt_ms: u32,
    /// RTP packets the inbound WebRTC streams reported lost. Zero on the
    /// libp2p transport, which does not expose a loss counter here.
    pub packets_lost: i64,
    /// Whether any selected WebRTC candidate pair is relayed through TURN.
    pub relayed: bool,
    /// Whether call media is sealed end to end. Voice is always encrypted in
    /// this phase, so this is true whenever the call has a media keyring.
    pub e2ee: bool,
    /// The media key epoch currently in use. Zero only when no keyring exists
    /// for the call, which no frontend should see for a live call.
    pub epoch: u32,
    /// Frames dropped because a key was missing, the epoch was wrong, the
    /// counter replayed or authentication failed. Also counts frames that
    /// could not be encrypted for a peer.
    pub e2ee_errors: u64,
}

// The counters are an exact snapshot apart from `rms`, whose partial equality
// is still the natural comparison.
impl Eq for VoiceAudioStats {}

/// The reason a call changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoiceChange {
    Joined(AuthorId),
    Left(AuthorId),
    Updated(AuthorId),
    SpeakingChanged { author: AuthorId, speaking: bool },
    Expired(AuthorId),
    /// The media hub, topology or key epoch changed without a membership
    /// change. The snapshot carries the new hub and epoch.
    MediaChanged,
}

/// A change plus the snapshot that results from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceUpdate {
    pub channel_id: ChannelId,
    pub change: VoiceChange,
    pub snapshot: VoiceSnapshot,
}

/// The live state of one voice call.
#[derive(Debug, Clone)]
pub struct VoiceCall {
    pub channel_id: ChannelId,
    pub call_id: CallId,
    pub members: BTreeMap<AuthorId, VoiceMember>,
    pub topology: CallTopology,
    pub scope: CallScope,
    pub hub_policy: HubPolicy,
    pub hub: Option<AuthorId>,
    pub key_epoch: u32,
    pub codec: Option<NegotiatedCodec>,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
    pub master: Option<AuthorId>,
    pub backup: Option<AuthorId>,
    pub term: u64,
    /// The `Auto` mesh limit this call uses.
    mesh_limit: usize,
}

impl VoiceCall {
    fn new(
        channel_id: ChannelId,
        now_ms: u64,
        scope: CallScope,
        hub_policy: HubPolicy,
        mesh_limit: usize,
    ) -> Self {
        Self {
            channel_id,
            call_id: CallId::for_channel(channel_id),
            members: BTreeMap::new(),
            topology: CallTopology::Mesh,
            scope,
            hub_policy,
            hub: None,
            key_epoch: 0,
            codec: None,
            started_at_ms: now_ms,
            updated_at_ms: now_ms,
            master: None,
            backup: None,
            term: 0,
            mesh_limit,
        }
    }

    pub fn snapshot(&self) -> VoiceSnapshot {
        VoiceSnapshot {
            channel_id: self.channel_id,
            call_id: self.call_id,
            members: self.members.values().cloned().collect(),
            topology: self.topology.clone(),
            hub: self.hub,
            scope: self.scope,
            hub_policy: self.hub_policy,
            key_epoch: self.key_epoch,
            codec: self.codec.clone(),
            started_at_ms: self.started_at_ms,
            updated_at_ms: self.updated_at_ms,
            master: self.master,
            backup: self.backup,
            term: self.term,
        }
    }

    /// Record the result of the media master/backup election for this call.
    pub fn set_coordinators(
        &mut self,
        master: Option<AuthorId>,
        backup: Option<AuthorId>,
        term: u64,
    ) {
        self.master = master;
        self.backup = backup;
        self.term = term;
    }

    fn recompute(&mut self, now_ms: u64) {
        let members: Vec<AuthorId> = self.members.keys().copied().collect();
        self.topology = topology_for_with_limit(&members, self.scope, self.hub, self.mesh_limit);
        self.hub = self.topology.hub();
        self.codec = negotiate(
            self.members.values().map(|member| member.profiles),
            self.members.values().any(|member| member.state.video),
        )
        .ok();
        self.updated_at_ms = now_ms;
    }

    /// Set the elected hub and recompute the topology. Returns whether the
    /// topology changed.
    pub fn set_hub(&mut self, hub: Option<AuthorId>, now_ms: u64) -> bool {
        let old = self.topology.clone();
        self.hub = hub;
        self.recompute(now_ms);
        self.topology != old
    }

    pub fn set_scope(&mut self, scope: CallScope, now_ms: u64) {
        self.scope = scope;
        self.recompute(now_ms);
    }

    pub fn set_key_epoch(&mut self, epoch: u32) {
        self.key_epoch = epoch;
    }
}

/// Every live voice call in a guild.
#[derive(Debug, Clone)]
pub struct VoicePlane {
    calls: BTreeMap<ChannelId, VoiceCall>,
    scope: CallScope,
    hub_policy: HubPolicy,
    mesh_limit: usize,
}

impl Default for VoicePlane {
    fn default() -> Self {
        Self::with_policy(CallScope::Auto, HubPolicy::default())
    }
}

impl VoicePlane {
    pub fn new() -> Self {
        Self::default()
    }

    /// A plane whose calls start with the commune's scope, hub policy and the
    /// default [`MESH_LIMIT`].
    pub fn with_policy(scope: CallScope, hub_policy: HubPolicy) -> Self {
        Self::with_policy_and_limit(scope, hub_policy, MESH_LIMIT)
    }

    /// A plane whose `Auto` calls use `mesh_limit` before switching to a flat
    /// star.
    pub fn with_policy_and_limit(
        scope: CallScope,
        hub_policy: HubPolicy,
        mesh_limit: usize,
    ) -> Self {
        Self {
            calls: BTreeMap::new(),
            scope,
            hub_policy,
            mesh_limit,
        }
    }

    pub fn scope(&self) -> CallScope {
        self.scope
    }

    pub fn hub_policy(&self) -> HubPolicy {
        self.hub_policy
    }

    /// The `Auto` mesh limit this plane applies to new calls.
    pub fn mesh_limit(&self) -> usize {
        self.mesh_limit
    }

    /// Set the elected hub on one call. Returns whether its topology changed.
    pub fn set_hub(&mut self, channel_id: ChannelId, hub: Option<AuthorId>, now_ms: u64) -> bool {
        self.calls
            .get_mut(&channel_id)
            .is_some_and(|call| call.set_hub(hub, now_ms))
    }

    pub fn set_key_epoch(&mut self, channel_id: ChannelId, epoch: u32) {
        if let Some(call) = self.calls.get_mut(&channel_id) {
            call.set_key_epoch(epoch);
        }
    }

    pub fn apply(
        &mut self,
        channel_id: ChannelId,
        author: AuthorId,
        action: VoiceAction,
        now_ms: u64,
    ) -> Result<VoiceUpdate> {
        match action {
            VoiceAction::Join {
                profiles,
                mut state,
            } => {
                // A muted member cannot be speaking.
                if state.muted {
                    state.speaking = false;
                }
                let scope = self.scope;
                let hub_policy = self.hub_policy;
                let mesh_limit = self.mesh_limit;
                let call = self.calls.entry(channel_id).or_insert_with(|| {
                    VoiceCall::new(channel_id, now_ms, scope, hub_policy, mesh_limit)
                });
                match call.members.get_mut(&author) {
                    Some(member) => {
                        member.last_seen_ms = now_ms;
                        member.state = state;
                        member.profiles = profiles;
                    }
                    None => {
                        call.members.insert(
                            author,
                            VoiceMember {
                                author,
                                joined_at_ms: now_ms,
                                last_seen_ms: now_ms,
                                state,
                                profiles,
                            },
                        );
                    }
                }
                call.recompute(now_ms);
                Ok(VoiceUpdate {
                    channel_id,
                    change: VoiceChange::Joined(author),
                    snapshot: call.snapshot(),
                })
            }
            VoiceAction::Leave => {
                let call = self
                    .calls
                    .get_mut(&channel_id)
                    .ok_or(MediaError::NotInCall)?;
                call.members.remove(&author).ok_or(MediaError::NotInCall)?;
                call.recompute(now_ms);
                let empty = call.members.is_empty();
                let snapshot = call.snapshot();
                if empty {
                    self.calls.remove(&channel_id);
                }
                Ok(VoiceUpdate {
                    channel_id,
                    change: VoiceChange::Left(author),
                    snapshot,
                })
            }
            VoiceAction::Update { mut state } => {
                if state.muted {
                    state.speaking = false;
                }
                let call = self
                    .calls
                    .get_mut(&channel_id)
                    .ok_or(MediaError::NotInCall)?;
                let member = call.members.get_mut(&author).ok_or(MediaError::NotInCall)?;
                let was_speaking = member.state.speaking;
                member.state = state;
                member.last_seen_ms = now_ms;
                let now_speaking = member.state.speaking;
                call.recompute(now_ms);
                let change = if now_speaking != was_speaking {
                    VoiceChange::SpeakingChanged {
                        author,
                        speaking: now_speaking,
                    }
                } else {
                    VoiceChange::Updated(author)
                };
                Ok(VoiceUpdate {
                    channel_id,
                    change,
                    snapshot: call.snapshot(),
                })
            }
        }
    }

    /// Refresh a member's heartbeat without producing an update.
    pub fn touch(&mut self, channel_id: ChannelId, author: AuthorId, now_ms: u64) -> Result<()> {
        let call = self
            .calls
            .get_mut(&channel_id)
            .ok_or(MediaError::NotInCall)?;
        let member = call.members.get_mut(&author).ok_or(MediaError::NotInCall)?;
        member.last_seen_ms = now_ms;
        Ok(())
    }

    /// Drop members whose last heartbeat is older than
    /// [`VOICE_MEMBER_TIMEOUT_MS`], emitting one update each.
    pub fn expire(&mut self, now_ms: u64) -> Vec<VoiceUpdate> {
        let mut updates = Vec::new();
        let channel_ids: Vec<ChannelId> = self.calls.keys().copied().collect();
        for channel_id in channel_ids {
            let expired: Vec<AuthorId> = match self.calls.get(&channel_id) {
                Some(call) => call
                    .members
                    .iter()
                    .filter(|(_, member)| {
                        now_ms.saturating_sub(member.last_seen_ms) > VOICE_MEMBER_TIMEOUT_MS
                    })
                    .map(|(author, _)| *author)
                    .collect(),
                None => continue,
            };
            for author in expired {
                let Some(call) = self.calls.get_mut(&channel_id) else {
                    break;
                };
                call.members.remove(&author);
                call.recompute(now_ms);
                let empty = call.members.is_empty();
                updates.push(VoiceUpdate {
                    channel_id,
                    change: VoiceChange::Expired(author),
                    snapshot: call.snapshot(),
                });
                if empty {
                    self.calls.remove(&channel_id);
                    break;
                }
            }
        }
        updates
    }

    /// Remove an author from every call, for a guild-level departure.
    pub fn remove_author(&mut self, author: AuthorId, now_ms: u64) -> Vec<VoiceUpdate> {
        let mut updates = Vec::new();
        let channel_ids: Vec<ChannelId> = self.calls.keys().copied().collect();
        for channel_id in channel_ids {
            let Some(call) = self.calls.get_mut(&channel_id) else {
                continue;
            };
            if call.members.remove(&author).is_none() {
                continue;
            }
            call.recompute(now_ms);
            let empty = call.members.is_empty();
            updates.push(VoiceUpdate {
                channel_id,
                change: VoiceChange::Expired(author),
                snapshot: call.snapshot(),
            });
            if empty {
                self.calls.remove(&channel_id);
            }
        }
        updates
    }

    pub fn snapshot(&self, channel_id: ChannelId) -> Option<VoiceSnapshot> {
        self.calls.get(&channel_id).map(VoiceCall::snapshot)
    }

    pub fn calls(&self) -> impl Iterator<Item = &VoiceCall> {
        self.calls.values()
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    pub fn member(&self, channel_id: ChannelId, author: AuthorId) -> Option<&VoiceMember> {
        self.calls
            .get(&channel_id)
            .and_then(|call| call.members.get(&author))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel() -> ChannelId {
        ChannelId([9; 16])
    }

    fn other_channel() -> ChannelId {
        ChannelId([7; 16])
    }

    fn author(index: u8) -> AuthorId {
        AuthorId([index; 32])
    }

    fn join(plane: &mut VoicePlane, chan: ChannelId, who: AuthorId, now_ms: u64) {
        plane
            .apply(
                chan,
                who,
                VoiceAction::Join {
                    profiles: ProfileSet::ALL,
                    state: VoiceMemberState::default(),
                },
                now_ms,
            )
            .expect("join succeeds");
    }

    fn state(muted: bool, speaking: bool) -> VoiceMemberState {
        VoiceMemberState {
            muted,
            speaking,
            ..VoiceMemberState::default()
        }
    }

    #[test]
    fn call_id_is_deterministic_and_hex_formatted() {
        let first = CallId::for_channel(channel());
        let second = CallId::for_channel(channel());
        assert_eq!(first, second);
        assert_ne!(first, CallId::for_channel(other_channel()));
        assert_eq!(first.to_hex().len(), 32);
        assert_eq!(first.to_string(), first.to_hex());
    }

    #[test]
    fn two_members_join_into_a_mesh_with_opus() {
        let mut plane = VoicePlane::new();
        join(&mut plane, channel(), author(1), 100);
        join(&mut plane, channel(), author(2), 200);

        let snapshot = plane.snapshot(channel()).expect("call exists");
        assert_eq!(snapshot.channel_id, channel());
        assert_eq!(snapshot.members.len(), 2);
        assert_eq!(snapshot.topology, CallTopology::Mesh);
        assert_eq!(
            snapshot.codec.as_ref().map(|codec| codec.audio.as_str()),
            Some("opus")
        );
        assert_eq!(snapshot.started_at_ms, 100);
        assert_eq!(snapshot.updated_at_ms, 200);
    }

    #[test]
    fn join_preserves_joined_at_and_refreshes_presence() {
        let mut plane = VoicePlane::new();
        join(&mut plane, channel(), author(1), 100);
        join(&mut plane, channel(), author(1), 500);
        let member = plane.member(channel(), author(1)).expect("member");
        assert_eq!(member.joined_at_ms, 100);
        assert_eq!(member.last_seen_ms, 500);
    }

    #[test]
    fn muting_forces_speaking_off_and_reports_the_transition() {
        let mut plane = VoicePlane::new();
        plane
            .apply(
                channel(),
                author(1),
                VoiceAction::Join {
                    profiles: ProfileSet::ALL,
                    state: state(false, true),
                },
                0,
            )
            .expect("join");
        let update = plane
            .apply(
                channel(),
                author(1),
                VoiceAction::Update {
                    state: state(true, true),
                },
                10,
            )
            .expect("update");
        assert_eq!(
            update.change,
            VoiceChange::SpeakingChanged {
                author: author(1),
                speaking: false
            }
        );
        let member = plane.member(channel(), author(1)).expect("member");
        assert!(!member.state.speaking);
        assert!(member.state.muted);
    }

    #[test]
    fn speaking_transition_yields_speaking_changed() {
        let mut plane = VoicePlane::new();
        join(&mut plane, channel(), author(1), 0);
        let update = plane
            .apply(
                channel(),
                author(1),
                VoiceAction::Update {
                    state: state(false, true),
                },
                10,
            )
            .expect("update");
        assert_eq!(
            update.change,
            VoiceChange::SpeakingChanged {
                author: author(1),
                speaking: true
            }
        );
    }

    #[test]
    fn update_without_a_speaking_change_is_updated() {
        let mut plane = VoicePlane::new();
        join(&mut plane, channel(), author(1), 0);
        let update = plane
            .apply(
                channel(),
                author(1),
                VoiceAction::Update {
                    state: state(false, false),
                },
                10,
            )
            .expect("update");
        assert_eq!(update.change, VoiceChange::Updated(author(1)));
    }

    #[test]
    fn update_on_an_absent_member_is_rejected() {
        let mut plane = VoicePlane::new();
        let error = plane
            .apply(
                channel(),
                author(1),
                VoiceAction::Update {
                    state: state(false, false),
                },
                0,
            )
            .unwrap_err();
        assert!(matches!(error, MediaError::NotInCall));
    }

    #[test]
    fn leaving_the_last_member_reports_an_empty_snapshot_then_removes_the_call() {
        let mut plane = VoicePlane::new();
        join(&mut plane, channel(), author(1), 0);
        let update = plane
            .apply(channel(), author(1), VoiceAction::Leave, 10)
            .expect("leave");
        assert_eq!(update.change, VoiceChange::Left(author(1)));
        assert!(update.snapshot.members.is_empty());
        assert!(plane.snapshot(channel()).is_none());
        assert!(plane.is_empty());
    }

    #[test]
    fn leaving_an_absent_member_is_rejected() {
        let mut plane = VoicePlane::new();
        assert!(matches!(
            plane.apply(channel(), author(1), VoiceAction::Leave, 0),
            Err(MediaError::NotInCall)
        ));
    }

    #[test]
    fn six_members_use_a_mesh_and_seven_a_flat_star() {
        let mut plane = VoicePlane::new();
        for index in 1..=6 {
            join(&mut plane, channel(), author(index), 0);
        }
        assert_eq!(
            plane.snapshot(channel()).expect("call").topology,
            CallTopology::Mesh
        );

        join(&mut plane, channel(), author(7), 0);
        let snapshot = plane.snapshot(channel()).expect("call");
        let CallTopology::Star { hub, leaves } = &snapshot.topology else {
            panic!("seven members must be a flat star");
        };
        assert_eq!(*hub, author(1));
        assert_eq!(
            leaves,
            &(2..=7).map(author).collect::<Vec<AuthorId>>(),
            "every non-hub member is a leaf, sorted by author id"
        );

        // One hop only: every leaf is parented by the hub, and the hub has no
        // parent of its own.
        for index in 2..=7 {
            let leaf = author(index);
            assert_eq!(snapshot.topology.parent_of(leaf), Some(*hub));
            assert!(snapshot.topology.children_of(*hub).contains(&leaf));
            assert_eq!(snapshot.topology.neighbours(leaf), vec![*hub]);
        }
        assert!(snapshot.topology.parent_of(*hub).is_none());
        assert_eq!(snapshot.topology.children_of(*hub), leaves.clone());
    }

    #[test]
    fn a_flat_star_has_no_leaf_to_leaf_edge() {
        let members = [author(1), author(2), author(3), author(4)];
        let topology = topology_for(&members, CallScope::Star, Some(author(3)));
        let CallTopology::Star { hub, leaves } = &topology else {
            panic!("star scope must forward");
        };
        assert_eq!(*hub, author(3));
        assert!(!leaves.contains(hub));
        for leaf in leaves {
            assert_eq!(
                topology.neighbours(*leaf),
                vec![*hub],
                "a leaf has exactly one media peer, the hub"
            );
            for other in leaves.iter().filter(|other| *other != leaf) {
                assert!(
                    !topology.neighbours(*leaf).contains(other),
                    "leaf {leaf:?} must not open a media path to leaf {other:?}"
                );
            }
        }
        assert_eq!(
            topology.neighbours(*hub).len(),
            leaves.len(),
            "the hub talks to every leaf and nobody else"
        );
    }

    #[test]
    fn the_mesh_limit_is_a_parameter() {
        let members: Vec<AuthorId> = (1..=4).map(author).collect();
        assert_eq!(
            topology_for_with_limit(&members, CallScope::Auto, None, 2),
            CallTopology::Star {
                hub: author(1),
                leaves: vec![author(2), author(3), author(4)],
            }
        );
        assert_eq!(
            topology_for_with_limit(&members, CallScope::Auto, None, 4),
            CallTopology::Mesh,
            "a limit of four still meshes four members"
        );
        assert_eq!(
            topology_for_with_limit(&members, CallScope::Auto, None, 3),
            CallTopology::Star {
                hub: author(1),
                leaves: vec![author(2), author(3), author(4)],
            }
        );

        let mut plane = VoicePlane::with_policy_and_limit(
            CallScope::Auto,
            HubPolicy::default(),
            2,
        );
        for index in 1..=3 {
            join(&mut plane, channel(), author(index), 0);
        }
        assert_eq!(plane.mesh_limit(), 2);
        assert!(plane.snapshot(channel()).expect("call").topology.is_star());
    }

    #[test]
    fn a_multi_level_parent_list_is_flattened_to_the_hub() {
        let hub = author(1);
        let a = author(2);
        let b = author(3);
        let c = author(4);
        // `b` names `a` and `c` names `b`, so the input describes a chain.
        let (topology, deep) =
            CallTopology::flatten_star(hub, &[(a, hub), (b, a), (c, b)]);
        assert!(deep, "the caller can see the input was deeper than one hop");
        assert_eq!(
            topology,
            CallTopology::Star {
                hub,
                leaves: vec![a, b, c],
            },
            "there is no multi-level variant; every child attaches to the hub"
        );
        for leaf in [a, b, c] {
            assert_eq!(topology.parent_of(leaf), Some(hub));
        }

        // A one-level list is not flagged as deeper.
        let (flat, deep) = CallTopology::flatten_star(hub, &[(a, hub), (b, hub)]);
        assert!(!deep);
        assert_eq!(flat, CallTopology::Star { hub, leaves: vec![a, b] });
    }

    #[test]
    fn a_star_constructor_rejects_a_short_call_or_a_foreign_hub() {
        assert_eq!(
            CallTopology::flat_star(author(1), &[author(1), author(2)]),
            CallTopology::Mesh,
            "two members are already a direct pair"
        );
        assert_eq!(
            CallTopology::flat_star(author(9), &[author(1), author(2), author(3)]),
            CallTopology::Mesh,
            "a hub outside the call cannot key the leaves"
        );
    }

    #[test]
    fn a_two_member_call_is_a_direct_pair_even_under_a_star_scope() {
        let members = [author(1), author(2)];
        assert_eq!(
            topology_for(&members, CallScope::Star, Some(author(1))),
            CallTopology::Mesh,
            "one direct pair is already the whole call"
        );
        assert!(
            topology_for(&members, CallScope::Auto, None)
                .hub()
                .is_none()
        );
    }

    #[test]
    fn a_star_scope_puts_three_members_behind_the_elected_hub() {
        let members = [author(1), author(2), author(3)];
        let topology = topology_for(&members, CallScope::Star, Some(author(2)));
        let CallTopology::Star { hub, leaves } = &topology else {
            panic!("star scope must forward");
        };
        assert_eq!(*hub, author(2));
        assert_eq!(leaves, &vec![author(1), author(3)]);
        for child in [author(1), author(3)] {
            assert_eq!(topology.parent_of(child), Some(author(2)));
        }
        assert_eq!(topology.neighbours(author(2)), vec![author(1), author(3)]);
        assert_eq!(topology.hub(), Some(author(2)));
    }

    #[test]
    fn a_mesh_scope_never_grows_a_hub_and_auto_waits_for_the_limit() {
        let members: Vec<AuthorId> = (1..=7).map(author).collect();
        assert_eq!(
            topology_for(&members, CallScope::Mesh, Some(author(1))),
            CallTopology::Mesh
        );
        let auto = topology_for(&members, CallScope::Auto, Some(author(4)));
        assert_eq!(auto.hub(), Some(author(4)));
    }

    #[test]
    fn a_hub_outside_the_call_falls_back_to_the_lowest_author() {
        let members = [author(2), author(3), author(4)];
        let topology = topology_for(&members, CallScope::Star, Some(author(9)));
        assert_eq!(topology.hub(), Some(author(2)));

        // The fallback is the minimum author id, not the first element of an
        // unordered slice.
        let unsorted = [author(4), author(2), author(3)];
        let topology = topology_for(&unsorted, CallScope::Star, None);
        assert_eq!(topology.hub(), Some(author(2)));
    }

    #[test]
    fn a_call_without_a_common_profile_keeps_no_codec() {
        let mut plane = VoicePlane::new();
        plane
            .apply(
                channel(),
                author(1),
                VoiceAction::Join {
                    profiles: ProfileSet::STANDARD,
                    state: VoiceMemberState::default(),
                },
                0,
            )
            .expect("join");
        plane
            .apply(
                channel(),
                author(2),
                VoiceAction::Join {
                    profiles: ProfileSet::from(crate::codec::VoiceProfile::Minimal),
                    state: VoiceMemberState::default(),
                },
                0,
            )
            .expect("join");
        assert!(plane.snapshot(channel()).expect("call").codec.is_none());
    }

    #[test]
    fn video_anywhere_negotiates_vp8() {
        let mut plane = VoicePlane::new();
        plane
            .apply(
                channel(),
                author(1),
                VoiceAction::Join {
                    profiles: ProfileSet::ALL,
                    state: VoiceMemberState {
                        video: true,
                        ..VoiceMemberState::default()
                    },
                },
                0,
            )
            .expect("join");
        let codec = plane
            .snapshot(channel())
            .expect("call")
            .codec
            .expect("codec");
        assert_eq!(codec.video.as_deref(), Some("vp8"));
    }

    #[test]
    fn expire_drops_only_silent_members() {
        let mut plane = VoicePlane::new();
        join(&mut plane, channel(), author(1), 0);
        join(&mut plane, channel(), author(2), 1_000);

        let updates = plane.expire(VOICE_MEMBER_TIMEOUT_MS + 1);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].change, VoiceChange::Expired(author(1)));
        assert!(plane.member(channel(), author(1)).is_none());
        assert!(plane.member(channel(), author(2)).is_some());
    }

    #[test]
    fn expire_removes_an_emptied_call() {
        let mut plane = VoicePlane::new();
        join(&mut plane, channel(), author(1), 0);
        let updates = plane.expire(VOICE_MEMBER_TIMEOUT_MS + 1);
        assert_eq!(updates.len(), 1);
        assert!(updates[0].snapshot.members.is_empty());
        assert!(plane.snapshot(channel()).is_none());
    }

    #[test]
    fn remove_author_clears_them_from_every_call() {
        let mut plane = VoicePlane::new();
        join(&mut plane, channel(), author(1), 0);
        join(&mut plane, channel(), author(2), 0);
        join(&mut plane, other_channel(), author(1), 0);
        join(&mut plane, other_channel(), author(3), 0);

        let updates = plane.remove_author(author(1), 10);
        assert_eq!(updates.len(), 2);
        for update in &updates {
            assert!(
                update
                    .snapshot
                    .members
                    .iter()
                    .all(|member| member.author != author(1))
            );
        }
        assert!(plane.member(channel(), author(1)).is_none());
        assert!(plane.member(other_channel(), author(1)).is_none());
        assert!(plane.snapshot(channel()).is_some());
        assert!(plane.snapshot(other_channel()).is_some());
    }

    #[test]
    fn touch_refreshes_and_rejects_absent_members() {
        let mut plane = VoicePlane::new();
        join(&mut plane, channel(), author(1), 0);
        plane.touch(channel(), author(1), 5_000).expect("touch");
        assert_eq!(
            plane
                .member(channel(), author(1))
                .expect("member")
                .last_seen_ms,
            5_000
        );
        assert!(matches!(
            plane.touch(channel(), author(2), 6_000),
            Err(MediaError::NotInCall)
        ));
        assert!(matches!(
            plane.touch(other_channel(), author(1), 6_000),
            Err(MediaError::NotInCall)
        ));
    }

    #[test]
    fn calls_and_snapshot_coordinators_are_visible() {
        let mut plane = VoicePlane::new();
        join(&mut plane, channel(), author(1), 0);
        assert_eq!(plane.calls().count(), 1);
        let mut snapshot = plane.snapshot(channel()).expect("call");
        snapshot.set_coordinators(Some(author(1)), None, 3);
        assert_eq!(snapshot.master, Some(author(1)));
        assert_eq!(snapshot.term, 3);
    }
}
