//! Per-call end-to-end media encryption for ovvc.
//!
//! Every call member generates an ephemeral X25519 keypair ([`MediaKeypair`])
//! and publishes the public half in a signed control frame. From that public
//! key and the peer's identity each side derives a pair of directional keys
//! ([`derive_pairwise`]); media is sealed with ChaCha20-Poly1305 under one key
//! per direction, a per-direction counter and an epoch.
//!
//! The wire format is:
//!
//! ```text
//! epoch u32 BE | counter u64 BE | ciphertext | tag (16 bytes)
//! ```
//!
//! The 12-byte header doubles as the AEAD nonce, so the nonce is never sent
//! twice as long as the send counter never repeats. A [`ReplayWindow`] per
//! incoming direction rejects duplicates and counters that fall out of a
//! 64-counter window while still accepting reordering inside it.
//!
//! [`MediaKeyring`] is the convenience layer for a whole call: it owns the
//! local keypair and one cipher per peer, and supports epoch rotation.
//!
//! # Trust boundary
//!
//! This crate performs no signature verification. The caller must verify that
//! the control frame carrying a peer's public key was signed by that peer's
//! [`AuthorId`](ovvc_core::AuthorId) before passing the key to
//! [`MediaKeyring::install_peer`].
//!
//! # Limits
//!
//! * Keys are per epoch, not a full ratchet: [`MediaKeyring::rotate`] gives
//!   forward secrecy for the old epoch but there is no per-message ratchet.
//! * BLAKE3's internal hasher state and `derive_key`'s internal copy of the
//!   key material are not zeroized by the dependency; the material buffer and
//!   the pairwise key arrays this crate owns are.
//! * There is no key confirmation or explicit peer-accept handshake. An
//!   install of the wrong key simply fails to authenticate later.

mod cipher;
mod error;
mod keyring;
mod keys;
mod replay;
mod topology;

pub use cipher::MediaCipher;
pub use error::{CryptoError, Result};
pub use keyring::MediaKeyring;
pub use keys::{MediaKeypair, PairwiseKeys, derive_pairwise};
pub use replay::{REPLAY_WINDOW_BITS, ReplayWindow};
pub use topology::KeyTopology;

/// Version of the media key schedule and wire format.
pub const MEDIA_KEY_VERSION: u8 = 1;

/// Size of the ChaCha20-Poly1305 nonce, which is the wire header.
pub const NONCE_BYTES: usize = 12;

/// Size of the Poly1305 authentication tag appended to the ciphertext.
pub const TAG_BYTES: usize = 16;

/// Size of the wire header: epoch `u32` big-endian and counter `u64` big-endian.
pub const HEADER_BYTES: usize = 12;

/// Bytes a wire payload adds to its plaintext: header plus tag.
///
/// A caller whose transport limit is `N` can carry a plaintext of at most
/// `N - WIRE_OVERHEAD_BYTES`.
pub const WIRE_OVERHEAD_BYTES: usize = HEADER_BYTES + TAG_BYTES;

/// Largest plaintext a single media frame may carry.
///
/// This is derived from the media packet's payload cap rather than equalling
/// it: the sealed payload is [`MAX_CIPHERTEXT_BYTES`], which must still fit in
/// one [`ovvc_media::MAX_MEDIA_PAYLOAD`]-sized packet on the libp2p path.
pub const MAX_PLAINTEXT_BYTES: usize = ovvc_media::MAX_MEDIA_PAYLOAD - WIRE_OVERHEAD_BYTES;

/// Largest complete wire payload: `HEADER_BYTES + MAX_PLAINTEXT_BYTES + TAG_BYTES`.
pub const MAX_CIPHERTEXT_BYTES: usize = HEADER_BYTES + MAX_PLAINTEXT_BYTES + TAG_BYTES;

// The nonce is the header, and both are one `u32` plus one `u64`.
const _: () = assert!(NONCE_BYTES == HEADER_BYTES);
const _: () = assert!(MEDIA_KEY_VERSION == 1);
const _: () = assert!(NONCE_BYTES == 12);
const _: () = assert!(TAG_BYTES == 16);
const _: () = assert!(MAX_PLAINTEXT_BYTES == 1172);
const _: () = assert!(WIRE_OVERHEAD_BYTES == 28);
const _: () = assert!(MAX_CIPHERTEXT_BYTES == 1200);
const _: () = assert!(MAX_CIPHERTEXT_BYTES == ovvc_media::MAX_MEDIA_PAYLOAD);
