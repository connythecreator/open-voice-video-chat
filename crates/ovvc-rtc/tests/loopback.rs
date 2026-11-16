//! In-process two-peer transport tests.
//!
//! These tests do not touch a sound card or a network beyond loopback. They
//! stand in for the daemon's signalling router: every [`RtcEvent::Signal`]
//! emitted by one manager is handed to the other, and the received
//! [`RtcEvent::Media`] is compared with the payload that was written.

use std::time::Duration;

use ovvc_core::AuthorId;
use ovvc_media::call::CallId;
use ovvc_media::packet::{MediaKind, MediaPacket};
use ovvc_rtc::{RtcConfig, RtcEvent, RtcManager, RtcSignal};
use tokio::sync::mpsc::{self, Receiver};

fn author(byte: u8) -> AuthorId {
    AuthorId([byte; 32])
}

/// Configuration for two peers on one machine: loopback candidates are needed
/// for the ICE agent to find each other, and they are opt-in in production.
fn test_config() -> RtcConfig {
    RtcConfig {
        allow_loopback_candidates: true,
        ..RtcConfig::default()
    }
}

/// Two managers and the event loop that connects them.
struct Harness {
    a: RtcManager,
    b: RtcManager,
    a_id: AuthorId,
    b_id: AuthorId,
    a_rx: Receiver<RtcEvent>,
    b_rx: Receiver<RtcEvent>,
    media: Vec<(AuthorId, MediaPacket)>,
    errors: Vec<String>,
}

impl Harness {
    fn new(a_id: AuthorId, b_id: AuthorId, call_id: CallId) -> Self {
        let (a_tx, a_rx) = mpsc::channel(64);
        let (b_tx, b_rx) = mpsc::channel(64);
        let a = RtcManager::new(a_id, call_id, test_config(), a_tx).expect("manager a");
        let b = RtcManager::new(b_id, call_id, test_config(), b_tx).expect("manager b");
        Self {
            a,
            b,
            a_id,
            b_id,
            a_rx,
            b_rx,
            media: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// Install each manager's current public key in the other, as the daemon
    /// does after verifying the signed control frame that carried it.
    fn install_matching_keys(&self) {
        let a_keys = self.a.keyring();
        let b_keys = self.b.keyring();
        let mut a = a_keys.write().expect("a keyring");
        let mut b = b_keys.write().expect("b keyring");
        let a_public = a.public_key();
        let a_epoch = a.epoch();
        let b_public = b.public_key();
        let b_epoch = b.epoch();
        a.install_peer(self.b_id, b_epoch, b_public)
            .expect("install b in a");
        b.install_peer(self.a_id, a_epoch, a_public)
            .expect("install a in b");
    }

    /// Deliver one event to the manager it names, if this harness owns it.
    async fn route(&mut self, source: AuthorId, event: RtcEvent) {
        match event {
            RtcEvent::Signal { to, signal } => {
                let result = if source != self.a_id && to == self.a_id {
                    self.a.on_signal(source, signal).await
                } else if source != self.b_id && to == self.b_id {
                    self.b.on_signal(source, signal).await
                } else {
                    // Addressed to a peer this harness does not model (used by
                    // the failure-isolation test).
                    Ok(())
                };
                if let Err(err) = result {
                    self.errors.push(err.to_string());
                }
            }
            RtcEvent::Media { from, packet } => self.media.push((from, packet)),
            RtcEvent::Error(err) => self.errors.push(err),
            RtcEvent::Stats(_) => {}
        }
    }

    /// Drain both event channels, sleeping only when there was nothing to do.
    async fn step(&mut self, idle: Duration) {
        let mut handled = false;
        while let Ok(event) = self.a_rx.try_recv() {
            self.route(self.a_id, event).await;
            handled = true;
        }
        while let Ok(event) = self.b_rx.try_recv() {
            self.route(self.b_id, event).await;
            handled = true;
        }
        if !handled {
            tokio::time::sleep(idle).await;
        }
    }

    async fn connected(&self) -> bool {
        let a = self.a.stats().await;
        let b = self.b.stats().await;
        a.peers.iter().any(|peer| peer.state == "connected")
            && b.peers.iter().any(|peer| peer.state == "connected")
    }

    async fn wait_connected(&mut self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.connected().await {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "peers did not connect; errors: {:?}; a: {:?}; b: {:?}",
                    self.errors,
                    self.a.stats().await,
                    self.b.stats().await
                );
            }
            self.step(Duration::from_millis(20)).await;
        }
    }

