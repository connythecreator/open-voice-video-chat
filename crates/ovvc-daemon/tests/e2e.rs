//! End-to-end tests that drive the real `ovvc-daemon` binary as an independent
//! OS process over its Unix socket.
//!
//! The daemon is the only process allowed to touch the store, so these tests
//! exercise the same path a GUI or CLI uses: spawn the binary, shake hands with
//! [`DaemonClient`], issue every important request, and shut the daemon down
//! again. The second scenario restarts the daemon and checks that durable state
//! survives while the socket and discovery record are rebuilt.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-daemon --test e2e -- --test-threads=1
//! ```

#![cfg(unix)]
// The serial guard deliberately spans the whole (async) test so one daemon
// scenario runs at a time; the test runtime is single-threaded, so the guard
// cannot block the executor it is running on.
#![allow(clippy::await_holding_lock)]

use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ovvc_control::{PresenceStatus, SessionId};
use ovvc_core::SyncCursor;
use ovvc_ipc::{DaemonClient, NetworkOptionsDto, Op, Reply};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-daemon");
const STARTUP: Duration = Duration::from_secs(15);
const SHUTDOWN: Duration = Duration::from_secs(10);

/// Two daemons in one test binary share the loopback interface and mDNS is not
/// involved here, but a fixed test name still makes failures easier to read if
/// they are ever run in parallel with a real two-daemon scenario.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct DaemonProcess {
    child: Child,
    dir: TempDir,
    socket: std::path::PathBuf,
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
        Self { child, dir, socket }
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    fn data_dir(&self) -> &Path {
        self.dir.path()
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

#[tokio::test(flavor = "multi_thread")]
async fn daemon_serves_a_full_frontend_session() {
    let _guard = serial();
    let mut daemon = DaemonProcess::start();
    let client = DaemonClient::new(daemon.socket());
    let connected = client
        .wait_connected(STARTUP)
        .await
        .expect("daemon handshake");
    let welcome = connected.daemon.expect("daemon info in the handshake");
    assert_eq!(welcome.protocol, ovvc_ipc::IPC_PROTOCOL);
    assert!(welcome.max_frame_bytes > 0);
    assert_eq!(welcome.data_dir, daemon.data_dir().display().to_string());

    let identity = expect(client.call(Op::Identity).await, |reply| match reply {
        Reply::Identity {
            author_id,
            public_key_hex,
            ..
        } => Some((*author_id, public_key_hex.clone())),
        _ => None,
    });
    assert_eq!(identity.0, welcome.author_id);
    assert_eq!(identity.1, welcome.public_key_hex);

    let guild = expect(
        client
            .call(Op::CreateGuild {
                name: "daemon test guild".to_string(),
            })
            .await,
        |reply| match reply {
            Reply::GuildCreated(guild) => Some(guild.clone()),
            _ => None,
        },
    );
    let guild_id = guild.id;

    let channels = expect(
        client.call(Op::ListChannels { guild_id }).await,
        |reply| match reply {
            Reply::Channels(channels) => Some(channels.clone()),
            _ => None,
        },
    );
    let general = channels
        .iter()
        .find(|channel| channel.name == "general")
        .cloned()
        .expect("default channel");
    let channel_id = general.id;

    let message = expect(
        client
            .call(Op::PostMessage {
                guild_id,
                channel_id,
                body: "hello from the daemon e2e test".to_string(),
            })
            .await,
        |reply| match reply {
            Reply::Event(event) => Some(event.clone()),
            _ => None,
        },
    );

    let history = expect(
        client
            .call(Op::History {
                guild_id,
                channel_id,
                limit: 50,
            })
            .await,
        |reply| match reply {
            Reply::Events(events) => Some(events.clone()),
            _ => None,
        },
    );
    assert!(
        history
            .iter()
            .any(|event| event.id().unwrap() == message.id().unwrap())
    );

    let page = expect(
        client
            .call(Op::HistoryBefore {
                guild_id,
                channel_id,
                cursor: None,
                limit: 1,
            })
            .await,
        |reply| match reply {
            Reply::HistoryPage(page) => Some(page.clone()),
            _ => None,
        },
    );
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].id().unwrap(), message.id().unwrap());

    let empty = expect(
        client
            .call(Op::HistoryBefore {
                guild_id,
                channel_id,
                cursor: Some(SyncCursor::at(&message).unwrap()),
                limit: 1,
            })
            .await,
        |reply| match reply {
            Reply::HistoryPage(page) => Some(page.clone()),
            _ => None,
        },
    );
    assert!(empty.events.len() <= 1);
    assert!(
        empty
            .events
            .iter()
            .all(|event| event.id().unwrap() != message.id().unwrap())
    );

    let reaction = expect(
        client
            .call(Op::AddReaction {
                guild_id,
                target: message.id().unwrap(),
                emoji: "\u{1f44d}".to_string(),
            })
            .await,
        |reply| match reply {
            Reply::Event(event) => Some(event.clone()),
            _ => None,
        },
    );
    assert!(reaction.body.kind.reaction().is_some());

    expect(
        client
            .call(Op::MarkRead {
                guild_id,
                channel_id,
                event_id: message.id().unwrap(),
            })
            .await,
        |reply| matches!(reply, Reply::Event(_)).then_some(()),
    );

    let unread = expect(
        client.call(Op::UnreadCounts { guild_id }).await,
        |reply| match reply {
            Reply::Unread(counts) => Some(counts.clone()),
            _ => None,
        },
    );
    assert_eq!(unread.get(&channel_id).copied().unwrap_or(0), 0);

    let hits = expect(
        client
            .call(Op::Search {
                guild_id,
                query: "daemon e2e".to_string(),
                limit: 10,
            })
            .await,
        |reply| match reply {
            Reply::Search(hits) => Some(hits.clone()),
            _ => None,
        },
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].guild_id, guild_id);
    assert_eq!(hits[0].channel_id, channel_id);
    assert!(hits[0].body.contains("daemon e2e"));

    let session_id = SessionId::for_guild(guild_id);
    let snapshot = expect(
        client
            .call(Op::ControlJoin {
                guild_id,
                session_id,
                display_name: "Tester".to_string(),
            })
            .await,
        |reply| match reply {
            Reply::Control(snapshot) => Some(snapshot.clone()),
            _ => None,
        },
    );
    assert!(
        snapshot
            .roster
            .iter()
            .any(|member| member.author == welcome.author_id)
    );

    expect(
        client
            .call(Op::SetPresence {
                guild_id,
                status: PresenceStatus::Away,
            })
            .await,
        |reply| matches!(reply, Reply::Unit).then_some(()),
    );
    expect(
        client
            .call(Op::SetTyping {
                guild_id,
                channel_id,
                typing: true,
            })
            .await,
        |reply| matches!(reply, Reply::Unit).then_some(()),
    );

    let snapshot = expect(
        client
            .call(Op::ControlSnapshot {
                guild_id,
                session_id,
            })
            .await,
        |reply| match reply {
            Reply::Control(snapshot) => Some(snapshot.clone()),
            _ => None,
        },
    );
    assert!(
        snapshot
            .typing
            .iter()
            .any(|typing| typing.channel_id == channel_id)
    );
    assert!(
        snapshot
            .roster
            .iter()
            .any(|member| member.author == welcome.author_id)
    );

    let status = expect(client.call(Op::Status).await, |reply| match reply {
        Reply::Status(status) => Some(status.clone()),
        _ => None,
    });
    assert_eq!(status.pid, daemon.child.id());
    assert_eq!(status.author_id, welcome.author_id);
    assert!(status.connected_guilds.is_empty());

    let connection = expect(
        client.call(Op::ConnectionState { guild_id }).await,
        |reply| match reply {
            Reply::Connection(state) => Some(state.clone()),
            _ => None,
        },
    );
    assert!(!connection.connected);
    assert!(connection.peer_id.is_none());
    assert!(connection.peers.is_empty());

    expect(client.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    let status = daemon
        .wait_exit(SHUTDOWN)
        .await
        .expect("daemon exits after ShutdownDaemon");
    assert!(status.success(), "daemon exit status: {status}");
    assert!(!daemon.socket().exists(), "socket should be removed");
    assert!(
        !daemon.data_dir().join(ovvc_ipc::DAEMON_INFO_FILE).exists(),
        "discovery record should be removed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_state_survives_a_daemon_restart() {
    let _guard = serial();
    let mut daemon = DaemonProcess::start();
    let socket = daemon.socket().to_path_buf();
    let data_dir = daemon.data_dir().to_path_buf();

    let first = DaemonClient::new(&socket);
    first
        .wait_connected(STARTUP)
        .await
        .expect("first handshake");
    let guild = expect(
        first
            .call(Op::CreateGuild {
                name: "persistent guild".to_string(),
            })
            .await,
        |reply| match reply {
            Reply::GuildCreated(guild) => Some(guild.clone()),
            _ => None,
        },
    );
    let message = expect(
        first
            .call(Op::PostMessage {
                guild_id: guild.id,
                channel_id: expect(
                    first.call(Op::ListChannels { guild_id: guild.id }).await,
                    |reply| match reply {
                        Reply::Channels(channels) => Some(channels[0].id),
                        _ => None,
                    },
                ),
                body: "persisted across restart".to_string(),
            })
            .await,
        |reply| match reply {
            Reply::Event(event) => Some(event.clone()),
            _ => None,
        },
    );
    expect(first.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    drop(first);
    // The first process must be gone before the store can be opened again.
    // Keep its `TempDir` alive: the second process reuses that data directory.
    let status = daemon
        .wait_exit(SHUTDOWN)
        .await
        .expect("first daemon exits");
    assert!(status.success());

    // A second process against the same data directory: the store reopens and
    // the identity, guild and message are all still there.
    let mut second = DaemonProcess::start_at(&data_dir);
    let client = DaemonClient::new(second.socket());
    let connected = client
        .wait_connected(STARTUP)
        .await
        .expect("second handshake");
    let welcome = connected
        .daemon
        .expect("daemon info in the second handshake");

    let guilds = expect(client.call(Op::ListGuilds).await, |reply| match reply {
        Reply::Guilds(guilds) => Some(guilds.clone()),
        _ => None,
    });
    assert!(
        guilds.iter().any(|candidate| candidate.id == guild.id),
        "guilds after restart: {guilds:?}; data dir {}; welcome dir {}",
        data_dir.display(),
        welcome.data_dir
    );
    let event = expect(
        client
            .call(Op::GetEvent {
                guild_id: guild.id,
                event_id: message.id().unwrap(),
            })
            .await,
        |reply| match reply {
            Reply::OptionalEvent(event) => Some(event.clone()),
            _ => None,
        },
    )
    .expect("message survives restart");
    assert_eq!(event.id().unwrap(), message.id().unwrap());
    assert_eq!(welcome.author_id, message.body.author);

    expect(client.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    let status = second
        .wait_exit(SHUTDOWN)
        .await
        .expect("second daemon exits");
    assert!(status.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_daemon_rejoins_the_network_it_was_online_in() {
    let _guard = serial();
    let mut daemon = DaemonProcess::start();
    let socket = daemon.socket().to_path_buf();
    let data_dir = daemon.data_dir().to_path_buf();

    let first = DaemonClient::new(&socket);
    first
        .wait_connected(STARTUP)
        .await
        .expect("first handshake");
    let guild_id = expect(
        first
            .call(Op::CreateGuild {
                name: "rejoin guild".to_string(),
            })
            .await,
        |reply| match reply {
            Reply::GuildCreated(guild) => Some(guild.id),
            _ => None,
        },
    );
    expect(
        first
            .call(Op::Connect {
                guild_id,
                options: NetworkOptionsDto {
                    listen: vec!["/ip4/127.0.0.1/tcp/0".to_string()],
                    ..NetworkOptionsDto::default()
                },
            })
            .await,
        |reply| matches!(reply, Reply::Unit).then_some(()),
    );
    assert!(
        wait_online(&first, guild_id, STARTUP).await,
        "the first daemon never listened"
    );
    let intent = network_intent(&data_dir);
    assert!(
        intent.contains(&guild_id.to_hex()),
        "the online intent was not recorded: {intent}"
    );
    assert!(
        intent.contains("\"online\": true"),
        "the online intent was not recorded as online: {intent}"
    );

    expect(first.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    drop(first);
    let status = daemon
        .wait_exit(SHUTDOWN)
        .await
        .expect("first daemon exits");
    assert!(status.success());

    // No frontend asks for anything after this point: the restart itself has
    // to rejoin, which is exactly what was broken when the two machines went
    // quiet after a daemon restart.
    let mut second = DaemonProcess::start_at(&data_dir);
    let client = DaemonClient::new(second.socket());
    client
        .wait_connected(STARTUP)
        .await
        .expect("second handshake");
    assert!(
        wait_online(&client, guild_id, STARTUP).await,
        "the restarted daemon did not rejoin the peer network"
    );

    expect(client.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    let status = second
        .wait_exit(SHUTDOWN)
        .await
        .expect("second daemon exits");
    assert!(status.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn leaving_a_guild_clears_its_network_intent() {
    let _guard = serial();
    let mut daemon = DaemonProcess::start();
    let socket = daemon.socket().to_path_buf();
    let data_dir = daemon.data_dir().to_path_buf();

    let first = DaemonClient::new(&socket);
    first
        .wait_connected(STARTUP)
        .await
        .expect("first handshake");
    let guild_id = expect(
        first
            .call(Op::CreateGuild {
                name: "left guild".to_string(),
            })
            .await,
        |reply| match reply {
            Reply::GuildCreated(guild) => Some(guild.id),
            _ => None,
        },
    );
    expect(
        first
            .call(Op::Connect {
                guild_id,
                options: NetworkOptionsDto {
                    listen: vec!["/ip4/127.0.0.1/tcp/0".to_string()],
                    ..NetworkOptionsDto::default()
                },
            })
            .await,
        |reply| matches!(reply, Reply::Unit).then_some(()),
    );
    assert!(wait_online(&first, guild_id, STARTUP).await);
    expect(first.call(Op::LeaveGuild { guild_id }).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    let intent = network_intent(&data_dir);
    assert!(
        !intent.contains(&guild_id.to_hex()),
        "the left guild is still recorded for the next start: {intent}"
    );

    expect(first.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    drop(first);
    let status = daemon
        .wait_exit(SHUTDOWN)
        .await
        .expect("first daemon exits");
    assert!(status.success());

    let mut second = DaemonProcess::start_at(&data_dir);
    let client = DaemonClient::new(second.socket());
    client
        .wait_connected(STARTUP)
        .await
        .expect("second handshake");
    // The restart runs resume in the background; give it long enough to act
    // if it were going to, then prove it did not dial the left guild.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(Reply::Connection(state)) = client.call(Op::ConnectionState { guild_id }).await {
            assert!(
                !state.connected,
                "the restarted daemon dialled a guild that was left"
            );
            assert!(
                state.listen_addresses.is_empty(),
                "the left guild bound an address: {:?}",
                state.listen_addresses
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    expect(client.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    let status = second
        .wait_exit(SHUTDOWN)
        .await
        .expect("second daemon exits");
    assert!(status.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interactive_daemon_connects_a_guild_with_no_saved_intent() {
    let _guard = serial();
    let mut daemon = DaemonProcess::start();
    let data_dir = daemon.data_dir().to_path_buf();

    let first = DaemonClient::new(daemon.socket());
    first
        .wait_connected(STARTUP)
        .await
        .expect("first handshake");
    let guild_id = expect(
        first
            .call(Op::CreateGuild {
                name: "default online guild".to_string(),
            })
            .await,
        |reply| match reply {
            Reply::GuildCreated(guild) => Some(guild.id),
            _ => None,
        },
    );
    // Never connect here: the guild has no saved intent at all.
    expect(first.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    drop(first);
    let status = daemon
        .wait_exit(SHUTDOWN)
        .await
        .expect("first daemon exits");
    assert!(status.success());
    assert!(
        network_intent(&data_dir).is_empty(),
        "nothing should have been recorded before ever going online"
    );

    let mut second = DaemonProcess::start_at_with(&data_dir, true);
    let client = DaemonClient::new(second.socket());
    client
        .wait_connected(STARTUP)
        .await
        .expect("second handshake");
    assert!(
        wait_online(&client, guild_id, STARTUP).await,
        "a daemon started with --online left the guild offline"
    );
    let intent = network_intent(&data_dir);
    assert!(
        intent.contains(&guild_id.to_hex()) && intent.contains("\"online\": true"),
        "--online did not record the guild as online: {intent}"
    );

    expect(client.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    let status = second
        .wait_exit(SHUTDOWN)
        .await
        .expect("second daemon exits");
    assert!(status.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disconnect_during_resume_is_not_undone() {
    let _guard = serial();
    let mut daemon = DaemonProcess::start();
    let data_dir = daemon.data_dir().to_path_buf();

    let first = DaemonClient::new(daemon.socket());
    first
        .wait_connected(STARTUP)
        .await
        .expect("first handshake");
    // Enough guilds that resume takes long enough for the click below to land
    // while it is still working through the list.
    let mut guilds = Vec::new();
    for index in 0..32 {
        let guild_id = expect(
            first
                .call(Op::CreateGuild {
                    name: format!("resume race {index}"),
                })
                .await,
            |reply| match reply {
                Reply::GuildCreated(guild) => Some(guild.id),
                _ => None,
            },
        );
        expect(
            first
                .call(Op::Connect {
                    guild_id,
                    options: NetworkOptionsDto {
                        listen: vec!["/ip4/127.0.0.1/tcp/0".to_string()],
                        ..NetworkOptionsDto::default()
                    },
                })
                .await,
            |reply| matches!(reply, Reply::Unit).then_some(()),
        );
        guilds.push(guild_id);
    }
    // The last guild is the last one resume reaches, so a click sent straight
    // after the handshake has a good chance of landing before its connect.
    let target = *guilds.last().expect("at least one guild");

    expect(first.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    drop(first);
    daemon
        .wait_exit(SHUTDOWN)
        .await
        .expect("first daemon exits");

    let mut second = DaemonProcess::start_at(&data_dir);
    let client = DaemonClient::new(second.socket());
    client
        .wait_connected(STARTUP)
        .await
        .expect("second handshake");
    expect(
        client.call(Op::Disconnect { guild_id: target }).await,
        |reply| matches!(reply, Reply::Unit).then_some(()),
    );

    // Resume must not bring it back after the click, and the repair pass must
    // not either, because [ go offline ] is remembered for this session.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(Reply::Connection(state)) =
            client.call(Op::ConnectionState { guild_id: target }).await
        {
            assert!(
                !state.connected,
                "resume overrode [ go offline ] for a guild it was still starting"
            );
            assert!(
                state.listen_addresses.is_empty(),
                "the guild bound an address after [ go offline ]: {:?}",
                state.listen_addresses
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The intent itself must survive, because the next start rejoins it.
    assert!(
        network_intent(&data_dir).contains(&target.to_hex()),
        "taking a guild offline erased the intent for the next start"
    );

    expect(client.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    let status = second
        .wait_exit(SHUTDOWN)
        .await
        .expect("second daemon exits");
    assert!(status.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_guild_whose_saved_port_is_busy_is_retried_without_a_restart() {
    let _guard = serial();
    // Hold a port so the saved intent cannot bind during the restart.
    let holder = std::net::TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let port = holder.local_addr().expect("holder address").port();

    let mut daemon = DaemonProcess::start();
    let data_dir = daemon.data_dir().to_path_buf();
    let first = DaemonClient::new(daemon.socket());
    first
        .wait_connected(STARTUP)
        .await
        .expect("first handshake");
    let guild_id = expect(
        first
            .call(Op::CreateGuild {
                name: "busy port guild".to_string(),
            })
            .await,
        |reply| match reply {
            Reply::GuildCreated(guild) => Some(guild.id),
            _ => None,
        },
    );
    expect(
        first
            .call(Op::Connect {
                guild_id,
                options: NetworkOptionsDto {
                    listen: vec!["/ip4/127.0.0.1/tcp/0".to_string()],
                    ..NetworkOptionsDto::default()
                },
            })
            .await,
        |reply| matches!(reply, Reply::Unit).then_some(()),
    );
    assert!(wait_online(&first, guild_id, STARTUP).await);
    expect(first.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    drop(first);
    daemon
        .wait_exit(SHUTDOWN)
        .await
        .expect("first daemon exits");

    // Point the saved listen address at the held port, the way an earlier
    // session would have saved a fixed one.
    let path = data_dir.join("network.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read intent")).expect("parse intent");
    value["guilds"][0]["listen"] = serde_json::json!([format!("/ip4/127.0.0.1/tcp/{port}")]);
    std::fs::write(&path, serde_json::to_vec_pretty(&value).expect("encode"))
        .expect("write intent");

    let mut second =
        DaemonProcess::start_at_with_env(&data_dir, &[("OVVC_NETWORK_REPAIR_SECS", "1")]);
    let client = DaemonClient::new(second.socket());
    client
        .wait_connected(STARTUP)
        .await
        .expect("second handshake");

    // The failed rejoin is an operational condition, not a bug: it reaches the
    // frontend as a diagnostic and leaves a classified line in the daemon log.
    let log_path = data_dir.join(ovvc_ipc::DAEMON_LOG_FILE);
    let line = wait_for_log_line(&log_path, "class=operational", Duration::from_secs(5));
    assert!(
        line.contains("context=\"resume network\""),
        "the failed rejoin must name its context: {line}"
    );
    assert!(
        line.contains(&format!("guild={}", guild_id.to_hex())),
        "the log line must name the guild: {line}"
    );
    assert!(line.contains("pid="), "the log line must name the pid: {line}");
    assert!(
        line.contains("version="),
        "the log line must name the daemon version: {line}"
    );

    // While the port is held the rejoin fails; the daemon must stay up.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(Reply::Connection(state)) = client.call(Op::ConnectionState { guild_id }).await {
            assert!(
                state.listen_addresses.is_empty(),
                "a guild bound a port another process owns: {:?}",
                state.listen_addresses
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    drop(holder);
    // With the port free, the retry loop must recover the guild on its own.
    assert!(
        wait_online(&client, guild_id, Duration::from_secs(10)).await,
        "the daemon never retried a rejoin that failed because the port was busy"
    );

    expect(client.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    let status = second
        .wait_exit(SHUTDOWN)
        .await
        .expect("second daemon exits");
    assert!(status.success());
}

impl DaemonProcess {
    fn start_at(data_dir: &Path) -> Self {
        Self::start_at_with(data_dir, false)
    }

    /// Restart with extra environment variables, for a test that needs to
    /// shorten a production retry interval.
    fn start_at_with_env(data_dir: &Path, env: &[(&str, &str)]) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let socket = dir.path().join("daemon.sock");
        let mut command = Command::new(BIN);
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--socket")
            .arg(&socket);
        for (key, value) in env {
            command.env(key, value);
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ovvc-daemon");
        Self { child, dir, socket }
    }

    /// Restart against an existing data directory, optionally as an
    /// interactive frontend would (`--online`).
    fn start_at_with(data_dir: &Path, online: bool) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let socket = dir.path().join("daemon.sock");
        let mut command = Command::new(BIN);
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--socket")
            .arg(&socket);
        if online {
            command.arg("--online");
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ovvc-daemon");
        Self { child, dir, socket }
    }
}

/// Poll the daemon until the guild is online with at least one listen address.
///
/// The node reports `is_connected` before it has bound a socket, and the point
/// of these tests is that a restart brings the guild back up on its own, so the
/// listen address is the proof rather than the flag alone.
async fn wait_online(
    client: &DaemonClient,
    guild_id: ovvc_core::GuildId,
    budget: Duration,
) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if let Ok(Reply::Connection(state)) = client.call(Op::ConnectionState { guild_id }).await
            && state.connected
            && !state.listen_addresses.is_empty()
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for one line of the daemon log containing `needle`.
///
/// The daemon is a separate process, so the log file is the observable proof
/// for a condition that may be reported before a frontend attaches.
fn wait_for_log_line(path: &Path, needle: &str, budget: Duration) -> String {
    let deadline = Instant::now() + budget;
    loop {
        if let Ok(contents) = std::fs::read_to_string(path)
            && let Some(line) = contents.lines().find(|line| line.contains(needle))
        {
            return line.to_string();
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out after {budget:?} waiting for {needle:?} in {}",
                path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The intent file is what makes the next start reconnect, so read it as text
/// rather than trusting the in-memory copy.
fn network_intent(data_dir: &Path) -> String {
    std::fs::read_to_string(data_dir.join("network.json")).unwrap_or_default()
}

/// Profile operations and the direct-message list are plain IPC round trips:
/// a frontend publishes a profile, resolves it back, and sees an empty DM list
/// until a conversation is opened.
#[tokio::test(flavor = "multi_thread")]
async fn profile_ops_and_the_direct_message_list_roundtrip() {
    use ovvc_core::EventKind;

    let _guard = serial();
    let mut daemon = DaemonProcess::start();
    let client = DaemonClient::new(daemon.socket());
    let connected = client
        .wait_connected(STARTUP)
        .await
        .expect("daemon handshake");
    let welcome = connected.daemon.expect("daemon info in the handshake");

    let guild = expect(
        client
            .call(Op::CreateGuild {
                name: "profiles".to_string(),
            })
            .await,
        |reply| match reply {
            Reply::GuildCreated(guild) => Some(guild.clone()),
            _ => None,
        },
    );

    let event = expect(
        client
            .call(Op::SetProfile {
                guild_id: guild.id,
                about: "built during the profile e2e".to_string(),
                pronouns: "they/them".to_string(),
                links: vec!["https://example.com".to_string()],
            })
            .await,
        |reply| match reply {
            Reply::Event(event) => Some(event.clone()),
            _ => None,
        },
    );
    assert_eq!(
        event.body.kind,
        EventKind::MemberProfileUpdated {
            about: "built during the profile e2e".to_string(),
            pronouns: "they/them".to_string(),
            links: vec!["https://example.com".to_string()],
        }
    );

    let profile = expect(
        client
            .call(Op::GetMember {
                guild_id: guild.id,
                author: welcome.author_id,
            })
            .await,
        |reply| match reply {
            Reply::Member(profile) => Some(profile.clone()),
            _ => None,
        },
    );
    assert_eq!(profile.author, welcome.author_id);
    assert_eq!(profile.about, "built during the profile e2e");
    assert_eq!(profile.pronouns, "they/them");
    assert_eq!(profile.links, ["https://example.com"]);

    let direct = expect(client.call(Op::ListDirectMessages).await, |reply| match reply {
        Reply::DirectMessages(direct) => Some(direct.clone()),
        _ => None,
    });
    assert!(
        direct.is_empty(),
        "a shared guild is not a direct conversation"
    );

    assert!(
        client
            .call(Op::OpenDirectMessage {
                context: guild.id,
                peer: welcome.author_id,
            })
            .await
            .is_err(),
        "a conversation needs another member"
    );

    expect(client.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    daemon.wait_exit(SHUTDOWN).await.expect("daemon exits");
}
