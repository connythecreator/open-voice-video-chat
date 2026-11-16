pub use libp2p::{Multiaddr, PeerId};

pub mod announce;
pub mod node;

use ovvc_core::AuthorId;

/// Derive the libp2p transport identity from an ovvc author identity.
///
/// Both are built from the same ed25519 key, so this is a deterministic
/// mapping: dialing `/p2p/<peer>` pins the author key at the transport layer.
pub fn peer_id_for_author(author: AuthorId) -> Option<PeerId> {
    let key = libp2p::identity::ed25519::PublicKey::try_from_bytes(author.as_bytes()).ok()?;
    Some(libp2p::identity::PublicKey::from(key).to_peer_id())
}
