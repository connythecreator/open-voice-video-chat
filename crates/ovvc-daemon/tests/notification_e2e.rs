//! Two independent daemons: B mentions A and A receives
//! [`DaemonEvent::Notification`] over its IPC socket.
//!
//! The daemon decides notifications itself, so this is the same path a GUI or
//! TUI uses: attach a client, subscribe to the daemon event stream, and post
//! from the other daemon. Both an `@name` mention and an explicit
//! `MessageV2::mentions` entry are covered.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-daemon --test notification_e2e -- --test-threads=1
//! ```

#![cfg(unix)]
#![allow(clippy::await_holding_lock)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ovvc_core::{AuthorId, ChannelId, EventId, GuildId};
use ovvc_ipc::{DaemonClient, DaemonEvent, NetworkOptionsDto, NotificationReason, Op, Reply};
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::timeout;

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
    /// Held so the daemon's data directory outlives the process.
    _dir: TempDir,
    socket: PathBuf,
}

impl DaemonProcess {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("daemon.sock");
        let child = Command::new(BIN)
            .arg("--data-dir")
            .arg(dir.path())
            .arg("--socket")
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ovvc-daemon");
        Self {
            child,
            _dir: dir,
            socket,
        }
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

async fn connect_client(daemon: &DaemonProcess) -> (DaemonClient, AuthorId) {
    let client = DaemonClient::new(daemon.socket());
    let connected = client
        .wait_connected(STARTUP)
        .await
        .expect("daemon handshake");
    let welcome = connected.daemon.expect("daemon info in the handshake");
    (client, welcome.author_id)
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

async fn general_channel(client: &DaemonClient, guild_id: GuildId) -> ChannelId {
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

async fn connection(client: &DaemonClient, guild_id: GuildId) -> ovvc_ipc::ConnectionStateDto {
    expect(
        client.call(Op::ConnectionState { guild_id }).await,
        |reply| match reply {
            Reply::Connection(state) => Some(state.clone()),
            _ => None,
        },
    )
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

async fn wait_listening(client: &DaemonClient, guild_id: GuildId) -> ovvc_ipc::ConnectionStateDto {
    wait_for(SYNC_BUDGET, "a listen address", || async {
        let state = connection(client, guild_id).await;
        (!state.listen_addresses.is_empty()).then_some(state)
    })
    .await
}

async fn wait_peers(client: &DaemonClient, guild_id: GuildId, want: usize) {
    wait_for(SYNC_BUDGET, "peer connections", || async {
        let state = connection(client, guild_id).await;
        (state.peers.len() >= want).then_some(())
    })
    .await;
}

async fn post(
    client: &DaemonClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    body: &str,
    mentions: Vec<AuthorId>,
) -> EventId {
    let reply = if mentions.is_empty() {
        client
            .call(Op::PostMessage {
                guild_id,
                channel_id,
                body: body.to_string(),
            })
            .await
    } else {
        client
            .call(Op::PostMessageFull {
                guild_id,
                channel_id,
                body: body.to_string(),
                reply_to: None,
                mentions,
            })
            .await
    };
    expect(reply, |reply| match reply {
        Reply::Event(event) => Some(event.id().expect("event id")),
        _ => None,
    })
}

async fn wait_notification(
    events: &mut broadcast::Receiver<DaemonEvent>,
    want: EventId,
) -> (ChannelId, AuthorId, NotificationReason) {
    let deadline = Instant::now() + SYNC_BUDGET;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for notification {want:?}");
        }
        match timeout(remaining, events.recv()).await {
            Ok(Ok(DaemonEvent::Notification {
                event_id,
                channel_id,
                author,
                reason,
                ..
            })) if event_id == want => return (channel_id, author, reason),
            Ok(Ok(_)) => {}
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(error)) => panic!("daemon event stream closed: {error}"),
            Err(_) => panic!("timed out waiting for notification {want:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mentioning_b_message_notifies_a_over_ipc() {
    let _guard = serial();
    let mut daemon_a = DaemonProcess::start();
    let mut daemon_b = DaemonProcess::start();
    let (client_a, author_a) = connect_client(&daemon_a).await;
    let (client_b, author_b) = connect_client(&daemon_b).await;

    let guild_a = create_guild(&client_a, "notification guild").await;
    let channel_a = general_channel(&client_a, guild_a).await;

    // A must have a display name for the `@name` form to resolve.
    expect(
        client_a
            .call(Op::SetDisplayName {
                guild_id: guild_a,
                display_name: "Alice".to_string(),
            })
            .await,
        |reply| matches!(reply, Reply::Unit).then_some(()),
    );

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
    wait_listening(&client_b, guild_a).await;
    wait_peers(&client_a, guild_a, 1).await;
    wait_peers(&client_b, guild_a, 1).await;

    // Subscribe before posting so the notification cannot race the test.
    let mut events_a = client_a.subscribe();

    // Plain `Message` plus `@name` text, resolved by A against its roster.
    let by_name = post(&client_b, guild_a, channel_b, "@Alice ping", Vec::new()).await;
    let (channel_id, author, reason) = wait_notification(&mut events_a, by_name).await;
    assert_eq!(channel_id, channel_a);
    assert_eq!(author, author_b);
    assert_eq!(reason, NotificationReason::Mention);

    // A `MessageV2` that carries the author id directly.
    let explicit = post(
        &client_b,
        guild_a,
        channel_b,
        "explicit mention",
        vec![author_a],
    )
    .await;
    let (_, author, reason) = wait_notification(&mut events_a, explicit).await;
    assert_eq!(author, author_b);
    assert_eq!(reason, NotificationReason::Mention);

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

    expect(client_a.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    drop(client_a);
    assert!(
        daemon_a
            .wait_exit(SHUTDOWN)
            .await
            .expect("daemon A exits")
            .success()
    );
}
