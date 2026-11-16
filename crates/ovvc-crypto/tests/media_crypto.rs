//! End-to-end behaviour of the pairwise media key schedule and cipher.

use ovvc_core::AuthorId;
use ovvc_crypto::{
    CryptoError, HEADER_BYTES, MAX_CIPHERTEXT_BYTES, MAX_PLAINTEXT_BYTES, MediaCipher,
    MediaKeyring, ReplayWindow, TAG_BYTES, WIRE_OVERHEAD_BYTES, derive_pairwise,
};
use ovvc_media::CallId;
use rand::rngs::OsRng;
use x25519_dalek::{PublicKey, StaticSecret};

fn author(seed: u8) -> AuthorId {
    AuthorId([seed; 32])
}

fn call(seed: u8) -> CallId {
    CallId([seed; 16])
}

/// Two ciphers that share a call id, epoch and pairwise secret.
fn matching_ciphers(call_seed: u8, epoch: u32) -> (MediaCipher, MediaCipher) {
    let a = author(1);
    let b = author(2);
    let call_id = call(call_seed);
    let secret_a = StaticSecret::random_from_rng(OsRng);
    let secret_b = StaticSecret::random_from_rng(OsRng);
    let public_a = PublicKey::from(&secret_a).to_bytes();
    let public_b = PublicKey::from(&secret_b).to_bytes();

    let a_keys = derive_pairwise(a, b, &secret_a, public_b, call_id, epoch).unwrap();
    let b_keys = derive_pairwise(b, a, &secret_b, public_a, call_id, epoch).unwrap();
    (
        MediaCipher::new(a_keys, epoch),
        MediaCipher::new(b_keys, epoch),
    )
}

#[test]
fn pairwise_directions_match_and_are_distinct() {
    let a = author(1);
    let b = author(2);
    let call_id = call(7);
    let secret_a = StaticSecret::random_from_rng(OsRng);
    let secret_b = StaticSecret::random_from_rng(OsRng);
    let public_a = PublicKey::from(&secret_a).to_bytes();
    let public_b = PublicKey::from(&secret_b).to_bytes();

    let a_cipher = MediaCipher::new(
        derive_pairwise(a, b, &secret_a, public_b, call_id, 1).unwrap(),
        1,
    );
    let b_cipher = MediaCipher::new(
        derive_pairwise(b, a, &secret_b, public_a, call_id, 1).unwrap(),
        1,
    );

    let to_b = a_cipher.encrypt(b"hello from a").unwrap();
    assert_eq!(b_cipher.decrypt(&to_b).unwrap(), b"hello from a");

    let to_a = b_cipher.encrypt(b"hello from b").unwrap();
    assert_eq!(a_cipher.decrypt(&to_a).unwrap(), b"hello from b");

    // A's receive key is not its send key, so it cannot open its own frame.
    let own = a_cipher.encrypt(b"self").unwrap();
    assert!(matches!(a_cipher.decrypt(&own), Err(CryptoError::Decrypt)));
}

#[test]
fn different_call_epoch_or_author_pair_gives_a_different_key() {
    let a = author(1);
    let b = author(2);
    let c = author(3);
    let call_id = call(8);
    let secret_a = StaticSecret::random_from_rng(OsRng);
    let secret_b = StaticSecret::random_from_rng(OsRng);
    let public_a = PublicKey::from(&secret_a).to_bytes();
    let public_b = PublicKey::from(&secret_b).to_bytes();
    let plaintext = b"binding";

    // Different call id.
    let sender = MediaCipher::new(
        derive_pairwise(a, b, &secret_a, public_b, call_id, 1).unwrap(),
        1,
    );
    let receiver = MediaCipher::new(
        derive_pairwise(b, a, &secret_b, public_a, call(9), 1).unwrap(),
        1,
    );
    let frame = sender.encrypt(plaintext).unwrap();
    assert!(matches!(
        receiver.decrypt(&frame),
        Err(CryptoError::Decrypt)
    ));

    // Different epoch in the KDF, with the cipher still claiming epoch one so
    // only the key differs.
    let sender = MediaCipher::new(
        derive_pairwise(a, b, &secret_a, public_b, call_id, 1).unwrap(),
        1,
    );
    let receiver = MediaCipher::new(
        derive_pairwise(b, a, &secret_b, public_a, call_id, 2).unwrap(),
        1,
    );
    let frame = sender.encrypt(plaintext).unwrap();
    assert!(matches!(
        receiver.decrypt(&frame),
        Err(CryptoError::Decrypt)
    ));

    // Different author pair.
    let sender = MediaCipher::new(
        derive_pairwise(a, b, &secret_a, public_b, call_id, 1).unwrap(),
        1,
    );
    let receiver = MediaCipher::new(
        derive_pairwise(c, a, &secret_b, public_a, call_id, 1).unwrap(),
        1,
    );
    let frame = sender.encrypt(plaintext).unwrap();
    assert!(matches!(
        receiver.decrypt(&frame),
        Err(CryptoError::Decrypt)
    ));
}

