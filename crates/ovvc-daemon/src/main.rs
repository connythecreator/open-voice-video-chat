//! `ovvc-daemon`: the local process that owns ovvc state and networking.
//!
//! Start it once per user/data directory. Frontends attach over the Unix socket
//! and never open the store themselves, which keeps redb single-writer and
//! lets a CLI, TUI and GUI run side by side.

mod capacity;
mod control_hub;
mod daemon;
mod daemon_log;
mod network_state;
mod notifications;
mod settings;
mod tray;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use ovvc_client::Client;
use ovvc_config::{ConfigPaths, LoadedConfig, Overrides, load};
use ovvc_ipc::server::{bind_socket, peer_uid_allowed, remove_socket};
use ovvc_ipc::{
    CAP_VOICE, CAP_VOICE_AUDIO, CLIENT_CAPABILITIES, DaemonEvent, DaemonInfo, Frame, IPC_PROTOCOL,
    MAX_HANDSHAKE_BYTES, Op, Welcome, capabilities_ok, default_data_dir, default_socket_path,
    protocol_major, read_frame, read_frame_limited, remove_daemon_info, unknown_capabilities,
    write_daemon_info, write_frame, write_frame_limited,
};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::timeout;

use crate::daemon::Daemon;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_QUEUE: usize = 256;
const CONTROL_TICK: Duration = Duration::from_secs(1);
/// How often a guild that should be online but is not gets another try.
const NETWORK_REPAIR_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Parser)]
#[command(name = "ovvc-daemon", about = "Local ovvc storage and network daemon")]
struct Args {
    /// Directory that holds the identity, store and daemon discovery record.
    #[arg(long, env = "OVVC_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Unix socket to listen on. Defaults to a per-user runtime path derived
    /// from the data directory.
    #[arg(long, env = "OVVC_SOCKET")]
    socket: Option<PathBuf>,

    /// Read configuration from this file instead of the user config file.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Apply this named profile from <config-dir>/profiles.
    #[arg(long, env = "OVVC_PROFILE", value_name = "NAME")]
    profile: Option<String>,

    /// Show a desktop notification when a message mentions or replies to you.
    /// Failures (no DBus, no notification service) are logged, never fatal.
    #[arg(long)]
    notify: bool,

    /// Show a StatusNotifierItem system tray icon. On by default through
    /// `daemon.tray = true`; this flag still forces it on for a run whose
    /// configuration turned it off. Without a tray host the daemon logs that
    /// the tray is unavailable and keeps running normally.
    #[arg(long)]
    tray: bool,

    /// Bring the peer network up on start. Guilds that were online when the
    /// daemon last stopped are always resumed; this flag additionally connects
    /// guilds with no saved record, on an ephemeral port. Interactive
    /// frontends (the GUI and TUI) pass it when they spawn the daemon, so
    /// opening the app puts you back online without another click.
    #[arg(long)]
    online: bool,
}

/// Whether this daemon should start the tray task.
///
/// The CLI flag can only force the tray on. The configuration decides the
/// default, and `daemon.tray = false` is the opt-out. Keeping this in one
/// small function makes the rule in the config comment executable and lets a
/// unit test prove that a daemon spawned without `--tray` still starts it.
fn tray_requested(flag: bool, configured: bool) -> bool {
    flag || configured
}

/// Load the layered configuration, naming the file in a failure.
#[cfg(unix)]
fn load_config(args: &Args) -> Result<LoadedConfig> {
    let paths = ConfigPaths::discover().context("locating configuration")?;
    let origin = args.config.clone().unwrap_or_else(|| paths.user.clone());
    let overrides = Overrides {
        config: args.config.clone(),
        profile: args.profile.clone(),
        data_dir: args.data_dir.clone(),
        socket: args.socket.clone(),
        theme: None,
    };
    load(&paths, overrides)
        .with_context(|| format!("loading configuration from {}", origin.display()))
}

#[cfg(unix)]
fn main() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?
        .block_on(run())
}

#[cfg(not(unix))]
fn main() -> Result<()> {
    anyhow::bail!("the ovvc daemon IPC transport is Unix-only")
}

