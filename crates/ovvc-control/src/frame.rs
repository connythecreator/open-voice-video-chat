use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use ovvc_core::model::decode_canonical;
use ovvc_core::{AuthorId, ChannelId, GuildId, Identity};
use serde::{Deserialize, Serialize};

use crate::error::{ControlError, Result};

/// Version 7 extends `Priority` with the memory and recent-failure samples
/// the daemon feeds into the election.
pub const CONTROL_VERSION: u16 = 7;
/// Control frames carry a roster, a codec decision, a typing marker or an
/// SDP/ICE signal, never media itself. A frame larger than this is a protocol
/// error, not a big payload.
pub const MAX_CONTROL_BYTES: usize = 64 * 1024;
/// An SDP offer or answer is text, but it is still a control payload: it must
/// fit through the same bounded frame path as everything else.
pub const MAX_SDP_BYTES: usize = 48 * 1024;
/// An ICE candidate line (and any trailing attributes) is far smaller than an
/// SDP body.
pub const MAX_CANDIDATE_BYTES: usize = 2 * 1024;
/// The media stream identification tag from the SDP.
pub const MAX_RTC_MID_BYTES: usize = 64;
/// Frames older or newer than this window are refused. A correct clock is not
/// assumed, but a frame cannot be replayed hours later either.
const MAX_CLOCK_SKEW_MS: u64 = 5 * 60 * 1000;
const CONTROL_SIGNING_DOMAIN: &[u8] = b"ovvc/control/v1\0";
const VOICE_SESSION_DOMAIN: &[u8] = b"ovvc/voice-session/v1\0";
pub const MAX_DISPLAY_NAME_BYTES: usize = 100;

/// A media/control session. For guild-wide presence and typing this is derived
/// from the guild id; a voice call uses [`SessionId::for_voice_channel`] so
/// both peers derive the same per-channel session without signalling.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId(pub [u8; 16]);

impl SessionId {
    /// The stable session used for guild-wide presence and typing.
    pub const fn for_guild(guild_id: GuildId) -> Self {
        let mut out = [0u8; 16];
        let mut index = 0;
        while index < 16 {
            out[index] = guild_id.0[index];
            index += 1;
        }
        Self(out)
    }

    /// The session for one voice channel. Deriving it from the guild and
    /// channel ids means every peer computes the same call session without a
    /// round trip, and two channels never share a replay namespace.
    pub fn for_voice_channel(guild_id: GuildId, channel_id: ChannelId) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(VOICE_SESSION_DOMAIN);
        hasher.update(&guild_id.0);
        hasher.update(&channel_id.0);
        let digest = hasher.finalize();
        let mut out = [0u8; 16];
        out.copy_from_slice(&digest.as_bytes()[..16]);
        Self(out)
    }

    pub fn to_hex(&self) -> String {
        hex16(&self.0)
    }
}

fn hex16(bytes: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SessionId")
            .field(&self.to_hex())
            .finish()
    }
}

impl FromStr for SessionId {
    type Err = ControlError;

    fn from_str(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.len() != 32 {
            return Err(ControlError::Invalid(
                "session id must be 32 hex characters".to_string(),
            ));
        }
        let mut bytes = [0u8; 16];
        for (index, chunk) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let text = std::str::from_utf8(chunk)
                .map_err(|_| ControlError::Invalid("invalid session id".to_string()))?;
            bytes[index] = u8::from_str_radix(text, 16)
                .map_err(|_| ControlError::Invalid("invalid session id".to_string()))?;
        }
        Ok(Self(bytes))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub audio: bool,
    pub video: bool,
    pub screen_share: bool,
    pub courier: bool,
    /// The daemon may act as media master or backup for a call.
    pub media_coordinator: bool,
}

impl Capabilities {
    pub const fn voice() -> Self {
        Self {
            audio: true,
            video: false,
            screen_share: false,
            courier: false,
            media_coordinator: true,
        }
    }

