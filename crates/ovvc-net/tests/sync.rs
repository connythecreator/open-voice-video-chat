use std::time::Duration;

use ovvc_core::{EventKind, Identity, shared_storage};
use ovvc_net::node::{NodeConfig, NodeEvent, start};
use ovvc_store::Store;
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_nodes_sync_signed_messages() {
    let alice = Identity::from_seed([11u8; 32]);
    let bob = Identity::from_seed([12u8; 32]);
    let alice_dir = tempdir().unwrap();
    let bob_dir = tempdir().unwrap();

    let alice_store = shared_storage(Store::open(alice_dir.path()).unwrap());
    let bob_store = shared_storage(Store::open(bob_dir.path()).unwrap());

    let created = alice_store
        .lock()
        .unwrap()
        .create_guild("networked", &alice)
        .unwrap();
    let guild_id = created.guild.id;
    let channel_id = created.general.id;
    alice_store
        .lock()
        .unwrap()
        .post_message(guild_id, channel_id, "hello from alice", &alice)
        .unwrap();

    let (alice_tx, mut alice_rx) = mpsc::unbounded_channel();
    let alice_node = start(
        NodeConfig {
            identity_seed: alice.seed(),
            guild_id,
            store: alice_store.clone(),
            listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
            dial: Vec::new(),
            relay: None,
            enable_upnp: false,
            enable_natpmp: false,
        },
        alice_tx,
    )
    .await
    .unwrap();

    let listen_addr = timeout(Duration::from_secs(5), async {
        loop {
            match alice_rx.recv().await {
                Some(NodeEvent::Listening(address)) => return address,
                Some(NodeEvent::Error(error)) => panic!("alice network error: {error}"),
                Some(_) => {}
                None => panic!("alice node stopped"),
            }
        }
    })
    .await
    .expect("alice listener timeout");
    let dial_addr = listen_addr.with_p2p(alice_node.peer_id).unwrap();

    let (bob_tx, mut bob_rx) = mpsc::unbounded_channel();
    let bob_node = start(
        NodeConfig {
            identity_seed: bob.seed(),
            guild_id,
            store: bob_store.clone(),
            listen: Vec::new(),
            dial: vec![dial_addr],
            relay: None,
            enable_upnp: false,
            enable_natpmp: false,
        },
        bob_tx,
    )
    .await
    .unwrap();

    timeout(Duration::from_secs(10), async {
        loop {
            if bob_store
                .lock()
                .unwrap()
                .get_guild(guild_id)
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("bob did not sync guild");

    // The guild and its message can arrive in separate sync rounds; wait for
    // the message too, otherwise this assertion can run in between the two.
    timeout(Duration::from_secs(10), async {
        loop {
            let arrived = bob_store
                .lock()
                .unwrap()
                .history(guild_id, channel_id, 50)
                .map(|events| {
                    events
                        .iter()
                        .any(|event| matches!(event.body.kind, EventKind::Message { .. }))
                })
                .unwrap_or(false);
            if arrived {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("bob synced the guild but not its message");

    let bob_messages: Vec<_> = bob_store
        .lock()
        .unwrap()
        .history(guild_id, channel_id, 50)
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event.body.kind, EventKind::Message { .. }))
        .collect();
    assert_eq!(bob_messages.len(), 1);
    assert_eq!(
        bob_messages[0].body.kind,
        EventKind::Message {
            body: "hello from alice".to_string()
        }
    );
    assert!(bob_messages[0].verify().is_ok());

    let bob_event = bob_store
        .lock()
        .unwrap()
        .post_message(guild_id, channel_id, "hello from bob", &bob)
        .unwrap();
    bob_node.broadcast(bob_event);

    timeout(Duration::from_secs(10), async {
        loop {
            let messages: Vec<_> = alice_store
                .lock()
                .unwrap()
                .history(guild_id, channel_id, 50)
                .unwrap()
                .into_iter()
                .filter(|event| matches!(event.body.kind, EventKind::Message { .. }))
                .collect();
            if messages.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("alice did not receive bob's message");

    let alice_messages: Vec<_> = alice_store
        .lock()
        .unwrap()
        .history(guild_id, channel_id, 50)
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event.body.kind, EventKind::Message { .. }))
        .collect();
    assert_eq!(alice_messages.len(), 2);
    assert_eq!(alice_messages[1].body.author, bob.author_id());
    assert!(alice_messages[1].verify().is_ok());

    // Drain a little so background tasks can process connection events.
    let _ = timeout(Duration::from_millis(100), bob_rx.recv()).await;

    alice_node.shutdown();
    bob_node.shutdown();
}

/// A blob larger than one chunk exercises the sequential request loop, not
/// just a single response. The fetched bytes must match the hash, be cached
/// on the receiver, and a hash nobody serves must fail instead of hanging.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_nodes_transfer_a_blob_in_chunks() {
    let alice = Identity::from_seed([21u8; 32]);
    let bob = Identity::from_seed([22u8; 32]);
    let alice_dir = tempdir().unwrap();
    let bob_dir = tempdir().unwrap();
    let alice_store = shared_storage(Store::open(alice_dir.path()).unwrap());
    let bob_store = shared_storage(Store::open(bob_dir.path()).unwrap());
    let created = alice_store
        .lock()
        .unwrap()
        .create_guild("blobs", &alice)
        .unwrap();
    let guild_id = created.guild.id;

    let bytes: Vec<u8> = (0..700 * 1024).map(|index| (index % 251) as u8).collect();
    let id = ovvc_core::hash_bytes(&bytes);
    alice_store.lock().unwrap().put_blob(id, &bytes).unwrap();

    let (alice_tx, mut alice_rx) = mpsc::unbounded_channel();
    let alice_node = start(
        NodeConfig {
            identity_seed: alice.seed(),
            guild_id,
            store: alice_store.clone(),
            listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
            dial: Vec::new(),
            relay: None,
            enable_upnp: false,
            enable_natpmp: false,
        },
        alice_tx,
    )
    .await
    .unwrap();
    let listen_addr = timeout(Duration::from_secs(5), async {
        loop {
            match alice_rx.recv().await {
                Some(NodeEvent::Listening(address)) => return address,
                Some(NodeEvent::Error(error)) => panic!("alice network error: {error}"),
                Some(_) => {}
                None => panic!("alice node stopped"),
            }
        }
    })
    .await
    .expect("alice listener timeout");
    let dial_addr = listen_addr.with_p2p(alice_node.peer_id).unwrap();

    let (bob_tx, _bob_rx) = mpsc::unbounded_channel();
    let bob_node = start(
        NodeConfig {
            identity_seed: bob.seed(),
            guild_id,
            store: bob_store.clone(),
            listen: Vec::new(),
            dial: vec![dial_addr],
            relay: None,
            enable_upnp: false,
            enable_natpmp: false,
        },
        bob_tx,
    )
    .await
    .unwrap();

    // Sync proves the Hello exchange completed in both directions, which is
    // what authorises bob to ask alice for a blob.
    timeout(Duration::from_secs(10), async {
        loop {
            if bob_store
                .lock()
                .unwrap()
                .get_guild(guild_id)
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("bob did not sync guild");

    let fetched = timeout(
        Duration::from_secs(30),
        bob_node.fetch_blob(id).expect("fetch command accepted"),
    )
    .await
    .expect("blob fetch timeout")
    .expect("fetch channel closed")
    .expect("blob fetch failed");
    assert_eq!(fetched, bytes);
    assert!(
        bob_store.lock().unwrap().has_blob(id).unwrap(),
        "the fetched blob must be cached"
    );

    let missing = ovvc_core::hash_bytes(b"alice does not have this");
    let outcome = timeout(
        Duration::from_secs(10),
        bob_node
            .fetch_blob(missing)
            .expect("fetch command accepted"),
    )
    .await
    .expect("missing blob fetch timeout")
    .expect("fetch channel closed");
    assert!(outcome.is_err(), "a blob nobody has must not succeed");
}

/// A guild can hold peers that never downloaded an attachment, so a fetch must
/// move on to the next peer instead of failing when the first one answers
/// "blob not found". Candidates are tried in peer-id order, which lets the test
/// place the blob on the peer that is asked second and guarantee the fallback
/// path runs rather than leaving it to `HashMap` iteration order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_fetch_falls_back_to_a_peer_that_has_it() {
    let alice = Identity::from_seed([31u8; 32]);
    let bob = Identity::from_seed([32u8; 32]);
    let dave = Identity::from_seed([33u8; 32]);
    let alice_dir = tempdir().unwrap();
    let bob_dir = tempdir().unwrap();
    let dave_dir = tempdir().unwrap();
    let alice_store = shared_storage(Store::open(alice_dir.path()).unwrap());
    let bob_store = shared_storage(Store::open(bob_dir.path()).unwrap());
    let dave_store = shared_storage(Store::open(dave_dir.path()).unwrap());
    let created = alice_store
        .lock()
        .unwrap()
        .create_guild("fallback", &alice)
        .unwrap();
    let guild_id = created.guild.id;

    let (alice_tx, mut alice_rx) = mpsc::unbounded_channel();
    let alice_node = start(
        NodeConfig {
            identity_seed: alice.seed(),
            guild_id,
            store: alice_store.clone(),
            listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
            dial: Vec::new(),
            relay: None,
            enable_upnp: false,
            enable_natpmp: false,
        },
        alice_tx,
    )
    .await
    .unwrap();
    let listen_addr = timeout(Duration::from_secs(5), async {
        loop {
            match alice_rx.recv().await {
                Some(NodeEvent::Listening(address)) => return address,
                Some(NodeEvent::Error(error)) => panic!("alice network error: {error}"),
                Some(_) => {}
                None => panic!("alice node stopped"),
            }
        }
    })
    .await
    .expect("alice listener timeout");
    let dial_addr = listen_addr.with_p2p(alice_node.peer_id).unwrap();

    let (bob_tx, _bob_rx) = mpsc::unbounded_channel();
    let bob_node = start(
        NodeConfig {
            identity_seed: bob.seed(),
            guild_id,
            store: bob_store.clone(),
            listen: Vec::new(),
            dial: vec![dial_addr.clone()],
            relay: None,
            enable_upnp: false,
            enable_natpmp: false,
        },
        bob_tx,
    )
    .await
    .unwrap();

    let (dave_tx, _dave_rx) = mpsc::unbounded_channel();
    let dave_node = start(
        NodeConfig {
            identity_seed: dave.seed(),
            guild_id,
            store: dave_store.clone(),
            listen: Vec::new(),
            dial: vec![dial_addr],
            relay: None,
            enable_upnp: false,
            enable_natpmp: false,
        },
        dave_tx,
    )
    .await
    .unwrap();

    // Both stores holding the guild proves both Hellos completed, which is what
    // puts bob and dave into alice's peer list.
    for store in [&bob_store, &dave_store] {
        timeout(Duration::from_secs(10), async {
            loop {
                if store.lock().unwrap().get_guild(guild_id).unwrap().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("peer did not sync guild");
    }

    let (first, second) = if bob_node.peer_id < dave_node.peer_id {
        (&bob_store, &dave_store)
    } else {
        (&dave_store, &bob_store)
    };
    assert!(
        second
            .lock()
            .unwrap()
            .get_guild(guild_id)
            .unwrap()
            .is_some(),
        "the peer that should hold the blob is in the guild"
    );
    let bytes: Vec<u8> = (0..4096).map(|index| (index % 239) as u8).collect();
    let id = ovvc_core::hash_bytes(&bytes);
    second.lock().unwrap().put_blob(id, &bytes).unwrap();
    assert!(
        first.lock().unwrap().get_blob(id).unwrap().is_none(),
        "the first peer asked must not have the blob, or the fallback is not tested"
    );

    let fetched = timeout(
        Duration::from_secs(30),
        alice_node.fetch_blob(id).expect("fetch command accepted"),
    )
    .await
    .expect("blob fetch timeout")
    .expect("fetch channel closed")
    .expect("the fetch should fall back to the peer that has the blob");
    assert_eq!(fetched, bytes);
    assert!(
        alice_store.lock().unwrap().has_blob(id).unwrap(),
        "the fetched blob must be cached on the requester"
    );
}
