use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A storage backend failed. Backends are free to use whatever error type
    /// they like internally; the domain only needs the message.
    #[error("storage backend error: {0}")]
    Backend(String),

    #[error("serialization error: {0}")]
    Postcard(#[from] postcard::Error),

    #[error("signature error: {0}")]
    Signature(#[from] ed25519_dalek::SignatureError),

    #[error("invalid signature")]
    InvalidSignature,

    #[error("invalid event id")]
    InvalidEventId,

    #[error("invalid identifier: {0}")]
    InvalidId(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("event too large: {0} bytes")]
    EventTooLarge(usize),

    #[error("corrupt event log at {path}: {reason}")]
    CorruptLog { path: PathBuf, reason: String },
}

pub type Result<T> = std::result::Result<T, CoreError>;