#[test]
fn author_order_controls_direction_assignment() {
    let a = author(1);
    let b = author(2);
    let call_id = call(15);
    let secret_a = StaticSecret::random_from_rng(OsRng);
    let secret_b = StaticSecret::random_from_rng(OsRng);
    let public_a = PublicKey::from(&secret_a).to_bytes();
    let public_b = PublicKey::from(&secret_b).to_bytes();

    let sender = MediaCipher::new(
        derive_pairwise(a, b, &secret_a, public_b, call_id, 1).unwrap(),
        1,
    );
    // B passes the pair in A's order instead of its own. The two keys exist,
    // but send and receive are assigned to the wrong directions on B.
    let receiver = MediaCipher::new(
        derive_pairwise(a, b, &secret_b, public_a, call_id, 1).unwrap(),
        1,
    );

    let frame = sender.encrypt(b"order").unwrap();
    assert!(matches!(
        receiver.decrypt(&frame),
        Err(CryptoError::Decrypt)
    ));
}

#[test]
fn wire_header_is_twelve_bytes_and_ciphertext_differs() {
    let (sender, receiver) = matching_ciphers(9, 1);
    let plaintext = b"header check";
    let frame = sender.encrypt(plaintext).unwrap();

    assert_eq!(&frame[..4], &1u32.to_be_bytes());
    assert_eq!(&frame[4..HEADER_BYTES], &1u64.to_be_bytes());
    assert_eq!(frame.len(), HEADER_BYTES + plaintext.len() + TAG_BYTES);
    let body = &frame[HEADER_BYTES..frame.len() - TAG_BYTES];
    assert_ne!(body, plaintext.as_slice());

    let second = sender.encrypt(plaintext).unwrap();
    assert_eq!(&second[4..HEADER_BYTES], &2u64.to_be_bytes());
    assert_ne!(body, &second[HEADER_BYTES..second.len() - TAG_BYTES]);

    assert_eq!(receiver.decrypt(&frame).unwrap(), plaintext);
    assert_eq!(receiver.decrypt(&second).unwrap(), plaintext);
}

#[test]
fn malformed_and_wrong_key_frames_are_rejected() {
    let a = author(1);
    let b = author(2);
    let call_id = call(10);
    let secret_a = StaticSecret::random_from_rng(OsRng);
    let secret_b = StaticSecret::random_from_rng(OsRng);
    let public_a = PublicKey::from(&secret_a).to_bytes();
    let public_b = PublicKey::from(&secret_b).to_bytes();

    let sender = MediaCipher::new(
        derive_pairwise(a, b, &secret_a, public_b, call_id, 1).unwrap(),
        1,
    );
    let receiver = MediaCipher::new(
        derive_pairwise(b, a, &secret_b, public_a, call_id, 1).unwrap(),
        1,
    );

    // Tampered tag.
    let mut frame = sender.encrypt(b"tamper").unwrap();
    let last = frame.len() - 1;
    frame[last] ^= 1;
    assert!(matches!(
        receiver.decrypt(&frame),
        Err(CryptoError::Decrypt)
    ));

    // Truncated below the minimum, then by a single byte.
    let frame = sender.encrypt(b"truncate").unwrap();
    assert!(matches!(
        receiver.decrypt(&frame[..HEADER_BYTES + TAG_BYTES - 1]),
        Err(CryptoError::Decrypt)
    ));
    assert!(matches!(
        receiver.decrypt(&frame[..frame.len() - 1]),
        Err(CryptoError::Decrypt)
    ));

    // Wrong epoch.
    let other_epoch = MediaCipher::new(
        derive_pairwise(a, b, &secret_a, public_b, call_id, 2).unwrap(),
        2,
    );
    let frame = sender.encrypt(b"epoch").unwrap();
    assert!(matches!(
        other_epoch.decrypt(&frame),
        Err(CryptoError::WrongEpoch)
    ));

    // Wrong key at the same epoch.
    let wrong_secret = StaticSecret::random_from_rng(OsRng);
    let wrong = MediaCipher::new(
        derive_pairwise(a, b, &wrong_secret, public_b, call_id, 1).unwrap(),
        1,
    );
    let frame = sender.encrypt(b"wrong key").unwrap();
    assert!(matches!(wrong.decrypt(&frame), Err(CryptoError::Decrypt)));
}

