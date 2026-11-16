//! Two independent daemon processes converge over a real loopback link.
//!
//! This is the automated stand-in for the two-machine LAN run: two
//! `ovvc-daemon` binaries, two data directories, two Unix sockets, two libp2p
//! nodes listening on real TCP ports, and one invite. The scenario deliberately
//! posts while both sides are disconnected so both authors use the same Lamport
//! clock, which is the tie boundary that used to lose events. Convergence is
//! checked in both directions after reconnecting, and again after restarting
//! one daemon from its own store.
//!
//! The physical cross-machine script remains `scripts/e2e-two-machines-lan.sh`;
//! this test covers the same protocol path without SSH or a second host.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-daemon --test lan_e2e -- --test-threads=1
//! ```

#![cfg(unix)]
#![allow(clippy::await_holding_lock)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ovvc_core::{GuildId, SignedEvent};
use ovvc_ipc::{ConnectionStateDto, DaemonClient, NetworkOptionsDto, Op, Reply};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-daemon");
const STARTUP: Duration = Duration::from_secs(15);
const SYNC_BUDGET: Duration = Duration::from_secs(45);
const SHUTDOWN: Duration = Duration::from_secs(10);

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct DaemonProcess {
    child: Child,
    dir: TempDir,
    socket: PathBuf,
}

impl DaemonProcess {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("daemon.sock");
        Self::spawn(dir, socket, None)
    }

    fn start_at(data_dir: &Path) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let socket = dir.path().join("daemon.sock");
        Self::spawn(dir, socket, Some(data_dir.to_path_buf()))
    }

    fn spawn(dir: TempDir, socket: PathBuf, data_dir: Option<PathBuf>) -> Self {
        let data_dir = data_dir.unwrap_or_else(|| dir.path().to_path_buf());
        let child = Command::new(BIN)
            .arg("--data-dir")
            .arg(&data_dir)
            .arg("--socket")
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ovvc-daemon");
        Self { child, dir, socket }
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    async fn wait_exit(&mut self, budget: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + budget;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Ok(None) => return None,
                Err(error) => panic!("waiting for daemon: {error}"),
            }
        }
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn expect<T>(result: Result<Reply, ovvc_ipc::IpcError>, want: impl Fn(&Reply) -> Option<T>) -> T {
    match result {
        Ok(reply) => want(&reply).unwrap_or_else(|| panic!("unexpected reply: {reply:?}")),
        Err(error) => panic!("daemon returned an error: {error:?}"),
    }
}

async fn connect_client(daemon: &DaemonProcess) -> DaemonClient {
    let client = DaemonClient::new(daemon.socket());
    client
        .wait_connected(STARTUP)
        .await
        .expect("daemon handshake");
    client
}

async fn create_guild(client: &DaemonClient, name: &str) -> GuildId {
    expect(
        client
            .call(Op::CreateGuild {
                name: name.to_string(),
            })
            .await,
        |reply| match reply {
            Reply::GuildCreated(guild) => Some(guild.id),
            _ => None,
        },
    )
}

async fn general_channel(client: &DaemonClient, guild_id: GuildId) -> ovvc_core::ChannelId {
    expect(
        client.call(Op::ListChannels { guild_id }).await,
        |reply| match reply {
            Reply::Channels(channels) => channels
                .iter()
                .find(|channel| channel.name == "general")
                .map(|channel| channel.id),
            _ => None,
        },
    )
}

async fn connect(client: &DaemonClient, guild_id: GuildId, listen: &str, dial: Vec<String>) {
    expect(
        client
            .call(Op::Connect {
                guild_id,
                options: NetworkOptionsDto {
                    listen: vec![listen.to_string()],
                    dial,
                    ..NetworkOptionsDto::default()
                },
            })
            .await,
        |reply| matches!(reply, Reply::Unit).then_some(()),
    );
}

async fn wait_listening(client: &DaemonClient, guild_id: GuildId) -> ConnectionStateDto {
    wait_for(SYNC_BUDGET, "a listen address", || async {
        let state = connection(client, guild_id).await;
        (!state.listen_addresses.is_empty()).then_some(state)
    })
    .await
}

async fn wait_peers(client: &DaemonClient, guild_id: GuildId, want: usize) -> ConnectionStateDto {
    wait_for(SYNC_BUDGET, "peer connections", || async {
        let state = connection(client, guild_id).await;
        (state.peers.len() >= want).then_some(state)
    })
    .await
}