#[cfg(unix)]
async fn run() -> Result<()> {
    let args = Args::parse();
    // Load before choosing the data directory and socket: an explicit --config
    // can point both of them somewhere else, and a broken file must stop the
    // daemon rather than silently falling back to defaults.
    let loaded = load_config(&args)?;

    // Refuse a configured backend this binary cannot open before binding the
    // socket. Otherwise the daemon starts, accepts a call and leaves the
    // joiner with no audio service, with the reason only on the event stream.
    let configured_backend =
        ovvc_audio::BackendKind::parse(&loaded.config.audio.backend).unwrap_or_default();
    ovvc_audio::validate_backend(configured_backend).map_err(|error| {
        anyhow::anyhow!(
            "configuration selects audio.backend = {:?}, but {error}",
            loaded.config.audio.backend
        )
    })?;

    // CLI/env (the clap bindings above) > configuration > built-in default.
    let data_dir = match args.data_dir.clone() {
        Some(dir) => dir,
        None => match loaded.config.data_dir.clone() {
            Some(dir) => dir,
            None => default_data_dir().context("could not determine a per-user data directory")?,
        },
    };
    std::fs::create_dir_all(&data_dir).context("creating data directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700));
    }

    let socket = match args.socket.clone() {
        Some(socket) => socket,
        None => loaded
            .config
            .socket
            .clone()
            .unwrap_or_else(|| default_socket_path(&data_dir)),
    };

    // Notifications are opt-in. The tray is on by default through the
    // configuration, which is the path a daemon spawned by the GUI or TUI
    // takes: neither frontend passes `--tray`, so the default has to start it.
    let notify = args.notify || loaded.config.daemon.notify;
    let tray_enabled = tray_requested(args.tray, loaded.config.daemon.tray);
    let online = args.online || loaded.config.daemon.online;

    // Bind first: if another daemon already owns this socket, it is still
    // holding the redb write lock and opening the store would only produce a
    // second, more confusing failure.
    let listener = bind_socket(&socket).await.with_context(|| {
        format!(
            "binding daemon socket {} (is another ovvc-daemon running?)",
            socket.display()
        )
    })?;

    let client = match Client::open_local(&data_dir) {
        Ok(client) => Arc::new(client),
        Err(error) => {
            remove_socket(&socket);
            return Err(error).context("opening local client");
        }
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let daemon = Arc::new(Daemon::new(
        Arc::clone(&client),
        Arc::new(loaded.config.clone()),
        data_dir.clone(),
        socket.clone(),
        shutdown_tx.clone(),
    ));

    let info = DaemonInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        data_dir: data_dir.display().to_string(),
        socket: socket.display().to_string(),
        started_at: daemon.started_at,
    };
    write_daemon_info(&data_dir, &info).context("writing daemon discovery record")?;

    eprintln!(
        "ovvc-daemon {} listening on {} (data: {})",
        env!("CARGO_PKG_VERSION"),
        socket.display(),
        data_dir.display()
    );
    let present: Vec<String> = loaded
        .sources
        .iter()
        .filter(|source| source.present)
        .map(|source| source.path.display().to_string())
        .collect();
    let sources = if present.is_empty() {
        "built-in defaults".to_string()
    } else {
        present.join(", ")
    };
    eprintln!("profile: {} (config: {sources})", loaded.config.profile);

    let pump = tokio::spawn(pump_client_events(Arc::clone(&daemon)));
    let audio = tokio::spawn(audio_pump(Arc::clone(&daemon)));
    let rtc = tokio::spawn(rtc_pump(Arc::clone(&daemon)));
    let media = tokio::spawn(media_tick(Arc::clone(&daemon), shutdown_rx.clone()));
    let ticker = tokio::spawn(expiry_tick(Arc::clone(&daemon), shutdown_rx.clone()));
    let accept = tokio::spawn(accept_loop(
        listener,
        Arc::clone(&daemon),
        shutdown_rx.clone(),
    ));
    // The daemon starts serving first, then rejoins the guilds that were
    // online last time. Running it in a task keeps a slow network start from
    // delaying the socket a frontend is already waiting on.
    let resume = tokio::spawn({
        let daemon = Arc::clone(&daemon);
        async move { daemon.resume_network(online).await }
    });

    // Notifications are opt-in, the tray is on unless the configuration turns
    // it off. `start` returns immediately (each integration runs as its own
    // task) and logs if this build lacks the feature or has no tray host, so a
    // headless daemon is unaffected.
    let notifier = if notify {
        notifications::start(daemon.subscribe(), daemon.settings_handle())
    } else {
        None
    };
    let tray = if tray_enabled {
        tray::start(
            data_dir.clone(),
            socket.clone(),
            shutdown_tx.clone(),
            Arc::clone(&daemon),
        )
    } else {
        None
    };

    wait_for_shutdown(shutdown_rx).await;

    accept.abort();
    ticker.abort();
    media.abort();
    pump.abort();
    audio.abort();
    rtc.abort();
    resume.abort();
    // Close every audio device and peer connection before the process exits.
    // The pumps are already gone, so the workers only have to observe `Stop`.
    daemon.stop_all_audio().await;
    daemon.stop_all_rtc().await;
    daemon.clear_all_crypto().await;
    if let Some(notifier) = notifier {
        notifier.abort();
    }
    if let Some(tray) = tray {
        tray.abort();
    }

    // Give already-queued responses and the shutdown reply a moment to reach
    // the socket before the process tears the runtime down.
    tokio::time::sleep(Duration::from_millis(150)).await;

    for guild in client.list_guilds().unwrap_or_default() {
        client.disconnect(guild.id);
    }
    remove_daemon_info(&data_dir);
    remove_socket(&socket);
    eprintln!("ovvc-daemon stopped");
    Ok(())
}

