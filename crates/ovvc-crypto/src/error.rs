//! Errors returned by media key agreement and encryption.

use thiserror::Error;

/// A media-encryption failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CryptoError {
    /// The peer public key produced an unusable all-zero shared secret, which
    /// is what a low-order X25519 point yields.
    #[error("invalid media key material")]
    InvalidKey,

    /// No pairwise key has been installed for that author.
    #[error("no peer key installed for this author")]
    NoPeerKey,

    /// A different public key was offered for an author at an epoch that
    /// already has an installed cipher. The call must rotate to a new epoch
    /// before the peer's key may change.
    #[error("a different media key is already installed for this epoch")]
    KeyConflict,

    /// The frame belongs to a different key epoch than the cipher expects.
    #[error("media frame is from the wrong epoch")]
    WrongEpoch,

    /// The frame counter was already accepted or has fallen out of the window.
    #[error("media frame counter is a replay")]
    Replay,

    /// The payload was truncated or failed AEAD authentication.
    #[error("media frame failed authentication")]
    Decrypt,

    /// The plaintext exceeds [`crate::MAX_PLAINTEXT_BYTES`].
    #[error("media plaintext is too large")]
    PlaintextTooLarge,

    /// The per-direction send counter would overflow.
    #[error("media send counter exhausted")]
    CounterExhausted,

    /// The operation is only valid at the star hub, but this keyring belongs
    /// to a leaf.
    #[error("media keyring is not the star hub")]
    NotHub,

    /// A star operation was attempted before a hub was configured.
    #[error("call has no star hub")]
    NoHub,
}

/// Convenience alias for crypto results.
pub type Result<T> = std::result::Result<T, CryptoError>;
