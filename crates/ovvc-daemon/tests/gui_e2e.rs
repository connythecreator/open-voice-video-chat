//! End-to-end test for the GUI client path.
//!
//! This is the closest a CI machine gets to running the desktop app: the real
//! `ovvc-daemon` binary is started as its own process, the real [`Bridge`]
//! connects over the Unix socket, replies and live events flow into
//! [`AppState`], and the real egui layout renders a headless frame. A break in
//! the daemon, the IPC client, the bridge or the widget code fails here.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-daemon --test gui_e2e -- --test-threads=1
//! ```

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use egui::{Context, RawInput, Rect, pos2, vec2};
use ovvc_client::{ChannelKind, QuietHours};
use ovvc_control::{PresenceStatus, SessionId};
use ovvc_gui::bridge::Bridge;
use ovvc_gui::state::{AppState, Message, RequestTag};
use ovvc_ipc::{NetworkOptionsDto, Op};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-daemon");
const BUDGET: Duration = Duration::from_secs(20);

/// Attach a `RemoteClient` to a daemon that is already running, the way the
/// TUI does, without letting it spawn a second daemon.
fn attach_remote(socket: &Path, data_dir: &Path) -> ovvc_remote::RemoteClient {
    let options = ovvc_remote::ConnectOptions {
        spawn_daemon: false,
        socket: Some(socket.to_path_buf()),
        ..Default::default()
    };
    ovvc_remote::RemoteClient::attach(data_dir, options).expect("attach to daemon")
}

/// One daemon test at a time keeps mDNS and the loopback ports quiet, and the
/// shared socket paths easier to reason about on failure.
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

    fn wait_ready(&self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if std::os::unix::net::UnixStream::connect(&self.socket).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    fn wait_exit(&mut self, budget: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + budget;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                _ => return None,
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
        let _ = &self.dir;
    }
}

/// Pump bridge messages into the state until `done` is happy or the budget
/// runs out.
fn pump(
    bridge: &mut Bridge,
    state: &mut AppState,
    budget: Duration,
    mut done: impl FnMut(&AppState) -> bool,
) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        for message in bridge.poll() {
            state.apply_message(message);
        }
        if done(state) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn render_frame(ctx: &Context, state: &mut AppState) {
    let input = RawInput {
        screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1180.0, 760.0))),
        ..Default::default()
    };
    for _ in 0..2 {
        let mut output = ctx.run_ui(input.clone(), |ui| {
            let mut actions = Vec::new();
            ovvc_gui::ui::draw(ui, state, &mut actions);
        });
        output.textures_delta.clear();
    }
}

#[test]
fn gui_drives_a_real_daemon_from_connect_to_shutdown() {
    let _serial = serial();
    let ctx = Context::default();
    ovvc_gui::theme::install(&ctx);

    let mut daemon = DaemonProcess::start();
    assert!(
        daemon.wait_ready(BUDGET),
        "ovvc-daemon did not create its socket"
    );
    let mut bridge = Bridge::start(daemon.socket().to_path_buf(), ctx.clone());
    let mut state = AppState::new();

    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state.connected),
        "the bridge never reported a daemon connection"
    );

    bridge.send(Op::Identity, RequestTag::Identity);
    bridge.send(Op::Status, RequestTag::Status);
    bridge.send(Op::ListGuilds, RequestTag::Guilds);
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.identity.is_some() && state.status.is_some()
        }),
        "identity and status replies never arrived"
    );
    let identity = state.identity.expect("identity");

    bridge.send(
        Op::CreateGuild {
            name: "gui e2e".to_string(),
        },
        RequestTag::CreateGuild,
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.guilds.len() == 1 && state.selected_guild.is_some()
        }),
        "the guild was not created, state: {:?}",
        state.guilds
    );
    let guild_id = state.selected_guild.expect("guild");

    bridge.send(
        Op::ListChannels { guild_id },
        RequestTag::Channels(guild_id),
    );
    bridge.send(Op::ListMembers { guild_id }, RequestTag::Members(guild_id));
    bridge.send(
        Op::ConnectionState { guild_id },
        RequestTag::Connection(guild_id),
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            !state.channels.is_empty() && state.selected_channel.is_some()
        }),
        "the default channel never appeared"
    );
    let channel_id = state.selected_channel.expect("channel");

    bridge.send(
        Op::History {
            guild_id,
            channel_id,
            limit: 200,
        },
        RequestTag::History {
            guild_id,
            channel_id,
        },
    );
    bridge.send(
        Op::ControlJoin {
            guild_id,
            session_id: SessionId::for_guild(guild_id),
            display_name: "GUI Tester".to_string(),
        },
        RequestTag::JoinControl {
            guild_id,
            session_id: SessionId::for_guild(guild_id),
        },
    );

    bridge.send(
        Op::PostMessageFull {
            guild_id,
            channel_id,
            body: "hello from the gui e2e".to_string(),
            reply_to: None,
            mentions: vec![identity],
        },
        RequestTag::SendMessage,
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state
                .messages
                .iter()
                .any(|message| message.body.contains("hello from the gui e2e"))
        }),
        "the posted message never reached the GUI state"
    );
    let message = state
        .messages
        .iter()
        .find(|message| message.body.contains("hello from the gui e2e"))
        .expect("message")
        .clone();

    bridge.send(
        Op::AddReaction {
            guild_id,
            target: message.event_id,
            emoji: "\u{1f44d}".to_string(),
        },
        RequestTag::Reaction,
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state
                .messages
                .iter()
                .any(|message| !message.reactions.is_empty())
        }),
        "the reaction never reached the GUI state"
    );

    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.control.is_some()
        }),
        "the control session never joined, state: {:?}",
        state.control
    );

    // The full layout runs headless against live daemon data.
    render_frame(&ctx, &mut state);

    // Every request above also left the daemon alive and responsive.
    bridge.send(Op::Status, RequestTag::Status);
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state
                .status
                .as_ref()
                .is_some_and(|status| status.uptime_secs < 600)
        }),
        "the daemon stopped answering status requests"
    );

    bridge.send(Op::ShutdownDaemon, RequestTag::Shutdown);
    let status = daemon
        .wait_exit(BUDGET)
        .expect("the daemon did not exit after ShutdownDaemon");
    assert!(status.success(), "daemon exited with {status}");
    assert!(
        !daemon.socket().exists(),
        "the shutdown left the socket behind"
    );
}