#[cfg(unix)]
async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).ok();
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow_and_update() {
                        return;
                    }
                }
                _ = tokio::signal::ctrl_c() => return,
                _ = async {
                    match terminate.as_mut() {
                        Some(terminate) => {
                            terminate.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => return,
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = shutdown.changed().await;
    }
}

async fn pump_client_events(daemon: Arc<Daemon>) {
    let mut events = daemon.client.subscribe();
    loop {
        match events.recv().await {
            Ok(event) => daemon.handle_client_event(event).await,
            Err(broadcast::error::RecvError::Lagged(dropped)) => daemon.report(
                ovvc_core::ErrorClass::Operational,
                None,
                "event stream",
                format!("dropped {dropped} network events"),
            ),
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Turn audio service events into network traffic and frontend events.
async fn audio_pump(daemon: Arc<Daemon>) {
    let mut events = daemon.subscribe_audio();
    loop {
        match events.recv().await {
            Ok(envelope) => daemon.handle_audio_event(envelope).await,
            Err(broadcast::error::RecvError::Lagged(dropped)) => daemon.report(
                ovvc_core::ErrorClass::Operational,
                None,
                "event stream",
                format!("dropped {dropped} audio events"),
            ),
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Turn WebRTC transport events into daemon actions.
///
/// Signals go back over the control session, incoming RTP payloads go to the
/// call's audio service, and a one-second tick samples each peer for the
/// transport fields in `VoiceAudioStats`. The channel is bounded so a burst of
/// RTP cannot grow memory: full RTP is dropped by the manager, while
/// signalling and stats wait for capacity and stay lossless.
async fn rtc_pump(daemon: Arc<Daemon>) {
    let Some(mut events) = daemon.take_rtc_events().await else {
        return;
    };
    let mut stats = tokio::time::interval(Duration::from_secs(1));
    stats.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    stats.tick().await;
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(envelope) => daemon.handle_rtc_event(envelope).await,
                None => return,
            },
            _ = stats.tick() => daemon.sample_rtc_stats().await,
        }
    }
}

/// The retry cadence for a guild that should be online but is not. The
/// environment override exists so a test can prove the retry without waiting
/// half a minute; production leaves it unset.
fn repair_interval() -> Duration {
    std::env::var("OVVC_NETWORK_REPAIR_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(NETWORK_REPAIR_INTERVAL)
}

/// Pump the star hub's per-listener mixer at the Opus frame period.
///
/// This is separate from the one-second control tick because the output
/// cadence is the frame length. A daemon that is not a hub has no mixers and
/// this loop does nothing.
async fn media_tick(daemon: Arc<Daemon>, mut shutdown: watch::Receiver<bool>) {
    let frame_ms = u64::from(daemon.config.audio.frame_ms.max(10));
    let mut interval = tokio::time::interval(Duration::from_millis(frame_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => daemon.media_tick().await,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow_and_update() {
                    return;
                }
            }
        }
    }
}

async fn expiry_tick(daemon: Arc<Daemon>, mut shutdown: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(CONTROL_TICK);
    // A slow tick must not be followed by a burst of catch-up ticks. Besides
    // doubling the heartbeat and media work, a burst samples CPU and transport
    // counters over a few milliseconds and turns ordinary load into a spurious
    // health breach.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut repair = tokio::time::interval(repair_interval());
    repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first interval tick is immediate. Hold it back briefly so the
    // startup resume is the only thing starting nodes during boot, then let
    // the first repair run soon after. A saved listen port can still be held
    // for a moment by a daemon that is shutting down, and without this early
    // pass a failed resume would stay dark until the next full interval.
    tokio::time::sleep(repair_interval().min(Duration::from_secs(5))).await;
    loop {
        tokio::select! {
            _ = interval.tick() => daemon.expire_control().await,
            _ = repair.tick() => daemon.repair_network().await,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow_and_update() {
                    return;
                }
            }
        }
    }
}

#[cfg(unix)]
async fn accept_loop(
    listener: tokio::net::UnixListener,
    daemon: Arc<Daemon>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow_and_update() {
                    return;
                }
            }
            accepted = listener.accept() => {
                let (stream, _address) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        daemon.report(
                            ovvc_core::ErrorClass::Operational,
                            None,
                            "accept",
                            format!("accept failed: {error}"),
                        );
                        continue;
                    }
                };
                match peer_uid_allowed(&stream) {
                    Ok(true) => {
                        let daemon = Arc::clone(&daemon);
                        let log_daemon = Arc::clone(&daemon);
                        tokio::spawn(async move {
                            if let Err(error) = serve_connection(stream, daemon).await {
                                log_daemon.log_only(
                                    ovvc_core::ErrorClass::Operational,
                                    None,
                                    "client session",
                                    format!("client connection ended: {error}"),
                                );
                            }
                        });
                    }
                    Ok(false) => {
                        daemon.report(
                            ovvc_core::ErrorClass::Security,
                            None,
                            "client auth",
                            "refused a client from another OS user",
                        );
                    }
                    Err(error) => daemon.log_only(
                        ovvc_core::ErrorClass::Operational,
                        None,
                        "client auth",
                        format!("could not read peer credentials: {error}"),
                    ),
                }
            }
        }
    }
}

