//! Late voice-call convergence across two real daemon processes.
//!
//! The audit found that a member who joined a commune after a call was already
//! live did not learn the existing call members, so one side saw a one-member
//! call with E2EE `true` and the other saw two members with `e2ee=false`,
//! `peers=0` and `played=0`. Nothing recovered until someone left and rejoined.
//!
//! These tests start two real `ovvc-daemon` binaries with their own data
//! directories, Unix sockets and libp2p nodes. In the A-first case A joins the
//! voice channel before B has even joined the commune, then B joins the guild
//! and the call. In the reverse case B joins the call first while both are in
//! the guild and A follows. Both call rosters must converge on both authors,
//! both keyrings must install the peer at the same epoch with E2EE on, and both
//! sides must play decoded peer audio with real energy.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-daemon --test voice_late_join_e2e -- --test-threads=1
//! ```

#![cfg(unix)]
#![allow(clippy::await_holding_lock)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ovvc_core::{AuthorId, ChannelId, ChannelKind, GuildId};
use ovvc_ipc::{DaemonClient, DaemonEvent, NetworkOptionsDto, Op, Reply, VoiceAudioStats};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-daemon");
const STARTUP: Duration = Duration::from_secs(15);
/// WebRTC setup and the one-second stats cadence need a little room; the
/// convergence repair itself is triggered by the first control frame.
const BUDGET: Duration = Duration::from_secs(60);
const SHUTDOWN: Duration = Duration::from_secs(10);
/// The synthetic source's tone, in hertz. The daemon reads this from the
/// environment only for tests.
const TEST_TONE_HZ: &str = "440";

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct DaemonProcess {
    child: Child,
    /// Kept alive so the data directory, config and socket outlive the process.
    _root: TempDir,
    socket: PathBuf,
}