    async fn wait_media(&mut self, timeout: Duration) -> (AuthorId, MediaPacket) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(index) = self.media.iter().position(|(from, _)| *from == self.a_id) {
                return self.media.remove(index);
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("no media arrived; errors: {:?}", self.errors);
            }
            self.step(Duration::from_millis(20)).await;
        }
    }

    async fn close(&mut self) {
        self.a.close().await;
        self.b.close().await;
    }
}

#[tokio::test]
async fn two_managers_carry_an_opus_payload_over_rtp() {
    let a_id = author(1);
    let b_id = author(2);
    let call_id = CallId([9; 16]);
    let mut harness = Harness::new(a_id, b_id, call_id);
    harness.install_matching_keys();

    harness.a.ensure_peer(b_id).await.expect("ensure peer");
    harness.wait_connected(Duration::from_secs(30)).await;

    // A 64-byte payload stands in for one encoded Opus frame. The transport
    // never decodes it, so the bytes must survive decryption untouched.
    let payload: Vec<u8> = (0..64u8).map(|byte| byte ^ 0x5a).collect();
    let packet = MediaPacket::new(MediaKind::Audio, call_id, a_id, 42, 1_234, payload.clone())
        .expect("packet");
    for _ in 0..12 {
        harness.a.send_audio(&packet).await.expect("send audio");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let (from, received) = harness.wait_media(Duration::from_secs(15)).await;
    assert_eq!(from, a_id);
    assert_eq!(received.author, a_id);
    assert_eq!(received.call_id, call_id);
    assert_eq!(received.kind, MediaKind::Audio);
    assert_eq!(received.payload, payload);
    assert!(
        harness.errors.is_empty(),
        "unexpected errors: {:?}",
        harness.errors
    );

    // The RTP payload B read before decryption is the sealed frame, not the
    // Opus plaintext, and it carries the wire header and tag.
    let wire = harness
        .b
        .last_wire_payload(a_id)
        .expect("a received RTP payload was recorded");
    assert_ne!(wire, payload, "the RTP payload must not be plaintext");
    assert_eq!(
        wire.len(),
        payload.len() + ovvc_crypto::WIRE_OVERHEAD_BYTES,
        "the wire payload carries the crypto header and tag"
    );

    // B can attribute the stream and reports E2EE for the peer.
    let stats = harness.b.stats().await;
    let peer = stats
        .peers
        .iter()
        .find(|peer| peer.author == a_id)
        .expect("peer stats");
    assert!(peer.e2ee, "the peer has a media key installed");
    assert_eq!(peer.key_epoch, Some(1));

    harness.close().await;
}

#[tokio::test]
async fn the_lower_author_offers_and_the_higher_waits() {
    let low = author(1);
    let high = author(2);
    let call_id = CallId([3; 16]);

    let (tx, mut rx) = mpsc::channel(64);
    let mut lower = RtcManager::new(low, call_id, test_config(), tx).expect("manager");
    lower.ensure_peer(high).await.expect("ensure peer");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut offer = None;
    while offer.is_none() {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("an offer is emitted")
            .expect("event channel");
        match event {
            RtcEvent::Signal {
                to,
                signal: RtcSignal::Offer { sdp },
            } => offer = Some((to, sdp)),
            RtcEvent::Error(err) => panic!("unexpected error: {err}"),
            _ => {}
        }
    }
    let (to, sdp) = offer.expect("offer");
    assert_eq!(to, high, "the offer must be addressed to the higher author");
    assert!(!sdp.is_empty());
    lower.close().await;

    let (tx, mut rx) = mpsc::channel(64);
    let mut higher = RtcManager::new(high, call_id, test_config(), tx).expect("manager");
    higher.ensure_peer(low).await.expect("ensure peer");

    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    let mut unexpected_offer = false;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(RtcEvent::Signal {
                signal: RtcSignal::Offer { .. },
                ..
            })) => unexpected_offer = true,
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    assert!(
        !unexpected_offer,
        "the higher author must wait for the lower author's offer"
    );
    higher.close().await;
}