#[test]
fn gui_takes_a_guild_online_and_shows_listen_addresses() {
    let _serial = serial();
    let ctx = Context::default();
    ovvc_gui::theme::install(&ctx);

    let mut daemon = DaemonProcess::start();
    assert!(
        daemon.wait_ready(BUDGET),
        "ovvc-daemon did not create its socket"
    );
    let mut bridge = Bridge::start(daemon.socket().to_path_buf(), ctx.clone());
    let mut state = AppState::new();
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state.connected),
        "the bridge never reported a daemon connection"
    );

    bridge.send(
        Op::CreateGuild {
            name: "net e2e".to_string(),
        },
        RequestTag::CreateGuild,
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.selected_guild.is_some()
        }),
        "the guild was not created"
    );
    let guild_id = state.selected_guild.expect("guild");
    assert!(!state.is_online(guild_id));

    // The same request the [ go online ] button emits from the network tab.
    bridge.send(
        Op::Connect {
            guild_id,
            options: NetworkOptionsDto {
                listen: vec!["/ip4/127.0.0.1/tcp/0".to_string()],
                dial: Vec::new(),
                relay: None,
                upnp: false,
                natpmp: false,
            },
        },
        RequestTag::Connect(guild_id),
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.is_online(guild_id)
                && state
                    .connection
                    .as_ref()
                    .is_some_and(|connection| !connection.listen_addresses.is_empty())
        }),
        "the guild never came online, connection: {:?}",
        state.connection
    );

    bridge.send(
        Op::Disconnect { guild_id },
        RequestTag::Disconnect(guild_id),
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            !state.is_online(guild_id)
                && state
                    .connection
                    .as_ref()
                    .is_some_and(|connection| connection.listen_addresses.is_empty())
        }),
        "the guild stayed online after disconnect, connection: {:?}",
        state.connection
    );

    bridge.send(Op::ShutdownDaemon, RequestTag::Shutdown);
    daemon
        .wait_exit(BUDGET)
        .expect("the daemon did not exit after ShutdownDaemon");
}

#[test]
fn an_idle_gui_keeps_its_presence_lease_alive() {
    let _serial = serial();
    let ctx = Context::default();
    ovvc_gui::theme::install(&ctx);

    let mut daemon = DaemonProcess::start();
    assert!(
        daemon.wait_ready(BUDGET),
        "ovvc-daemon did not create its socket"
    );
    let mut bridge = Bridge::start(daemon.socket().to_path_buf(), ctx.clone());
    let mut state = AppState::new();
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state.connected),
        "the bridge never reported a daemon connection"
    );

    bridge.send(Op::Identity, RequestTag::Identity);
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state
            .identity
            .is_some()),
        "the identity never arrived"
    );
    let author = state.identity.expect("identity");

    bridge.send(
        Op::CreateGuild {
            name: "presence e2e".to_string(),
        },
        RequestTag::CreateGuild,
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.selected_guild.is_some()
        }),
        "the guild was not created"
    );
    let guild_id = state.selected_guild.expect("guild");

    bridge.send(
        Op::Connect {
            guild_id,
            options: NetworkOptionsDto {
                listen: vec!["/ip4/127.0.0.1/tcp/0".to_string()],
                dial: Vec::new(),
                relay: None,
                upnp: false,
                natpmp: false,
            },
        },
        RequestTag::Connect(guild_id),
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.is_online(guild_id)
        }),
        "the guild never came online, connection: {:?}",
        state.connection
    );

    let session_id = SessionId::for_guild(guild_id);
    bridge.send(
        Op::ControlJoin {
            guild_id,
            session_id,
            display_name: "e2e".to_string(),
        },
        RequestTag::JoinControl {
            guild_id,
            session_id,
        },
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state
            .control
            .is_some()),
        "the control session never joined, state: {:?}",
        state.control
    );

    // Idle for longer than two lease windows, the point where the daemon marks
    // a silent member offline. The keepalive is the same call the GUI's request
    // loop makes, so this fails if either the gate or the op is wrong.
    let started = Instant::now();
    let mut sent = 0;
    while started.elapsed() < Duration::from_secs(9) {
        if let Some(op) = state.presence_keepalive(ovvc_control::now_ms(), guild_id) {
            bridge.send(op, RequestTag::SetPresence);
            sent += 1;
        }
        pump(&mut bridge, &mut state, Duration::from_millis(200), |_| {
            false
        });
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(
        sent >= 3,
        "only {sent} keepalives went out over nine seconds"
    );

    bridge.send(
        Op::ControlSnapshot {
            guild_id,
            session_id,
        },
        RequestTag::Control {
            guild_id,
            session_id,
        },
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.control.as_ref().is_some_and(|snapshot| {
                snapshot.roster.iter().any(|member| {
                    member.author == author && member.presence != PresenceStatus::Offline
                })
            })
        }),
        "the daemon expired an idle member despite {sent} keepalives: {:?}",
        state.control
    );

    bridge.send(Op::ShutdownDaemon, RequestTag::Shutdown);
    daemon
        .wait_exit(BUDGET)
        .expect("the daemon did not exit after ShutdownDaemon");
}

