//! Star media across three real daemon processes.
//!
//! Three `ovvc-daemon` binaries, three data directories, three Unix sockets,
//! three libp2p nodes on loopback and one star-scoped commune. The daemons run
//! the null audio backend with a synthetic 440 Hz tone, so a decoded sample
//! proves the Opus packet crossed the real peer connection.
//!
//! Each assertion is written against the public IPC contract
//! (`VoiceSnapshot`, `ControlSnapshot` and `VoiceAudioStats`), so a failure
//! names the gap in the media plane instead of a private implementation
//! detail.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-daemon --test star_media_e2e -- --test-threads=1 --nocapture
//! ```

#![cfg(unix)]
#![allow(clippy::await_holding_lock)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ovvc_control::{
    CallTopology, ControlSnapshot, Priority, SessionId, VoiceAudioStats, VoiceSnapshot,
};
use ovvc_core::{AuthorId, CallScope, ChannelId, ChannelKind, GuildId, HubMode};
use ovvc_ipc::{ConnectionStateDto, DaemonClient, NetworkOptionsDto, Op, Reply};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-daemon");
const STARTUP: Duration = Duration::from_secs(15);
const BUDGET: Duration = Duration::from_secs(45);
/// Shorter than the 20 s voice member timeout, so an epoch change observed
/// inside this budget must come from the takeover rather than from the dead
/// member timing out of the roster.
const TAKEOVER_BUDGET: Duration = Duration::from_secs(15);
const SHUTDOWN: Duration = Duration::from_secs(10);
/// The synthetic source's tone, in hertz. The daemon reads this from the
/// environment only for tests.
const TEST_TONE_HZ: &str = "440";
/// The tone's RMS is around 8,500 in signed 16-bit units. A decoded peer
/// sample well above this floor cannot be a silent frame.
const AUDIBLE_RMS: f32 = 100.0;
/// A silent downlink may decode a few codec-noise samples. Anything at or
/// below this is silence rather than a leaked stream.
const SILENT_RMS: f32 = 1.0;

/// A commune whose media policy elects a trusted hub and starts a star as soon
/// as the call has three members. The audio backend is the hardware-free null
/// backend with the test tone from `OVVC_AUDIO_TEST_TONE`, and media uses the
/// libp2p transport so the control and media frames share the real peer
/// connection the test already drives.
const STAR_AUDIO_CONFIG: &str = r#"version = 2

[audio]
backend = "null"
sample_rate = 48000
frame_ms = 20
profile = "voice-standard"

