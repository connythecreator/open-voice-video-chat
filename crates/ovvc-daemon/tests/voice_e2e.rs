//! Voice-call signalling across two real daemon processes.
//!
//! This is the automated stand-in for a two-machine call: two
//! `ovvc-daemon` binaries, two data directories, two Unix sockets, two libp2p
//! nodes on loopback TCP, and one invite. Every step goes through the same IPC
//! ops a GUI or TUI uses, and the control frames travel over the real peer
//! connection rather than being applied in one process.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-daemon --test voice_e2e -- --test-threads=1
//! ```

#![cfg(unix)]
#![allow(clippy::await_holding_lock)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ovvc_control::{CallTopology, ControlSnapshot, SessionId, VoiceSnapshot};
use ovvc_core::{AuthorId, ChannelId, ChannelKind, GuildId};
use ovvc_ipc::{ConnectionStateDto, DaemonClient, ErrorKind, NetworkOptionsDto, Op, Reply};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-daemon");
const STARTUP: Duration = Duration::from_secs(15);
const BUDGET: Duration = Duration::from_secs(30);
const SHUTDOWN: Duration = Duration::from_secs(10);

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct DaemonProcess {
    child: Child,
    /// Kept alive so the data directory and socket outlive the process.
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

async fn wait_listening(client: &DaemonClient, guild_id: GuildId) -> ConnectionStateDto {
    wait_for(BUDGET, "a listen address", || async {
        let state = connection(client, guild_id).await;
        (!state.listen_addresses.is_empty()).then_some(state)
    })
    .await
}

async fn wait_peers(client: &DaemonClient, guild_id: GuildId, want: usize) -> ConnectionStateDto {
    wait_for(BUDGET, "peer connections", || async {
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
) -> Option<VoiceSnapshot> {
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

async fn control_snapshot(
    client: &DaemonClient,
    guild_id: GuildId,
    session_id: SessionId,
) -> ControlSnapshot {
    expect(
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

fn voice_member(snapshot: &VoiceSnapshot, author: AuthorId) -> Option<&ovvc_control::VoiceMember> {
    snapshot.members.iter().find(|member| member.author == author)
}

async fn shutdown(mut daemon: DaemonProcess, client: &DaemonClient) {
    expect(client.call(Op::ShutdownDaemon).await, |reply| {
        matches!(reply, Reply::Unit).then_some(())
    });
    assert!(
        daemon
            .wait_exit(SHUTDOWN)
            .await
            .expect("daemon exits after ShutdownDaemon")
            .success(),
        "daemon did not exit cleanly"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn voice_signalling_crosses_two_real_daemons() {
    let _guard = serial();
    let daemon_a = DaemonProcess::start();
    let daemon_b = DaemonProcess::start();
    let client_a = connect_client(&daemon_a).await;
    let client_b = connect_client(&daemon_b).await;

    // (a) A owns the guild, a voice channel and a text channel.
    let guild_id = create_guild(&client_a, "voice-lab").await;
    let voice_channel = create_channel(&client_a, guild_id, "standup", ChannelKind::Voice).await;
    let text_channel = create_channel(&client_a, guild_id, "text", ChannelKind::Text).await;

    connect(&client_a, guild_id, "/ip4/127.0.0.1/tcp/0", Vec::new()).await;
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

    // (b) B joins the guild over the invite, and both nodes connect and sync.
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
    // Control frames are only applied for a known member, so both rosters
    // must carry the peer before a call can be joined.
    wait_members(&client_a, guild_id, 2).await;
    wait_members(&client_b, guild_id, 2).await;

    // (c) B joins the call; the signed VoiceJoin must reach A over the peer
    // connection, not just mutate B's own hub.
    expect(
        client_b
            .call(Op::VoiceJoin {
                guild_id,
                channel_id: voice_channel,
                muted: false,
                deafened: false,
                push_to_talk: false,
            })
            .await,
        |reply| matches!(reply, Reply::Voice(Some(_))).then_some(()),
    );
    let on_a = wait_for(BUDGET, "B in A's call snapshot", || async {
        let snapshot = voice_snapshot(&client_a, guild_id, voice_channel).await;
        snapshot
            .filter(|snapshot| voice_member(snapshot, author_b).is_some())
    })
    .await;
    assert!(
        voice_member(&on_a, author_a).is_none(),
        "A should not be in the call before it joins"
    );
    let on_b = wait_for(BUDGET, "B in its own call snapshot", || async {
        let snapshot = voice_snapshot(&client_b, guild_id, voice_channel).await;
        snapshot
            .filter(|snapshot| voice_member(snapshot, author_b).is_some())
    })
    .await;
    assert!(voice_member(&on_b, author_b).is_some());

    // (d) A joins too. Both see a two-member mesh with a negotiated codec,
    // and the voice session's election has a master and a backup.
    expect(
        client_a
            .call(Op::VoiceJoin {
                guild_id,
                channel_id: voice_channel,
                muted: false,
                deafened: false,
                push_to_talk: false,
            })
            .await,
        |reply| matches!(reply, Reply::Voice(Some(_))).then_some(()),
    );
    let both_a = wait_for(BUDGET, "both members in A's call", || async {
        let snapshot = voice_snapshot(&client_a, guild_id, voice_channel).await;
        snapshot.filter(|snapshot| snapshot.members.len() == 2)
    })
    .await;
    let both_b = wait_for(BUDGET, "both members in B's call", || async {
        let snapshot = voice_snapshot(&client_b, guild_id, voice_channel).await;
        snapshot.filter(|snapshot| snapshot.members.len() == 2)
    })
    .await;

    for (label, snapshot) in [("A", &both_a), ("B", &both_b)] {
        assert_eq!(
            snapshot.topology,
            CallTopology::Mesh,
            "{label} did not settle on a mesh topology"
        );
        let codec = snapshot
            .codec
            .as_ref()
            .unwrap_or_else(|| panic!("{label} has no negotiated codec"));
        assert_eq!(codec.audio, "opus", "{label} negotiated the wrong codec");
    }

    let session_id = SessionId::for_voice_channel(guild_id, voice_channel);
    let election = wait_for(BUDGET, "a voice master and backup", || async {
        let snapshot = control_snapshot(&client_a, guild_id, session_id).await;
        (snapshot.election.master.is_some() && snapshot.election.backup.is_some())
            .then_some(snapshot)
    })
    .await;
    let master = election
        .election
        .master
        .as_ref()
        .expect("a master")
        .author;
    let backup = election
        .election
        .backup
        .as_ref()
        .expect("a backup")
        .author;
    assert_ne!(
        master, backup,
        "the election must name two distinct coordinators"
    );
    assert_eq!(
        election.voice.len(),
        1,
        "the voice session should carry exactly one call"
    );

    // A quiet call must survive both of the timers that can silently kill it:
    // the 3.5 s election lease and the 20 s member timeout. The daemon's
    // heartbeat tick has to renew both with no user input.
    let term = election.election.term;
    let joined_at = Instant::now();
    tokio::time::sleep(Duration::from_millis(4_500)).await;
    let after_lease = control_snapshot(&client_a, guild_id, session_id).await;
    assert_eq!(
        after_lease
            .election
            .master
            .as_ref()
            .map(|candidate| candidate.author),
        Some(master),
        "the master rotated after lease_ms + takeover_grace_ms"
    );
    assert_eq!(
        after_lease
            .election
            .backup
            .as_ref()
            .map(|candidate| candidate.author),
        Some(backup),
        "the backup changed after lease_ms + takeover_grace_ms"
    );
    assert_eq!(
        after_lease.election.term, term,
        "the election term churned after the lease lapsed"
    );

    let remaining = Duration::from_millis(21_000).saturating_sub(joined_at.elapsed());
    tokio::time::sleep(remaining).await;
    for (label, client) in [("A", &client_a), ("B", &client_b)] {
        let snapshot = voice_snapshot(client, guild_id, voice_channel)
            .await
            .unwrap_or_else(|| panic!("{label} dropped the call after the member timeout"));
        assert!(
            voice_member(&snapshot, author_a).is_some(),
            "{label} timed out A despite its heartbeat"
        );
        assert!(
            voice_member(&snapshot, author_b).is_some(),
            "{label} timed out B despite its heartbeat"
        );
    }

    // (e) A SetVoiceState from B must reach A.
    expect(
        client_b
            .call(Op::SetVoiceState {
                guild_id,
                channel_id: voice_channel,
                muted: true,
                deafened: false,
                push_to_talk: false,
                speaking: false,
                video: false,
                screen_share: false,
            })
            .await,
        |reply| matches!(reply, Reply::Voice(Some(_))).then_some(()),
    );
    wait_for(BUDGET, "B muted in A's call", || async {
        let snapshot = voice_snapshot(&client_a, guild_id, voice_channel).await;
        snapshot
            .filter(|snapshot| voice_member(snapshot, author_b).is_some_and(|member| member.state.muted))
    })
    .await;

    // (f) B leaves; A is left alone in the call.
    expect(
        client_b
            .call(Op::VoiceLeave {
                guild_id,
                channel_id: voice_channel,
            })
            .await,
        |reply| matches!(reply, Reply::Voice(Some(_))).then_some(()),
    );
    wait_for(BUDGET, "B gone from A's call", || async {
        let snapshot = voice_snapshot(&client_a, guild_id, voice_channel).await;
        snapshot.filter(|snapshot| {
            snapshot.members.len() == 1 && voice_member(snapshot, author_a).is_some()
        })
    })
    .await;

    // (g) A leaves; the call is removed and the snapshot is None.
    expect(
        client_a
            .call(Op::VoiceLeave {
                guild_id,
                channel_id: voice_channel,
            })
            .await,
        |reply| matches!(reply, Reply::Voice(None)).then_some(()),
    );
    let empty = voice_snapshot(&client_a, guild_id, voice_channel).await;
    assert!(empty.is_none(), "the call survived its last member");

    // (h) A voice op against a text channel is refused as invalid input.
    let error = client_a
        .call(Op::VoiceJoin {
            guild_id,
            channel_id: text_channel,
            muted: false,
            deafened: false,
            push_to_talk: false,
        })
        .await
        .expect_err("joining a text channel as voice must fail");
    assert_eq!(
        error.kind,
        ErrorKind::InvalidInput,
        "wrong error kind: {error:?}"
    );
    let error = client_a
        .call(Op::VoiceLeave {
            guild_id,
            channel_id: text_channel,
        })
        .await
        .expect_err("leaving a text channel as voice must fail");
    assert_eq!(
        error.kind,
        ErrorKind::InvalidInput,
        "wrong error kind: {error:?}"
    );
    let error = client_a
        .call(Op::SetVoiceState {
            guild_id,
            channel_id: text_channel,
            muted: true,
            deafened: false,
            push_to_talk: false,
            speaking: false,
            video: false,
            screen_share: false,
        })
        .await
        .expect_err("a text channel never carries voice state");
    assert_eq!(
        error.kind,
        ErrorKind::InvalidInput,
        "wrong error kind: {error:?}"
    );
    assert!(
        voice_snapshot(&client_a, guild_id, text_channel)
            .await
            .is_none(),
        "a text channel reported a call snapshot"
    );

    shutdown(daemon_b, &client_b).await;
    shutdown(daemon_a, &client_a).await;
}