#[test]
fn replay_and_out_of_order_counters() {
    let (sender, receiver) = matching_ciphers(4, 1);
    let first = sender.encrypt(b"one").unwrap();
    let second = sender.encrypt(b"two").unwrap();
    let third = sender.encrypt(b"three").unwrap();

    assert_eq!(receiver.decrypt(&third).unwrap(), b"three");
    assert_eq!(receiver.decrypt(&first).unwrap(), b"one");
    assert_eq!(receiver.decrypt(&second).unwrap(), b"two");
    assert!(matches!(
        receiver.decrypt(&second),
        Err(CryptoError::Replay)
    ));
    assert!(matches!(receiver.decrypt(&first), Err(CryptoError::Replay)));

    // A counter that was never seen but has fallen outside the 64-wide window
    // is also rejected.
    let (sender, receiver) = matching_ciphers(5, 1);
    let frames: Vec<Vec<u8>> = (0..70u8)
        .map(|value| sender.encrypt(&[value]).unwrap())
        .collect();
    for (index, frame) in frames.iter().enumerate() {
        if index == 4 {
            continue;
        }
        assert!(receiver.decrypt(frame).is_ok(), "frame {}", index + 1);
    }
    assert!(matches!(
        receiver.decrypt(&frames[4]),
        Err(CryptoError::Replay)
    ));
}

#[test]
fn counter_zero_is_rejected() {
    let (sender, receiver) = matching_ciphers(6, 1);
    let mut frame = sender.encrypt(b"zero").unwrap();
    frame[4..HEADER_BYTES].copy_from_slice(&0u64.to_be_bytes());
    assert!(matches!(
        receiver.decrypt(&frame),
        Err(CryptoError::Decrypt)
    ));
}

#[test]
fn replay_window_width_and_reset() {
    let mut window = ReplayWindow::new();
    assert!(!window.accept(0));
    assert!(window.accept(1));
    assert!(window.accept(3));
    assert!(window.accept(2));
    assert!(!window.accept(2));
    assert!(!window.accept(1));

    let mut window = ReplayWindow::new();
    assert!(window.accept(100));
    assert!(window.accept(37));
    assert!(!window.accept(36));
    assert!(!window.accept(37));
    assert!(window.accept(101));

    window.reset();
    assert!(window.accept(1));
    assert!(!window.accept(1));
}

#[test]
fn plaintext_bound_and_overhead_are_exposed() {
    let (sender, receiver) = matching_ciphers(11, 1);
    let max = vec![0xA5u8; MAX_PLAINTEXT_BYTES];
    let frame = sender.encrypt(&max).unwrap();
    assert_eq!(frame.len(), MAX_CIPHERTEXT_BYTES);
    assert_eq!(frame.len(), MAX_PLAINTEXT_BYTES + WIRE_OVERHEAD_BYTES);
    assert_eq!(receiver.decrypt(&frame).unwrap(), max);

    let too_big = vec![0u8; MAX_PLAINTEXT_BYTES + 1];
    assert!(matches!(
        sender.encrypt(&too_big),
        Err(CryptoError::PlaintextTooLarge)
    ));

    // A wire payload padded past the plaintext bound is rejected before AEAD.
    let mut oversized = frame.clone();
    oversized.push(0);
    assert!(matches!(
        receiver.decrypt(&oversized),
        Err(CryptoError::PlaintextTooLarge)
    ));
}

