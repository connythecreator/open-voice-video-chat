//! The per-call keyring and its per-peer ciphers.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use ovvc_core::AuthorId;
use ovvc_media::CallId;
use rand::rngs::OsRng;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

use crate::cipher::MediaCipher;
use crate::error::{CryptoError, Result};
use crate::keys::{MediaKeypair, derive_pairwise};
use crate::topology::KeyTopology;

/// One peer's cipher state.
struct PeerCipher {
    epoch: u32,
    /// The authenticated public key the cipher was derived from, so a repeat
    /// install can be recognised instead of rebuilding the cipher.
    public: [u8; 32],
    /// The peer's media state, serialised so concurrent sends and receives
    /// cannot race on its counter and replay window.
    cipher: Mutex<MediaCipher>,
}

/// The media keyring for one call member.
///
/// It owns the local ephemeral X25519 keypair and one [`MediaCipher`] per
/// peer. `encrypt_for` and `decrypt_from` take `&self`, so the caller can put
/// the keyring behind an `Arc<RwLock<MediaKeyring>>` and encrypt from several
/// threads; `install_peer`, `remove_peer`, `rotate`, `rotate_to`, `set_hub` and
/// `clear` need exclusive access.
///
/// A star call sets its hub with [`set_hub`](Self::set_hub). The hub role then
/// has [`decrypt_from_leaf`](Self::decrypt_from_leaf),
/// [`encrypt_for_listener`](Self::encrypt_for_listener) and
/// [`reencrypt_for_listener`](Self::reencrypt_for_listener), which is the
/// decrypt, mix and re-seal path. A leaf uses the ordinary
/// [`encrypt_for`](Self::encrypt_for) and [`decrypt_from`](Self::decrypt_from)
/// with the hub as its only peer.
///
/// The peer's public key must already have been authenticated by the signed
/// control frame that carried it. This type does not verify signatures.
pub struct MediaKeyring {
    local_author: AuthorId,
    call_id: CallId,
    secret: StaticSecret,
    public: [u8; 32],
    epoch: u32,
    /// The star hub for this call, `None` for a mesh or a direct pair.
    hub: Option<AuthorId>,
    peers: BTreeMap<AuthorId, PeerCipher>,
}

impl MediaKeyring {
    /// Start a call at epoch one with a fresh local keypair.
    pub fn generate(local_author: AuthorId, call_id: CallId) -> Self {
        let keypair = MediaKeypair::generate();
        let (secret, public) = keypair.into_parts();
        Self {
            local_author,
            call_id,
            secret,
            public,
            epoch: 1,
            hub: None,
            peers: BTreeMap::new(),
        }
    }

    /// The current key epoch.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// The local public key to publish in a signed control frame.
    pub fn public_key(&self) -> [u8; 32] {
        self.public
    }

    /// Derive and install the cipher for `author`'s public key.
    ///
    /// Only a peer key from the local epoch is accepted; an older or newer
    /// epoch returns [`CryptoError::WrongEpoch`]. Installing the same key for
    /// the same epoch again is a no-op, so the send counter and replay window
    /// are preserved. A different key at an already installed epoch returns
    /// [`CryptoError::KeyConflict`]: accepting it would start the send counter
    /// at one under a new key at the same epoch, and a rotation is required
    /// before the peer's key may change.
    pub fn install_peer(&mut self, author: AuthorId, epoch: u32, public: [u8; 32]) -> Result<()> {
        if epoch != self.epoch {
            return Err(CryptoError::WrongEpoch);
        }
        if author == self.local_author {
            return Err(CryptoError::InvalidKey);
        }
        if let Some(existing) = self.peers.get(&author) {
            if existing.epoch == epoch && existing.public == public {
                return Ok(());
            }
            if existing.epoch == epoch {
                return Err(CryptoError::KeyConflict);
            }
        }
        let keys = derive_pairwise(
            self.local_author,
            author,
            &self.secret,
            public,
            self.call_id,
            epoch,
        )?;
        self.peers.insert(
            author,
            PeerCipher {
                epoch,
                public,
                cipher: Mutex::new(MediaCipher::new(keys, epoch)),
            },
        );
        Ok(())
    }

