use std::time::Duration;

use ovvc_core::{Identity, shared_storage};
use ovvc_net::node::{NodeConfig, NodeEvent, start};
use ovvc_store::Store;
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dht_lookup_finds_announced_peer() {
    let alice = Identity::from_seed([21u8; 32]);
    let bob = Identity::from_seed([22u8; 32]);
    let alice_dir = tempdir().unwrap();
    let bob_dir = tempdir().unwrap();

    let alice_store = shared_storage(Store::open(alice_dir.path()).unwrap());
    let bob_store = shared_storage(Store::open(bob_dir.path()).unwrap());
    let created = alice_store
        .lock()
        .unwrap()
        .create_guild("dht", &alice)
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

    // Alice publishes its announcement when a peer connects.
    timeout(Duration::from_secs(10), async {
        loop {
            match alice_rx.recv().await {
                Some(NodeEvent::PeerConnected(peer)) if peer == bob_node.peer_id => break,
                Some(NodeEvent::Error(error)) => eprintln!("alice network: {error}"),
                Some(_) => {}
                None => panic!("alice node stopped"),
            }
        }
    })
    .await
    .expect("alice never saw bob connect");
    // Alice announces once Hello proved the guild, and the announcement runs on
    // the maintenance tick rather than inside the request handler. Poll the
    // lookup instead of assuming a fixed publish delay: the record can take up
    // to a tick to reach the store the query is answered from.
    let mut last_reason = String::new();
    let mut found = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        bob_node.lookup(alice.author_id());
        let wait = timeout(Duration::from_secs(5), async {
            loop {
                match bob_rx.recv().await {
                    Some(NodeEvent::PeerFound { author, addresses }) => {
                        assert_eq!(author, alice.author_id());
                        return Ok::<_, String>(addresses);
                    }
                    Some(NodeEvent::LookupFailed { reason, .. }) => return Err(reason),
                    Some(NodeEvent::Error(error)) => eprintln!("bob network: {error}"),
                    Some(_) => {}
                    None => panic!("bob node stopped"),
                }
            }
        })
        .await;
        match wait {
            Ok(Ok(addresses)) => {
                found = Some(addresses);
                break;
            }
            Ok(Err(reason)) => last_reason = reason,
            Err(_) => last_reason = "lookup timed out".to_string(),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let addresses =
        found.unwrap_or_else(|| panic!("lookup for {} failed: {last_reason}", alice.author_id()));

    assert!(!addresses.is_empty(), "announcement carried no addresses");
    // Announced addresses must not be wildcard bind addresses.
    for address in &addresses {
        let text = address.to_string();
        assert!(!text.starts_with("/ip4/0.0.0.0/"), "{text}");
        assert!(!text.starts_with("/ip6/::/"), "{text}");
    }

    alice_node.shutdown();
    bob_node.shutdown();
}
