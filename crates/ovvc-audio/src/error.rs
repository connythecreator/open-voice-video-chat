//! Errors for Opus coding, audio backends and the voice engine.

use thiserror::Error;

/// Everything that can go wrong in the audio path.
#[derive(Debug, Error)]
pub enum AudioError {
    /// An Opus encoder or decoder rejected a frame or packet.
    #[error("codec error: {0}")]
    Codec(String),

    /// The device backend failed to read, write or start.
    #[error("backend error: {0}")]
    Backend(String),

    /// A codec, engine or backend setting is unsupported.
    #[error("configuration error: {0}")]
    Config(String),

    /// An operating-system call failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, AudioError>;
