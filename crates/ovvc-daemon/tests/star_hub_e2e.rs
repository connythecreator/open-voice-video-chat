//! The elected media hub across three real daemon processes.
//!
//! Three `ovvc-daemon` binaries, three data directories, three Unix sockets,
//! three libp2p nodes on loopback and one star-scoped commune. Every step uses
//! the same IPC ops a frontend uses, and the control frames travel over the
//! real peer connections.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-daemon --test star_hub_e2e -- --test-threads=1 --nocapture
//! ```

#![cfg(unix)]
#![allow(clippy::await_holding_lock)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ovvc_control::{CallTopology, ControlSnapshot, SessionId, VoiceSnapshot};
use ovvc_core::{AuthorId, CallScope, ChannelId, ChannelKind, GuildId, HubMode};
use ovvc_ipc::{ConnectionStateDto, DaemonClient, NetworkOptionsDto, Op, Reply};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-daemon");
const STARTUP: Duration = Duration::from_secs(15);
const BUDGET: Duration = Duration::from_secs(30);
const FAILOVER_BUDGET: Duration = Duration::from_secs(30);
/// Shorter than the 20 s member timeout, so the reverse failover test proves
/// the positive-liveness hold rather than the claim expiry.
const REVERSE_FAILOVER_BUDGET: Duration = Duration::from_secs(10);
/// The voice member timeout is 20 s, plus the election tick.
const MEMBER_TIMEOUT_BUDGET: Duration = Duration::from_secs(35);
const SHUTDOWN: Duration = Duration::from_secs(10);

/// A commune whose media policy elects a trusted hub and starts a star as soon
/// as the call has three members.
const STAR_CONFIG: &str = r#"version = 2

[policy.media]
call_scope = "star"

