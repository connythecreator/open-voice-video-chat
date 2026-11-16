//! The leaf-to-hub key schedule for a star call.
//!
//! These tests model the intended star key ring directly: a leaf installs the
//! hub key only, the hub installs every leaf key, and a takeover starts a new
//! epoch that does not inherit the old hub's pairwise keys. They are the
//! crypto half of the star media acceptance contract. The daemon e2e
//! (`ovvc-daemon --test star_media_e2e`) proves the runtime wiring.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-crypto --test star_keyring
//! ```

use ovvc_core::{AuthorId, CallScope};
use ovvc_crypto::{CryptoError, KeyTopology, MediaKeyring};
use ovvc_media::{CallId, CallTopology, topology_for};

const CALL: CallId = CallId([0x5a; 16]);

fn author(seed: u8) -> AuthorId {
    AuthorId([seed; 32])
}

/// The flat star for a hub and two leaves under an explicit star scope.
fn star(hub: AuthorId, leaves: [AuthorId; 2]) -> CallTopology {
    let members = [leaves[0], hub, leaves[1]];
    topology_for(&members, CallScope::Star, Some(hub))
}

/// Install only the peers `KeyTopology` names for `local`.
fn install_visible(
    local: AuthorId,
    members: &[AuthorId],
    keys: KeyTopology,
    keyrings: &mut [(AuthorId, MediaKeyring)],
) {
    let visible: Vec<(AuthorId, u32, [u8; 32])> = keys
        .peers_for(local, members)
        .into_iter()
        .map(|peer| {
            let (_, keyring) = keyrings
                .iter()
                .find(|(candidate, _)| *candidate == peer)
                .expect("every star peer has a keyring");
            (peer, keyring.epoch(), keyring.public_key())
        })
        .collect();
    let (_, local_keyring) = keyrings
        .iter_mut()
        .find(|(candidate, _)| *candidate == local)
        .expect("the local member has a keyring");
    for (peer, epoch, public) in visible {
        local_keyring
            .install_peer(peer, epoch, public)
            .unwrap_or_else(|error| panic!("installing {peer:?}: {error}"));
    }
}

#[test]
fn a_leaf_installs_only_the_hub_and_the_hub_installs_every_leaf() {
    let leaf_a = author(1);
    let leaf_b = author(2);
    let hub = author(3);
    let members = [leaf_a, hub, leaf_b];
    let topology = star(hub, [leaf_a, leaf_b]);
    let keys = KeyTopology::for_call(&topology);
    assert_eq!(keys, KeyTopology::Star { hub });
    assert_eq!(keys.peers_for(leaf_a, &members), vec![hub]);
    assert_eq!(keys.peers_for(leaf_b, &members), vec![hub]);
    assert_eq!(
        keys.peers_for(hub, &members),
        vec![leaf_a, leaf_b],
        "the hub re-seals each recipient's copy"
    );

    let mut keyrings = [
        (leaf_a, MediaKeyring::generate(leaf_a, CALL)),
        (hub, MediaKeyring::generate(hub, CALL)),
        (leaf_b, MediaKeyring::generate(leaf_b, CALL)),
    ];
    for local in members {
        install_visible(local, &members, keys, &mut keyrings);
    }
    let leaf_a_keys = &keyrings[0].1;
    let hub_keys = &keyrings[1].1;
    let leaf_b_keys = &keyrings[2].1;

    assert!(leaf_a_keys.has_peer(hub), "the leaf must hold the hub key");
    assert!(
        !leaf_a_keys.has_peer(leaf_b),
        "the leaf must not hold another leaf's key: {:?}",
        leaf_a_keys.peer_epoch(leaf_b)
    );
    assert!(leaf_b_keys.has_peer(hub));
    assert!(!leaf_b_keys.has_peer(leaf_a));
    assert!(hub_keys.has_peer(leaf_a));
    assert!(hub_keys.has_peer(leaf_b));
    assert!(
        !hub_keys.has_peer(hub),
        "the hub must not key itself as a peer"
    );
}

