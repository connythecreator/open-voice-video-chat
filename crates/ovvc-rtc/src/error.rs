//! Errors returned by the WebRTC transport.

use ovvc_core::AuthorId;

/// Something the RTC layer refused to do.
///
/// Per-peer failures are not returned to the caller as errors by the manager's
/// event-driven paths: they are reported as [`RtcEvent::Error`](crate::RtcEvent::Error)
/// and the affected peer is removed. This type covers caller mistakes, invalid
/// configuration and failures creating the first connection for a peer.
#[derive(Debug, thiserror::Error)]
pub enum RtcError {
    /// The configuration is not something the manager can run.
    #[error("invalid RTC configuration: {0}")]
    Config(String),

    /// A signalling message is malformed or too large.
    #[error("invalid RTC signal: {0}")]
    Signal(String),

    /// The packet handed to [`RtcManager::send_audio`](crate::RtcManager::send_audio)
    /// is not a usable audio packet.
    #[error("invalid media packet: {0}")]
    Packet(String),

    /// No peer connection exists for this author.
    #[error("no RTC peer for author {0}")]
    UnknownPeer(AuthorId),

    /// The manager was asked to connect to its own author id.
    #[error("the local author cannot be its own RTC peer")]
    SelfPeer,

    /// The underlying WebRTC implementation returned an error.
    #[error("webrtc: {0}")]
    WebRtc(String),

    /// Media key agreement or encryption refused the operation.
    #[error("media crypto: {0}")]
    Crypto(String),

    /// The internal peer map was poisoned by a panic in another thread.
    #[error("RTC manager state is poisoned")]
    Poisoned,
}

/// The result type used throughout this crate.
pub type Result<T> = std::result::Result<T, RtcError>;
