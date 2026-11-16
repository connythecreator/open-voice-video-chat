//! X25519 key agreement and the pairwise media KDF.

use ovvc_core::AuthorId;
use ovvc_media::CallId;
use rand::rngs::OsRng;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

use crate::error::{CryptoError, Result};

/// BLAKE3 key-derivation context for every pairwise media key.
const KDF_CONTEXT: &str = "ovvc/media/v1";

// The direction label is chosen from the canonically sorted author pair, not
// from "local" and "peer". Both sides therefore derive the same two keys and
// simply assign them to opposite directions.
const DIRECTION_FIRST_TO_SECOND: &[u8] = b"ovvc/media/dir/v1/first-to-second";
const DIRECTION_SECOND_TO_FIRST: &[u8] = b"ovvc/media/dir/v1/second-to-first";

/// An ephemeral X25519 keypair for one call.
///
/// Call members generate one of these, publish [`public_key`](Self::public_key)
/// in a signed control frame, and keep the secret for pairwise key agreement.
/// The secret is zeroized when the value is dropped.
pub struct MediaKeypair {
    secret: StaticSecret,
    public: [u8; 32],
}

impl MediaKeypair {
    /// Generate a fresh keypair from the operating system RNG.
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret).to_bytes();
        Self { secret, public }
    }

    /// The public key to publish in a signed control frame.
    pub fn public_key(&self) -> [u8; 32] {
        self.public
    }

    /// Move both halves out for [`crate::MediaKeyring`].
    pub(crate) fn into_parts(self) -> (StaticSecret, [u8; 32]) {
        (self.secret, self.public)
    }
}

/// The two directional AEAD keys shared with one peer.
///
/// The fields are private so a caller cannot mix up send and receive keys.
/// Dropping the value zeroizes both arrays.
pub struct PairwiseKeys {
    send: [u8; 32],
    recv: [u8; 32],
}

impl PairwiseKeys {
    /// Copy the keys out for [`crate::MediaCipher`].
    ///
    /// The returned arrays are copies. The caller is expected to move them
    /// into AEAD ciphers and then zeroize them, which
    /// [`MediaCipher::new`](crate::MediaCipher::new) does.
    pub(crate) fn into_keys(self) -> ([u8; 32], [u8; 32]) {
        (self.send, self.recv)
    }
}

impl Drop for PairwiseKeys {
    fn drop(&mut self) {
        self.send.zeroize();
        self.recv.zeroize();
    }
}

/// Derive the send and receive keys shared with `peer_author`.
///
/// Both sides must pass the same call id and epoch. The authors are sorted
/// bytewise before they enter the KDF, and the direction labels are relative
/// to that sorted order, so one side's send key is exactly the other side's
/// receive key.
///
/// The peer public key must come from a control frame whose signature has
/// already been verified against `peer_author`. This function binds the key to
/// the call through the KDF, not to the identity by itself.
pub fn derive_pairwise(
    local_author: AuthorId,
    peer_author: AuthorId,
    local_secret: &StaticSecret,
    peer_public: [u8; 32],
    call_id: CallId,
    epoch: u32,
) -> Result<PairwiseKeys> {
    let peer = PublicKey::from(peer_public);
    let shared = local_secret.diffie_hellman(&peer);
    let shared_bytes = shared.as_bytes();

    // A low-order public key yields the all-zero point, which would make the
    // pairwise key predictable.
    if shared_bytes.iter().all(|byte| *byte == 0) {
        return Err(CryptoError::InvalidKey);
    }

    let local_bytes = local_author.as_bytes();
    let peer_bytes = peer_author.as_bytes();
    let local_is_first = local_bytes <= peer_bytes;
    let (first, second) = if local_is_first {
        (local_bytes, peer_bytes)
    } else {
        (peer_bytes, local_bytes)
    };

    let mut first_key = derive_direction(
        shared_bytes,
        &call_id,
        epoch,
        first,
        second,
        DIRECTION_FIRST_TO_SECOND,
    );
    let mut second_key = derive_direction(
        shared_bytes,
        &call_id,
        epoch,
        first,
        second,
        DIRECTION_SECOND_TO_FIRST,
    );

    let (mut send, mut recv) = if local_is_first {
        (first_key, second_key)
    } else {
        (second_key, first_key)
    };
    let keys = PairwiseKeys { send, recv };

    // The arrays above are copies; wipe the stack copies now that the owned
    // value holds them. `PairwiseKeys` zeroizes again when it is dropped.
    first_key.zeroize();
    second_key.zeroize();
    send.zeroize();
    recv.zeroize();
    Ok(keys)
}

/// BLAKE3 `derive_key` over the canonical pairwise material.
///
/// The material layout is:
/// `shared || call_id || epoch BE || first author || second author || label`.
fn derive_direction(
    shared: &[u8; 32],
    call_id: &CallId,
    epoch: u32,
    first: &[u8; 32],
    second: &[u8; 32],
    direction: &[u8],
) -> [u8; 32] {
    let mut material = Vec::with_capacity(32 + 16 + 4 + 32 + 32 + direction.len());
    material.extend_from_slice(shared);
    material.extend_from_slice(&call_id.0);
    material.extend_from_slice(&epoch.to_be_bytes());
    material.extend_from_slice(first);
    material.extend_from_slice(second);
    material.extend_from_slice(direction);
    let key = blake3::derive_key(KDF_CONTEXT, &material);
    material.zeroize();
    key
}