#[test]
fn install_requires_the_local_epoch() {
    let a = author(1);
    let b = author(2);
    let mut keyring = MediaKeyring::generate(a, call(12));
    let peer_secret = StaticSecret::random_from_rng(OsRng);
    let peer_public = PublicKey::from(&peer_secret).to_bytes();

    assert_eq!(keyring.epoch(), 1);
    assert!(matches!(
        keyring.install_peer(b, 2, peer_public),
        Err(CryptoError::WrongEpoch)
    ));
    assert!(matches!(
        keyring.install_peer(b, 0, peer_public),
        Err(CryptoError::WrongEpoch)
    ));

    keyring.install_peer(b, 1, peer_public).unwrap();
    assert!(keyring.has_peer(b));
    assert_eq!(keyring.peer_epoch(b), Some(1));

    // An author is not its own peer.
    assert!(matches!(
        keyring.install_peer(a, 1, keyring.public_key()),
        Err(CryptoError::InvalidKey)
    ));
}

#[test]
fn keyring_round_trip_rotate_and_clear() {
    let a = author(1);
    let b = author(2);
    let call_id = call(13);
    let mut alice = MediaKeyring::generate(a, call_id);
    let mut bob = MediaKeyring::generate(b, call_id);
    alice
        .install_peer(b, bob.epoch(), bob.public_key())
        .unwrap();
    bob.install_peer(a, alice.epoch(), alice.public_key())
        .unwrap();

    let frame = alice.encrypt_for(b, b"hello bob").unwrap();
    assert_eq!(bob.decrypt_from(a, &frame).unwrap(), b"hello bob");
    let reply = bob.encrypt_for(a, b"hello alice").unwrap();
    assert_eq!(alice.decrypt_from(b, &reply).unwrap(), b"hello alice");

    let unknown = author(9);
    assert!(matches!(
        alice.encrypt_for(unknown, b"x"),
        Err(CryptoError::NoPeerKey)
    ));
    assert!(matches!(
        alice.decrypt_from(unknown, b"x"),
        Err(CryptoError::NoPeerKey)
    ));

    let old_frame = alice.encrypt_for(b, b"before rotate").unwrap();
    let old_peer_public = bob.public_key();

    assert_eq!(alice.rotate(), 2);
    assert_eq!(alice.epoch(), 2);
    assert!(!alice.has_peer(b));
    assert_eq!(alice.peer_epoch(b), None);
    assert!(matches!(
        alice.install_peer(b, 1, old_peer_public),
        Err(CryptoError::WrongEpoch)
    ));

    assert_eq!(bob.rotate(), 2);
    alice.install_peer(b, 2, bob.public_key()).unwrap();
    bob.install_peer(a, 2, alice.public_key()).unwrap();
    let new_frame = alice.encrypt_for(b, b"after rotate").unwrap();
    assert_eq!(bob.decrypt_from(a, &new_frame).unwrap(), b"after rotate");

    // The old epoch frame cannot be opened by the new cipher.
    assert!(matches!(
        bob.decrypt_from(a, &old_frame),
        Err(CryptoError::WrongEpoch)
    ));

    alice.clear();
    assert_eq!(alice.epoch(), 1);
    assert!(!alice.has_peer(b));
    assert_eq!(alice.peer_epoch(b), None);
}

#[test]
fn reinstalling_the_same_peer_key_preserves_the_counter() {
    let a = author(1);
    let b = author(2);
    let call_id = call(14);
    let mut alice = MediaKeyring::generate(a, call_id);
    let peer_secret = StaticSecret::random_from_rng(OsRng);
    let peer_public = PublicKey::from(&peer_secret).to_bytes();

    alice.install_peer(b, 1, peer_public).unwrap();
    let first = alice.encrypt_for(b, b"first").unwrap();
    assert_eq!(&first[4..HEADER_BYTES], &1u64.to_be_bytes());

    // A repeated authenticated frame for the same key must not rebuild the
    // cipher: doing so would reset the send counter and reuse nonce
    // epoch||1 under the same key.
    alice.install_peer(b, 1, peer_public).unwrap();
    let second = alice.encrypt_for(b, b"second").unwrap();
    assert_eq!(&second[4..HEADER_BYTES], &2u64.to_be_bytes());

    let bob = MediaCipher::new(
        derive_pairwise(b, a, &peer_secret, alice.public_key(), call_id, 1).unwrap(),
        1,
    );
    assert_eq!(bob.decrypt(&first).unwrap(), b"first");
    assert_eq!(bob.decrypt(&second).unwrap(), b"second");
}