    pub const fn all() -> Self {
        Self {
            audio: true,
            video: true,
            screen_share: true,
            courier: true,
            media_coordinator: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PresenceStatus {
    #[default]
    Online,
    Away,
    DoNotDisturb,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Master,
    Backup,
    Member,
}

impl Role {
    pub const fn is_coordinator(self) -> bool {
        matches!(self, Role::Master | Role::Backup)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Topology {
    #[default]
    Mesh,
    Star {
        center: AuthorId,
    },
    ForwardingMesh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecDecision {
    pub audio: String,
    pub video: Option<String>,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterMember {
    pub author: AuthorId,
    pub display_name: String,
    pub capabilities: Capabilities,
    pub joined_at_ms: u64,
    pub last_seen_ms: u64,
    pub presence: PresenceStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlKind {
    Join {
        display_name: String,
        capabilities: Capabilities,
    },
    Leave,
    /// Offer to coordinate the session. The election ranks the candidates.
    Claim {
        priority: crate::election::Priority,
    },
    Heartbeat {
        role: Role,
        priority: crate::election::Priority,
    },
    /// A coordinator's authoritative view. Peers accept only a higher term.
    State {
        master: Option<AuthorId>,
        backup: Option<AuthorId>,
        term: u64,
        topology: Topology,
        codec: Option<CodecDecision>,
    },
    Roster {
        members: Vec<RosterMember>,
    },
    Presence {
        status: PresenceStatus,
        until_ms: u64,
    },
    Typing {
        channel_id: ChannelId,
        until_ms: u64,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    /// Join (or refresh the profiles of) a voice call. The media plane keys
    /// calls by channel, so the session id is the channel's derived one.
    VoiceJoin {
        channel_id: ChannelId,
        profiles: ovvc_media::ProfileSet,
        muted: bool,
        deafened: bool,
        push_to_talk: bool,
        video: bool,
        screen_share: bool,
    },
    VoiceLeave {
        channel_id: ChannelId,
    },
    /// A member's own audio/video flags. The media plane decides whether the
    /// change is a speaking transition or a plain update.
    VoiceState {
        channel_id: ChannelId,
        muted: bool,
        deafened: bool,
        push_to_talk: bool,
        speaking: bool,
        video: bool,
        screen_share: bool,
    },
    /// Presence within the call; the media plane times members out without one.
    VoiceHeartbeat {
        channel_id: ChannelId,
    },
    /// WebRTC signalling for the call on `channel_id`, addressed to one peer.
    ///
    /// The frame is signed, so the SDP (and the fingerprint inside it) is
    /// authenticated by its author rather than trusted from the transport. The
    /// control plane verifies and routes these frames; it never stores the SDP
    /// or ICE state.
    RtcOffer {
        channel_id: ChannelId,
        to: AuthorId,
        sdp: String,
    },
    RtcAnswer {
        channel_id: ChannelId,
        to: AuthorId,
        sdp: String,
    },
    RtcCandidate {
        channel_id: ChannelId,
        to: AuthorId,
        candidate: String,
        mid: Option<String>,
    },
    /// End the media session with one peer for this call.
    RtcBye {
        channel_id: ChannelId,
        to: AuthorId,
    },
    /// A call member's ephemeral X25519 media public key for one key epoch.
    ///
    /// The control frame's signature is what binds the key to its author, so
    /// peers can trust the key for as long as they trust the sender's identity.
    /// The frame is broadcast to the members of the call; the control plane
    /// verifies and routes it but never stores the key.
    VoiceKey {
        channel_id: ChannelId,
        epoch: u32,
        public_key: [u8; 32],
    },
    /// Ask the other members of a live call to re-announce their membership and
    /// current epoch key.
    ///
    /// A member sends this when it receives a voice frame from an author its
    /// local call roster is missing, which means the two views diverged. The
    /// frame changes no session state; it only prompts the members that are
    /// already in the call to publish their `VoiceJoin` and key again.
    VoiceSyncRequest {
        channel_id: ChannelId,
    },
}

impl ControlKind {
    /// The call channel a voice or RTC frame belongs to, if it is one.
    ///
    /// These frames are only meaningful on the channel's derived session, so
    /// both the session and the daemon use this to bind the two together.
    pub fn voice_channel(&self) -> Option<ChannelId> {
        match self {
            ControlKind::VoiceJoin { channel_id, .. }
            | ControlKind::VoiceLeave { channel_id }
            | ControlKind::VoiceState { channel_id, .. }
            | ControlKind::VoiceHeartbeat { channel_id }
            | ControlKind::RtcOffer { channel_id, .. }
            | ControlKind::RtcAnswer { channel_id, .. }
            | ControlKind::RtcCandidate { channel_id, .. }
            | ControlKind::RtcBye { channel_id, .. }
            | ControlKind::VoiceKey { channel_id, .. }
            | ControlKind::VoiceSyncRequest { channel_id } => Some(*channel_id),
            _ => None,
        }
    }

    /// The call and peer an RTC signal is addressed to, if this is one.
    ///
    /// The daemon routes an RTC frame to exactly one peer; the session has
    /// already checked that the target is another member of the guild.
    pub fn rtc_target(&self) -> Option<(ChannelId, AuthorId)> {
        match self {
            ControlKind::RtcOffer { channel_id, to, .. }
            | ControlKind::RtcAnswer { channel_id, to, .. }
            | ControlKind::RtcCandidate { channel_id, to, .. }
            | ControlKind::RtcBye { channel_id, to } => Some((*channel_id, *to)),
            _ => None,
        }
    }
}

/// A signed, ephemeral control message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlFrame {
    pub version: u16,
    pub guild_id: GuildId,
    pub session_id: SessionId,
    pub term: u64,
    pub nonce: u64,
    pub timestamp_ms: u64,
    pub from: AuthorId,
    pub kind: ControlKind,
    pub signature: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct UnsignedFrame {
    version: u16,
    guild_id: GuildId,
    session_id: SessionId,
    term: u64,
    nonce: u64,
    timestamp_ms: u64,
    from: AuthorId,
    kind: ControlKind,
}

impl ControlFrame {
    /// Sign a frame for `guild_id` and `session_id`.
    ///
    /// `nonce` must be unique for this author and session. The daemon keeps a
    /// [`crate::replay::ReplayGuard`] per author, so reuse within the clock
    /// window is rejected.
    pub fn sign(
        identity: &Identity,
        guild_id: GuildId,
        session_id: SessionId,
        term: u64,
        kind: ControlKind,
        nonce: u64,
    ) -> Result<Self> {
        let timestamp_ms = now_ms();
        Self::sign_at(
            identity,
            guild_id,
            session_id,
            term,
            kind,
            nonce,
            timestamp_ms,
        )
    }

    pub fn sign_at(
        identity: &Identity,
        guild_id: GuildId,
        session_id: SessionId,
        term: u64,
        kind: ControlKind,
        nonce: u64,
        timestamp_ms: u64,
    ) -> Result<Self> {
        validate_kind(&kind)?;
        let unsigned = UnsignedFrame {
            version: CONTROL_VERSION,
            guild_id,
            session_id,
            term,
            nonce,
            timestamp_ms,
            from: identity.author_id(),
            kind: kind.clone(),
        };
        let bytes = signing_bytes(&unsigned)?;
        let signature = identity.sign_bytes(&bytes);
        let frame = Self {
            version: CONTROL_VERSION,
            guild_id,
            session_id,
            term,
            nonce,
            timestamp_ms,
            from: identity.author_id(),
            kind,
            signature,
        };
        frame.ensure_size()?;
        Ok(frame)
    }

    /// Verify the signature and the frame's own invariants (version, size,
    /// guild/session agreement). Freshness and replay are checked separately so
    /// callers can choose their own clock and replay window.
    pub fn verify(&self) -> Result<()> {
        self.ensure_size()?;
        if self.version != CONTROL_VERSION {
            return Err(ControlError::UnsupportedVersion(self.version));
        }
        if self.signature.len() != 64 {
            return Err(ControlError::InvalidSignature);
        }
        validate_kind(&self.kind)?;
        let unsigned = UnsignedFrame {
            version: self.version,
            guild_id: self.guild_id,
            session_id: self.session_id,
            term: self.term,
            nonce: self.nonce,
            timestamp_ms: self.timestamp_ms,
            from: self.from,
            kind: self.kind.clone(),
        };
        let bytes = signing_bytes(&unsigned)?;
        if !self.from.verify(&bytes, &self.signature) {
            return Err(ControlError::InvalidSignature);
        }
        Ok(())
    }

    pub fn check_freshness(&self, now_ms: u64) -> Result<()> {
        if self.timestamp_ms == 0 {
            return Err(ControlError::Stale);
        }
        let too_old = now_ms.saturating_sub(self.timestamp_ms) > MAX_CLOCK_SKEW_MS;
        let too_new = self.timestamp_ms.saturating_sub(now_ms) > MAX_CLOCK_SKEW_MS;
        if too_old || too_new {
            return Err(ControlError::Stale);
        }
        Ok(())
    }

    pub fn check_session(&self, guild_id: GuildId, session_id: SessionId) -> Result<()> {
        if self.guild_id != guild_id || self.session_id != session_id {
            return Err(ControlError::WrongSession);
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.ensure_size()?;
        Ok(postcard::to_allocvec(self)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_CONTROL_BYTES {
            return Err(ControlError::TooLarge(bytes.len()));
        }
        Ok(decode_canonical(bytes)?)
    }

    fn ensure_size(&self) -> Result<()> {
        let size = postcard::to_allocvec(self)?;
        if size.len() > MAX_CONTROL_BYTES {
            return Err(ControlError::TooLarge(size.len()));
        }
        Ok(())
    }
}

fn signing_bytes(unsigned: &UnsignedFrame) -> Result<Vec<u8>> {
    let body = postcard::to_allocvec(unsigned)?;
    let mut bytes = Vec::with_capacity(CONTROL_SIGNING_DOMAIN.len() + body.len());
    bytes.extend_from_slice(CONTROL_SIGNING_DOMAIN);
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

fn validate_kind(kind: &ControlKind) -> Result<()> {
    match kind {
        ControlKind::Join { display_name, .. } => {
            if display_name.trim().is_empty() {
                return Err(ControlError::Invalid(
                    "display name cannot be empty".to_string(),
                ));
            }
            if display_name.len() > MAX_DISPLAY_NAME_BYTES {
                return Err(ControlError::Invalid(
                    "display name is too long".to_string(),
                ));
            }
            if display_name.chars().any(|c| c.is_control()) {
                return Err(ControlError::Invalid(
                    "display name contains control characters".to_string(),
                ));
            }
        }
        ControlKind::State {
            topology, codec, ..
        } => {
            if let Topology::Star { center } = topology
                && center.0.iter().all(|byte| *byte == 0)
            {
                return Err(ControlError::Invalid(
                    "star topology has an empty centre".to_string(),
                ));
            }
            if let Some(codec) = codec
                && (codec.audio.len() > 64
                    || codec.video.as_ref().is_some_and(|video| video.len() > 64))
            {
                return Err(ControlError::Invalid("codec name is too long".to_string()));
            }
        }
        ControlKind::Roster { members } if members.len() > 4096 => {
            return Err(ControlError::Invalid("roster is too large".to_string()));
        }
        ControlKind::RtcOffer { to, sdp, .. } | ControlKind::RtcAnswer { to, sdp, .. } => {
            validate_rtc_target(to)?;
            if !(1..=MAX_SDP_BYTES).contains(&sdp.len()) {
                return Err(ControlError::Invalid(format!(
                    "rtc sdp must be 1 to {MAX_SDP_BYTES} bytes, got {}",
                    sdp.len()
                )));
            }
        }
        ControlKind::RtcCandidate {
            to, candidate, mid, ..
        } => {
            validate_rtc_target(to)?;
            if !(1..=MAX_CANDIDATE_BYTES).contains(&candidate.len()) {
                return Err(ControlError::Invalid(format!(
                    "rtc candidate must be 1 to {MAX_CANDIDATE_BYTES} bytes, got {}",
                    candidate.len()
                )));
            }
            if let Some(mid) = mid
                && mid.len() > MAX_RTC_MID_BYTES
            {
                return Err(ControlError::Invalid(format!(
                    "rtc mid must be at most {MAX_RTC_MID_BYTES} bytes, got {}",
                    mid.len()
                )));
            }
        }
        ControlKind::RtcBye { to, .. } => validate_rtc_target(to)?,
        ControlKind::VoiceKey {
            epoch, public_key, ..
        } => {
            if *epoch == 0 {
                return Err(ControlError::Invalid(
                    "media key epoch must be greater than zero".to_string(),
                ));
            }
            if public_key.iter().all(|byte| *byte == 0) {
                return Err(ControlError::Invalid(
                    "media public key cannot be all zeros".to_string(),
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// An RTC signal cannot be addressed to the all-zero author.
fn validate_rtc_target(to: &AuthorId) -> Result<()> {
    if to.0.iter().all(|byte| *byte == 0) {
        return Err(ControlError::Invalid(
            "rtc signal has an empty target".to_string(),
        ));
    }
    Ok(())
}

/// Monotonic-enough nonce source. The high bits are the wall clock, the low
/// bits a per-process counter, so two frames made in the same millisecond do
/// not collide and a restart starts from a fresh range.
#[derive(Debug, Default)]
pub struct NonceSource {
    counter: AtomicU64,
}

impl NonceSource {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next(&self) -> u64 {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed) & 0x000f_ffff;
        (now_ms() << 20) | counter
    }
}

pub fn now_ms() -> u64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    u64::try_from(nanos / 1_000_000).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_core::GuildId;

    fn guild() -> GuildId {
        GuildId([7; 32])
    }

    #[test]
    fn signed_frame_verifies_and_tamper_fails() {
        let identity = Identity::generate();
        let frame = ControlFrame::sign(
            &identity,
            guild(),
            SessionId::for_guild(guild()),
            0,
            ControlKind::Join {
                display_name: "alice".to_string(),
                capabilities: Capabilities::voice(),
            },
            1,
        )
        .unwrap();
        frame.verify().unwrap();

        let mut tampered = frame.clone();
        tampered.term = 9;
        assert!(matches!(
            tampered.verify(),
            Err(ControlError::InvalidSignature)
        ));
    }

    #[test]
    fn roundtrip_is_canonical() {
        let identity = Identity::generate();
        let frame = ControlFrame::sign(
            &identity,
            guild(),
            SessionId::for_guild(guild()),
            3,
            ControlKind::Typing {
                channel_id: ChannelId([2; 16]),
                until_ms: 42,
            },
            7,
        )
        .unwrap();
        let bytes = frame.to_bytes().unwrap();
        assert_eq!(ControlFrame::from_bytes(&bytes).unwrap(), frame);
        let mut trailing = bytes;
        trailing.push(0);
        assert!(ControlFrame::from_bytes(&trailing).is_err());
    }

    #[test]
    fn stale_and_future_frames_are_rejected() {
        let identity = Identity::generate();
        let now = now_ms();
        let frame = ControlFrame::sign_at(
            &identity,
            guild(),
            SessionId::for_guild(guild()),
            0,
            ControlKind::Leave,
            1,
            now,
        )
        .unwrap();
        frame.verify().unwrap();
        frame.check_freshness(now).unwrap();
        assert!(matches!(
            frame.check_freshness(now + MAX_CLOCK_SKEW_MS + 1),
            Err(ControlError::Stale)
        ));
        assert!(matches!(
            frame.check_freshness(now.saturating_sub(MAX_CLOCK_SKEW_MS + 1)),
            Err(ControlError::Stale)
        ));
    }

    #[test]
    fn oversized_display_name_is_refused() {
        let identity = Identity::generate();
        let result = ControlFrame::sign(
            &identity,
            guild(),
            SessionId::for_guild(guild()),
            0,
            ControlKind::Join {
                display_name: "x".repeat(MAX_DISPLAY_NAME_BYTES + 1),
                capabilities: Capabilities::default(),
            },
            1,
        );
        assert!(result.is_err());
    }

    #[test]
    fn nonces_increase() {
        let source = NonceSource::new();
        let first = source.next();
        let second = source.next();
        assert!(second > first);
    }

    fn rtc_offer(sdp: String, to: AuthorId) -> ControlKind {
        ControlKind::RtcOffer {
            channel_id: ChannelId([2; 16]),
            to,
            sdp,
        }
    }

    #[test]
    fn valid_rtc_signals_sign_and_verify() {
        let identity = Identity::generate();
        let target = Identity::generate().author_id();
        for kind in [
            rtc_offer("v=0\r\n".to_string(), target),
            ControlKind::RtcAnswer {
                channel_id: ChannelId([2; 16]),
                to: target,
                sdp: "v=0\r\na=answer\r\n".to_string(),
            },
            ControlKind::RtcCandidate {
                channel_id: ChannelId([2; 16]),
                to: target,
                candidate: "candidate:1 1 udp 1 127.0.0.1 5000 typ host".to_string(),
                mid: Some("audio".to_string()),
            },
            ControlKind::RtcBye {
                channel_id: ChannelId([2; 16]),
                to: target,
            },
        ] {
            let frame = ControlFrame::sign(
                &identity,
                guild(),
                SessionId::for_guild(guild()),
                0,
                kind,
                1,
            )
            .unwrap();
            frame.verify().unwrap();
        }
    }

    #[test]
    fn oversized_sdp_and_candidate_are_refused_before_signing() {
        let identity = Identity::generate();
        let target = Identity::generate().author_id();
        let sign = |kind| {
            ControlFrame::sign(
                &identity,
                guild(),
                SessionId::for_guild(guild()),
                0,
                kind,
                1,
            )
        };

        let too_big = rtc_offer("x".repeat(MAX_SDP_BYTES + 1), target);
        assert!(matches!(sign(too_big), Err(ControlError::Invalid(_))));
        assert!(matches!(
            sign(rtc_offer(String::new(), target)),
            Err(ControlError::Invalid(_))
        ));

        let too_big = ControlKind::RtcCandidate {
            channel_id: ChannelId([2; 16]),
            to: target,
            candidate: "x".repeat(MAX_CANDIDATE_BYTES + 1),
            mid: None,
        };
        assert!(matches!(sign(too_big), Err(ControlError::Invalid(_))));
    }

    #[test]
    fn an_rtc_signal_to_the_zero_author_is_refused() {
        let identity = Identity::generate();
        let result = ControlFrame::sign(
            &identity,
            guild(),
            SessionId::for_guild(guild()),
            0,
            rtc_offer("v=0\r\n".to_string(), AuthorId([0; 32])),
            1,
        );
        assert!(matches!(result, Err(ControlError::Invalid(_))));
    }

    #[test]
    fn a_media_key_frame_signs_and_validates_its_shape() {
        let identity = Identity::generate();
        let sign = |kind| {
            ControlFrame::sign(
                &identity,
                guild(),
                SessionId::for_guild(guild()),
                0,
                kind,
                1,
            )
        };

        let valid = ControlKind::VoiceKey {
            channel_id: ChannelId([2; 16]),
            epoch: 1,
            public_key: [9; 32],
        };
        let frame = sign(valid).unwrap();
        frame.verify().unwrap();
        assert_eq!(frame.kind.voice_channel(), Some(ChannelId([2; 16])));

        let zero_epoch = ControlKind::VoiceKey {
            channel_id: ChannelId([2; 16]),
            epoch: 0,
            public_key: [9; 32],
        };
        assert!(matches!(sign(zero_epoch), Err(ControlError::Invalid(_))));

        let zero_key = ControlKind::VoiceKey {
            channel_id: ChannelId([2; 16]),
            epoch: 1,
            public_key: [0; 32],
        };
        assert!(matches!(sign(zero_key), Err(ControlError::Invalid(_))));
    }
}