    /// Forget the cipher for `author`, if any.
    pub fn remove_peer(&mut self, author: AuthorId) {
        self.peers.remove(&author);
    }

    /// Whether a cipher is installed for `author`.
    pub fn has_peer(&self, author: AuthorId) -> bool {
        self.peers.contains_key(&author)
    }

    /// The epoch of the installed cipher for `author`, if any.
    pub fn peer_epoch(&self, author: AuthorId) -> Option<u32> {
        self.peers.get(&author).map(|peer| peer.epoch)
    }

    /// The star hub this keyring is configured for, `None` for a mesh or a
    /// direct pair.
    pub fn hub(&self) -> Option<AuthorId> {
        self.hub
    }

    /// Whether this member is the star hub.
    pub fn is_hub(&self) -> bool {
        self.hub == Some(self.local_author)
    }

    /// The local author id, for role checks without a separate field.
    pub fn local_author(&self) -> AuthorId {
        self.local_author
    }

    /// Install a peer key only when `keys` makes `author` a key peer of the
    /// local member.
    ///
    /// This is the per-call key rule: in a star a leaf installs only the hub
    /// and the hub installs every leaf, while a mesh installs everyone else.
    /// A member never installs a key for itself, and an author outside
    /// `members` is ignored. Returns whether a cipher was installed.
    pub fn install_peer_for_topology(
        &mut self,
        keys: KeyTopology,
        author: AuthorId,
        epoch: u32,
        public: [u8; 32],
        members: &[AuthorId],
    ) -> Result<bool> {
        if !keys.includes(self.local_author, author, members) {
            return Ok(false);
        }
        self.install_peer(author, epoch, public)?;
        Ok(true)
    }

    /// Configure the star hub for this call.
    ///
    /// When the hub changes, the keyring moves to the next epoch and clears
    /// every peer cipher, because those keys belonged to the old hub's epoch.
    /// This is what makes a takeover a re-key: the old epoch cannot be read by
    /// the new hub and no leaf keeps a stale leaf-to-hub key. Returns the new
    /// epoch, or `None` when the hub did not change.
    ///
    /// A mesh or direct pair leaves the hub at `None`; repeated `None` calls
    /// do not rotate, so the mesh and two-person cases are unchanged.
    pub fn set_hub(&mut self, hub: Option<AuthorId>) -> Option<u32> {
        if self.hub == hub {
            return None;
        }
        self.hub = hub;
        Some(self.rotate())
    }

    /// Configure the star hub and adopt the epoch the control plane names.
    ///
    /// The control plane advances the key epoch when the hub changes, and
    /// every daemon should use that value so all members converge on it. If
    /// the named epoch is newer than the local one, the keyring rotates to it.
    /// If it is the epoch the keyring already reached (for example because the
    /// roster change that removed the old hub rotated first), the hub is
    /// adopted and the old peer ciphers are dropped without spinning the
    /// epoch again. A named epoch older than the local one returns
    /// [`CryptoError::WrongEpoch`]. Returns the epoch, or `None` when the hub
    /// did not change.
    pub fn set_hub_at(&mut self, hub: Option<AuthorId>, epoch: u32) -> Result<Option<u32>> {
        if self.hub == hub {
            return Ok(None);
        }
        if epoch < self.epoch {
            return Err(CryptoError::WrongEpoch);
        }
        self.hub = hub;
        if epoch > self.epoch {
            self.rotate_to(epoch)?;
        } else {
            // The epoch already moved, but the ciphers belonged to the old
            // hub's epoch. Peers reinstall for the current epoch.
            self.peers.clear();
        }
        Ok(Some(epoch))
    }

    /// Decrypt a leaf's uplink at the hub.
    ///
    /// Only the hub may call this, and a leaf never sends to itself. The
    /// resulting plaintext can be mixed and then sealed with
    /// [`encrypt_for_listener`](Self::encrypt_for_listener).
    pub fn decrypt_from_leaf(&self, leaf: AuthorId, payload: &[u8]) -> Result<Vec<u8>> {
        self.require_hub()?;
        if leaf == self.local_author {
            return Err(CryptoError::InvalidKey);
        }
        self.decrypt_from(leaf, payload)
    }