/// The other half of the presence test: without the keepalive the daemon does
/// expire the member, which is why the GUI has to send one.
#[test]
fn a_silent_member_is_expired_after_two_lease_windows() {
    let _serial = serial();
    let ctx = Context::default();
    ovvc_gui::theme::install(&ctx);

    let mut daemon = DaemonProcess::start();
    assert!(
        daemon.wait_ready(BUDGET),
        "ovvc-daemon did not create its socket"
    );
    let mut bridge = Bridge::start(daemon.socket().to_path_buf(), ctx.clone());
    let mut state = AppState::new();
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state.connected),
        "the bridge never reported a daemon connection"
    );
    bridge.send(Op::Identity, RequestTag::Identity);
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state
            .identity
            .is_some()),
        "the identity never arrived"
    );
    let author = state.identity.expect("identity");
    bridge.send(
        Op::CreateGuild {
            name: "presence expiry".to_string(),
        },
        RequestTag::CreateGuild,
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.selected_guild.is_some()
        }),
        "the guild was not created"
    );
    let guild_id = state.selected_guild.expect("guild");
    let session_id = SessionId::for_guild(guild_id);
    bridge.send(
        Op::ControlJoin {
            guild_id,
            session_id,
            display_name: "e2e".to_string(),
        },
        RequestTag::JoinControl {
            guild_id,
            session_id,
        },
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state
            .control
            .is_some()),
        "the control session never joined"
    );

    // Say nothing for longer than the two-window lease.
    let until = Instant::now() + Duration::from_secs(9);
    while Instant::now() < until {
        pump(&mut bridge, &mut state, Duration::from_millis(200), |_| {
            false
        });
        std::thread::sleep(Duration::from_millis(250));
    }

    bridge.send(
        Op::ControlSnapshot {
            guild_id,
            session_id,
        },
        RequestTag::Control {
            guild_id,
            session_id,
        },
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.control.as_ref().is_some_and(|snapshot| {
                snapshot.roster.iter().any(|member| {
                    member.author == author && member.presence == PresenceStatus::Offline
                })
            })
        }),
        "a silent member was never expired, so the keepalive test proves nothing: {:?}",
        state.control
    );

    bridge.send(Op::ShutdownDaemon, RequestTag::Shutdown);
    daemon
        .wait_exit(BUDGET)
        .expect("the daemon did not exit after ShutdownDaemon");
}

#[test]
fn gui_surfaces_a_refused_join_in_the_error_strip() {
    let _serial = serial();
    let ctx = Context::default();
    ovvc_gui::theme::install(&ctx);

    let mut daemon = DaemonProcess::start();
    assert!(
        daemon.wait_ready(BUDGET),
        "ovvc-daemon did not create its socket"
    );
    let mut bridge = Bridge::start(daemon.socket().to_path_buf(), ctx.clone());
    let mut state = AppState::new();
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state.connected),
        "the bridge never reported a daemon connection"
    );

    // A malformed invite is refused by the real daemon. The GUI must keep the
    // reason, because the error strip is the only place it is ever shown.
    bridge.send(
        Op::JoinGuild {
            invite: "not-an-invite".to_string(),
            options: NetworkOptionsDto {
                listen: Vec::new(),
                dial: Vec::new(),
                relay: None,
                upnp: false,
                natpmp: false,
            },
        },
        RequestTag::JoinGuild,
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state
            .error
            .is_some()),
        "a refused join left no error in the GUI state"
    );

    // The frame that draws the strip runs without panicking.
    render_frame(&ctx, &mut state);
    assert!(state.error.is_some());

    bridge.send(Op::ShutdownDaemon, RequestTag::Shutdown);
    daemon
        .wait_exit(BUDGET)
        .expect("the daemon did not exit after ShutdownDaemon");
}