[policy.media.hub]
mode = "elected"
trust = "trusted"
allow_mixing = true
allow_video_forwarding = true
min_trust_tier = 0
"#;

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
    fn start_with_config(config: Option<&str>) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("daemon.sock");
        let mut command = Command::new(BIN);
        command
            .arg("--data-dir")
            .arg(dir.path())
            .arg("--socket")
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(config) = config {
            let path = dir.path().join("config.toml");
            fs::write(&path, config).expect("write daemon config");
            command.arg("--config").arg(&path);
        }
        let child = command.spawn().expect("spawn ovvc-daemon");
        Self {
            child,
            _dir: dir,
            socket,
        }
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    /// Stop the daemon abruptly, as a machine failure would. The caller must
    /// already have moved every client that still matters to another daemon.
    fn kill(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
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

async fn create_voice_channel(client: &DaemonClient, guild_id: GuildId, name: &str) -> ChannelId {
    expect(
        client
            .call(Op::CreateChannelIn {
                guild_id,
                name: name.to_string(),
                kind: ChannelKind::Voice,
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

async fn connection(client: &DaemonClient, guild_id: GuildId) -> ConnectionStateDto {
    expect(
        client.call(Op::ConnectionState { guild_id }).await,
        |reply| match reply {
            Reply::Connection(state) => Some(state.clone()),
            _ => None,
        },
    )
}

async fn wait_listening(client: &DaemonClient, guild_id: GuildId) -> ConnectionStateDto {
    wait_for(BUDGET, "a listen address", || async {
        let state = connection(client, guild_id).await;
        (!state.listen_addresses.is_empty()).then_some(state)
    })
    .await
}

async fn wait_peers(client: &DaemonClient, guild_id: GuildId, want: usize) {
    wait_for(BUDGET, "peer connections", || async {
        let state = connection(client, guild_id).await;
        (state.peers.len() >= want).then_some(())
    })
    .await;
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
    channel_id: ChannelId,
) -> ControlSnapshot {
    expect(
        client
            .call(Op::ControlSnapshot {
                guild_id,
                session_id: SessionId::for_voice_channel(guild_id, channel_id),
            })
            .await,
        |reply| match reply {
            Reply::Control(snapshot) => Some(snapshot.clone()),
            _ => None,
        },
    )
}

async fn join_voice(client: &DaemonClient, guild_id: GuildId, channel_id: ChannelId) {
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

/// Wait until all three daemons report the same 3-member star, hub, backup and
/// a published key epoch.
async fn converged_star(
    client_a: &DaemonClient,
    client_b: &DaemonClient,
    client_c: &DaemonClient,
    guild_id: GuildId,
    voice_channel: ChannelId,
) -> Option<(VoiceSnapshot, VoiceSnapshot, VoiceSnapshot)> {
    let snapshots = [
        voice_snapshot(client_a, guild_id, voice_channel).await,
        voice_snapshot(client_b, guild_id, voice_channel).await,
        voice_snapshot(client_c, guild_id, voice_channel).await,
    ];
    let [Some(a), Some(b), Some(c)] = snapshots else {
        return None;
    };
    let hubs = [a.hub, b.hub, c.hub];
    let backups = [a.backup, b.backup, c.backup];
    // All three daemons advertise the same local capacity in this test, so
    // the deterministic result is the lowest author id as hub and the
    // runner-up as backup. Waiting for that, rather than the first agreement,
    // keeps a transient agreement from passing the test before the tie
    // settles.
    let mut ids: Vec<AuthorId> = a.members.iter().map(|member| member.author).collect();
    ids.sort();
    let expected_hub = ids.first().copied();
    let expected_backup = ids.get(1).copied();
    let ready = a.members.len() == 3
        && b.members.len() == 3
        && c.members.len() == 3
        && a.key_epoch >= 1
        && b.key_epoch >= 1
        && c.key_epoch >= 1
        && hubs[0].is_some()
        && hubs[0] == expected_hub
        && hubs[0] == hubs[1]
        && hubs[1] == hubs[2]
        && backups[0].is_some()
        && backups[0] == expected_backup
        && backups[0] == backups[1]
        && backups[1] == backups[2];
    ready.then_some((a, b, c))
}

/// The star snapshot every daemon must agree on.
fn assert_star(snapshot: &VoiceSnapshot, members: &[AuthorId], label: &str) -> AuthorId {
    assert_eq!(
        snapshot.members.len(),
        3,
        "{label} did not see three call members"
    );
    assert_eq!(snapshot.scope, CallScope::Star, "{label} scope");
    assert_eq!(
        snapshot.hub_policy.mode,
        HubMode::Elected,
        "{label} hub mode"
    );
    assert!(
        snapshot.key_epoch >= 1,
        "{label} has not announced a media key epoch"
    );
    let hub = snapshot
        .hub
        .unwrap_or_else(|| panic!("{label} has no elected media hub"));
    let master = snapshot
        .master
        .unwrap_or_else(|| panic!("{label} has no elected master"));
    assert_eq!(hub, master, "{label} hub and master diverged");
    let backup = snapshot
        .backup
        .unwrap_or_else(|| panic!("{label} has no backup"));
    assert_ne!(hub, backup, "{label} chose the same master and backup");
    assert!(members.contains(&hub), "{label} hub is not a call member");
    assert!(
        members.contains(&backup),
        "{label} backup is not a call member"
    );
    let CallTopology::Star {
        hub: topology_hub,
        leaves,
    } = &snapshot.topology
    else {
        panic!("{label} did not report a flat star");
    };
    assert_eq!(*topology_hub, hub, "{label} topology hub is not the hub");
    assert_eq!(leaves.len(), 2, "{label} star does not have two leaves");
    for leaf in leaves {
        assert_ne!(*leaf, *topology_hub, "{label} hub is listed as its own leaf");
        assert!(members.contains(leaf), "{label} leaf is not a member");
    }
    hub
}

#[tokio::test(flavor = "multi_thread")]
async fn three_daemons_elect_a_star_hub_and_backup() {
    let _guard = serial();
    let daemon_a = DaemonProcess::start_with_config(Some(STAR_CONFIG));
    let daemon_b = DaemonProcess::start_with_config(Some(STAR_CONFIG));
    let daemon_c = DaemonProcess::start_with_config(Some(STAR_CONFIG));
    let client_a = connect_client(&daemon_a).await;
    let client_b = connect_client(&daemon_b).await;
    let client_c = connect_client(&daemon_c).await;

    // (a) A owns the commune and its voice channel.
    let guild_id = create_guild(&client_a, "star-lab").await;
    let voice_channel = create_voice_channel(&client_a, guild_id, "standup").await;

    connect(&client_a, guild_id, "/ip4/127.0.0.1/tcp/0", Vec::new()).await;
    let state_a = wait_listening(&client_a, guild_id).await;
    let peer_a = state_a.peer_id.clone().expect("A peer id");
    let addr_a = state_a.listen_addresses[0].clone();

    let invite_a = expect(
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

    // (b) B joins A over the first invite.
    expect(
        client_b
            .call(Op::JoinGuild {
                invite: invite_a,
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
    let state_b = wait_listening(&client_b, guild_id).await;
    let peer_b = state_b.peer_id.clone().expect("B peer id");
    let addr_b = state_b.listen_addresses[0].clone();
    wait_peers(&client_a, guild_id, 1).await;
    wait_peers(&client_b, guild_id, 1).await;
    wait_members(&client_a, guild_id, 2).await;

    // (c) A shares a second invite naming both existing peers; C dials both.
    let invite_ab = expect(
        client_a
            .call(Op::CreateInvite {
                guild_id,
                peers: vec![
                    format!("{addr_a}/p2p/{peer_a}"),
                    format!("{addr_b}/p2p/{peer_b}"),
                ],
                expires_in_secs: Some(3600),
            })
            .await,
        |reply| match reply {
            Reply::Invite(invite) => Some(invite.encode().expect("invite encoding")),
            _ => None,
        },
    );
    expect(
        client_c
            .call(Op::JoinGuild {
                invite: invite_ab,
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
    wait_peers(&client_a, guild_id, 2).await;
    wait_peers(&client_b, guild_id, 2).await;
    wait_peers(&client_c, guild_id, 2).await;
    let roster = wait_members(&client_a, guild_id, 3).await;
    wait_members(&client_b, guild_id, 3).await;
    wait_members(&client_c, guild_id, 3).await;
    let author_a = author_id(&client_a).await;
    let author_b = author_id(&client_b).await;
    let author_c = author_id(&client_c).await;
    assert!(
        [author_a, author_b, author_c]
            .iter()
            .all(|author| roster.contains(author)),
        "the roster does not carry all three identities"
    );

    // (d) All three join the same channel. The VoiceJoin and Claim frames
    // cross the real peer connections.
    join_voice(&client_a, guild_id, voice_channel).await;
    join_voice(&client_b, guild_id, voice_channel).await;
    join_voice(&client_c, guild_id, voice_channel).await;

    // (e) Wait for every daemon to converge on the same 3-member star, a hub,
    // a distinct backup and a published key epoch.
    let call_members = [author_a, author_b, author_c];
    let converged = wait_for(BUDGET, "three daemons to agree on the star hub", || {
        converged_star(&client_a, &client_b, &client_c, guild_id, voice_channel)
    })
    .await;

    let hub_a = assert_star(&converged.0, &call_members, "A");
    let hub_b = assert_star(&converged.1, &call_members, "B");
    let hub_c = assert_star(&converged.2, &call_members, "C");
    assert_eq!(hub_a, hub_b, "A and B elected different hubs");
    assert_eq!(hub_b, hub_c, "B and C elected different hubs");

    // The ranking is deterministic: with identical advertised capacity the
    // lowest author id wins and the runner-up is the backup.
    let mut sorted = call_members;
    sorted.sort();
    assert_eq!(hub_a, sorted[0], "the lowest author id must win the tie");
    let backup_a = converged.0.backup.expect("a backup");
    assert_eq!(backup_a, sorted[1], "the runner-up must be the backup");
    let backup_b = converged.1.backup.expect("B backup");
    let backup_c = converged.2.backup.expect("C backup");
    assert_eq!(backup_a, backup_b);
    assert_eq!(backup_b, backup_c);

    shutdown(daemon_c, &client_c).await;
    shutdown(daemon_b, &client_b).await;
    shutdown(daemon_a, &client_a).await;
}

struct StarLab {
    daemons: [DaemonProcess; 3],
    clients: [DaemonClient; 3],
    guild_id: GuildId,
    channel_id: ChannelId,
    authors: [AuthorId; 3],
}

/// Start three daemons, join one commune and one voice channel, and return
/// the three identities in process order A, B, C.
async fn start_star_lab() -> StarLab {
    let daemon_a = DaemonProcess::start_with_config(Some(STAR_CONFIG));
    let daemon_b = DaemonProcess::start_with_config(Some(STAR_CONFIG));
    let daemon_c = DaemonProcess::start_with_config(Some(STAR_CONFIG));
    let client_a = connect_client(&daemon_a).await;
    let client_b = connect_client(&daemon_b).await;
    let client_c = connect_client(&daemon_c).await;

    let guild_id = create_guild(&client_a, "star-failover").await;
    let channel_id = create_voice_channel(&client_a, guild_id, "standup").await;

    connect(&client_a, guild_id, "/ip4/127.0.0.1/tcp/0", Vec::new()).await;
    let state_a = wait_listening(&client_a, guild_id).await;
    let peer_a = state_a.peer_id.clone().expect("A peer id");
    let addr_a = state_a.listen_addresses[0].clone();
    let invite_a = expect(
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
    expect(
        client_b
            .call(Op::JoinGuild {
                invite: invite_a,
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
    let state_b = wait_listening(&client_b, guild_id).await;
    let peer_b = state_b.peer_id.clone().expect("B peer id");
    let addr_b = state_b.listen_addresses[0].clone();
    wait_peers(&client_a, guild_id, 1).await;
    wait_peers(&client_b, guild_id, 1).await;
    wait_members(&client_a, guild_id, 2).await;

    let invite_ab = expect(
        client_a
            .call(Op::CreateInvite {
                guild_id,
                peers: vec![
                    format!("{addr_a}/p2p/{peer_a}"),
                    format!("{addr_b}/p2p/{peer_b}"),
                ],
                expires_in_secs: Some(3600),
            })
            .await,
        |reply| match reply {
            Reply::Invite(invite) => Some(invite.encode().expect("invite encoding")),
            _ => None,
        },
    );
    expect(
        client_c
            .call(Op::JoinGuild {
                invite: invite_ab,
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
    wait_peers(&client_a, guild_id, 2).await;
    wait_peers(&client_b, guild_id, 2).await;
    wait_peers(&client_c, guild_id, 2).await;
    wait_members(&client_a, guild_id, 3).await;
    wait_members(&client_b, guild_id, 3).await;
    wait_members(&client_c, guild_id, 3).await;

    let authors = [
        author_id(&client_a).await,
        author_id(&client_b).await,
        author_id(&client_c).await,
    ];
    join_voice(&client_a, guild_id, channel_id).await;
    join_voice(&client_b, guild_id, channel_id).await;
    join_voice(&client_c, guild_id, channel_id).await;

    StarLab {
        daemons: [daemon_a, daemon_b, daemon_c],
        clients: [client_a, client_b, client_c],
        guild_id,
        channel_id,
        authors,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn killing_the_master_then_the_backup_never_elects_a_dead_peer() {
    let _guard = serial();
    let mut lab = start_star_lab().await;
    let guild_id = lab.guild_id;
    let channel_id = lab.channel_id;

    let converged = wait_for(BUDGET, "three daemons to agree on the star hub", || {
        converged_star(
            &lab.clients[0],
            &lab.clients[1],
            &lab.clients[2],
            guild_id,
            channel_id,
        )
    })
    .await;
    let hub = converged.0.hub.expect("a hub");
    let backup = converged.0.backup.expect("a backup");
    let third = lab
        .authors
        .iter()
        .copied()
        .find(|author| *author != hub && *author != backup)
        .expect("the third member");
    let hub_index = lab.authors.iter().position(|author| *author == hub).unwrap();
    let backup_index = lab
        .authors
        .iter()
        .position(|author| *author == backup)
        .unwrap();
    let third_index = lab
        .authors
        .iter()
        .position(|author| *author == third)
        .unwrap();

    // (a) Kill A. B must take over, A must not become the backup, and A must
    // disappear from the live candidate list at takeover.
    lab.daemons[hub_index].kill();
    let live = [backup_index, third_index];
    let (snapshot_b, snapshot_c) = wait_for(FAILOVER_BUDGET, "the backup to take over", || {
        let client_b = &lab.clients[live[0]];
        let client_c = &lab.clients[live[1]];
        async move {
            let snapshot_b = voice_snapshot(client_b, guild_id, channel_id).await?;
            let snapshot_c = voice_snapshot(client_c, guild_id, channel_id).await?;
            let control_b = control_snapshot(client_b, guild_id, channel_id).await;
            let control_c = control_snapshot(client_c, guild_id, channel_id).await;
            let candidates = |control: &ControlSnapshot| -> Vec<AuthorId> {
                control
                    .election
                    .candidates
                    .iter()
                    .map(|candidate| candidate.author)
                    .collect()
            };
            let candidates_b = candidates(&control_b);
            let candidates_c = candidates(&control_c);
            let ready = snapshot_b.master == Some(backup)
                && snapshot_c.master == Some(backup)
                && snapshot_b.backup == Some(third)
                && snapshot_c.backup == Some(third)
                && snapshot_b.hub == Some(backup)
                && snapshot_c.hub == Some(backup)
                && !candidates_b.contains(&hub)
                && !candidates_c.contains(&hub);
            ready.then_some((snapshot_b, snapshot_c))
        }
    })
    .await;
    assert_eq!(
        snapshot_b.master,
        Some(backup),
        "the old backup did not take over"
    );
    assert_eq!(
        snapshot_b.backup,
        Some(third),
        "the third member did not become the backup"
    );
    assert_ne!(
        snapshot_b.backup,
        Some(hub),
        "the dead master became the backup"
    );
    assert_ne!(snapshot_c.backup, Some(hub));
    assert_eq!(snapshot_b.hub, Some(backup));
    assert_eq!(snapshot_c.hub, Some(backup));

    // (b) Kill B, now the master. C must become master and must not elect the
    // dead A as master or backup.
    lab.daemons[backup_index].kill();
    let survivor = &lab.clients[third_index];
    let snapshot = wait_for(FAILOVER_BUDGET, "the last member to take over", || async move {
        let snapshot = voice_snapshot(survivor, guild_id, channel_id).await?;
        let control = control_snapshot(survivor, guild_id, channel_id).await;
        let candidates: Vec<AuthorId> = control
            .election
            .candidates
            .iter()
            .map(|candidate| candidate.author)
            .collect();
        (snapshot.master == Some(third)
            && snapshot.hub == Some(third)
            && snapshot.backup.is_none()
            && candidates == vec![third])
            .then_some(snapshot)
    })
    .await;
    assert_eq!(snapshot.master, Some(third));
    assert_eq!(snapshot.hub, Some(third));
    assert!(snapshot.backup.is_none());

    // (c) The two dead members are still in the voice roster until the 20 s
    // member timeout. Wait for that timeout and prove the survivor is alone.
    let remaining = wait_for(
        MEMBER_TIMEOUT_BUDGET,
        "the dead members to time out of the call",
        || async move {
            let snapshot = voice_snapshot(survivor, guild_id, channel_id).await?;
            (snapshot.members.len() == 1 && snapshot.members[0].author == third)
                .then_some(snapshot)
        },
    )
    .await;
    assert_eq!(remaining.members.len(), 1);
    assert_eq!(remaining.members[0].author, third);
    assert_eq!(remaining.master, Some(third));
    assert!(remaining.backup.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn killing_the_backup_then_the_master_does_not_promote_the_dead_backup() {
    let _guard = serial();
    let mut lab = start_star_lab().await;
    let guild_id = lab.guild_id;
    let channel_id = lab.channel_id;

    let converged = wait_for(BUDGET, "three daemons to agree on the star hub", || {
        converged_star(
            &lab.clients[0],
            &lab.clients[1],
            &lab.clients[2],
            guild_id,
            channel_id,
        )
    })
    .await;
    let hub = converged.0.hub.expect("a hub");
    let backup = converged.0.backup.expect("a backup");
    let third = lab
        .authors
        .iter()
        .copied()
        .find(|author| *author != hub && *author != backup)
        .expect("the third member");
    let hub_index = lab.authors.iter().position(|author| *author == hub).unwrap();
    let backup_index = lab
        .authors
        .iter()
        .position(|author| *author == backup)
        .unwrap();
    let third_index = lab
        .authors
        .iter()
        .position(|author| *author == third)
        .unwrap();

    // Kill the backup first, then the master inside the 20 s claim window.
    // The backup's claim stays live for the roster but must be too stale to
    // promote, so the survivor becomes master and the dead backup is never
    // elected.
    lab.daemons[backup_index].kill();
    lab.daemons[hub_index].kill();

    let survivor = &lab.clients[third_index];
    let deadline = Instant::now() + REVERSE_FAILOVER_BUDGET;
    let mut saw_backup_as_master = false;
    let snapshot = loop {
        let snapshot = voice_snapshot(survivor, guild_id, channel_id)
            .await
            .expect("the survivor still answers VoiceSnapshot");
        if snapshot.master == Some(backup) {
            saw_backup_as_master = true;
        }
        if snapshot.master == Some(third)
            && snapshot.hub == Some(third)
            && snapshot.backup != Some(backup)
            && snapshot.backup != Some(hub)
        {
            break snapshot;
        }
        if Instant::now() >= deadline {
            panic!(
                "the survivor did not take over inside {REVERSE_FAILOVER_BUDGET:?}: \
                 master={:?} hub={:?} backup={:?}",
                snapshot.master, snapshot.hub, snapshot.backup
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    assert!(
        !saw_backup_as_master,
        "the dead backup was master at some point during the takeover"
    );
    assert_eq!(
        snapshot.master,
        Some(third),
        "the survivor did not become master"
    );
    assert_ne!(
        snapshot.master,
        Some(backup),
        "the dead backup was promoted to master"
    );
    assert_eq!(snapshot.hub, Some(third));
    assert_ne!(snapshot.backup, Some(backup));
    assert_ne!(snapshot.backup, Some(hub));
}