[media]
transport = "libp2p"

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
    /// Kept alive so the data directory, config and socket outlive the
    /// process.
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
        fs::create_dir_all(&config_dir).expect("config dir");
        fs::create_dir_all(&data_dir).expect("data dir");
        fs::create_dir_all(&run_dir).expect("run dir");
        fs::create_dir_all(home.join("runtime")).expect("home runtime dir");

        let socket = run_dir.join("daemon.sock");
        let config_file = root.path().join("daemon.toml");
        fs::write(&config_file, STAR_AUDIO_CONFIG).expect("write config");

        // Isolate discovery completely: the explicit file supplies the
        // policy, and HOME/XDG point into the temp root so a regression cannot
        // reach the developer's own configuration.
        let child = Command::new(BIN)
            .arg("--config")
            .arg(&config_file)
            .arg("--data-dir")
            .arg(&data_dir)
            .arg("--socket")
            .arg(&socket)
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
            .expect("spawn ovvc-daemon");
        Self {
            child,
            _root: root,
            socket,
        }
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    /// Stop the daemon abruptly, as a machine failure would.
    fn kill(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
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

async fn voice_leave(client: &DaemonClient, guild_id: GuildId, channel_id: ChannelId) {
    expect(
        client
            .call(Op::VoiceLeave {
                guild_id,
                channel_id,
            })
            .await,
        |reply| matches!(reply, Reply::Voice(_)).then_some(()),
    );
}

async fn set_voice_state(
    client: &DaemonClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    muted: bool,
    deafened: bool,
    push_to_talk: bool,
) {
    expect(
        client
            .call(Op::SetVoiceState {
                guild_id,
                channel_id,
                muted,
                deafened,
                push_to_talk,
                speaking: false,
                video: false,
                screen_share: false,
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

/// Wait until all three daemons report the same 3-member star, hub, backup and
/// a published key epoch. The tie-break is deterministic: the lowest author id
/// is the hub and the runner-up is the backup.
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
        // The media path needs one epoch everywhere, and the test asserts
        // that below. Waiting for it here keeps a convergence race from
        // turning into a confusing failure after the audio checks.
        && a.key_epoch == b.key_epoch
        && b.key_epoch == c.key_epoch
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
    assert_eq!(
        snapshot.master,
        Some(hub),
        "{label} hub and master diverged"
    );
    assert_ne!(
        snapshot.backup,
        Some(hub),
        "{label} chose the hub as its own backup"
    );
    assert!(members.contains(&hub), "{label} hub is not a call member");
    // Use the topology accessors rather than a variant shape. The intended
    // flat-star type and the interim forwarding type both answer these, so
    // the assertion states the leaf-to-hub rule without pinning the enum.
    let topology = &snapshot.topology;
    assert!(topology.is_star(), "{label} did not report a star");
    assert_eq!(topology.hub(), Some(hub), "{label} topology hub");
    let mut leaves: Vec<AuthorId> = members
        .iter()
        .copied()
        .filter(|author| *author != hub)
        .collect();
    leaves.sort();
    let mut hub_peers = topology.neighbours(hub);
    hub_peers.sort();
    assert_eq!(
        hub_peers, leaves,
        "{label} hub does not connect to exactly its leaves"
    );
    for leaf in &leaves {
        assert_eq!(
            topology.parent_of(*leaf),
            Some(hub),
            "{label} leaf {leaf:?} is not parented by the hub"
        );
        assert_eq!(
            topology.neighbours(*leaf),
            vec![hub],
            "{label} leaf {leaf:?} has a media peer other than the hub"
        );
    }
    hub
}

struct StarLab {
    /// `None` once a daemon has been killed or shut down, so a test can stop
    /// the survivors without starting a replacement process.
    daemons: [Option<DaemonProcess>; 3],
    clients: [DaemonClient; 3],
    guild_id: GuildId,
    channel_id: ChannelId,
    authors: [AuthorId; 3],
}

impl StarLab {
    fn index_of(&self, author: AuthorId) -> usize {
        self.authors
            .iter()
            .position(|candidate| *candidate == author)
            .unwrap_or_else(|| panic!("{author:?} is not one of the lab members"))
    }

    fn take_daemon(&mut self, index: usize) -> Option<DaemonProcess> {
        self.daemons[index].take()
    }

    /// Stop the survivors cleanly. A daemon already killed has no process
    /// left to shut down.
    async fn shutdown_survivors(&mut self) {
        for index in 0..3 {
            if let Some(daemon) = self.take_daemon(index) {
                shutdown(daemon, &self.clients[index]).await;
            }
        }
    }
}

/// Start three daemons, join one commune and one voice channel, and return the
/// three identities in process order A, B, C.
async fn start_star_lab() -> StarLab {
    let mut daemon_a = DaemonProcess::start();
    let mut daemon_b = DaemonProcess::start();
    let mut daemon_c = DaemonProcess::start();
    let client_a = connect_client(&mut daemon_a).await;
    let client_b = connect_client(&mut daemon_b).await;
    let client_c = connect_client(&mut daemon_c).await;

    let guild_id = create_guild(&client_a, "star-media").await;
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
        daemons: [Some(daemon_a), Some(daemon_b), Some(daemon_c)],
        clients: [client_a, client_b, client_c],
        guild_id,
        channel_id,
        authors,
    }
}

async fn wait_star(lab: &StarLab) -> (VoiceSnapshot, VoiceSnapshot, VoiceSnapshot) {
    let [client_a, client_b, client_c] = &lab.clients;
    wait_for(BUDGET, "three daemons to agree on the star hub", || {
        converged_star(client_a, client_b, client_c, lab.guild_id, lab.channel_id)
    })
    .await
}

/// Wait until all three daemons agree on one key epoch and it stops moving.
/// Joins and roster changes rotate the epoch, so a takeover test needs a
/// stable starting value for `key_epoch > before` to mean anything.
async fn wait_epoch_stable(lab: &StarLab) -> u32 {
    let deadline = Instant::now() + BUDGET;
    let mut last = None;
    let mut since = Instant::now();
    loop {
        let snapshots = [
            voice_snapshot(&lab.clients[0], lab.guild_id, lab.channel_id).await,
            voice_snapshot(&lab.clients[1], lab.guild_id, lab.channel_id).await,
            voice_snapshot(&lab.clients[2], lab.guild_id, lab.channel_id).await,
        ];
        let epoch = match snapshots {
            [Some(a), Some(b), Some(c)]
                if a.key_epoch == b.key_epoch && b.key_epoch == c.key_epoch =>
            {
                Some(a.key_epoch)
            }
            _ => None,
        };
        match epoch {
            Some(epoch) if Some(epoch) == last => {
                if since.elapsed() >= Duration::from_secs(2) {
                    return epoch;
                }
            }
            Some(epoch) => {
                last = Some(epoch);
                since = Instant::now();
            }
            None => {
                last = None;
                since = Instant::now();
            }
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for the three daemons to settle on one key epoch");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
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

/// Wait for the local engine to capture, encode and seal media under the
/// call's epoch.
async fn wait_audio_ready(
    client: &DaemonClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    label: &str,
) -> VoiceAudioStats {
    let deadline = Instant::now() + BUDGET;
    let mut last = None;
    loop {
        if let Some(stats) = audio_stats(client, guild_id, channel_id).await {
            let ready = stats.backend == "null"
                && stats.transport == "libp2p"
                && stats.captured > 0
                && stats.packets > 0
                && stats.e2ee
                && stats.epoch >= 1;
            if ready {
                return stats;
            }
            last = Some(stats);
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out after {BUDGET:?} waiting for {label}'s engine; last sample: {last:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait until this member is playing decoded remote audio with real energy.
async fn wait_hears_audio(
    client: &DaemonClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    label: &str,
) -> VoiceAudioStats {
    wait_for(
        BUDGET,
        &format!("{label} to play decoded peer audio"),
        || async {
            let stats = audio_stats(client, guild_id, channel_id).await?;
            (stats.played > 0 && stats.peak > 0 && stats.rms > AUDIBLE_RMS).then_some(stats)
        },
    )
    .await
}

/// Wait until the member's playback carries no energy. A silent downlink may
/// still decode frames, so `played` is not used as the silence signal.
async fn wait_silent(
    client: &DaemonClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    label: &str,
) -> VoiceAudioStats {
    wait_for(
        BUDGET,
        &format!("{label}'s playback to go silent"),
        || async {
            let stats = audio_stats(client, guild_id, channel_id).await?;
            (stats.peak == 0 && stats.rms <= SILENT_RMS).then_some(stats)
        },
    )
    .await
}

/// Set the mute gate for every member so exactly `speaker` is unmuted.
async fn only_unmuted(lab: &StarLab, speaker: AuthorId) {
    for (index, author) in lab.authors.iter().enumerate() {
        set_voice_state(
            &lab.clients[index],
            lab.guild_id,
            lab.channel_id,
            *author != speaker,
            false,
            false,
        )
        .await;
    }
}

/// Assert that only `speaker` is heard: the speaker does not hear its own
/// stream, and every other member plays the speaker's audio.
async fn assert_only_speaker_is_heard(lab: &StarLab, speaker: AuthorId, label: &str) {
    only_unmuted(lab, speaker).await;

    let speaker_index = lab.index_of(speaker);
    let speaker_stats = wait_silent(
        &lab.clients[speaker_index],
        lab.guild_id,
        lab.channel_id,
        &format!("{label} speaking to itself"),
    )
    .await;
    assert!(
        speaker_stats.peak == 0 && speaker_stats.rms <= SILENT_RMS,
        "{label} heard its own stream through the hub: {speaker_stats:?}"
    );

    for (index, author) in lab.authors.iter().enumerate() {
        if *author == speaker {
            continue;
        }
        let listener = wait_hears_audio(
            &lab.clients[index],
            lab.guild_id,
            lab.channel_id,
            &format!("{label}'s listener"),
        )
        .await;
        assert!(
            listener.peak > 0 && listener.rms > AUDIBLE_RMS,
            "a listener for {label} did not hear the speaker: {listener:?}"
        );
    }

    // Hold the state for another stats window. A mixer that alternates
    // between the correct and the leaked stream must not pass on one sample.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let speaker_stats = audio_stats(&lab.clients[speaker_index], lab.guild_id, lab.channel_id)
        .await
        .expect("the speaker still has audio stats");
    assert!(
        speaker_stats.rms <= SILENT_RMS,
        "{label} heard its own stream after the hold: {speaker_stats:?}"
    );
    for (index, author) in lab.authors.iter().enumerate() {
        if *author == speaker {
            continue;
        }
        let listener = audio_stats(&lab.clients[index], lab.guild_id, lab.channel_id)
            .await
            .expect("a listener still has audio stats");
        assert!(
            listener.rms > AUDIBLE_RMS,
            "a listener for {label} stopped hearing the speaker: {listener:?}"
        );
    }
}

/// The star is flat: the hub has one media stream per leaf, and every leaf has
/// exactly one stream, the hub's mixed downlink. The engine's `peers` counter
/// is the number of decoded remote authors, which is the observable media
/// recipient count on the libp2p path.
#[tokio::test(flavor = "multi_thread")]
async fn three_daemons_route_star_media_leaf_to_hub_only() {
    let _guard = serial();
    let mut lab = start_star_lab().await;
    let (snapshot_a, snapshot_b, snapshot_c) = wait_star(&lab).await;
    // A key published during the last roster rotation can still be in flight
    // when the topology first agrees. Hold for a settled epoch before the
    // audio checks compare the epoch they started and finished on.
    wait_epoch_stable(&lab).await;
    let hub = assert_star(&snapshot_a, &lab.authors, "A");
    assert_eq!(
        assert_star(&snapshot_b, &lab.authors, "B"),
        hub,
        "A and B elected different hubs"
    );
    assert_eq!(
        assert_star(&snapshot_c, &lab.authors, "C"),
        hub,
        "B and C elected different hubs"
    );

    let mut stats = Vec::new();
    for (index, label) in ["A", "B", "C"].into_iter().enumerate() {
        let ready =
            wait_audio_ready(&lab.clients[index], lab.guild_id, lab.channel_id, label).await;
        let heard =
            wait_hears_audio(&lab.clients[index], lab.guild_id, lab.channel_id, label).await;
        assert_eq!(
            ready.epoch, heard.epoch,
            "{label} listened on another epoch"
        );
        stats.push(heard);
    }

    let hub_index = lab.index_of(hub);
    let hub_stats = &stats[hub_index];
    assert_eq!(
        hub_stats.peers, 2,
        "the hub must decode one stream per leaf, got {hub_stats:?}"
    );
    for (index, author) in lab.authors.iter().enumerate() {
        if *author == hub {
            continue;
        }
        assert_eq!(
            stats[index].peers, 1,
            "leaf {author:?} must decode only the hub's mixed downlink, got {:?}",
            stats[index]
        );
    }

    // The hub hears both leaves and each leaf hears the hub's mix. This is
    // the audible half of the same claim the recipient counts make.
    assert!(
        hub_stats.played > 0 && hub_stats.peak > 0 && hub_stats.rms > AUDIBLE_RMS,
        "the hub never played leaf audio: {hub_stats:?}"
    );
    for (index, author) in lab.authors.iter().enumerate() {
        if *author == hub {
            continue;
        }
        assert!(
            stats[index].played > 0 && stats[index].peak > 0,
            "leaf {author:?} never played the hub mix: {:?}",
            stats[index]
        );
    }

    // The topology and epoch agree on every daemon, so the media recipient
    // count is not a local accident.
    assert_eq!(snapshot_a.key_epoch, snapshot_b.key_epoch);
    assert_eq!(snapshot_b.key_epoch, snapshot_c.key_epoch);

    for client in &lab.clients {
        voice_leave(client, lab.guild_id, lab.channel_id).await;
    }
    lab.shutdown_survivors().await;
}

/// Every listener has its own mix. A participant must not hear its own voice,
/// and the hub must not play its own microphone, while every other member
/// hears the one active speaker.
#[tokio::test(flavor = "multi_thread")]
async fn the_hub_mix_excludes_the_listener_and_the_hub_microphone() {
    let _guard = serial();
    let mut lab = start_star_lab().await;
    let (snapshot_a, _, _) = wait_star(&lab).await;
    let hub = assert_star(&snapshot_a, &lab.authors, "A");
    for (index, label) in ["A", "B", "C"].into_iter().enumerate() {
        wait_audio_ready(&lab.clients[index], lab.guild_id, lab.channel_id, label).await;
    }
    // Let the initial all-unmuted mixes reach the speakers before narrowing
    // the state, so a stale jitter buffer cannot be mistaken for an echo.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let leaves: Vec<AuthorId> = lab
        .authors
        .iter()
        .copied()
        .filter(|author| *author != hub)
        .collect();
    for (leaf_index, leaf) in leaves.iter().enumerate() {
        assert_only_speaker_is_heard(&lab, *leaf, &format!("leaf {}", leaf_index + 1)).await;
    }
    assert_only_speaker_is_heard(&lab, hub, "hub").await;

    for client in &lab.clients {
        voice_leave(client, lab.guild_id, lab.channel_id).await;
    }
    lab.shutdown_survivors().await;
}

/// Killing the hub promotes the backup, advances the media key epoch before
/// the dead member times out of the roster, and resumes star audio through the
/// new hub.
#[tokio::test(flavor = "multi_thread")]
async fn killing_the_hub_rekeys_the_backup_and_resumes_audio() {
    let _guard = serial();
    let mut lab = start_star_lab().await;
    let (snapshot_a, _, _) = wait_star(&lab).await;
    let hub = assert_star(&snapshot_a, &lab.authors, "A");
    let backup = snapshot_a.backup.expect("a backup");
    let third = lab
        .authors
        .iter()
        .copied()
        .find(|author| *author != hub && *author != backup)
        .expect("the third member");
    let old_epoch = wait_epoch_stable(&lab).await;
    for (index, label) in ["A", "B", "C"].into_iter().enumerate() {
        wait_audio_ready(&lab.clients[index], lab.guild_id, lab.channel_id, label).await;
        wait_hears_audio(&lab.clients[index], lab.guild_id, lab.channel_id, label).await;
    }

    let hub_index = lab.index_of(hub);
    let backup_index = lab.index_of(backup);
    let third_index = lab.index_of(third);
    if let Some(daemon) = lab.daemons[hub_index].as_mut() {
        daemon.kill();
    }
    lab.daemons[hub_index] = None;

    // The dead hub is still in the voice roster until the 20 s member
    // timeout. Requiring the epoch advance inside a shorter budget is what
    // separates a takeover re-key from a later roster rotation.
    let deadline = Instant::now() + TAKEOVER_BUDGET;
    let (snapshot_b, snapshot_c) = loop {
        let snapshot_b = voice_snapshot(&lab.clients[backup_index], lab.guild_id, lab.channel_id)
            .await
            .expect("the backup still answers VoiceSnapshot");
        let snapshot_c = voice_snapshot(&lab.clients[third_index], lab.guild_id, lab.channel_id)
            .await
            .expect("the third member still answers VoiceSnapshot");
        let ready = snapshot_b.master == Some(backup)
            && snapshot_c.master == Some(backup)
            && snapshot_b.hub == Some(backup)
            && snapshot_c.hub == Some(backup)
            && snapshot_b.backup == Some(third)
            && snapshot_c.backup == Some(third)
            && snapshot_b.key_epoch > old_epoch
            && snapshot_c.key_epoch > old_epoch
            // The survivors must meet on the new epoch, which the assertions
            // below require; waiting for it keeps a propagation race from
            // failing the test after the election has already converged.
            && snapshot_b.key_epoch == snapshot_c.key_epoch
            && snapshot_b.members.iter().any(|member| member.author == hub);
        if ready {
            break (snapshot_b, snapshot_c);
        }
        if Instant::now() >= deadline {
            panic!(
                "the backup did not take over with a new epoch inside {TAKEOVER_BUDGET:?}: \
                 backup sees master={:?} hub={:?} backup={:?} epoch={} members={}, \
                 third sees master={:?} hub={:?} backup={:?} epoch={} members={}",
                snapshot_b.master,
                snapshot_b.hub,
                snapshot_b.backup,
                snapshot_b.key_epoch,
                snapshot_b.members.len(),
                snapshot_c.master,
                snapshot_c.hub,
                snapshot_c.backup,
                snapshot_c.key_epoch,
                snapshot_c.members.len(),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        snapshot_b.key_epoch > old_epoch,
        "the takeover did not advance the media epoch: {old_epoch} -> {}",
        snapshot_b.key_epoch
    );
    assert_eq!(snapshot_b.key_epoch, snapshot_c.key_epoch);
    assert_eq!(snapshot_b.hub, Some(backup));
    assert_ne!(
        snapshot_b.backup,
        Some(hub),
        "the dead hub became the backup"
    );

    // Media resumes through the new hub on the new epoch. The surviving leaf
    // and the new hub each carry one decoded stream, not the old all-pairs
    // set.
    let new_epoch = snapshot_b.key_epoch;
    let resumed = wait_for(
        BUDGET,
        "star audio to resume through the new hub",
        || async {
            let hub_stats =
                audio_stats(&lab.clients[backup_index], lab.guild_id, lab.channel_id).await?;
            let leaf_stats =
                audio_stats(&lab.clients[third_index], lab.guild_id, lab.channel_id).await?;
            let ready = hub_stats.epoch == new_epoch
                && leaf_stats.epoch == new_epoch
                && hub_stats.e2ee
                && leaf_stats.e2ee
                && hub_stats.peak > 0
                && hub_stats.rms > AUDIBLE_RMS
                && leaf_stats.peak > 0
                && leaf_stats.rms > AUDIBLE_RMS;
            ready.then_some((hub_stats, leaf_stats))
        },
    )
    .await;
    assert_eq!(
        resumed.0.peers, 1,
        "the new hub must decode the one surviving leaf, got {:?}",
        resumed.0
    );
    assert_eq!(
        resumed.1.peers, 1,
        "the surviving leaf must decode only the new hub, got {:?}",
        resumed.1
    );
    for index in [backup_index, third_index] {
        if let Some(daemon) = lab.take_daemon(index) {
            shutdown(daemon, &lab.clients[index]).await;
        }
    }
}

/// A leaf that leaves and rejoins lands on a newer epoch, reconnects to the
/// hub and resumes star audio. The two-member pair in between is a direct
/// connection, not a hub.
#[tokio::test(flavor = "multi_thread")]
async fn a_leaf_rejoin_rekeys_and_resumes_star_audio() {
    let _guard = serial();
    let mut lab = start_star_lab().await;
    let (snapshot_a, _, _) = wait_star(&lab).await;
    let hub = assert_star(&snapshot_a, &lab.authors, "A");
    let backup = snapshot_a.backup.expect("a backup");
    let third = lab
        .authors
        .iter()
        .copied()
        .find(|author| *author != hub && *author != backup)
        .expect("the third member");
    let third_index = lab.index_of(third);
    let first_epoch = snapshot_a.key_epoch;
    for (index, label) in ["A", "B", "C"].into_iter().enumerate() {
        wait_audio_ready(&lab.clients[index], lab.guild_id, lab.channel_id, label).await;
        wait_hears_audio(&lab.clients[index], lab.guild_id, lab.channel_id, label).await;
    }

    voice_leave(&lab.clients[third_index], lab.guild_id, lab.channel_id).await;
    let pair = wait_for(
        BUDGET,
        "the remaining pair to become a direct call",
        || async {
            let snapshot = voice_snapshot(&lab.clients[0], lab.guild_id, lab.channel_id).await?;
            let ready = snapshot.members.len() == 2
                && snapshot.key_epoch > first_epoch
                && snapshot.hub.is_none()
                && snapshot.topology == CallTopology::Mesh;
            ready.then_some(snapshot)
        },
    )
    .await;
    assert_eq!(
        pair.topology,
        CallTopology::Mesh,
        "a two-member call must be a direct pair even under a star scope"
    );
    assert!(
        pair.hub.is_none(),
        "the surviving pair still reports a hub: {pair:?}"
    );

    join_voice(&lab.clients[third_index], lab.guild_id, lab.channel_id).await;
    let rejoined = wait_for(BUDGET, "the rejoined star on a newer epoch", || async {
        let (a, b, c) = converged_star(
            &lab.clients[0],
            &lab.clients[1],
            &lab.clients[2],
            lab.guild_id,
            lab.channel_id,
        )
        .await?;
        (a.key_epoch > pair.key_epoch).then_some((a, b, c))
    })
    .await;
    let new_hub = assert_star(&rejoined.0, &lab.authors, "A after rejoin");
    assert_eq!(new_hub, hub, "the rejoin changed the elected hub");
    assert!(
        rejoined.0.key_epoch > pair.key_epoch,
        "the rejoin did not advance the epoch: {} -> {}",
        pair.key_epoch,
        rejoined.0.key_epoch
    );

    let new_epoch = rejoined.0.key_epoch;
    let resumed = wait_for(BUDGET, "star audio to resume after the rejoin", || async {
        let mut stats = Vec::new();
        for client in &lab.clients {
            let stats_at = audio_stats(client, lab.guild_id, lab.channel_id).await?;
            if stats_at.epoch != new_epoch
                || !stats_at.e2ee
                || stats_at.peak == 0
                || stats_at.rms <= AUDIBLE_RMS
            {
                return None;
            }
            stats.push(stats_at);
        }
        Some(stats)
    })
    .await;
    let hub_index = lab.index_of(new_hub);
    assert_eq!(
        resumed[hub_index].peers, 2,
        "the hub did not return to two leaf streams: {:?}",
        resumed[hub_index]
    );
    for (index, author) in lab.authors.iter().enumerate() {
        if *author == new_hub {
            continue;
        }
        assert_eq!(
            resumed[index].peers, 1,
            "leaf {author:?} did not return to one hub downlink: {:?}",
            resumed[index]
        );
    }

    for client in &lab.clients {
        voice_leave(client, lab.guild_id, lab.channel_id).await;
    }
    lab.shutdown_survivors().await;
}

/// The elected hub must advertise a sampled capacity, not the constant
/// `Priority::local` placeholder. This is the acceptance test for the
/// capacity-sampling item, and it is expected to fail until the daemon feeds
/// real samples into the heartbeat.
#[tokio::test(flavor = "multi_thread")]
async fn the_elected_hub_advertises_sampled_capacity() {
    let _guard = serial();
    let mut lab = start_star_lab().await;
    let (snapshot_a, _, _) = wait_star(&lab).await;
    let hub = assert_star(&snapshot_a, &lab.authors, "A");

    // Let a few heartbeat ticks pass so a sampler has something to report.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let control = control_snapshot(
        &lab.clients[lab.index_of(hub)],
        lab.guild_id,
        lab.channel_id,
    )
    .await;
    let candidates = control.election.candidates;
    let placeholder = Priority::local_for_call(3);
    let hub_priority = candidates
        .iter()
        .find(|candidate| candidate.author == hub)
        .unwrap_or_else(|| panic!("the hub is not in the candidate list: {candidates:?}"))
        .priority;
    assert_ne!(
        hub_priority, placeholder,
        "the elected hub still advertises the constant Priority::local placeholder, so CPU, \
         memory, bandwidth, RTT, loss and active calls are not sampled"
    );

    for client in &lab.clients {
        voice_leave(client, lab.guild_id, lab.channel_id).await;
    }
    lab.shutdown_survivors().await;
}