#[tokio::test]
async fn a_failed_peer_does_not_break_a_connected_one() {
    let a_id = author(1);
    let b_id = author(2);
    let c_id = author(3);
    let call_id = CallId([4; 16]);
    let mut harness = Harness::new(a_id, b_id, call_id);
    harness.install_matching_keys();

    harness.a.ensure_peer(b_id).await.expect("ensure b");
    harness.wait_connected(Duration::from_secs(30)).await;

    // A second peer that never gets a usable answer. The offer to C is
    // intentionally not delivered by the harness; the corrupt answer is fed
    // straight in so the failure path is deterministic.
    harness.a.ensure_peer(c_id).await.expect("ensure c");
    harness
        .a
        .on_signal(
            c_id,
            RtcSignal::Answer {
                sdp: "this is not an sdp".to_owned(),
            },
        )
        .await
        .expect("a peer-local failure is handled, not returned");

    let stats = harness.a.stats().await;
    assert_eq!(
        stats.peers.len(),
        1,
        "the failed peer must be removed: {stats:?}"
    );
    assert_eq!(stats.peers[0].author, b_id);
    assert_eq!(stats.peers[0].state, "connected");

    harness.step(Duration::from_millis(10)).await;
    assert!(
        harness.errors.iter().any(|err| err.contains("answer")),
        "expected an answer-related error, got {:?}",
        harness.errors
    );

    // Removing the peer that is already gone is a no-op.
    harness.a.remove_peer(c_id).await;

    // The surviving peer still carries media.
    let payload = vec![0x78u8, 1, 2, 3, 4, 5, 6, 7];
    let packet =
        MediaPacket::new(MediaKind::Audio, call_id, a_id, 7, 0, payload.clone()).expect("packet");
    for _ in 0..12 {
        harness
            .a
            .send_audio(&packet)
            .await
            .expect("send to survivor");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (from, received) = harness.wait_media(Duration::from_secs(15)).await;
    assert_eq!(from, a_id);
    assert_eq!(received.payload, payload);

    harness.close().await;
}

#[tokio::test]
async fn a_peer_without_a_key_receives_no_media_and_the_sender_reports_once() {
    let a_id = author(1);
    let b_id = author(2);
    let call_id = CallId([21; 16]);
    let mut harness = Harness::new(a_id, b_id, call_id);

    harness.a.ensure_peer(b_id).await.expect("ensure peer");
    harness.wait_connected(Duration::from_secs(30)).await;

    let payload = vec![1u8, 2, 3, 4, 5];
    let packet = MediaPacket::new(MediaKind::Audio, call_id, a_id, 1, 0, payload).expect("packet");
    for _ in 0..6 {
        harness.a.send_audio(&packet).await.expect("send audio");
    }
    for _ in 0..10 {
        harness.step(Duration::from_millis(20)).await;
    }

    assert!(
        harness.media.is_empty(),
        "a peer without a key must receive no media: {:?}",
        harness.media
    );
    let reports = harness
        .errors
        .iter()
        .filter(|error| error.contains("media key"))
        .count();
    assert_eq!(
        reports, 1,
        "the missing key is reported once per peer per epoch: {:?}",
        harness.errors
    );

    let stats = harness.a.stats().await;
    let peer = stats
        .peers
        .iter()
        .find(|peer| peer.author == b_id)
        .expect("peer stats");
    assert!(!peer.e2ee);
    assert_eq!(peer.key_epoch, None);

    harness.close().await;
}

#[tokio::test]
async fn tampered_and_replayed_rtp_payloads_are_dropped_and_counted() {
    let a_id = author(1);
    let b_id = author(2);
    let call_id = CallId([22; 16]);
    let mut harness = Harness::new(a_id, b_id, call_id);
    harness.install_matching_keys();

    harness.a.ensure_peer(b_id).await.expect("ensure peer");
    harness.wait_connected(Duration::from_secs(30)).await;

    // Two authentic frames from A's send direction for B. Counters one and
    // two are both fresh to B.
    let (first, tamper_source) = {
        let keys = harness.a.keyring();
        let keys = keys.read().expect("a keyring");
        (
            keys.encrypt_for(b_id, b"authentic").expect("encrypt"),
            keys.encrypt_for(b_id, b"tamper me").expect("encrypt"),
        )
    };

    // The first delivery is accepted; the identical wire payload replayed is
    // a replay and is not emitted again.
    harness
        .b
        .ingest_rtp_payload(a_id, 100, 1_000, &first)
        .expect("ingest");
    harness
        .b
        .ingest_rtp_payload(a_id, 100, 1_000, &first)
        .expect("ingest replay");
    harness.step(Duration::from_millis(10)).await;
    assert_eq!(
        harness.media.len(),
        1,
        "a replay must not be emitted: {:?}",
        harness.media
    );
    assert_eq!(harness.media[0].1.payload, b"authentic");

    // A fresh counter with a flipped tag byte fails authentication.
    let mut tampered = tamper_source;
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    harness
        .b
        .ingest_rtp_payload(a_id, 101, 1_020, &tampered)
        .expect("ingest tampered");
    harness.step(Duration::from_millis(10)).await;
    assert_eq!(
        harness.media.len(),
        1,
        "a tampered frame must not be emitted: {:?}",
        harness.media
    );

    let stats = harness.b.stats().await;
    assert_eq!(
        stats.decrypt_errors, 2,
        "the replay and the tamper are both counted: {stats:?}"
    );
    let peer = stats
        .peers
        .iter()
        .find(|peer| peer.author == a_id)
        .expect("peer stats");
    assert_eq!(peer.decrypt_errors, 2);
    assert!(peer.e2ee);
    assert_eq!(peer.key_epoch, Some(1));

    harness.close().await;
}

#[tokio::test]
async fn rotation_drops_old_epoch_media_and_carries_new_epoch_media() {
    let a_id = author(1);
    let b_id = author(2);
    let call_id = CallId([23; 16]);
    let mut harness = Harness::new(a_id, b_id, call_id);
    harness.install_matching_keys();

    harness.a.ensure_peer(b_id).await.expect("ensure peer");
    harness.wait_connected(Duration::from_secs(30)).await;

    // A frame sealed at epoch one, kept for the check after rotation.
    let old_frame = {
        let keys = harness.a.keyring();
        keys.read()
            .expect("a keyring")
            .encrypt_for(b_id, b"epoch one")
            .expect("encrypt")
    };

    let a_epoch = harness.a.rotate_keyring().expect("rotate a");
    let b_epoch = harness.b.rotate_keyring().expect("rotate b");
    assert_eq!((a_epoch, b_epoch), (2, 2));
    // Peers must publish and install the new epoch's public keys.
    harness.install_matching_keys();

    // The old-epoch frame cannot be opened at the new epoch; it is a counted
    // drop, not a manager failure.
    harness
        .b
        .ingest_rtp_payload(a_id, 200, 2_000, &old_frame)
        .expect("ingest old");
    harness.step(Duration::from_millis(10)).await;
    assert!(
        harness.media.is_empty(),
        "old-epoch media must be dropped: {:?}",
        harness.media
    );

    // New-epoch media flows after both sides install the new keys.
    let payload = b"epoch two".to_vec();
    let packet = MediaPacket::new(MediaKind::Audio, call_id, a_id, 201, 2_020, payload.clone())
        .expect("packet");
    for _ in 0..12 {
        harness.a.send_audio(&packet).await.expect("send audio");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (from, received) = harness.wait_media(Duration::from_secs(15)).await;
    assert_eq!(from, a_id);
    assert_eq!(received.payload, payload);

    let stats = harness.b.stats().await;
    assert_eq!(
        stats.decrypt_errors, 1,
        "the wrong-epoch frame is counted: {stats:?}"
    );
    let peer = stats
        .peers
        .iter()
        .find(|peer| peer.author == a_id)
        .expect("peer stats");
    assert_eq!(peer.key_epoch, Some(2));
    assert!(peer.e2ee);

    harness.close().await;
}

#[tokio::test]
async fn close_is_idempotent() {
    let local = author(1);
    let remote = author(2);
    let call_id = CallId([5; 16]);
    let (tx, _rx) = mpsc::channel(64);
    let mut manager = RtcManager::new(local, call_id, test_config(), tx).expect("manager");

    manager.ensure_peer(remote).await.expect("ensure peer");
    manager.close().await;
    manager.close().await;
    assert!(manager.stats().await.peers.is_empty());

    manager.remove_peer(remote).await;
}

#[tokio::test]
async fn a_turn_server_with_credentials_builds_a_peer() {
    let (tx, _rx) = mpsc::channel(64);
    let config = RtcConfig {
        ice_servers: vec!["turn:turn.example.org:3478?transport=udp".to_owned()],
        turn_username: "alice".to_owned(),
        turn_credential: "secret".to_owned(),
        ..test_config()
    };
    let mut manager = RtcManager::new(author(2), CallId([11; 16]), config, tx).expect("manager");
    // The higher author answers, so this only builds the PeerConnection. The
    // old code failed here with "turn server credentials required".
    manager
        .ensure_peer(author(1))
        .await
        .expect("a credentialed TURN server must build a peer");
    manager.close().await;
}

#[tokio::test]
async fn a_lost_offer_is_reoffered_by_the_reconcile_tick() {
    let (tx, mut rx) = mpsc::channel(64);
    let config = RtcConfig {
        negotiation_timeout_ms: 100,
        ..test_config()
    };
    let mut manager = RtcManager::new(author(1), CallId([12; 16]), config, tx).expect("manager");
    manager.ensure_peer(author(2)).await.expect("ensure peer");

    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the first offer")
        .expect("event channel");
    assert!(matches!(
        first,
        RtcEvent::Signal {
            signal: RtcSignal::Offer { .. },
            ..
        }
    ));

    // The first offer is deliberately not delivered. Reconcile must time the
    // peer out and emit a second offer instead of returning Ok forever.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut reoffered = false;
    while !reoffered && tokio::time::Instant::now() < deadline {
        manager.ensure_peer(author(2)).await.expect("ensure peer");
        while let Ok(event) = rx.try_recv() {
            if matches!(
                event,
                RtcEvent::Signal {
                    signal: RtcSignal::Offer { .. },
                    ..
                }
            ) {
                reoffered = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(reoffered, "a lost offer was never re-sent");
    manager.close().await;
}

#[tokio::test]
async fn a_loopback_candidate_is_refused_unless_enabled() {
    let (tx, mut rx) = mpsc::channel(64);
    let mut manager =
        RtcManager::new(author(2), CallId([13; 16]), RtcConfig::default(), tx).expect("manager");

    manager
        .on_signal(
            author(1),
            RtcSignal::Candidate {
                candidate: "candidate:1 1 udp 1 127.0.0.1 9000 typ host".to_owned(),
                mid: None,
            },
        )
        .await
        .expect("the signal itself is well formed");

    let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("a rejection event")
        .expect("event channel");
    match event {
        RtcEvent::Error(message) => assert!(
            message.contains("not permitted"),
            "unexpected rejection: {message}"
        ),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn a_private_candidate_is_refused_when_lan_candidates_are_off() {
    let (tx, mut rx) = mpsc::channel(64);
    let config = RtcConfig {
        allow_lan_candidates: false,
        ..RtcConfig::default()
    };
    let mut manager = RtcManager::new(author(2), CallId([14; 16]), config, tx).expect("manager");

    manager
        .on_signal(
            author(1),
            RtcSignal::Candidate {
                candidate: "candidate:1 1 udp 1 10.0.0.10 9000 typ host".to_owned(),
                mid: None,
            },
        )
        .await
        .expect("the signal itself is well formed");

    let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("a rejection event")
        .expect("event channel");
    match event {
        RtcEvent::Error(message) => assert!(
            message.contains("not permitted"),
            "unexpected rejection: {message}"
        ),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn a_candidate_embedded_in_remote_sdp_is_refused() {
    let (tx, mut rx) = mpsc::channel(64);
    let mut manager =
        RtcManager::new(author(2), CallId([15; 16]), RtcConfig::default(), tx).expect("manager");

    let result = manager
        .on_signal(
            author(1),
            RtcSignal::Offer {
                sdp: "v=0\r\na=candidate:1 1 udp 1 127.0.0.1 9000 typ host\r\n".to_owned(),
            },
        )
        .await;
    assert!(result.is_err(), "the description must be refused");

    let event = rx.try_recv().expect("a rejection event");
    match event {
        RtcEvent::Error(message) => assert!(
            message.contains("not permitted"),
            "unexpected rejection: {message}"
        ),
        other => panic!("expected a rejection, got {other:?}"),
    }
}