    /// Re-seal a downlink for one listener at the hub.
    ///
    /// This seals `plaintext` with the listener's leaf-to-hub key. The caller
    /// has already decrypted and mixed the leaf streams; the hub's own voice
    /// goes through the same call. A listener must not be the hub itself.
    pub fn encrypt_for_listener(
        &self,
        listener: AuthorId,
        plaintext: &[u8],
    ) -> Result<Vec<u8>> {
        self.require_hub()?;
        if listener == self.local_author {
            return Err(CryptoError::InvalidKey);
        }
        self.encrypt_for(listener, plaintext)
    }

    /// Decrypt one leaf's frame and re-seal it for another listener.
    ///
    /// This is the one-shot hub re-encrypt step. The frame is decrypted, so
    /// the replay window consumes it: use this once per frame, or use
    /// [`reencrypt_for_listeners`](Self::reencrypt_for_listeners) when one
    /// uplink becomes several downlinks. The listener must not be the source
    /// leaf: a listener's downlink excludes its own stream.
    pub fn reencrypt_for_listener(
        &self,
        leaf: AuthorId,
        listener: AuthorId,
        payload: &[u8],
    ) -> Result<Vec<u8>> {
        self.require_hub()?;
        if leaf == listener {
            return Err(CryptoError::InvalidKey);
        }
        let plaintext = self.decrypt_from_leaf(leaf, payload)?;
        self.encrypt_for_listener(listener, &plaintext)
    }

    /// Decrypt one leaf's frame once and re-seal it for every listener.
    ///
    /// This is the hub path that matches per-listener encryption: the replay
    /// window sees the uplink once, and each listener gets its own ciphertext
    /// under its own leaf-to-hub key. The source leaf is skipped, because a
    /// listener never receives its own stream. The hub itself is not a
    /// listener; the hub's own voice is sealed separately for every leaf.
    pub fn reencrypt_for_listeners(
        &self,
        leaf: AuthorId,
        listeners: &[AuthorId],
        payload: &[u8],
    ) -> Result<Vec<(AuthorId, Vec<u8>)>> {
        self.require_hub()?;
        let plaintext = self.decrypt_from_leaf(leaf, payload)?;
        let mut sealed = Vec::with_capacity(listeners.len());
        for listener in listeners {
            if *listener == leaf {
                continue;
            }
            sealed.push((
                *listener,
                self.encrypt_for_listener(*listener, &plaintext)?,
            ));
        }
        Ok(sealed)
    }

    fn require_hub(&self) -> Result<()> {
        match self.hub {
            Some(hub) if hub == self.local_author => Ok(()),
            Some(_) => Err(CryptoError::NotHub),
            None => Err(CryptoError::NoHub),
        }
    }

    /// Encrypt a frame for `author`.
    pub fn encrypt_for(&self, author: AuthorId, plaintext: &[u8]) -> Result<Vec<u8>> {
        let peer = self.peers.get(&author).ok_or(CryptoError::NoPeerKey)?;
        if peer.epoch != self.epoch {
            return Err(CryptoError::WrongEpoch);
        }
        lock_cipher(&peer.cipher).encrypt(plaintext)
    }

    /// Decrypt a frame from `author`.
    pub fn decrypt_from(&self, author: AuthorId, payload: &[u8]) -> Result<Vec<u8>> {
        let peer = self.peers.get(&author).ok_or(CryptoError::NoPeerKey)?;
        if peer.epoch != self.epoch {
            return Err(CryptoError::WrongEpoch);
        }
        lock_cipher(&peer.cipher).decrypt(payload)
    }

    /// Move directly to `epoch` with a fresh local secret.
    ///
    /// This is for initial alignment with a call that has already rotated many
    /// times, where stepping one epoch at a time would be needlessly expensive
    /// and could trip the daemon's epoch-advance limit. Like [`rotate`], it
    /// clears every peer cipher because they were derived from the old secret.
    /// `epoch` must be greater than the current epoch; an equal or older value
    /// returns [`CryptoError::WrongEpoch`].
    ///
    /// [`rotate`]: MediaKeyring::rotate
    pub fn rotate_to(&mut self, epoch: u32) -> Result<u32> {
        if epoch <= self.epoch {
            return Err(CryptoError::WrongEpoch);
        }
        self.secret.zeroize();
        self.secret = StaticSecret::random_from_rng(OsRng);
        self.public = PublicKey::from(&self.secret).to_bytes();
        self.peers.clear();
        self.epoch = epoch;
        Ok(epoch)
    }