async fn connection(client: &DaemonClient, guild_id: GuildId) -> ConnectionStateDto {
    expect(
        client.call(Op::ConnectionState { guild_id }).await,
        |reply| match reply {
            Reply::Connection(state) => Some(state.clone()),
            _ => None,
        },
    )
}

async fn history(
    client: &DaemonClient,
    guild_id: GuildId,
    channel_id: ovvc_core::ChannelId,
) -> Vec<SignedEvent> {
    expect(
        client
            .call(Op::History {
                guild_id,
                channel_id,
                limit: 200,
            })
            .await,
        |reply| match reply {
            Reply::Events(events) => Some(events.clone()),
            _ => None,
        },
    )
}

async fn wait_for_history(
    client: &DaemonClient,
    guild_id: GuildId,
    channel_id: ovvc_core::ChannelId,
    bodies: &[&str],
) -> Vec<SignedEvent> {
    wait_for(SYNC_BUDGET, "history convergence", || async {
        let events = history(client, guild_id, channel_id).await;
        let seen: HashSet<&str> = events
            .iter()
            .filter_map(|event| match &event.body.kind {
                ovvc_core::EventKind::Message { body } => Some(body.as_str()),
                ovvc_core::EventKind::MessageV2 { body, .. } => Some(body.as_str()),
                _ => None,
            })
            .collect();
        bodies
            .iter()
            .all(|body| seen.contains(body))
            .then_some(events)
    })
    .await
}

