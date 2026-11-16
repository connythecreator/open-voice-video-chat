//! WebRTC audio across two real daemon processes.
//!
//! This is the automated stand-in for a two-machine call: two
//! `ovvc-daemon` binaries, each with its own data directory, Unix socket and
//! libp2p node. Call media is configured for the WebRTC transport, so the
//! control session only carries SDP and ICE candidates while the Opus payload
//! crosses an RTP track over loopback.
//!
//! The daemons run the null audio backend with a synthetic 440 Hz tone, so a
//! playback counter with real energy proves the tone was encoded, written as
//! an RTP sample, received by the peer and decoded by its audio service.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-daemon --test rtc_audio_e2e -- --test-threads=1
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
use tokio::sync::broadcast;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-daemon");
const STARTUP: Duration = Duration::from_secs(15);
/// WebRTC setup on loopback usually takes a second; the budget leaves room for
/// ICE checks and the audio service's one-second stats cadence.
const BUDGET: Duration = Duration::from_secs(45);
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

        // Isolate discovery completely: the explicit file supplies every
        // path, and HOME/XDG point into the temp root so a regression cannot
        // reach the developer's own configuration.
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

/// Wait for a `VoiceAudio` event that satisfies `accept`.
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

/// Wait until `packets` has not changed for `window`.
async fn wait_packets_stable(
    client: &DaemonClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    window: Duration,
) -> u64 {
    let deadline = Instant::now() + BUDGET;
    let mut last = audio_stats(client, guild_id, channel_id)
        .await
        .map(|stats| stats.packets)
        .unwrap_or(0);
    let mut stable_since = Instant::now();
    loop {
        if Instant::now() >= deadline {
            panic!("timed out waiting for encoded packets to stop increasing");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        let packets = audio_stats(client, guild_id, channel_id)
            .await
            .map(|stats| stats.packets)
            .unwrap_or(last);
        if packets == last {
            if stable_since.elapsed() >= window {
                return packets;
            }
        } else {
            last = packets;
            stable_since = Instant::now();
        }
    }
}

/// Wait until `played` has not changed for `window`.
async fn wait_played_stable(
    client: &DaemonClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    window: Duration,
) -> u64 {
    let deadline = Instant::now() + BUDGET;
    let mut last = audio_stats(client, guild_id, channel_id)
        .await
        .map(|stats| stats.played)
        .unwrap_or(0);
    let mut stable_since = Instant::now();
    loop {
        if Instant::now() >= deadline {
            panic!("timed out waiting for played frames to stop increasing");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        let played = audio_stats(client, guild_id, channel_id)
            .await
            .map(|stats| stats.played)
            .unwrap_or(last);
        if played == last {
            if stable_since.elapsed() >= window {
                return played;
            }
        } else {
            last = played;
            stable_since = Instant::now();
        }
    }
}

fn voice_member(
    snapshot: &ovvc_control::VoiceSnapshot,
    author: AuthorId,
) -> Option<&ovvc_control::VoiceMember> {
    snapshot
        .members
        .iter()
        .find(|member| member.author == author)
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

/// Two daemons negotiate WebRTC over the real control session, then carry the
/// synthetic 440 Hz tone as Opus on an RTP track in both directions.
#[tokio::test(flavor = "multi_thread")]
async fn rtc_audio_crosses_two_real_daemons() {
    let _guard = serial();
    let mut daemon_a = DaemonProcess::start();
    let mut daemon_b = DaemonProcess::start();
    let client_a = connect_client(&mut daemon_a).await;
    let client_b = connect_client(&mut daemon_b).await;

    // Subscribe before any call exists so the first stats event is seen.
    let mut events_a = client_a.subscribe();
    let mut events_b = client_b.subscribe();

    // (a) A owns the guild and the voice channel; B joins over an invite.
    let guild_id = create_guild(&client_a, "rtc-audio-lab").await;
    let voice_channel = create_channel(&client_a, guild_id, "standup", ChannelKind::Voice).await;

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

    // (b) Both join the call. The lower author offers and both managers dial
    // each other's loopback candidate through the control session.
    voice_join(&client_a, guild_id, voice_channel).await;
    voice_join(&client_b, guild_id, voice_channel).await;
    for (label, client, author) in [("A", &client_a, author_a), ("B", &client_b, author_b)] {
        wait_for(
            BUDGET,
            &format!("{label} to see both call members"),
            || async {
                voice_snapshot(client, guild_id, voice_channel)
                    .await
                    .filter(|snapshot| {
                        snapshot.members.len() == 2 && voice_member(snapshot, author).is_some()
                    })
            },
        )
        .await;
    }

    // (c) Real Opus crossed an RTP track. `played`, `peak` and `rms` can only
    // be non-zero once the remote tone was decoded, and the stats must name
    // the WebRTC transport.
    for (label, events) in [("A", &mut events_a), ("B", &mut events_b)] {
        let captured = wait_audio_event(
            events,
            guild_id,
            voice_channel,
            &format!("{label}'s engine to capture and encode sealed audio"),
            |stats| {
                stats.backend == "null"
                    && stats.captured > 0
                    && stats.packets > 0
                    && stats.e2ee
                    && stats.epoch >= 1
            },
        )
        .await;
        assert_eq!(captured.backend, "null", "{label} opened the wrong backend");
        assert_eq!(
            captured.transport, "webrtc",
            "{label} reported the wrong media transport"
        );
        assert!(
            captured.e2ee,
            "{label} reported plaintext media: {captured:?}"
        );
        assert!(
            captured.epoch >= 1,
            "{label} reported no media key epoch: {captured:?}"
        );

        let played = wait_audio_event(
            events,
            guild_id,
            voice_channel,
            &format!("{label} to play decoded RTP audio"),
            |stats| {
                stats.transport == "webrtc"
                    && stats.e2ee
                    && stats.epoch >= 1
                    && stats.played > 0
                    && stats.peak > 0
                    && stats.rms > 100.0
            },
        )
        .await;
        assert!(
            played.played > 0 && played.peak > 0 && played.rms > 100.0,
            "{label} never decoded the peer tone over RTP with real energy: {played:?}"
        );
        assert!(
            played.e2ee && played.epoch >= 1,
            "{label} played media without reporting E2EE: {played:?}"
        );
    }

    // (d) Muting B stops its encoder. The control session carries the state,
    // while the RTP track simply goes quiet.
    set_voice_state(&client_b, guild_id, voice_channel, true, false, false).await;
    let muted_packets = wait_packets_stable(
        &client_b,
        guild_id,
        voice_channel,
        Duration::from_millis(2_500),
    )
    .await;

    set_voice_state(&client_b, guild_id, voice_channel, false, false, false).await;
    let resumed_packets = wait_for(BUDGET, "B to resume encoding after unmuting", || async {
        let stats = audio_stats(&client_b, guild_id, voice_channel).await?;
        (stats.packets > muted_packets).then_some(stats.packets)
    })
    .await;
    assert!(
        resumed_packets > muted_packets,
        "B stayed muted: packets {muted_packets} -> {resumed_packets}"
    );

    // (e) Deafening A stops playback while its encoder keeps running, so the
    // call itself stays up.
    set_voice_state(&client_a, guild_id, voice_channel, false, true, false).await;
    let deafened_played = wait_played_stable(
        &client_a,
        guild_id,
        voice_channel,
        Duration::from_millis(2_500),
    )
    .await;

    set_voice_state(&client_a, guild_id, voice_channel, false, false, false).await;
    let resumed_played = wait_for(BUDGET, "A to resume playback after undeafening", || async {
        let stats = audio_stats(&client_a, guild_id, voice_channel).await?;
        (stats.played > deafened_played).then_some(stats.played)
    })
    .await;
    assert!(
        resumed_played > deafened_played,
        "A stayed deafened: played {deafened_played} -> {resumed_played}"
    );

    let with_peer = wait_for(BUDGET, "A's stats to report the peer", || async {
        let stats = audio_stats(&client_a, guild_id, voice_channel).await?;
        (stats.peers == 1).then_some(stats)
    })
    .await;
    assert_eq!(with_peer.peers, 1, "A did not report B as a mixed peer");

    // (f) B leaves; A's RTC peer is closed and its snapshot loses B.
    voice_leave(&client_b, guild_id, voice_channel).await;
    wait_for(BUDGET, "B to leave A's call snapshot", || async {
        let snapshot = voice_snapshot(&client_a, guild_id, voice_channel).await?;
        (voice_member(&snapshot, author_a).is_some() && voice_member(&snapshot, author_b).is_none())
            .then_some(())
    })
    .await;
    wait_for(BUDGET, "A's call peer count to drop", || async {
        let stats = audio_stats(&client_a, guild_id, voice_channel).await?;
        (stats.peers == 0).then_some(stats.peers)
    })
    .await;

    // (g) A leaves; the call, its audio service and its RTC transport are gone.
    voice_leave(&client_a, guild_id, voice_channel).await;
    wait_for(BUDGET, "A's call to disappear", || async {
        voice_snapshot(&client_a, guild_id, voice_channel)
            .await
            .is_none()
            .then_some(())
    })
    .await;
    wait_for(BUDGET, "A's audio service to stop", || async {
        audio_stats(&client_a, guild_id, voice_channel)
            .await
            .is_none()
            .then_some(())
    })
    .await;

    shutdown(daemon_b, &client_b).await;
    shutdown(daemon_a, &client_a).await;
}
