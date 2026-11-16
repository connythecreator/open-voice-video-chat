//! Star media across three real daemon processes.
//!
//! Three `ovvc-daemon` binaries, three data directories, three Unix sockets
//! and three libp2p nodes on loopback. The commune is star-scoped and the
//! hub is elected, so the media plane must be a flat hub: a leaf opens no
//! media peer to another leaf, the hub decodes both leaves, each leaf hears
//! the hub and the other leaf but not itself, and killing the hub promotes
//! the backup, publishes a new key epoch and resumes media.
//!
//! The daemons run the null audio backend with a synthetic 440 Hz tone, so a
//! non-zero playback counter with real energy proves the tone reached the
//! device through the star path.
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

use ovvc_control::{CallTopology, VoiceSnapshot};
use ovvc_core::{AuthorId, ChannelId, ChannelKind, GuildId};
use ovvc_ipc::{DaemonClient, DaemonEvent, NetworkOptionsDto, Op, Reply, VoiceAudioStats};
use tempfile::TempDir;
use tokio::sync::broadcast;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-daemon");
const STARTUP: Duration = Duration::from_secs(15);
const BUDGET: Duration = Duration::from_secs(45);
const FAILOVER_BUDGET: Duration = Duration::from_secs(30);
/// The synthetic source's tone, in hertz.
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
    fn start(transport: &str) -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let config_dir = root.path().join("config");
        let data_dir = root.path().join("data");
        let run_dir = root.path().join("run");
        let home = root.path().join("home");
        fs::create_dir_all(&config_dir).expect("config dir");
        fs::create_dir_all(&run_dir).expect("run dir");
        fs::create_dir_all(home.join("runtime")).expect("home runtime dir");

        let socket = run_dir.join("daemon.sock");
        let config_file = root.path().join("daemon.toml");
        fs::write(
            &config_file,
            format!(
                "version = 2\n\
                 data_dir = {:?}\n\
                 socket = {:?}\n\
                 [audio]\n\
                 backend = \"null\"\n\
                 sample_rate = 48000\n\
                 frame_ms = 20\n\
                 profile = \"voice-standard\"\n\
                 [media]\n\
                 transport = {:?}\n\
                 ice_servers = []\n\
                 force_relay = false\n\
                 allow_loopback_candidates = true\n\
                 allow_lan_candidates = true\n\
                 [policy.media]\n\
                 call_scope = \"star\"\n\
                 [policy.media.hub]\n\
                 mode = \"elected\"\n\
                 trust = \"trusted\"\n\
                 allow_mixing = true\n\
                 allow_video_forwarding = true\n\
                 min_trust_tier = 0\n",
                data_dir.display().to_string(),
                socket.display().to_string(),
                transport
            ),
        )
        .expect("write daemon config");

        // Isolate discovery and configuration from the developer's own files.
        let child = Command::new(BIN)
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