async fn wait_for<T, F, Fut>(budget: Duration, what: &str, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + budget;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn posted(reply: &Reply) -> Option<SignedEvent> {
    match reply {
        Reply::Event(event) => Some(event.clone()),
        _ => None,
    }
}

async fn post(
    client: &DaemonClient,
    guild_id: GuildId,
    channel_id: ovvc_core::ChannelId,
    body: &str,
) -> SignedEvent {
    expect(
        client
            .call(Op::PostMessage {
                guild_id,
                channel_id,
                body: body.to_string(),
            })
            .await,
        posted,
    )
}

async fn disconnect(client: &DaemonClient, guild_id: GuildId) {
    expect(client.call(Op::Disconnect { guild_id }).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    wait_for(SYNC_BUDGET, "disconnect", || async {
        (!connection(client, guild_id).await.connected).then_some(())
    })
    .await;
}

fn message_body(event: &SignedEvent) -> Option<&str> {
    match &event.body.kind {
        ovvc_core::EventKind::Message { body } => Some(body.as_str()),
        ovvc_core::EventKind::MessageV2 { body, .. }
        | ovvc_core::EventKind::MessageV3 { body, .. } => Some(body.as_str()),
        _ => None,
    }
}

fn ordered_ids(events: &[SignedEvent]) -> Vec<ovvc_core::EventId> {
    let mut ordered: Vec<_> = events
        .iter()
        .filter_map(|event| event.id().ok().map(|id| (event.body.lamport, id)))
        .collect();
    ordered.sort();
    ordered.into_iter().map(|(_, id)| id).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_daemons_converge_a_lamport_tie_after_offline_posts() {
    let _guard = serial();
    let mut daemon_a = DaemonProcess::start();
    let mut daemon_b = DaemonProcess::start();
    let client_a = connect_client(&daemon_a).await;
    let client_b = connect_client(&daemon_b).await;

    let guild_a = create_guild(&client_a, "lan tie guild").await;
    let channel_a = general_channel(&client_a, guild_a).await;

    connect(&client_a, guild_a, "/ip4/127.0.0.1/tcp/0", Vec::new()).await;
    let state_a = wait_listening(&client_a, guild_a).await;
    let peer_a = state_a.peer_id.clone().expect("A peer id");
    let addr_a = state_a.listen_addresses[0].clone();

    let invite = expect(
        client_a
            .call(Op::CreateInvite {
                guild_id: guild_a,
                peers: vec![format!("{addr_a}/p2p/{peer_a}")],
                expires_in_secs: Some(3600),
            })
            .await,
        |reply| match reply {
            Reply::Invite(invite) => Some(invite.encode().expect("invite encoding")),
            _ => None,
        },
    );

    let guild_b = expect(
        client_b
            .call(Op::JoinGuild {
                invite,
                options: NetworkOptionsDto {
                    listen: vec!["/ip4/127.0.0.1/tcp/0".to_string()],
                    ..NetworkOptionsDto::default()
                },
            })
            .await,
        |reply| match reply {
            Reply::Guild(Some(guild)) => Some(guild.id),
            _ => None,
        },
    );
    assert_eq!(guild_a, guild_b);
    let channel_b = general_channel(&client_b, guild_a).await;
    let state_b = wait_listening(&client_b, guild_a).await;
    let addr_b = state_b.listen_addresses[0].clone();
    wait_peers(&client_a, guild_a, 1).await;
    wait_peers(&client_b, guild_a, 1).await;

    let before = post(&client_a, guild_a, channel_a, "before the split").await;
    wait_for_history(&client_b, guild_a, channel_b, &["before the split"]).await;

    // Both sides now agree on the clock. Disconnect them, then post one
    // message each: same guild Lamport value, different event ids.
    disconnect(&client_a, guild_a).await;
    disconnect(&client_b, guild_a).await;
    let tie_a = post(&client_a, guild_a, channel_a, "offline from A").await;
    let tie_b = post(&client_b, guild_a, channel_b, "offline from B").await;
    assert_eq!(
        tie_a.body.lamport, tie_b.body.lamport,
        "the offline posts must share a Lamport value to exercise the tie path"
    );
    assert_ne!(tie_a.id().unwrap(), tie_b.id().unwrap());

    // B reconnects and dials A's stable address. Hello in both directions
    // starts a sync from each side, so the tie group crosses the page
    // boundary once in the forward direction and once in reverse.
    connect(&client_a, guild_a, &addr_a, Vec::new()).await;
    wait_listening(&client_a, guild_a).await;
    let peer_a_again = connection(&client_a, guild_a)
        .await
        .peer_id
        .expect("A peer id after restart of its node");
    connect(
        &client_b,
        guild_a,
        &addr_b,
        vec![format!("{addr_a}/p2p/{peer_a_again}")],
    )
    .await;
    wait_peers(&client_a, guild_a, 1).await;
    wait_peers(&client_b, guild_a, 1).await;

    let events_a = wait_for_history(
        &client_a,
        guild_a,
        channel_a,
        &["before the split", "offline from A", "offline from B"],
    )
    .await;
    let events_b = wait_for_history(
        &client_b,
        guild_a,
        channel_b,
        &["before the split", "offline from A", "offline from B"],
    )
    .await;

    // The tie group appears exactly once on each side, and both stores folded
    // it into the same total order.
    for body in ["offline from A", "offline from B"] {
        assert_eq!(
            events_a
                .iter()
                .filter(|event| message_body(event) == Some(body))
                .count(),
            1,
            "{body} duplicated on A"
        );
        assert_eq!(
            events_b
                .iter()
                .filter(|event| message_body(event) == Some(body))
                .count(),
            1,
            "{body} duplicated on B"
        );
    }
    assert!(
        events_a
            .iter()
            .any(|event| event.id().unwrap() == before.id().unwrap())
    );
    assert!(
        events_b
            .iter()
            .any(|event| event.id().unwrap() == before.id().unwrap())
    );
    assert_eq!(
        ordered_ids(&events_a),
        ordered_ids(&events_b),
        "both replicas must agree on (lamport, event_id) ordering"
    );

    // Restarting A from its own store must not duplicate or lose the tie.
    expect(client_a.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    drop(client_a);
    let status = daemon_a
        .wait_exit(SHUTDOWN)
        .await
        .expect("daemon A exits after ShutdownDaemon");
    assert!(status.success());

    let mut daemon_a2 = DaemonProcess::start_at(daemon_a.dir.path());
    let client_a2 = connect_client(&daemon_a2).await;
    let after_restart = wait_for_history(
        &client_a2,
        guild_a,
        channel_a,
        &["before the split", "offline from A", "offline from B"],
    )
    .await;
    assert_eq!(
        ordered_ids(&after_restart),
        ordered_ids(&events_a),
        "the restart must preserve the converged order"
    );

    expect(client_b.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    drop(client_b);
    assert!(
        daemon_b
            .wait_exit(SHUTDOWN)
            .await
            .expect("daemon B exits")
            .success()
    );
    expect(client_a2.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    drop(client_a2);
    assert!(
        daemon_a2
            .wait_exit(SHUTDOWN)
            .await
            .expect("the restarted daemon exits")
            .success()
    );
}