#[test]
fn the_hub_reseals_per_listener_and_leaves_cannot_read_each_other() {
    let leaf_a = author(1);
    let leaf_b = author(2);
    let hub = author(3);
    let members = [leaf_a, hub, leaf_b];
    let topology = star(hub, [leaf_a, leaf_b]);
    let keys = KeyTopology::for_call(&topology);
    let mut keyrings = [
        (leaf_a, MediaKeyring::generate(leaf_a, CALL)),
        (hub, MediaKeyring::generate(hub, CALL)),
        (leaf_b, MediaKeyring::generate(leaf_b, CALL)),
    ];
    for local in members {
        install_visible(local, &members, keys, &mut keyrings);
    }
    let leaf_a_keys = &keyrings[0].1;
    let hub_keys = &keyrings[1].1;
    let leaf_b_keys = &keyrings[2].1;

    // A leaf seals its uplink only for the hub, and the hub can open it.
    let uplink_a = leaf_a_keys.encrypt_for(hub, b"leaf a uplink").unwrap();
    assert_eq!(
        hub_keys.decrypt_from(leaf_a, &uplink_a).unwrap(),
        b"leaf a uplink"
    );
    // Another leaf has no cipher for A at all, so it cannot even try.
    assert!(matches!(
        leaf_b_keys.decrypt_from(leaf_a, &uplink_a),
        Err(CryptoError::NoPeerKey)
    ));

    // The hub re-seals the same mix for each listener under that listener's
    // pairwise key. The two ciphertexts must differ, and each only opens with
    // its own listener key.
    let mix = b"the hub mix for one listener";
    let for_a = hub_keys.encrypt_for(leaf_a, mix).unwrap();
    let for_b = hub_keys.encrypt_for(leaf_b, mix).unwrap();
    assert_ne!(
        for_a, for_b,
        "the hub sent the same ciphertext to two listeners"
    );
    // Try the cross-listener frames before either side opens its own copy,
    // so the replay window cannot be what rejects them. A wrong AEAD key is
    // the only way to fail here.
    assert!(
        matches!(
            leaf_a_keys.decrypt_from(hub, &for_b),
            Err(CryptoError::Decrypt)
        ),
        "leaf A opened a frame the hub sealed for leaf B"
    );
    assert!(
        matches!(
            leaf_b_keys.decrypt_from(hub, &for_a),
            Err(CryptoError::Decrypt)
        ),
        "leaf B opened a frame the hub sealed for leaf A"
    );
    assert_eq!(leaf_a_keys.decrypt_from(hub, &for_a).unwrap(), mix);
    assert_eq!(leaf_b_keys.decrypt_from(hub, &for_b).unwrap(), mix);

    // A leaf has no key for the other leaf, so it cannot encrypt a direct
    // frame either. This is the crypto half of the no-leaf-to-leaf rule.
    assert!(matches!(
        leaf_a_keys.encrypt_for(leaf_b, b"direct"),
        Err(CryptoError::NoPeerKey)
    ));
}