    /// Move to the next epoch with a fresh local secret.
    ///
    /// Every peer cipher is cleared, because its keys belonged to the old
    /// epoch and secret; peers must be reinstalled from that epoch's control
    /// frames. Returns the new epoch.
    pub fn rotate(&mut self) -> u32 {
        self.secret.zeroize();
        self.secret = StaticSecret::random_from_rng(OsRng);
        self.public = PublicKey::from(&self.secret).to_bytes();
        self.peers.clear();
        // Exhausting a u32 epoch counter is not reachable in practice; a wrap
        // is still safer than a panic.
        self.epoch = self.epoch.checked_add(1).unwrap_or(1);
        self.epoch
    }

    /// Drop every peer cipher and start again at epoch one with a fresh local
    /// keypair. The local author and call id are retained.
    pub fn clear(&mut self) {
        self.peers.clear();
        self.secret.zeroize();
        self.secret = StaticSecret::random_from_rng(OsRng);
        self.public = PublicKey::from(&self.secret).to_bytes();
        self.epoch = 1;
    }
}

fn lock_cipher(cipher: &Mutex<MediaCipher>) -> MutexGuard<'_, MediaCipher> {
    match cipher.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_id() -> CallId {
        CallId([7; 16])
    }

    fn author(index: u8) -> AuthorId {
        AuthorId([index; 32])
    }

    struct Star {
        hub: MediaKeyring,
        leaves: Vec<MediaKeyring>,
        hub_author: AuthorId,
        leaf_authors: Vec<AuthorId>,
    }

    /// Three members, all starting at the same epoch, with the hub set and
    /// every leaf-to-hub key installed on both sides.
    fn star() -> Star {
        let hub_author = author(1);
        let leaf_authors = vec![author(2), author(3)];
        let mut hub = MediaKeyring::generate(hub_author, call_id());
        let mut leaves = leaf_authors
            .iter()
            .map(|leaf| MediaKeyring::generate(*leaf, call_id()))
            .collect::<Vec<_>>();

        let hub_epoch = hub.set_hub(Some(hub_author)).expect("hub epoch");
        for leaf in &mut leaves {
            let epoch = leaf.set_hub(Some(hub_author)).expect("leaf epoch");
            assert_eq!(epoch, hub_epoch, "every member starts on one epoch");
        }

        let hub_public = hub.public_key();
        for (leaf, leaf_author) in leaves.iter_mut().zip(&leaf_authors) {
            hub.install_peer(*leaf_author, hub_epoch, leaf.public_key())
                .expect("hub installs the leaf");
            leaf.install_peer(hub_author, hub_epoch, hub_public)
                .expect("leaf installs the hub");
        }

        Star {
            hub,
            leaves,
            hub_author,
            leaf_authors,
        }
    }

    #[test]
    fn the_hub_decrypts_a_leaf_and_reencrypts_for_the_other_listener() {
        let star = star();
        let (a, b) = (star.leaf_authors[0], star.leaf_authors[1]);
        let epoch = star.hub.epoch();
        assert_eq!(star.hub.hub(), Some(star.hub_author));
        assert!(star.hub.is_hub());
        assert!(!star.leaves[0].is_hub());

        // One uplink: decrypt once, then seal the mixed plaintext per
        // listener. This is the path the hub mixer uses.
        let uplink = star.leaves[0]
            .encrypt_for(star.hub_author, b"leaf-a frame")
            .expect("leaf uplink");
        let plaintext = star.hub.decrypt_from_leaf(a, &uplink).expect("hub decrypt");
        assert_eq!(plaintext, b"leaf-a frame");
        let downlink = star.hub
            .encrypt_for_listener(b, &plaintext)
            .expect("hub re-encrypt");
        assert_eq!(
            star.leaves[1].decrypt_from(star.hub_author, &downlink),
            Ok(b"leaf-a frame".to_vec())
        );
        // A's own cipher must not open the downlink for B.
        assert!(star.leaves[0].decrypt_from(star.hub_author, &downlink).is_err());
        assert_eq!(star.hub.epoch(), epoch);

        // The one-shot re-encrypt, with its own frame.
        let second = star.leaves[0]
            .encrypt_for(star.hub_author, b"second frame")
            .expect("leaf uplink");
        let downlink = star.hub
            .reencrypt_for_listener(a, b, &second)
            .expect("hub re-encrypt");
        assert_eq!(
            star.leaves[1].decrypt_from(star.hub_author, &downlink),
            Ok(b"second frame".to_vec())
        );
    }

    #[test]
    fn one_uplink_becomes_one_downlink_per_listener() {
        let star = star();
        let a = star.leaf_authors[0];
        let b = star.leaf_authors[1];
        let uplink = star.leaves[0]
            .encrypt_for(star.hub_author, b"leaf-a frame")
            .expect("leaf uplink");
        let sealed = star
            .hub
            .reencrypt_for_listeners(a, &[a, b], &uplink)
            .expect("one decrypt, two seals");
        assert_eq!(
            sealed.iter().map(|(to, _)| *to).collect::<Vec<_>>(),
            vec![b],
            "the source leaf is excluded from its own downlink"
        );
        assert_eq!(
            star.leaves[1].decrypt_from(star.hub_author, &sealed[0].1),
            Ok(b"leaf-a frame".to_vec())
        );

        // The uplink was decrypted once. A second re-encrypt of the same
        // ciphertext is a replay, not another free decrypt.
        assert_eq!(
            star.hub.reencrypt_for_listeners(a, &[b], &uplink),
            Err(CryptoError::Replay)
        );
    }

    #[test]
    fn the_hub_seals_its_own_voice_for_each_listener() {
        let star = star();
        let b = star.leaf_authors[1];
        let downlink = star
            .hub
            .encrypt_for_listener(b, b"hub voice")
            .expect("hub voice");
        assert_eq!(
            star.leaves[1].decrypt_from(star.hub_author, &downlink),
            Ok(b"hub voice".to_vec())
        );
        // The hub never seals a downlink for itself.
        assert_eq!(
            star.hub.encrypt_for_listener(star.hub_author, b"self"),
            Err(CryptoError::InvalidKey)
        );
    }

    #[test]
    fn a_leaf_cannot_use_the_hub_operations() {
        let star = star();
        let (a, b) = (star.leaf_authors[0], star.leaf_authors[1]);
        let uplink = star.leaves[0]
            .encrypt_for(star.hub_author, b"leaf-a frame")
            .expect("leaf uplink");
        assert_eq!(
            star.leaves[0].decrypt_from_leaf(a, &uplink),
            Err(CryptoError::NotHub)
        );
        assert_eq!(
            star.leaves[0].encrypt_for_listener(b, b"nope"),
            Err(CryptoError::NotHub)
        );
        assert_eq!(
            star.leaves[0].reencrypt_for_listener(a, b, &uplink),
            Err(CryptoError::NotHub)
        );
    }

    #[test]
    fn a_hub_never_reencrypts_a_leaf_frame_back_to_the_same_leaf() {
        let star = star();
        let a = star.leaf_authors[0];
        let uplink = star.leaves[0]
            .encrypt_for(star.hub_author, b"leaf-a frame")
            .expect("leaf uplink");
        assert_eq!(
            star.hub.reencrypt_for_listener(a, a, &uplink),
            Err(CryptoError::InvalidKey),
            "a listener's downlink excludes its own stream"
        );
    }

    #[test]
    fn a_star_operation_before_a_hub_is_configured_is_refused() {
        let mut ring = MediaKeyring::generate(author(1), call_id());
        let peer = author(2);
        ring.install_peer(peer, 1, [9; 32]).ok();
        assert_eq!(ring.hub(), None);
        assert_eq!(
            ring.encrypt_for_listener(peer, b"no hub"),
            Err(CryptoError::NoHub)
        );
        assert_eq!(
            ring.decrypt_from_leaf(peer, &[0; 32]),
            Err(CryptoError::NoHub)
        );
    }

    #[test]
    fn a_hub_change_rotates_the_epoch_and_clears_the_old_ciphers() {
        let mut star = star();
        let a = star.leaf_authors[0];
        let old_epoch = star.hub.epoch();
        let old_uplink = star.leaves[0]
            .encrypt_for(star.hub_author, b"before takeover")
            .expect("leaf uplink");

        let new_hub = author(9);
        let new_epoch = star.hub.set_hub(Some(new_hub)).expect("a new epoch");
        assert_eq!(new_epoch, old_epoch + 1);
        assert_eq!(star.hub.epoch(), new_epoch);
        assert_eq!(star.hub.hub(), Some(new_hub));
        assert!(!star.hub.is_hub(), "the old hub is now a leaf");
        assert!(
            !star.hub.has_peer(a),
            "the old hub's leaf ciphers are gone with the old epoch"
        );
        assert_eq!(
            star.hub.decrypt_from_leaf(a, &old_uplink),
            Err(CryptoError::NotHub),
            "the old hub is a leaf now, and its old epoch ciphers are gone"
        );

        // Naming the same hub again changes nothing and does not spin the
        // epoch.
        assert_eq!(star.hub.set_hub(Some(new_hub)), None);
        assert_eq!(star.hub.epoch(), new_epoch);
    }

    #[test]
    fn set_hub_at_adopts_the_epoch_the_control_plane_names() {
        let mut ring = MediaKeyring::generate(author(1), call_id());
        assert_eq!(ring.set_hub_at(Some(author(1)), 4), Ok(Some(4)));
        assert_eq!(ring.epoch(), 4);

        // The same hub does not spin the epoch, even with a newer value.
        assert_eq!(ring.set_hub_at(Some(author(1)), 5), Ok(None));
        assert_eq!(ring.epoch(), 4);

        // A new hub with a newer epoch rotates to exactly that epoch.
        assert_eq!(ring.set_hub_at(Some(author(2)), 5), Ok(Some(5)));
        assert_eq!(ring.epoch(), 5);

        // The roster rotation may have already moved the epoch; adopting the
        // hub at that same epoch must not rotate again.
        assert_eq!(ring.set_hub_at(Some(author(3)), 5), Ok(Some(5)));
        assert_eq!(ring.epoch(), 5);
        assert_eq!(ring.hub(), Some(author(3)));

        // An epoch behind the local one cannot be adopted.
        assert_eq!(
            ring.set_hub_at(Some(author(4)), 4),
            Err(CryptoError::WrongEpoch),
            "a hub change cannot move the epoch backwards"
        );
        assert_eq!(ring.hub(), Some(author(3)), "a failed change is not adopted");
    }

    #[test]
    fn a_leaf_installs_only_the_hub_key_in_a_star() {
        let members = [author(1), author(2), author(3)];
        let hub = author(1);
        let keys = KeyTopology::for_call(&ovvc_media::CallTopology::Star {
            hub,
            leaves: vec![author(2), author(3)],
        });
        let mut leaf = MediaKeyring::generate(author(2), call_id());
        assert!(
            leaf.install_peer_for_topology(keys, hub, 1, [5; 32], &members)
                .expect("hub key")
        );
        assert!(leaf.has_peer(hub));
        assert!(
            !leaf
                .install_peer_for_topology(keys, author(3), 1, [6; 32], &members)
                .expect("other leaf")
        );
        assert!(!leaf.has_peer(author(3)), "leaves never key each other");
        assert!(
            !leaf
                .install_peer_for_topology(keys, author(2), 1, [7; 32], &members)
                .expect("self")
        );
        assert!(
            !leaf
                .install_peer_for_topology(keys, author(9), 1, [8; 32], &members)
                .expect("outside the call")
        );
    }

    #[test]
    fn the_hub_installs_a_key_for_every_leaf() {
        let members = [author(1), author(2), author(3)];
        let hub = author(1);
        let keys = KeyTopology::for_call(&ovvc_media::CallTopology::Star {
            hub,
            leaves: vec![author(2), author(3)],
        });
        let mut ring = MediaKeyring::generate(hub, call_id());
        for (index, leaf) in [author(2), author(3)].into_iter().enumerate() {
            assert!(
                ring.install_peer_for_topology(keys, leaf, 1, [index as u8 + 5; 32], &members)
                    .expect("leaf key")
            );
        }
        assert!(ring.has_peer(author(2)));
        assert!(ring.has_peer(author(3)));
    }
}