impl DaemonProcess {
    fn start() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let config_dir = root.path().join("config");
        let data_dir = root.path().join("data");
        let run_dir = root.path().join("run");
        let home = root.path().join("home");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&run_dir).expect("run dir");
        std::fs::create_dir_all(home.join("runtime")).expect("home runtime dir");

        let socket = run_dir.join("daemon.sock");
        let config_file = root.path().join("daemon.toml");
        std::fs::write(
            &config_file,
            format!(
                "data_dir = {:?}\nsocket = {:?}\n\
                 [audio]\n\
                 backend = \"null\"\n\
                 sample_rate = 48000\n\
                 frame_ms = 20\n\
                 profile = \"voice-standard\"\n\
                 [media]\n\
                 transport = \"webrtc\"\n\
                 ice_servers = []\n\
                 force_relay = false\n\
                 allow_loopback_candidates = true\n\
                 allow_lan_candidates = true\n",
                data_dir.display().to_string(),
                socket.display().to_string()
            ),
        )
        .expect("write config");

        let child = Self::spawn(&root);
        Self {
            child,
            _root: root,
            socket,
        }
    }

    /// Spawn a daemon for an existing root, config and data directory.
    ///
    /// `restart` uses this to bring the same identity and store back after a
    /// clean shutdown, which is the state that exposed the stale voice-frame
    /// rejections.
    fn spawn(root: &TempDir) -> Child {
        let config_dir = root.path().join("config");
        let home = root.path().join("home");
        let config_file = root.path().join("daemon.toml");
        Command::new(BIN)
            .arg("--config")
            .arg(&config_file)
            .env("OVVC_AUDIO_TEST_TONE", TEST_TONE_HZ)
            .env("OVVC_CONFIG_DIR", &config_dir)
            .env("OVVC_SYSTEM_CONFIG", config_dir.join("system.toml"))
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local").join("share"))
            .env("XDG_RUNTIME_DIR", home.join("runtime"))
            .env_remove("OVVC_DATA_DIR")
            .env_remove("OVVC_SOCKET")
            .env_remove("OVVC_PROFILE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn ovvc-daemon")
    }

    /// Restart this daemon with the same data directory, identity and config.
    async fn restart(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = std::fs::remove_file(&self.socket);
        self.child = Self::spawn(&self._root);
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    async fn wait_for_socket(&mut self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            if self.socket.exists() {
                return true;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                let mut stderr = String::new();
                if let Some(mut pipe) = self.child.stderr.take() {
                    use std::io::Read;
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("daemon exited before binding its socket: {status}\n{stderr}");
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_exit(&mut self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) if Instant::now() >= deadline => return false,
                Ok(None) => {}
                Err(error) => panic!("waiting for daemon: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
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

async fn connect_client(daemon: &mut DaemonProcess) -> DaemonClient {
    assert!(
        daemon.wait_for_socket(STARTUP).await,
        "daemon did not bind {}",
        daemon.socket().display()
    );
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

async fn create_channel(
    client: &DaemonClient,
    guild_id: GuildId,
    name: &str,
    kind: ChannelKind,
) -> ChannelId {
    expect(
        client
            .call(Op::CreateChannelIn {
                guild_id,
                name: name.to_string(),
                kind,
                parent: None,
            })
            .await,
        |reply| match reply {
            Reply::Channel(channel) => Some(channel.id),
            _ => None,
        },
    )
}

async fn author_id(client: &DaemonClient) -> AuthorId {
    expect(client.call(Op::Identity).await, |reply| match reply {
        Reply::Identity { author_id, .. } => Some(*author_id),
        _ => None,
    })
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

async fn wait_listening(client: &DaemonClient, guild_id: GuildId) -> ovvc_ipc::ConnectionStateDto {
    wait_for(BUDGET, "a listen address", || async {
        let state = connection(client, guild_id).await;
        (!state.listen_addresses.is_empty()).then_some(state)
    })
    .await
}

async fn wait_peers(
    client: &DaemonClient,
    guild_id: GuildId,
    want: usize,
) -> ovvc_ipc::ConnectionStateDto {
    wait_for(BUDGET, "peer connections", || async {
        let state = connection(client, guild_id).await;
        (state.peers.len() >= want).then_some(state)
    })
    .await
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

async fn members(client: &DaemonClient, guild_id: GuildId) -> Vec<AuthorId> {
    expect(
        client.call(Op::ListMembers { guild_id }).await,
        |reply| match reply {
            Reply::Members(members) => Some(members.iter().map(|member| member.author).collect()),
            _ => None,
        },
    )
}

async fn wait_members(client: &DaemonClient, guild_id: GuildId, want: usize) -> Vec<AuthorId> {
    wait_for(BUDGET, "the guild roster", || async {
        let members = members(client, guild_id).await;
        (members.len() >= want).then_some(members)
    })
    .await
}

async fn voice_snapshot(
    client: &DaemonClient,
    guild_id: GuildId,
    channel_id: ChannelId,
) -> Option<ovvc_control::VoiceSnapshot> {
    expect(
        client
            .call(Op::VoiceSnapshot {
                guild_id,
                channel_id,
            })
            .await,
        |reply| match reply {
            Reply::Voice(snapshot) => Some(snapshot.clone()),
            _ => None,
        },
    )
}

async fn voice_join(client: &DaemonClient, guild_id: GuildId, channel_id: ChannelId) {
    expect(
        client
            .call(Op::VoiceJoin {
                guild_id,
                channel_id,
                muted: false,
                deafened: false,
                push_to_talk: false,
            })
            .await,
        |reply| matches!(reply, Reply::Voice(Some(_))).then_some(()),
    );
}

async fn audio_stats(
    client: &DaemonClient,
    guild_id: GuildId,
    channel_id: ChannelId,
) -> Option<VoiceAudioStats> {
    expect(
        client
            .call(Op::VoiceAudio {
                guild_id,
                channel_id,
            })
            .await,
        |reply| match reply {
            Reply::VoiceAudio(stats) => Some(stats.clone()),
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
            panic!("timed out after {budget:?} waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn has_both(snapshot: &ovvc_control::VoiceSnapshot, one: AuthorId, two: AuthorId) -> bool {
    snapshot.members.len() == 2
        && snapshot.members.iter().any(|member| member.author == one)
        && snapshot.members.iter().any(|member| member.author == two)
}

/// Drain the events and return any voice-control rejections.
///
/// A late-join repair is not a failed frame. The repaired divergence must not
/// be surfaced as an error, including when the repair rate limiter suppresses
/// a second announcement inside its cooldown.
fn voice_rejections(events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        match events.try_recv() {
            Ok(DaemonEvent::Error { message, .. })
                if message.contains("outside the call")
                    || message.contains("rejected control frame") =>
            {
                out.push(message);
            }
            Ok(DaemonEvent::Diagnostic(diagnostic))
                if diagnostic.message.contains("outside the call")
                    || diagnostic.message.contains("rejected control frame") =>
            {
                out.push(diagnostic.message);
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
        }
    }
    out
}

async fn shutdown(mut daemon: DaemonProcess, client: &DaemonClient) {
    expect(client.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    assert!(
        daemon.wait_exit(SHUTDOWN).await,
        "daemon did not exit after ShutdownDaemon"
    );
}

/// Run the two-daemon scenario.
///
/// `call_first` names the daemon that joins the voice call before the other
/// joins the call. The guild is always owned by A. With `call_first = "A"`
/// this is the audit's failing path: A calls before B is even in the commune.
async fn run_late_join(call_first: &str) {
    let _guard = serial();
    let mut daemon_a = DaemonProcess::start();
    let mut daemon_b = DaemonProcess::start();
    let client_a = connect_client(&mut daemon_a).await;
    let client_b = connect_client(&mut daemon_b).await;
    let mut events_a = client_a.subscribe();
    let mut events_b = client_b.subscribe();

    // (a) A owns the commune, the voice channel and the first listen address.
    let guild_id = create_guild(&client_a, "late-join-lab").await;
    let voice_channel = create_channel(&client_a, guild_id, "standup", ChannelKind::Voice).await;

    connect(&client_a, guild_id, "/ip4/127.0.0.1/tcp/0", Vec::new()).await;
    let state_a = wait_listening(&client_a, guild_id).await;
    let peer_a = state_a.peer_id.clone().expect("A peer id");
    let addr_a = state_a.listen_addresses[0].clone();

    // (b) The audit's failing order: A is alone in the commune and joins the
    // call before B has joined the guild, so A's VoiceJoin and first epoch key
    // are broadcast to nobody. B's later join must still converge.
    if call_first == "A" {
        voice_join(&client_a, guild_id, voice_channel).await;
    }

    let invite = expect(
        client_a
            .call(Op::CreateInvite {
                guild_id,
                peers: vec![format!("{addr_a}/p2p/{peer_a}")],
                expires_in_secs: Some(3600),
            })
            .await,
        |reply| match reply {
            Reply::Invite(invite) => Some(invite.encode().expect("invite encoding")),
            _ => None,
        },
    );

    // (c) B joins the commune after A's call is already live.
    let joined = expect(
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
    assert_eq!(joined, guild_id, "B joined a different guild");
    wait_listening(&client_b, guild_id).await;
    wait_peers(&client_a, guild_id, 1).await;
    wait_peers(&client_b, guild_id, 1).await;
    let author_a = author_id(&client_a).await;
    let author_b = author_id(&client_b).await;
    assert_ne!(author_a, author_b, "two daemons share an identity");
    wait_members(&client_a, guild_id, 2).await;
    wait_members(&client_b, guild_id, 2).await;

    // (d) The other daemon joins the call. This is the late joiner.
    if call_first == "A" {
        voice_join(&client_b, guild_id, voice_channel).await;
    } else {
        voice_join(&client_b, guild_id, voice_channel).await;
        voice_join(&client_a, guild_id, voice_channel).await;
    }

    // (e) Both directions must converge: every roster carries both authors,
    // not just the joiner's own side.
    for (label, client) in [("A", &client_a), ("B", &client_b)] {
        wait_for(
            BUDGET,
            &format!("{label}'s call roster to contain both authors"),
            || async {
                voice_snapshot(client, guild_id, voice_channel)
                    .await
                    .filter(|snapshot| has_both(snapshot, author_a, author_b))
            },
        )
        .await;
    }

    // (f) Media must flow with E2EE on both sides, at one compatible epoch,
    // with decoded peer audio carrying real energy.
    let mut epochs = Vec::new();
    for (label, client) in [("A", &client_a), ("B", &client_b)] {
        let stats = wait_for(
            BUDGET,
            &format!("{label} to play sealed peer audio"),
            || async {
                let stats = audio_stats(client, guild_id, voice_channel).await?;
                (stats.transport == "webrtc"
                    && stats.e2ee
                    && stats.epoch >= 1
                    && stats.peers == 1
                    && stats.played > 0
                    && stats.peak > 0
                    && stats.rms > 100.0
                    && stats.e2ee_errors == 0)
                    .then_some(stats)
            },
        )
        .await;
        assert_eq!(
            stats.transport, "webrtc",
            "{label} used the wrong transport"
        );
        assert!(
            stats.e2ee,
            "{label} reported plaintext media after late join: {stats:?}"
        );
        assert!(
            stats.played > 0 && stats.peak > 0 && stats.rms > 100.0,
            "{label} never decoded the peer tone after late join: {stats:?}"
        );
        assert_eq!(
            stats.e2ee_errors, 0,
            "{label} dropped sealed media after late join: {stats:?}"
        );
        epochs.push((label, stats.epoch));
        eprintln!(
            "late-join order={call_first} {label}: roster=2 epoch={} e2ee={} peers={} \
             played={} peak={} rms={:.1} e2ee_errors={}",
            stats.epoch,
            stats.e2ee,
            stats.peers,
            stats.played,
            stats.peak,
            stats.rms,
            stats.e2ee_errors
        );
    }
    assert_eq!(
        epochs[0].1, epochs[1].1,
        "the two sides settled on different media epochs: {epochs:?}"
    );

    // The repair is not a failed frame. No side may have surfaced a voice
    // control rejection while converging, including during the resync cooldown.
    let mut rejections = voice_rejections(&mut events_a);
    rejections.extend(voice_rejections(&mut events_b));
    assert!(
        rejections.is_empty(),
        "late join surfaced voice control errors: {rejections:?}"
    );

    shutdown(daemon_b, &client_b).await;
    shutdown(daemon_a, &client_a).await;
}

/// The audit's failing path: A joins the call before B joins the commune.
#[tokio::test(flavor = "multi_thread")]
async fn a_joins_the_call_before_b_joins_the_guild() {
    run_late_join("A").await;
}

/// The mirror order: both are in the commune and B joins the call first.
#[tokio::test(flavor = "multi_thread")]
async fn b_joins_the_call_before_a() {
    run_late_join("B").await;
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind a free port");
    listener.local_addr().expect("local addr").port()
}

/// A restart must not turn a stale call view into user-visible frame errors.
///
/// The restarting daemon has the same identity and store, but its control hub
/// has no live call when it comes back. The peer keeps sending coordinator,
/// key and RTC frames for the call it still holds. The session rejects those
/// as "outside the call" until the rosters converge. That is a repair window,
/// not a protocol failure, and none of it may reach the frontends as a
/// rejected control frame.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_does_not_surface_stale_voice_control_errors() {
    let _guard = serial();
    let mut daemon_a = DaemonProcess::start();
    let mut daemon_b = DaemonProcess::start();
    let client_a = connect_client(&mut daemon_a).await;
    let client_b = connect_client(&mut daemon_b).await;

    let guild_id = create_guild(&client_a, "restart-lab").await;
    let voice_channel = create_channel(&client_a, guild_id, "standup", ChannelKind::Voice).await;
    let listen_a = format!("/ip4/127.0.0.1/tcp/{}", free_port());
    connect(&client_a, guild_id, &listen_a, Vec::new()).await;
    let state_a = wait_listening(&client_a, guild_id).await;
    let peer_a = state_a.peer_id.clone().expect("A peer id");
    let addr_a = state_a.listen_addresses[0].clone();

    let invite = expect(
        client_a
            .call(Op::CreateInvite {
                guild_id,
                peers: vec![format!("{addr_a}/p2p/{peer_a}")],
                expires_in_secs: Some(3600),
            })
            .await,
        |reply| match reply {
            Reply::Invite(invite) => Some(invite.encode().expect("invite encoding")),
            _ => None,
        },
    );
    let joined = expect(
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
    assert_eq!(joined, guild_id, "B joined a different guild");
    wait_listening(&client_b, guild_id).await;
    wait_peers(&client_a, guild_id, 1).await;
    wait_peers(&client_b, guild_id, 1).await;
    let author_a = author_id(&client_a).await;
    let author_b = author_id(&client_b).await;
    wait_members(&client_a, guild_id, 2).await;
    wait_members(&client_b, guild_id, 2).await;

    voice_join(&client_a, guild_id, voice_channel).await;
    voice_join(&client_b, guild_id, voice_channel).await;
    wait_for(BUDGET, "both call rosters", || async {
        let a = voice_snapshot(&client_a, guild_id, voice_channel).await;
        let b = voice_snapshot(&client_b, guild_id, voice_channel).await;
        (a.as_ref()
            .is_some_and(|snapshot| has_both(snapshot, author_a, author_b))
            && b.as_ref()
                .is_some_and(|snapshot| has_both(snapshot, author_a, author_b)))
        .then_some(())
    })
    .await;
    // Let the heartbeat loop run at least once before the restart.
    tokio::time::sleep(Duration::from_millis(1100)).await;

    let mut events_b = client_b.subscribe();
    expect(client_a.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    assert!(
        daemon_a.wait_exit(SHUTDOWN).await,
        "A did not exit before the restart"
    );
    daemon_a.restart().await;
    let client_a2 = connect_client(&mut daemon_a).await;
    let mut events_a2 = client_a2.subscribe();

    // The saved intent normally binds the old address again. If the peer is
    // not back yet, dial B explicitly. Do not rebind the listen address in
    // that case, because the resumed listener may already own it.
    let mut reconnected = false;
    for _ in 0..40 {
        if !connection(&client_a2, guild_id).await.peers.is_empty() {
            reconnected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !reconnected {
        let state_b = wait_listening(&client_b, guild_id).await;
        let peer_b = state_b.peer_id.clone().expect("B peer id");
        let addr_b = state_b.listen_addresses[0].clone();
        expect(
            client_a2
                .call(Op::Connect {
                    guild_id,
                    options: NetworkOptionsDto {
                        listen: Vec::new(),
                        dial: vec![format!("{addr_b}/p2p/{peer_b}")],
                        ..NetworkOptionsDto::default()
                    },
                })
                .await,
            |reply| matches!(reply, Reply::Unit).then_some(()),
        );
    }
    wait_peers(&client_a2, guild_id, 1).await;
    wait_peers(&client_b, guild_id, 1).await;
    wait_members(&client_a2, guild_id, 2).await;
    wait_members(&client_b, guild_id, 2).await;
    voice_join(&client_a2, guild_id, voice_channel).await;

    // The rejection window is during convergence. Collect for a few seconds
    // instead of waiting on audio counters, which are not the subject here.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut rejections = voice_rejections(&mut events_a2);
    rejections.extend(voice_rejections(&mut events_b));
    assert!(
        rejections.is_empty(),
        "a daemon restart surfaced stale voice control errors: {rejections:?}"
    );

    shutdown(daemon_b, &client_b).await;
    shutdown(daemon_a, &client_a2).await;
}