#[test]
fn a_takeover_rekeys_every_leaf_and_rejects_the_old_epoch() {
    let leaf_a = author(1);
    let backup = author(2);
    let leaf_c = author(3);
    let old_hub = author(4);

    // A four-member star at epoch one. The backup is a normal leaf until it
    // takes over, so it installs only the old hub key.
    let mut keyrings = [
        (leaf_a, MediaKeyring::generate(leaf_a, CALL)),
        (backup, MediaKeyring::generate(backup, CALL)),
        (leaf_c, MediaKeyring::generate(leaf_c, CALL)),
        (old_hub, MediaKeyring::generate(old_hub, CALL)),
    ];
    let members = [leaf_a, backup, leaf_c, old_hub];
    let old_keys = KeyTopology::for_call(&topology_for(&members, CallScope::Star, Some(old_hub)));
    assert_eq!(old_keys, KeyTopology::Star { hub: old_hub });
    for local in members {
        install_visible(local, &members, old_keys, &mut keyrings);
    }
    let old_hub_public = keyrings[3].1.public_key();
    let old_downlink = keyrings[3]
        .1
        .encrypt_for(leaf_a, b"epoch one downlink")
        .unwrap();
    let old_uplink = keyrings[0]
        .1
        .encrypt_for(old_hub, b"epoch one uplink")
        .unwrap();

    // The backup becomes the hub and starts epoch two. Every survivor rotates
    // to the new epoch and installs the new hub key only. A rotate clears
    // every old peer cipher, so the old hub key does not survive.
    let new_hub = backup;
    let new_epoch = 2;
    for (_, keyring) in &mut keyrings {
        keyring.rotate_to(new_epoch).unwrap();
    }
    let new_members = [leaf_a, new_hub, leaf_c];
    let new_keys =
        KeyTopology::for_call(&topology_for(&new_members, CallScope::Star, Some(new_hub)));
    assert_eq!(new_keys, KeyTopology::Star { hub: new_hub });
    for local in new_members {
        install_visible(local, &new_members, new_keys, &mut keyrings);
    }

    let stale_install = keyrings[0].1.install_peer(old_hub, 1, old_hub_public);
    let leaf_a_keys = &keyrings[0].1;
    let new_hub_keys = &keyrings[1].1;
    let leaf_c_keys = &keyrings[2].1;
    assert_eq!(leaf_a_keys.epoch(), new_epoch);
    assert_eq!(new_hub_keys.epoch(), new_epoch);
    assert_eq!(leaf_c_keys.epoch(), new_epoch);
    assert!(
        !leaf_a_keys.has_peer(old_hub),
        "the leaf still holds the dead hub's pairwise key"
    );
    assert!(
        matches!(stale_install, Err(CryptoError::WrongEpoch)),
        "the old epoch key was accepted after the takeover"
    );

    // The old hub is no longer a key peer, so its epoch-one downlink is not
    // even a ciphertext leaf A can try to open. A frame from leaf A sealed
    // for the old hub at epoch one is authenticated under the old pairwise
    // key, so the new hub rejects it rather than forwarding it.
    assert!(matches!(
        leaf_a_keys.decrypt_from(old_hub, &old_downlink),
        Err(CryptoError::NoPeerKey)
    ));
    assert!(
        matches!(
            new_hub_keys.decrypt_from(leaf_a, &old_uplink),
            Err(CryptoError::WrongEpoch) | Err(CryptoError::Decrypt)
        ),
        "the new hub accepted an epoch-one uplink"
    );

    // The new epoch works through the new hub in both directions.
    let uplink = leaf_a_keys
        .encrypt_for(new_hub, b"epoch two uplink")
        .unwrap();
    assert_eq!(
        new_hub_keys.decrypt_from(leaf_a, &uplink).unwrap(),
        b"epoch two uplink"
    );
    let downlink = new_hub_keys
        .encrypt_for(leaf_a, b"epoch two downlink")
        .unwrap();
    let for_c = new_hub_keys
        .encrypt_for(leaf_c, b"epoch two for c")
        .unwrap();

    // Per-listener encryption survives the takeover: a copy sealed for leaf C
    // is not readable by leaf A. Cross-check before either leaf opens its own
    // copy, so the replay window cannot be what rejects it.
    assert!(
        matches!(
            leaf_a_keys.decrypt_from(new_hub, &for_c),
            Err(CryptoError::Decrypt)
        ),
        "leaf A opened a frame the new hub sealed for leaf C"
    );
    assert_eq!(
        leaf_a_keys.decrypt_from(new_hub, &downlink).unwrap(),
        b"epoch two downlink"
    );
    assert_eq!(
        leaf_c_keys.decrypt_from(new_hub, &for_c).unwrap(),
        b"epoch two for c"
    );
}

#[test]
fn a_two_member_call_uses_one_direct_pairwise_key_and_no_hub() {
    let a = author(1);
    let b = author(2);
    let members = [a, b];
    // A two-member channel call is a direct pair under every scope, including
    // an explicit star scope.
    let topology = topology_for(&members, CallScope::Star, Some(a));
    assert_eq!(
        topology,
        CallTopology::Mesh,
        "a two-member call must not grow a hub"
    );
    assert_eq!(topology.hub(), None);

    let keys = KeyTopology::for_call(&topology);
    assert_eq!(keys, KeyTopology::AllPairs);
    assert_eq!(keys.hub(), None);
    assert_eq!(keys.peers_for(a, &members), vec![b]);
    assert_eq!(keys.peers_for(b, &members), vec![a]);

    let mut a_keys = MediaKeyring::generate(a, CALL);
    let mut b_keys = MediaKeyring::generate(b, CALL);
    a_keys.install_peer(b, 1, b_keys.public_key()).unwrap();
    b_keys.install_peer(a, 1, a_keys.public_key()).unwrap();
    let from_a = a_keys.encrypt_for(b, b"direct pair").unwrap();
    assert_eq!(b_keys.decrypt_from(a, &from_a).unwrap(), b"direct pair");
    let from_b = b_keys.encrypt_for(a, b"direct pair reply").unwrap();
    assert_eq!(
        a_keys.decrypt_from(b, &from_b).unwrap(),
        b"direct pair reply"
    );
}