#[test]
fn a_different_peer_key_at_the_same_epoch_is_refused() {
    let a = author(1);
    let b = author(2);
    let mut alice = MediaKeyring::generate(a, call(14));
    let old_secret = StaticSecret::random_from_rng(OsRng);
    let new_secret = StaticSecret::random_from_rng(OsRng);

    alice
        .install_peer(b, 1, PublicKey::from(&old_secret).to_bytes())
        .unwrap();
    assert!(matches!(
        alice.install_peer(b, 1, PublicKey::from(&new_secret).to_bytes()),
        Err(CryptoError::KeyConflict)
    ));

    // The installed key still encrypts under its original counter state.
    let frame = alice.encrypt_for(b, b"old key").unwrap();
    assert_eq!(&frame[4..HEADER_BYTES], &1u64.to_be_bytes());
}

#[test]
fn a_fresh_keyring_can_align_with_a_long_lived_call_epoch() {
    let a = author(1);
    let b = author(2);
    let call_id = call(17);
    let mut joiner = MediaKeyring::generate(b, call_id);
    let mut existing = MediaKeyring::generate(a, call_id);

    // A call that has rotated many times, as it would after a long series of
    // joins and leaves, sits far beyond a new member's epoch two.
    for _ in 0..100 {
        existing.rotate();
    }
    joiner.rotate();
    assert_eq!(joiner.epoch(), 2);
    assert_eq!(existing.epoch(), 101);

    joiner.rotate_to(existing.epoch()).unwrap();
    assert_eq!(joiner.epoch(), existing.epoch());
    assert!(matches!(
        joiner.rotate_to(existing.epoch()),
        Err(CryptoError::WrongEpoch)
    ));

    joiner
        .install_peer(a, existing.epoch(), existing.public_key())
        .unwrap();
    existing
        .install_peer(b, joiner.epoch(), joiner.public_key())
        .unwrap();
    let frame = existing.encrypt_for(b, b"aligned").unwrap();
    assert_eq!(joiner.decrypt_from(a, &frame).unwrap(), b"aligned");
}

#[test]
fn the_largest_wire_payload_fits_a_media_packet() {
    assert_eq!(MAX_CIPHERTEXT_BYTES, ovvc_media::MAX_MEDIA_PAYLOAD);
    assert_eq!(
        MAX_PLAINTEXT_BYTES + WIRE_OVERHEAD_BYTES,
        ovvc_media::MAX_MEDIA_PAYLOAD
    );
    let (sender, _) = matching_ciphers(18, 1);
    let frame = sender.encrypt(&vec![0u8; MAX_PLAINTEXT_BYTES]).unwrap();
    ovvc_media::MediaPacket::new(
        ovvc_media::MediaKind::Audio,
        call(18),
        author(1),
        1,
        0,
        frame,
    )
    .expect("the largest sealed frame must fit the media payload cap");
}

#[test]
fn remove_peer_forgets_the_cipher() {
    let a = author(1);
    let b = author(2);
    let mut keyring = MediaKeyring::generate(a, call(16));
    let peer_secret = StaticSecret::random_from_rng(OsRng);
    keyring
        .install_peer(b, 1, PublicKey::from(&peer_secret).to_bytes())
        .unwrap();
    assert!(keyring.has_peer(b));

    keyring.remove_peer(b);
    assert!(!keyring.has_peer(b));
    assert_eq!(keyring.peer_epoch(b), None);
    assert!(matches!(
        keyring.encrypt_for(b, b"x"),
        Err(CryptoError::NoPeerKey)
    ));
}