async fn author_id(client: &DaemonClient) -> AuthorId {
    expect(client.call(Op::Identity).await, |reply| match reply {
        Reply::Identity { author_id, .. } => Some(*author_id),
        _ => None,
    })
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

async fn wait_listening(
    client: &DaemonClient,
    guild_id: GuildId,
) -> ovvc_ipc::ConnectionStateDto {
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

async fn wait_members(client: &DaemonClient, guild_id: GuildId, want: usize) -> Vec<AuthorId> {
    wait_for(BUDGET, "the guild roster", || async {
        let members: Vec<AuthorId> = expect(
            client.call(Op::ListMembers { guild_id }).await,
            |reply| match reply {
                Reply::Members(members) => {
                    Some(members.iter().map(|member| member.author).collect())
                }
                _ => None,
            },
        );
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

async fn wait_audio_event<F>(
    events: &mut broadcast::Receiver<DaemonEvent>,
    guild_id: GuildId,
    channel_id: ChannelId,
    what: &str,
    mut accept: F,
) -> VoiceAudioStats
where
    F: FnMut(&VoiceAudioStats) -> bool,
{
    let deadline = Instant::now() + BUDGET;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("timed out after {BUDGET:?} waiting for {what}");
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(DaemonEvent::VoiceAudio {
                guild_id: event_guild,
                channel_id: event_channel,
                stats,
            })) if event_guild == guild_id && event_channel == channel_id => {
                if accept(&stats) {
                    return stats;
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(error)) => panic!("event stream closed while waiting for {what}: {error}"),
            Err(_) => panic!("timed out after {BUDGET:?} waiting for {what}"),
        }
    }
}

/// Wait until all three daemons report the same 3-member flat star with a hub,
/// a distinct backup and a published key epoch.
async fn converged_star(
    clients: [&DaemonClient; 3],
    guild_id: GuildId,
    channel_id: ChannelId,
) -> Option<(VoiceSnapshot, VoiceSnapshot, VoiceSnapshot)> {
    let snapshots = [
        voice_snapshot(clients[0], guild_id, channel_id).await,
        voice_snapshot(clients[1], guild_id, channel_id).await,
        voice_snapshot(clients[2], guild_id, channel_id).await,
    ];
    let [Some(a), Some(b), Some(c)] = snapshots else {
        return None;
    };
    let hubs = [a.hub, b.hub, c.hub];
    let masters = [a.master, b.master, c.master];
    let backups = [a.backup, b.backup, c.backup];
    let ready = a.members.len() == 3
        && b.members.len() == 3
        && c.members.len() == 3
        && a.key_epoch >= 1
        && b.key_epoch >= 1
        && c.key_epoch >= 1
        && hubs[0].is_some()
        && hubs[0] == hubs[1]
        && hubs[1] == hubs[2]
        && masters == hubs
        && backups[0].is_some()
        && backups[0] != hubs[0]
        && backups[0] == backups[1]
        && backups[1] == backups[2]
        && matches!(a.topology, CallTopology::Star { .. });
    ready.then_some((a, b, c))
}

/// Start three daemons, join one commune and one voice channel, and return the
/// clients and identities in process order A, B, C.
async fn start_lab(
    transport: &str,
) -> (
    [DaemonProcess; 3],
    [DaemonClient; 3],
    GuildId,
    ChannelId,
    [AuthorId; 3],
) {
    let mut daemons = [
        DaemonProcess::start(transport),
        DaemonProcess::start(transport),
        DaemonProcess::start(transport),
    ];
    let clients = [
        connect_client(&mut daemons[0]).await,
        connect_client(&mut daemons[1]).await,
        connect_client(&mut daemons[2]).await,
    ];

    let guild_id = create_guild(&clients[0], "star-media").await;
    let channel_id = create_voice_channel(&clients[0], guild_id, "standup").await;

    connect(&clients[0], guild_id, "/ip4/127.0.0.1/tcp/0", Vec::new()).await;
    let state_a = wait_listening(&clients[0], guild_id).await;
    let peer_a = state_a.peer_id.clone().expect("A peer id");
    let addr_a = state_a.listen_addresses[0].clone();
    let invite_a = expect(
        clients[0]
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
        clients[1]
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
    let state_b = wait_listening(&clients[1], guild_id).await;
    let peer_b = state_b.peer_id.clone().expect("B peer id");
    let addr_b = state_b.listen_addresses[0].clone();
    wait_peers(&clients[0], guild_id, 1).await;
    wait_peers(&clients[1], guild_id, 1).await;
    wait_members(&clients[0], guild_id, 2).await;

    let invite_ab = expect(
        clients[0]
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
        clients[2]
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
    wait_peers(&clients[0], guild_id, 2).await;
    wait_peers(&clients[1], guild_id, 2).await;
    wait_peers(&clients[2], guild_id, 2).await;
    wait_members(&clients[0], guild_id, 3).await;
    wait_members(&clients[1], guild_id, 3).await;
    wait_members(&clients[2], guild_id, 3).await;

    let authors = [
        author_id(&clients[0]).await,
        author_id(&clients[1]).await,
        author_id(&clients[2]).await,
    ];
    join_voice(&clients[0], guild_id, channel_id).await;
    join_voice(&clients[1], guild_id, channel_id).await;
    join_voice(&clients[2], guild_id, channel_id).await;

    (daemons, clients, guild_id, channel_id, authors)
}

#[tokio::test(flavor = "multi_thread")]
async fn three_daemon_star_carries_media_only_through_the_hub() {
    let _guard = serial();
    let (mut daemons, clients, guild_id, channel_id, authors) = start_lab("libp2p").await;
    // Subscribe before the first call so the first stats sample is not lost.
    let mut events = [
        clients[0].subscribe(),
        clients[1].subscribe(),
        clients[2].subscribe(),
    ];

    let converged = wait_for(BUDGET, "three daemons to agree on the star", || {
        converged_star([&clients[0], &clients[1], &clients[2]], guild_id, channel_id)
    })
    .await;
    let hub = converged.0.hub.expect("a hub");
    let backup = converged.0.backup.expect("a backup");
    assert_ne!(hub, backup);
    let hub_index = authors
        .iter()
        .position(|author| *author == hub)
        .expect("the hub is one of the three daemons");
    let leaf_indices: Vec<usize> = (0..3).filter(|index| *index != hub_index).collect();
    assert_eq!(leaf_indices.len(), 2);

    // (a) Every member captures and encodes sealed audio.
    let mut played_before = [0u64; 3];
    for index in 0..3 {
        let is_hub = index == hub_index;
        let label = if is_hub { "hub" } else { "leaf" };
        let captured = wait_audio_event(
            &mut events[index],
            guild_id,
            channel_id,
            &format!("{label} to capture and seal audio"),
            |stats| {
                stats.backend == "null"
                    && stats.transport == "libp2p"
                    && stats.e2ee
                    && stats.epoch >= 1
                    && stats.captured > 0
                    && stats.packets > 0
            },
        )
        .await;
        assert_eq!(captured.backend, "null", "{label} backend");
        assert_eq!(captured.transport, "libp2p", "{label} transport");
        assert!(captured.e2ee, "{label} reported plaintext: {captured:?}");

        // (b) The leaf decodes exactly one stream: the hub's per-listener mix.
        // A second peer would mean the leaf is decoding another leaf directly,
        // which is the all-pairs path this phase removes. The hub's audio
        // service is not fed leaf packets at all; its playback comes from the
        // mixer, so its `peers` counter is not asserted here.
        let playback = wait_audio_event(
            &mut events[index],
            guild_id,
            channel_id,
            &format!("{label} to play the star mix"),
            |stats| {
                stats.e2ee
                    && stats.epoch >= 1
                    && stats.played > 0
                    && stats.peak > 0
                    && stats.rms > 100.0
                    && (is_hub || stats.peers == 1)
            },
        )
        .await;
        assert!(
            playback.peak > 0 && playback.rms > 100.0,
            "{label} never played the tone with real energy: {playback:?}"
        );
        if !is_hub {
            assert_eq!(
                playback.peers, 1,
                "a leaf decoded more than the hub's mixed stream: {playback:?}"
            );
        }
        played_before[index] = playback.played;
    }

    // (c) Kill the hub. The backup must take over, publish a higher epoch and
    // resume media with the remaining leaf. The dead hub stays in the voice
    // roster for its lease, so the test must not wait for it to disappear.
    let epoch_before = converged
        .0
        .key_epoch
        .max(converged.1.key_epoch)
        .max(converged.2.key_epoch);
    daemons[hub_index].kill();

    let survivors = leaf_indices;
    let (new_hub, new_epoch) = wait_for(FAILOVER_BUDGET, "the backup to take over", || {
        let clients = &clients;
        let survivors = survivors.clone();
        async move {
            let first = voice_snapshot(&clients[survivors[0]], guild_id, channel_id).await?;
            let second = voice_snapshot(&clients[survivors[1]], guild_id, channel_id).await?;
            let took_over = first.master == second.master
                && first.master.is_some()
                && first.master != Some(hub)
                && first.hub == first.master
                && second.hub == second.master
                && first.key_epoch > epoch_before
                && second.key_epoch > epoch_before;
            took_over.then_some((
                first.master.expect("a new master"),
                first.key_epoch.max(second.key_epoch),
            ))
        }
    })
    .await;
    assert!(
        survivors
            .iter()
            .any(|index| authors[*index] == new_hub),
        "the new hub is not one of the surviving leaves"
    );

    // (d) Both survivors play the new hub's mix under the new epoch. The
    // counter must climb by more than a few buffered frames, so a stale
    // pre-takeover buffer cannot pass this check.
    for &index in &survivors {
        let label = if authors[index] == new_hub {
            "the new hub"
        } else {
            "the remaining leaf"
        };
        let resumed = wait_audio_event(
            &mut events[index],
            guild_id,
            channel_id,
            &format!("{label} to resume media after the takeover"),
            |stats| {
                stats.e2ee
                    && stats.epoch >= new_epoch
                    && stats.peak > 0
                    && stats.rms > 100.0
                    && stats.played >= played_before[index].saturating_add(20)
            },
        )
        .await;
        assert!(
            resumed.epoch >= new_epoch,
            "{label} did not install the new epoch: {resumed:?}"
        );
        assert!(
            resumed.played >= played_before[index].saturating_add(20),
            "{label} did not resume playback after the takeover: {resumed:?}"
        );
    }

    // Clean shutdown of the survivors. The killed daemon's drop is a no-op.
    for &index in &survivors {
        let _ = clients[index].call(Op::ShutdownDaemon).await;
    }
    for &index in &survivors {
        daemons[index].kill();
    }
}

/// The same star over WebRTC.
///
/// `reconcile_rtc` must dial only the hub from a leaf and every leaf from the
/// hub. Once the peer connections carry RTP, the hub's mixer sends each leaf
/// its own mixed frame through `send_audio_to`, and a leaf's audio engine
/// decodes exactly one stream: the hub's mix.
#[tokio::test(flavor = "multi_thread")]
async fn webrtc_star_carries_media_only_through_the_hub() {
    let _guard = serial();
    let (mut daemons, clients, guild_id, channel_id, authors) = start_lab("webrtc").await;
    let mut events = [
        clients[0].subscribe(),
        clients[1].subscribe(),
        clients[2].subscribe(),
    ];

    let converged = wait_for(BUDGET, "three daemons to agree on the star", || {
        converged_star([&clients[0], &clients[1], &clients[2]], guild_id, channel_id)
    })
    .await;
    let hub = converged.0.hub.expect("a hub");
    let hub_index = authors
        .iter()
        .position(|author| *author == hub)
        .expect("the hub is one of the three daemons");

    for index in 0..3 {
        let is_hub = index == hub_index;
        let label = if is_hub { "hub" } else { "leaf" };
        let playback = wait_audio_event(
            &mut events[index],
            guild_id,
            channel_id,
            &format!("{label} to play the WebRTC star mix"),
            |stats| {
                stats.backend == "null"
                    && stats.transport == "webrtc"
                    && stats.e2ee
                    && stats.epoch >= 1
                    && stats.captured > 0
                    && stats.packets > 0
                    && stats.played > 0
                    && stats.peak > 0
                    && stats.rms > 100.0
                    && (is_hub || stats.peers == 1)
            },
        )
        .await;
        assert_eq!(playback.transport, "webrtc", "{label} transport");
        assert!(playback.e2ee, "{label} reported plaintext: {playback:?}");
        if !is_hub {
            assert_eq!(
                playback.peers, 1,
                "a WebRTC leaf decoded more than the hub's mixed stream: {playback:?}"
            );
        }
    }

    for index in 0..3 {
        let _ = clients[index].call(Op::ShutdownDaemon).await;
    }
    for daemon in &mut daemons {
        daemon.kill();
    }
}