#[test]
fn gui_survives_a_daemon_that_is_not_running_and_reconnects() {
    let _serial = serial();
    let ctx = Context::default();
    ovvc_gui::theme::install(&ctx);

    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("daemon.sock");

    // No daemon yet: the GUI must render its offline screen instead of
    // blocking or panicking.
    let mut bridge = Bridge::start(socket.clone(), ctx.clone());
    let mut state = AppState::new();
    std::thread::sleep(Duration::from_millis(300));
    let _ = pump(&mut bridge, &mut state, Duration::from_millis(200), |_| {
        false
    });
    assert!(!state.connected);
    render_frame(&ctx, &mut state);

    // Start the daemon against the same socket and the bridge reconnects.
    let mut daemon = DaemonProcess::start_at(&dir, &socket);
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state.connected),
        "the bridge did not reconnect to a daemon started later"
    );
    render_frame(&ctx, &mut state);

    bridge.send(Op::ShutdownDaemon, RequestTag::Shutdown);
    let status = daemon
        .wait_exit(BUDGET)
        .expect("the daemon did not exit after ShutdownDaemon");
    assert!(status.success(), "daemon exited with {status}");
}

#[test]
fn gui_reconnects_after_the_daemon_is_killed_and_restarted() {
    let _serial = serial();
    let ctx = Context::default();
    ovvc_gui::theme::install(&ctx);
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("daemon.sock");

    let mut daemon = DaemonProcess::start_at(&dir, &socket);
    let mut bridge = Bridge::start(socket.clone(), ctx.clone());
    let mut state = AppState::new();
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state.connected),
        "the bridge never connected to the first daemon"
    );
    bridge.send(
        Op::CreateGuild {
            name: "survivor".to_string(),
        },
        RequestTag::CreateGuild,
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.guilds.len() == 1 && state.selected_guild.is_some()
        }),
        "the guild was not created"
    );
    let guild_id = state.selected_guild.expect("guild");
    bridge.send(
        Op::ListChannels { guild_id },
        RequestTag::Channels(guild_id),
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.selected_channel.is_some()
        }),
        "the default channel never appeared"
    );
    let channel_id = state.selected_channel.expect("channel");
    bridge.send(
        Op::PostMessageFull {
            guild_id,
            channel_id,
            body: "survived a crash".to_string(),
            reply_to: None,
            mentions: Vec::new(),
        },
        RequestTag::SendMessage,
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state
                .messages
                .iter()
                .any(|message| message.body == "survived a crash")
        }),
        "the message was not stored before the crash"
    );

    // Kill the daemon with no clean shutdown, the way a crash looks.
    daemon.child.kill().expect("kill ovvc-daemon");
    let status = daemon.wait_exit(BUDGET).expect("the daemon did not die");
    assert!(!status.success(), "a killed daemon should not exit cleanly");
    assert!(
        pump(&mut bridge, &mut state, Duration::from_secs(10), |state| {
            !state.connected
        }),
        "the GUI never noticed the daemon going away"
    );

    // A restarted daemon owns the same data dir and socket. The bridge must
    // reconnect on its own, without the GUI process restarting.
    let mut daemon = DaemonProcess::start_at(&dir, &socket);
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state.connected),
        "the bridge did not reconnect after the daemon restart"
    );

    // Reload history so the assertion reads from the restarted daemon's store,
    // not the state left over from before the crash.
    state.messages.clear();
    state.loaded_channel = None;
    bridge.send(
        Op::History {
            guild_id,
            channel_id,
            limit: 200,
        },
        RequestTag::History {
            guild_id,
            channel_id,
        },
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.loaded_channel == Some(channel_id)
        }),
        "history did not reload from the restarted daemon"
    );
    assert!(
        state
            .messages
            .iter()
            .any(|message| message.body == "survived a crash"),
        "the message did not survive the daemon restart: {:?}",
        state.messages
    );
    render_frame(&ctx, &mut state);

    bridge.send(Op::ShutdownDaemon, RequestTag::Shutdown);
    let status = daemon
        .wait_exit(BUDGET)
        .expect("the restarted daemon did not exit");
    assert!(status.success(), "daemon exited with {status}");
}