#[cfg(unix)]
async fn serve_connection(stream: UnixStream, daemon: Arc<Daemon>) -> anyhow::Result<()> {
    let (mut reader, mut writer) = stream.into_split();

    // The handshake is read under the small pre-auth cap: an unauthenticated
    // peer must not be able to make the daemon allocate a post-handshake frame
    // before it has proved which protocol it speaks.
    let first = match timeout(
        HANDSHAKE_TIMEOUT,
        read_frame_limited(&mut reader, MAX_HANDSHAKE_BYTES),
    )
    .await
    {
        Ok(result) => result.context("reading Hello")?,
        Err(_) => anyhow::bail!("timed out waiting for Hello"),
    };
    let hello = match first {
        Frame::Hello(hello) => hello,
        _ => {
            daemon.log_only(
                ovvc_core::ErrorClass::Security,
                None,
                "handshake",
                "rejected a client whose first frame was not Hello",
            );
            reject_handshake(&mut writer, "first frame was not Hello").await?;
            return Ok(());
        }
    };
    if protocol_major(&hello.protocol) != protocol_major(IPC_PROTOCOL) {
        let reason = match protocol_major(&hello.protocol) {
            Some(major) => {
                format!("client speaks IPC protocol major {major}, daemon speaks {IPC_PROTOCOL}")
            }
            None => format!(
                "client sent a protocol string that is not major.minor, daemon speaks {IPC_PROTOCOL}"
            ),
        };
        daemon.log_only(
            ovvc_core::ErrorClass::Operational,
            None,
            "handshake",
            format!("rejected an incompatible client: {reason}"),
        );
        reject_handshake(&mut writer, reason).await?;
        return Ok(());
    }
    // The registry is closed: an unknown name is almost always a typo, and
    // silently ignoring it would suppress the events the client asked for.
    let unknown = unknown_capabilities(&hello.capabilities, CLIENT_CAPABILITIES);
    if !unknown.is_empty() {
        daemon.log_only(
            ovvc_core::ErrorClass::Operational,
            None,
            "handshake",
            format!(
                "rejected unknown client capabilities: {}",
                unknown.join(", ")
            ),
        );
        reject_handshake(
            &mut writer,
            format!("unknown client capabilities: {}", unknown.join(", ")),
        )
        .await?;
        return Ok(());
    }
    let client_capabilities = hello.capabilities;

    let welcome = make_welcome(&daemon);
    write_frame_limited(&mut writer, &Frame::Welcome(welcome), MAX_HANDSHAKE_BYTES)
        .await
        .context("sending Welcome")?;

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Frame>(WRITE_QUEUE);
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = outbound_rx.recv().await {
            if write_frame(&mut writer, &frame).await.is_err() {
                return;
            }
        }
    });

    let mut events = daemon.subscribe();
    let event_tx = outbound_tx.clone();
    let forward_task = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    // A peer that did not advertise a capability cannot use
                    // those events, and old frontends should not receive a
                    // frame shape they never negotiated. Everything else is
                    // still best-effort fanout.
                    if !event_allowed(&event, &client_capabilities) {
                        continue;
                    }
                    if event_tx.send(Frame::Event(event)).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(frame) => frame,
            Err(_) => break,
        };
        match frame {
            Frame::Request { id, op } => {
                let daemon = Arc::clone(&daemon);
                let outbound = outbound_tx.clone();
                tokio::spawn(async move {
                    let shutdown_after = matches!(op, Op::ShutdownDaemon);
                    let result = daemon.dispatch(op).await;
                    let _ = outbound.send(Frame::Response { id, result }).await;
                    if shutdown_after {
                        // Let the response drain before the runtime stops.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        daemon.request_shutdown();
                    }
                });
            }
            Frame::Ping(nonce) => {
                let _ = outbound_tx.send(Frame::Pong(nonce)).await;
            }
            Frame::Goodbye => break,
            Frame::Hello(_)
            | Frame::Welcome(_)
            | Frame::Rejected { .. }
            | Frame::Response { .. }
            | Frame::Event(_)
            | Frame::Pong(_) => {
                // Not valid client-to-daemon frames after the handshake.
                break;
            }
        }
    }

    // The reader is gone, so nothing else can arrive. Give in-flight requests a
    // brief window to post their replies, then stop the forwarding tasks.
    drop(outbound_tx);
    tokio::time::sleep(Duration::from_millis(50)).await;
    forward_task.abort();
    writer_task.abort();
    Ok(())
}

