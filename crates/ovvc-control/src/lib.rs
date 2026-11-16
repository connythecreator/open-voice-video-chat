//! `ovvc-c`, the ephemeral control plane.
//!
//! Durable guild state belongs in the signed event log. Call signalling,
//! presence, typing, codec negotiation, WebRTC SDP/ICE exchange and the media
//! master/backup election do not: they are latency-sensitive, expire on their
//! own, and would bloat a log that is replicated to every member forever.
//!
//! Every frame here is:
//!
//! * **versioned** — an older daemon can detect and refuse a newer frame;
//! * **signed** by the sender's identity key, so the transport cannot forge it;
//! * **bounded** in size, and carries a timestamp and a nonce so replay is
//!   detectable;
//! * **transport agnostic** — the encoding has no assumption about TCP, QUIC,
//!   or a store-and-forward medium.
//!
//! Protocol version 3 adds the `Rtc*` signalling kinds. The SDP is carried in a
//! signed control frame, so the fingerprint it advertises is authenticated by
//! its author, but the session verifies and routes it without keeping any media
//! state.
//!
//! Protocol version 4 adds `VoiceKey`: a call member broadcasts its ephemeral
//! X25519 media public key for a key epoch. The frame signature binds the key
//! to the sender's identity, and the session verifies and routes it like the
//! RTC signals without storing the key.
//!
//! Protocol version 5 adds `VoiceSyncRequest`: a member whose call roster is
//! missing an author that is visibly in the call asks the existing members to
//! re-announce their membership and current epoch key. This is the recovery
//! path for a member that joins the commune after a call is already live.
//!
//! Protocol version 6 extends the election `Priority` with download headroom,
//! CPU load, core count, hardware encode/decode, active calls, hub cooldown,
//! worst-case RTT, loss and per-member reachability, so the media hub election
//! can rank a real capacity-aware candidate instead of a six-field proxy. The
//! extension is not postcard-compatible. The frame is decoded before the
//! version field is checked, so an incompatible shape can fail at decode or at
//! the version check, and an old frame is not promised to reach the check.
//!
//! The crate deliberately contains no networking. The daemon decides how a
//! frame reaches a peer, and [`session::ControlSession`] decides what it means.

pub mod election;
pub mod error;
pub mod frame;
pub mod replay;
pub mod session;

pub use election::{Candidate, Election, ElectionConfig, ElectionView, Priority};
pub use error::{ControlError, Result};
pub use frame::{
    CONTROL_VERSION, Capabilities, CodecDecision, ControlFrame, ControlKind, MAX_CANDIDATE_BYTES,
    MAX_CONTROL_BYTES, MAX_RTC_MID_BYTES, MAX_SDP_BYTES, NonceSource, PresenceStatus, Role,
    RosterMember, SessionId, Topology, now_ms,
};
pub use replay::{RateLimiter, ReplayGuard};
pub use session::{ControlSession, ControlSnapshot, SessionEffect, TypingState};

// Downstream crates (daemon, IPC, GUI) should not need a direct ovvc-media
// dependency just to read the voice state carried in control frames.
pub use ovvc_media::{
    CallTopology, MediaError, NegotiatedCodec, ProfileSet, VoiceAction, VoiceAudioStats,
    VoiceChange, VoiceMember, VoiceMemberState, VoicePlane, VoiceProfile, VoiceSnapshot,
    VoiceUpdate,
};