#[test]
fn the_daemon_refuses_an_invite_with_no_dialable_address() {
    let _serial = serial();
    let ctx = Context::default();
    ovvc_gui::theme::install(&ctx);

    let mut daemon = DaemonProcess::start();
    assert!(
        daemon.wait_ready(BUDGET),
        "ovvc-daemon did not create its socket"
    );
    let mut bridge = Bridge::start(daemon.socket().to_path_buf(), ctx.clone());
    let mut state = AppState::new();
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| state.connected),
        "the bridge never reported a daemon connection"
    );
    bridge.send(
        Op::CreateGuild {
            name: "invite gate".to_string(),
        },
        RequestTag::CreateGuild,
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state.selected_guild.is_some()
        }),
        "the guild was not created"
    );
    let guild_id = state.selected_guild.expect("guild");

    // A wildcard listen address is what a node reports before it knows its
    // LAN address, and no peer can dial it, so an invite made from it is dead
    // on arrival. The CLI reaches this op too, so the gate belongs in the
    // daemon and not only in the GUI button.
    bridge.send(
        Op::CreateInvite {
            guild_id,
            peers: vec!["/ip4/0.0.0.0/tcp/41000".to_string()],
            expires_in_secs: None,
        },
        RequestTag::CreateInvite(guild_id),
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            state
                .error
                .as_deref()
                .is_some_and(|error| error.contains("dialable"))
        }),
        "the daemon minted an invite from a wildcard address, error: {:?}",
        state.error
    );
    assert!(
        state.invite_code.is_empty(),
        "a refused invite still produced a code: {:?}",
        state.invite_code
    );

    // The control: one dialable address among the wildcard ones is enough, so
    // the gate is not simply refusing every invite.
    state.error = None;
    bridge.send(
        Op::CreateInvite {
            guild_id,
            peers: vec![
                "/ip4/0.0.0.0/tcp/41000".to_string(),
                "/ip4/192.0.2.10/tcp/41000".to_string(),
            ],
            expires_in_secs: None,
        },
        RequestTag::CreateInvite(guild_id),
    );
    assert!(
        pump(&mut bridge, &mut state, BUDGET, |state| {
            !state.invite_code.is_empty()
        }),
        "a dialable invite was not minted, error: {:?}",
        state.error
    );

    bridge.send(Op::ShutdownDaemon, RequestTag::Shutdown);
    daemon
        .wait_exit(BUDGET)
        .expect("the daemon did not exit after ShutdownDaemon");
}

/// The TUI talks to the daemon through `ovvc_remote::RemoteClient`, not the
/// GUI's bridge, so its control join and presence keepalive need their own
/// real-daemon check. This walks the same ops the TUI sends: join the session,
/// announce Away, keep announcing every two seconds, then stop and watch the
/// lease expire.
#[test]
fn a_tui_style_remote_client_holds_its_presence_lease() {
    let _serial = serial();
    let daemon = DaemonProcess::start();
    assert!(
        daemon.wait_ready(BUDGET),
        "ovvc-daemon did not create its socket"
    );

    let client = attach_remote(daemon.socket(), daemon.dir.path());
    let guild = client.create_guild("tui presence").expect("guild");
    client
        .connect(
            guild.id,
            ovvc_client::NetworkOptions {
                listen: vec!["/ip4/127.0.0.1/tcp/0".parse().expect("listen address")],
                dial: Vec::new(),
                relay: None,
                upnp: false,
                natpmp: false,
            },
        )
        .expect("connect");

    let session = SessionId::for_guild(guild.id);
    client
        .control_join(guild.id, session, "tui-user")
        .expect("control join");
    client
        .set_presence(guild.id, PresenceStatus::Away)
        .expect("announce away");

    // The TUI's two-second tick, played by hand for a little over two lease
    // windows.
    for _ in 0..5 {
        std::thread::sleep(Duration::from_secs(2));
        client
            .set_presence(guild.id, PresenceStatus::Away)
            .expect("keepalive presence");
    }

    // A second client watches the roster, the way another frontend would.
    let observer = attach_remote(daemon.socket(), daemon.dir.path());
    let snapshot = observer
        .control_snapshot(guild.id, session)
        .expect("snapshot");
    let member = snapshot
        .roster
        .iter()
        .find(|member| member.author == client.author_id())
        .expect("the joined member is in the roster");
    assert_eq!(
        member.display_name, "tui-user",
        "the join frame carried the wrong display name"
    );
    assert_eq!(
        member.presence,
        PresenceStatus::Away,
        "ten seconds of keepalives did not hold the member Away"
    );

    // Stop announcing. The lease is two three-second windows, so seven seconds
    // of silence must be enough for the daemon to mark the member offline.
    // Subscribe first: the expiry is a `Presence` event with no control frame
    // behind it, and the TUI paints from that stream.
    let mut expiry_events = observer.subscribe();
    std::thread::sleep(Duration::from_secs(7));
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut expiry_seen = false;
    while Instant::now() < deadline && !expiry_seen {
        match expiry_events.try_recv() {
            Ok(ovvc_client::ClientEvent::Presence {
                guild_id,
                author,
                status: PresenceStatus::Offline,
            }) if guild_id == guild.id && author == client.author_id() => expiry_seen = true,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("event stream failed: {error}"),
        }
    }
    assert!(
        expiry_seen,
        "the expired lease never reached a frontend as a Presence event, \
         so the TUI would keep painting the member Away"
    );
    let snapshot = observer
        .control_snapshot(guild.id, session)
        .expect("snapshot after silence");
    let member = snapshot
        .roster
        .iter()
        .find(|member| member.author == client.author_id())
        .expect("the member is still in the roster");
    assert_eq!(
        member.presence,
        PresenceStatus::Offline,
        "silence did not expire the lease, so the keepalive half proves nothing"
    );

    // `DaemonProcess` kills and reaps the daemon on drop.
    drop(observer);
    drop(client);
    drop(daemon);
}