#[cfg(unix)]
async fn reject_handshake(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    reason: impl Into<String>,
) -> anyhow::Result<()> {
    write_frame_limited(
        writer,
        &Frame::Rejected {
            protocol: IPC_PROTOCOL.to_string(),
            reason: reason.into(),
        },
        MAX_HANDSHAKE_BYTES,
    )
    .await
    .context("sending Rejected")?;
    Ok(())
}

/// Whether a peer that advertised `capabilities` should see `event`.
///
/// Only the two voice event classes are gated for now. The older event kinds
/// predate the capability registry, so they keep flowing to every frontend
/// rather than making an older client silently lose messages.
fn event_allowed(event: &DaemonEvent, capabilities: &[String]) -> bool {
    match event {
        DaemonEvent::Voice { .. } => capabilities_ok(capabilities, &[CAP_VOICE]),
        DaemonEvent::VoiceAudio { .. } => capabilities_ok(capabilities, &[CAP_VOICE_AUDIO]),
        _ => true,
    }
}

#[cfg(unix)]
fn make_welcome(daemon: &Daemon) -> Welcome {
    let peer_id = daemon
        .client
        .list_guilds()
        .unwrap_or_default()
        .into_iter()
        .find_map(|guild| daemon.client.peer_id(guild.id))
        .map(|peer| peer.to_string());
    let mut welcome = Welcome::for_daemon(
        env!("CARGO_PKG_VERSION"),
        daemon.client.author_id(),
        daemon.client.identity().public_key_hex(),
        peer_id,
        daemon.data_dir.display().to_string(),
        daemon.socket.display().to_string(),
    );
    // Build the advertised names from the running daemon rather than the
    // compile-time constant, so a capability the build does not serve is not
    // promised in the handshake.
    welcome.capabilities = daemon.capability_names();
    welcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_default_starts_the_tray_without_a_flag() {
        let config = ovvc_config::Config::default();
        assert!(
            config.daemon.tray,
            "DaemonConfig::default must leave the tray on"
        );
        assert!(
            tray_requested(false, config.daemon.tray),
            "the GUI and TUI spawn the daemon without --tray, so the config \
             default has to be enough"
        );
    }

    #[test]
    fn the_tray_flag_and_the_config_opt_out_combine() {
        assert!(
            tray_requested(true, false),
            "an explicit --tray still overrides a config opt-out"
        );
        assert!(tray_requested(true, true));
        assert!(
            !tray_requested(false, false),
            "daemon.tray = false must be honoured when no flag is passed"
        );
    }
}
