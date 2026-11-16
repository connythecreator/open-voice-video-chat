use std::time::Duration;

use ovvc_core::{Identity, shared_storage};
use ovvc_net::Multiaddr;
use ovvc_net::PeerId;
use ovvc_net::node::{NodeConfig, NodeEvent, NodeHandle, start};
use ovvc_store::Store;
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio::time::timeout;

async fn next_listening(rx: &mut mpsc::UnboundedReceiver<NodeEvent>, who: &str) -> Multiaddr {
    timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Some(NodeEvent::Listening(address)) => return address,
                Some(NodeEvent::Error(error)) => eprintln!("{who} network: {error}"),
                Some(_) => {}
                None => panic!("{who} node stopped"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{who} listener timeout"))
}

async fn wait_connected(node: &NodeHandle, peer: PeerId, who: &str) {
    timeout(Duration::from_secs(15), async {
        loop {
            if node.status().connected.contains(&peer) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{who} never connected"))
}

async fn wait_disconnected(node: &NodeHandle, peer: PeerId, who: &str) {
    timeout(Duration::from_secs(15), async {
        loop {
            if !node.status().connected.contains(&peer) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{who} never disconnected"))
}

/// A peer that restarts on a new ephemeral port announces that port in its
/// Hello. The remembered address list must follow it, or every later
/// reconnect dials a port nothing listens on while the real one is discarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_after_restart_replaces_the_old_addresses() {
    let alice = Identity::from_seed([31u8; 32]);
    let bob = Identity::from_seed([32u8; 32]);
    let alice_dir = tempdir().unwrap();
    let bob_dir = tempdir().unwrap();
    let alice_store = shared_storage(Store::open(alice_dir.path()).unwrap());
    let bob_store = shared_storage(Store::open(bob_dir.path()).unwrap());
    let guild_id = alice_store
        .lock()
        .unwrap()
        .create_guild("restart", &alice)
        .unwrap()
        .guild
        .id;

    // Two ports held at once so the restart cannot be handed the first one,
    // which would make "the old address is gone" indistinguishable from "the
    // address did not change".
    let first_bind = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second_bind = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let first_addr: Multiaddr = format!(
        "/ip4/127.0.0.1/tcp/{}",
        first_bind.local_addr().unwrap().port()
    )
    .parse()
    .unwrap();
    let second_addr: Multiaddr = format!(
        "/ip4/127.0.0.1/tcp/{}",
        second_bind.local_addr().unwrap().port()
    )
    .parse()
    .unwrap();
    assert_ne!(first_addr, second_addr);

    // Bob listens so the restarted Alice can dial it back.
    let (bob_tx, mut bob_rx) = mpsc::unbounded_channel();
    let bob_node = start(
        NodeConfig {
            identity_seed: bob.seed(),
            guild_id,
            store: bob_store.clone(),
            listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
            dial: Vec::new(),
            relay: None,
            enable_upnp: false,
            enable_natpmp: false,
        },
        bob_tx,
    )
    .await
    .unwrap();
    let bob_listen = next_listening(&mut bob_rx, "bob").await;
    let bob_dial = bob_listen.with_p2p(bob_node.peer_id).unwrap();

    drop(first_bind);
    let (alice_tx, _alice_rx) = mpsc::unbounded_channel();
    let alice_first = start(
        NodeConfig {
            identity_seed: alice.seed(),
            guild_id,
            store: alice_store.clone(),
            listen: vec![first_addr.clone()],
            dial: vec![bob_dial.clone()],
            relay: None,
            enable_upnp: false,
            enable_natpmp: false,
        },
        alice_tx,
    )
    .await
    .unwrap();
    let alice_peer = alice_first.peer_id;
    wait_connected(&bob_node, alice_peer, "bob").await;
    wait_connected(&alice_first, bob_node.peer_id, "alice").await;

    // Bob learned the first process's listening address from its Hello, not
    // from dialling it, so this is the address a reconnect would use. The
    // connection is observed before the Hello that follows it is processed,
    // so poll for the address instead of reading the snapshot immediately.
    // On the old merge behaviour this wait still passes, because it kept the
    // first address beside the second; only the final check below fails.
    let recorded = timeout(Duration::from_secs(10), async {
        loop {
            let known = bob_node.known_addrs(alice_peer);
            if known.contains(&first_addr) {
                return known;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        recorded.is_ok(),
        "bob did not record the announced address {first_addr}: {:?}",
        bob_node.known_addrs(alice_peer)
    );

    alice_first.shutdown();
    wait_disconnected(&bob_node, alice_peer, "bob").await;

    // The second process is the same author on a new port.
    drop(second_bind);
    let (alice_tx, _alice_rx) = mpsc::unbounded_channel();
    let alice_second = start(
        NodeConfig {
            identity_seed: alice.seed(),
            guild_id,
            store: alice_store.clone(),
            listen: vec![second_addr.clone()],
            dial: vec![bob_dial],
            relay: None,
            enable_upnp: false,
            enable_natpmp: false,
        },
        alice_tx,
    )
    .await
    .unwrap();
    wait_connected(&bob_node, alice_peer, "bob").await;

    let replaced = timeout(Duration::from_secs(10), async {
        loop {
            let known = bob_node.known_addrs(alice_peer);
            if known.contains(&second_addr) && !known.contains(&first_addr) {
                return known;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        replaced.is_ok(),
        "bob still dials the pre-restart address: {:?}",
        bob_node.known_addrs(alice_peer)
    );

    alice_second.shutdown();
    bob_node.shutdown();
}