/// The TUI sets its online state and status line from the `Listening` event,
/// and calls `ControlJoin` from that arm. This proves the daemon emits the
/// event to a `RemoteClient` subscriber at all, which no other test covers:
/// the GUI polls `ConnectionState` and would stay green with this path broken.
#[test]
fn a_remote_client_receives_the_listening_event() {
    let _serial = serial();
    let daemon = DaemonProcess::start();
    assert!(
        daemon.wait_ready(BUDGET),
        "ovvc-daemon did not create its socket"
    );

    let client = attach_remote(daemon.socket(), daemon.dir.path());
    let guild = client.create_guild("listen event").expect("guild");
    // Subscribe before connecting, the way a frontend does at startup.
    let mut events = client.subscribe();
    client
        .connect(
            guild.id,
            ovvc_client::NetworkOptions {
                listen: vec!["/ip4/127.0.0.1/tcp/0".parse().expect("listen address")],
                dial: Vec::new(),
                relay: None,
                upnp: false,
                natpmp: false,
            },
        )
        .expect("connect");

    let deadline = Instant::now() + BUDGET;
    let mut listened = None;
    while Instant::now() < deadline && listened.is_none() {
        match events.try_recv() {
            Ok(ovvc_client::ClientEvent::Listening { guild_id, address })
                if guild_id == guild.id =>
            {
                listened = Some(address);
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("event stream failed: {error}"),
        }
    }
    let address = listened.expect("no Listening event reached a RemoteClient subscriber");
    assert!(
        address.to_string().contains("/tcp/"),
        "the event carried a useless address: {address}"
    );

    drop(client);
    drop(daemon);
}

impl DaemonProcess {
    fn start_at(dir: &TempDir, socket: &Path) -> Self {
        let child = Command::new(BIN)
            .arg("--data-dir")
            .arg(dir.path())
            .arg("--socket")
            .arg(socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ovvc-daemon");
        Self {
            child,
            // The caller owns the real directory; this empty one only satisfies
            // the field so `Drop` stays uniform.
            dir: tempfile::tempdir().expect("temp dir"),
            socket: socket.to_path_buf(),
        }
    }
}

#[test]
fn two_frontends_do_not_overwrite_each_others_settings() {
    let _guard = serial();
    let daemon = DaemonProcess::start();
    assert!(daemon.wait_ready(BUDGET), "the daemon did not start");
    let socket = daemon.socket().to_path_buf();
    let data_dir = daemon.dir.path().to_path_buf();
    // Two independent sockets, the way the GUI and the TUI attach at once.
    let gui = attach_remote(&socket, &data_dir);
    let tui = attach_remote(&socket, &data_dir);

    let guild = gui.create_guild("Settings").expect("create guild");
    let channel = gui.default_channel(guild.id).expect("default channel");
    let quiet = QuietHours {
        start_minute: 22 * 60,
        end_minute: 7 * 60,
    };

    // Each frontend changes a different field. The daemon merges each change
    // into its current copy; neither writer holds a stale whole file.
    gui.set_do_not_disturb(true).expect("set dnd");
    tui.set_channel_muted(channel.id, true)
        .expect("mute channel");
    gui.set_quiet_hours(Some(quiet)).expect("set quiet hours");
    tui.set_desktop_notifications(false).expect("desktop off");
    tui.set_guild_muted(guild.id, true).expect("mute guild");

    let expected = ovvc_core::NotificationSettings {
        desktop: false,
        dnd: true,
        quiet_hours: Some(quiet),
        muted_guilds: vec![guild.id],
        muted_channels: vec![channel.id],
    };
    assert_eq!(
        gui.notification_settings().expect("read settings"),
        expected,
        "the two frontends clobbered each other's changes"
    );
    assert_eq!(
        tui.notification_settings().expect("read settings"),
        expected,
        "the second frontend sees a different answer"
    );

    // The merged state is what a restart would read from the file.
    let text = std::fs::read_to_string(daemon.dir.path().join("notifications.json"))
        .expect("read notifications.json");
    let stored: ovvc_core::NotificationSettings =
        serde_json::from_str(&text).expect("parse notifications.json");
    assert_eq!(stored, expected);

    // Clearing one field leaves the rest alone.
    gui.set_channel_muted(channel.id, false)
        .expect("unmute channel");
    let after = tui.notification_settings().expect("read settings");
    assert!(after.muted_channels.is_empty());
    assert!(after.dnd && !after.desktop && after.muted_guilds == vec![guild.id]);
}

#[test]
fn the_gui_bridge_stages_sends_and_fetches_an_attachment() {
    let _guard = serial();
    let daemon = DaemonProcess::start();
    assert!(daemon.wait_ready(BUDGET), "the daemon did not start");
    let socket = daemon.socket().to_path_buf();
    let data_dir = daemon.dir.path().to_path_buf();
    let remote = attach_remote(&socket, &data_dir);
    let guild = remote.create_guild("Attachments").expect("create guild");
    let channel = remote.default_channel(guild.id).expect("default channel");

    let cache = daemon.dir.path().join("cache");
    let mut bridge = Bridge::start_with_cache(socket, Context::default(), cache.clone());
    let mut state = AppState::new();

    let source = daemon.dir.path().join("recipe.txt");
    std::fs::write(&source, b"two eggs\n").expect("write the file");

    bridge.stage_attachment(source.clone());
    let deadline = Instant::now() + BUDGET;
    let mut staged = None;
    while Instant::now() < deadline && staged.is_none() {
        for message in bridge.poll() {
            match message {
                Message::AttachmentStaged {
                    result: Ok(file), ..
                } => staged = Some(file),
                other => state.apply_message(other),
            }
        }
        if staged.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let staged = staged.unwrap_or_else(|| {
        panic!(
            "the file was never staged; the UI error is {:?}",
            state.error
        )
    });
    assert_eq!(staged.attachment.name, "recipe.txt");
    assert_eq!(staged.attachment.size, 9);
    assert_eq!(staged.attachment.mime, "text/plain");

    bridge.send_rich(
        guild.id,
        channel.id,
        "with a file".to_string(),
        None,
        Vec::new(),
        vec![staged.clone()],
        1,
    );
    let deadline = Instant::now() + BUDGET;
    let mut send_result = None;
    while Instant::now() < deadline && send_result.is_none() {
        for message in bridge.poll() {
            match message {
                Message::Reply {
                    tag: RequestTag::SendRichMessage(1),
                    result,
                } => send_result = Some(result),
                other => state.apply_message(other),
            }
        }
        if send_result.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let sent = send_result
        .unwrap_or_else(|| panic!("the send never answered; UI error {:?}", state.error))
        .unwrap_or_else(|error| panic!("the send failed: {error}"));
    assert!(
        matches!(sent, ovvc_ipc::Reply::Event(_)),
        "the daemon acknowledged the send without an event: {sent:?}"
    );

    // A separate client reads the announced metadata back from history.
    let events = remote
        .history(guild.id, channel.id, 20)
        .expect("read history");
    let announced = events
        .iter()
        .find_map(|event| {
            let files = event.body.kind.attachments();
            (!files.is_empty()).then(|| files[0].clone())
        })
        .expect("the sent message announces no file");
    assert_eq!(announced, staged.attachment);

    // Fetching writes the bytes into the bridge's cache, not the user's.
    bridge.save_attachment(guild.id, announced.clone());
    let deadline = Instant::now() + BUDGET;
    let mut saved = None;
    while Instant::now() < deadline && saved.is_none() {
        for message in bridge.poll() {
            // Deliberately not applied to the UI: that would spawn `xdg-open`
            // on the test machine.
            if let Message::AttachmentSaved { name, result } = message {
                saved = Some((name, result));
            }
        }
        if saved.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let (name, path) = saved.expect("the fetch never completed");
    assert_eq!(name, "recipe.txt");
    let path = path.expect("the fetch failed");
    assert!(
        path.starts_with(&cache),
        "the download went outside the cache: {}",
        path.display()
    );
    assert!(
        path.file_name()
            .map(|name| name.to_string_lossy().contains("recipe.txt"))
            .unwrap_or(false),
        "the saved file lost its announced name: {}",
        path.display()
    );
    assert_eq!(
        std::fs::read(&path).expect("read the saved file"),
        b"two eggs\n"
    );
}

#[test]
fn gui_bridge_covers_organisation_profiles_search_and_read_state() {
    let _serial = serial();
    let ctx = Context::default();
    ovvc_gui::theme::install(&ctx);

    let mut daemon = DaemonProcess::start();
    assert!(daemon.wait_ready(BUDGET), "daemon did not become ready");
    let mut bridge = Bridge::start(daemon.socket().to_path_buf(), ctx.clone());
    let mut state = AppState::new();
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| state.connected));

    bridge.send(Op::Identity, RequestTag::Identity);
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| state.identity.is_some()));
    let me = state.identity.expect("identity");

    bridge.send(
        Op::CreateGuild {
            name: "frontend coverage".to_string(),
        },
        RequestTag::CreateGuild,
    );
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state.selected_guild.is_some()
    }));
    let guild_id = state.selected_guild.expect("guild");
    bridge.send(
        Op::ListChannels { guild_id },
        RequestTag::Channels(guild_id),
    );
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state.selected_channel.is_some()
    }));
    let general = state.selected_channel.expect("general");

    // Channels: category, a typed child, rename and move all round-trip.
    bridge.send(
        Op::CreateCategory {
            guild_id,
            name: "projects".to_string(),
        },
        RequestTag::CreateCategory(guild_id),
    );
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state.categories.iter().any(|category| category.name == "projects")
    }));
    let category = state
        .categories
        .iter()
        .find(|category| category.name == "projects")
        .expect("category")
        .id;

    bridge.send(
        Op::CreateChannelIn {
            guild_id,
            name: "standup".to_string(),
            kind: ChannelKind::Voice,
            parent: Some(category),
        },
        RequestTag::CreateChannel(guild_id),
    );
    bridge.send(Op::ChannelTree { guild_id }, RequestTag::ChannelTree(guild_id));
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state.channels.iter().any(|channel| {
            channel.name == "standup"
                && channel.kind == ChannelKind::Voice
                && channel.parent == Some(category)
        })
    }));
    let standup = state
        .channels
        .iter()
        .find(|channel| channel.name == "standup")
        .expect("standup")
        .id;

    bridge.send(
        Op::RenameChannel {
            guild_id,
            channel_id: standup,
            name: "daily".to_string(),
        },
        RequestTag::RenameChannel(guild_id),
    );
    bridge.send(Op::ChannelTree { guild_id }, RequestTag::ChannelTree(guild_id));
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state.channels.iter().any(|channel| channel.name == "daily")
    }));

    bridge.send(
        Op::MoveChannel {
            guild_id,
            channel_id: standup,
            parent: None,
        },
        RequestTag::MoveChannel(guild_id),
    );
    bridge.send(Op::ChannelTree { guild_id }, RequestTag::ChannelTree(guild_id));
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state
            .channels
            .iter()
            .any(|channel| channel.name == "daily" && channel.parent.is_none())
    }));

    // A message, an edit, a reaction pair and a search.
    bridge.send(
        Op::PostMessageFull {
            guild_id,
            channel_id: general,
            body: "organisation message".to_string(),
            reply_to: None,
            mentions: vec![],
        },
        RequestTag::SendMessage,
    );
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state
            .messages
            .iter()
            .any(|message| message.body == "organisation message")
    }));
    let message = state
        .messages
        .iter()
        .find(|message| message.body == "organisation message")
        .expect("message")
        .clone();

    bridge.send(
        Op::EditMessage {
            guild_id,
            target: message.event_id,
            body: "organisation edited".to_string(),
        },
        RequestTag::EditMessage,
    );
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state.messages.iter().any(|message| {
            message.event_id == message.event_id
                && message.body == "organisation edited"
                && message.edited
        })
    }));

    bridge.send(
        Op::AddReaction {
            guild_id,
            target: message.event_id,
            emoji: "\u{1f44d}".to_string(),
        },
        RequestTag::Reaction,
    );
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state
            .messages
            .iter()
            .any(|message| message.reacted_by("\u{1f44d}", me))
    }));

    bridge.send(
        Op::RemoveReaction {
            guild_id,
            target: message.event_id,
            emoji: "\u{1f44d}".to_string(),
        },
        RequestTag::Reaction,
    );
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state
            .messages
            .iter()
            .all(|message| !message.reacted_by("\u{1f44d}", me))
    }));

    bridge.send(
        Op::Search {
            guild_id,
            query: "organisation edited".to_string(),
            limit: 10,
        },
        RequestTag::Search(guild_id),
    );
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state
            .search_hits
            .iter()
            .any(|hit| hit.body.contains("organisation edited"))
    }));

    // Read markers: after marking the channel read, unread is zero.
    bridge.send(
        Op::MarkRead {
            guild_id,
            channel_id: general,
            event_id: message.event_id,
        },
        RequestTag::MarkRead,
    );
    bridge.send(
        Op::UnreadCounts { guild_id },
        RequestTag::Unread(guild_id),
    );
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state.unread.get(&general).copied().unwrap_or(0) == 0
    }));

    // Profiles and the DM list.
    bridge.send(
        Op::SetProfile {
            guild_id,
            about: "frontend tested".to_string(),
            pronouns: "they/them".to_string(),
            links: vec!["https://example.com".to_string()],
        },
        RequestTag::SetProfile,
    );
    // Wait for the save before asking for the derived profile, so the read
    // cannot race the append on the daemon.
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state.notice.as_deref() == Some("profile saved")
    }));
    // The profile panel drops replies for a dialog it is not showing, so put
    // the state in the same shape a click on "my profile" would.
    state.profile_guild = Some(guild_id);
    state.profile_author = Some(me);
    state.profile_loading = true;
    bridge.send(
        Op::GetMember {
            guild_id,
            author: me,
        },
        RequestTag::MemberProfile {
            guild_id,
            author: me,
        },
    );
    let profile_ok = pump(&mut bridge, &mut state, BUDGET, |state| {
        state
            .profile
            .as_ref()
            .is_some_and(|profile| profile.about == "frontend tested")
    });
    assert!(
        profile_ok,
        "error={:?} profile={:?} guild={:?} author={:?} loading={} stale={} notice={:?}",
        state.error,
        state.profile.as_ref().map(|profile| profile.about.clone()),
        state.profile_guild,
        state.profile_author,
        state.profile_loading,
        state.profile_stale,
        state.notice
    );

    bridge.send(Op::ListDirectMessages, RequestTag::DirectMessages);
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state.direct_messages.is_empty()
    }));

    // Delete last, because the search index no longer returns it afterwards.
    bridge.send(
        Op::DeleteMessage {
            guild_id,
            target: message.event_id,
        },
        RequestTag::DeleteMessage,
    );
    assert!(pump(&mut bridge, &mut state, BUDGET, |state| {
        state
            .messages
            .iter()
            .any(|message| message.event_id == message.event_id && message.deleted)
    }));

    render_frame(&ctx, &mut state);

    bridge.send(Op::ShutdownDaemon, RequestTag::Shutdown);
    let status = daemon.wait_exit(BUDGET).expect("daemon exited");
    assert!(status.success());
}
