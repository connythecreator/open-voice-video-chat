//! Errors for voice-call state and media framing.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MediaError {
    /// No profile survives the intersection of every member's set.
    #[error("no common voice profile")]
    NoCommonCodec,

    /// The channel named by a caller is not a voice channel or is unknown.
    #[error("unknown voice channel")]
    UnknownChannel,

    /// The author is not a member of the call being addressed.
    #[error("author is not in this call")]
    NotInCall,

    /// A media payload exceeds [`crate::packet::MAX_MEDIA_PAYLOAD`].
    #[error("media payload is too large: {0} bytes")]
    PacketTooLarge(usize),

    /// A wire packet does not have a shape this version understands.
    #[error("malformed media packet: {0}")]
    Malformed(String),

    /// The jitter buffer already holds [`crate::jitter::MAX_BUFFERED`] packets.
    #[error("jitter buffer is full")]
    BufferFull,
}

pub type Result<T> = std::result::Result<T, MediaError>;
