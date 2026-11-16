//! WebRTC audio transport for ovvc calls.
//!
//! The interim media path carries an Opus payload in an
//! [`ovvc_media::MediaPacket`](ovvc_media::packet::MediaPacket). This crate
//! keeps that payload and the same [`MediaPacket`](ovvc_media::packet::MediaPacket)
//! boundary, but replaces the direct peer-to-peer transport with webrtc-rs
//! 0.21.1: each remote author gets a `PeerConnection`, the payload is written
//! as a 20 ms RTP sample, and the remote track is read back into a
//! `MediaPacket` event for the daemon to route to the audio service.
//!
//! Signalling is deliberately transport-free. [`RtcManager`] emits
//! [`RtcEvent::Signal`] values carrying an SDP or an ICE candidate; the daemon
//! is expected to move them over libp2p request-response. Identity and control
//! stay on libp2p.
//!
//! # Media encryption
//!
//! Every RTP payload is sealed end to end with
//! [`ovvc_crypto::MediaKeyring`] before it is written, and opened again before
//! it becomes a [`RtcEvent::Media`]. A peer with no installed key sends
//! nothing and its received frames are dropped and counted; plaintext is never
//! written or emitted. The daemon publishes [`RtcManager::public_key`] in the
//! signed control frame, installs each authenticated peer key with
//! [`RtcManager::install_peer_key`], and calls
//! [`RtcManager::rotate_keyring`] to move epoch.
//!
//! # Offerer rule
//!
//! The lower [`AuthorId`](ovvc_core::AuthorId) creates the offer. Calling
//! [`RtcManager::ensure_peer`] on the higher side creates the connection but
//! emits nothing until the offer arrives.
//!
//! # Failure isolation
//!
//! A peer whose connection fails, or whose description cannot be applied, is
//! reported as [`RtcEvent::Error`] and removed. Every other peer keeps running;
//! one bad connection never stops the call.
//!
//! # Limitations of webrtc-rs 0.21.1
//!
//! * `webrtc` does not re-export `MediaStreamTrack`, `RTCRtpCodec`,
//!   `RTCRtpCodingParameters`, `RTCRtpEncodingParameters`, `RtpCodecKind` or
//!   `rtc::media::Sample`, so `rtc` is a direct dependency as it is in the
//!   crate's own examples. `Sample::data` is `bytes::Bytes`, so `bytes` is one
//!   too.
//! * `RTCSessionDescription::parsed` is crate-private; descriptions are built
//!   with the `offer`/`answer` constructors.
//! * `get_stats` is a full report snapshot per call. It exposes the selected
//!   candidate pair's RTT and the inbound loss counter, which is what
//!   [`RtcPeerStats`] reports. There is no cheaper per-peer accessor.
//! * `on_track` fires on the first RTP packet, not at negotiation completion, so
//!   callers should tolerate a short silence before the first remote frame.
//! * The manager binds an IPv4 wildcard and an IPv4 loopback UDP socket per
//!   peer. IPv6 candidates are not gathered.

pub mod config;
pub mod error;
pub mod manager;
pub mod signal;

pub use config::{
    ALLOWED_FRAME_MS, MAX_BITRATE_KBPS, MAX_ICE_URL_BYTES, MIN_BITRATE_KBPS, RtcConfig,
};
pub use error::{Result, RtcError};
pub use manager::{RtcEvent, RtcManager, RtcPeerStats, RtcStats};
pub use signal::{MAX_CANDIDATE_BYTES, MAX_MID_BYTES, MAX_SDP_BYTES, RtcSignal};
