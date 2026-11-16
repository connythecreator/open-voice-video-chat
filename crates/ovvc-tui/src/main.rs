//! Terminal frontend for ovvc, styled like the GUI's phosphor terminal.
//!
//! Runs against the same daemon, identity and event store as the GUI and
//! `ovvc-cli`, so the three can be mixed freely.

mod app;
mod attach;
mod history;
mod input;
mod profile;
mod spinner;
mod theme;
mod ui;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use ovvc_client::Multiaddr;
use ovvc_config::{Config, LoadedConfig};
use ovvc_remote::{ConnectOptions, Frontend, RemoteClient};
use ratatui::crossterm::event::{self, Event};
use ratatui::{DefaultTerminal, try_restore};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::{Action, App, AppOptions, Notice, SPINNER_FRAME_MS};

#[derive(Parser)]
#[command(name = "ovvc-tui", about = "Terminal client for ovvc", version)]
struct Cli {
    /// Data directory. Defaults to the configuration's `data_dir`, then the
    /// platform data directory. `--data` is kept as an alias.
    #[arg(
        long = "data-dir",
        visible_alias = "data",
        env = "OVVC_DATA_DIR",
        value_name = "DIR"
    )]
    data_dir: Option<PathBuf>,

    /// Daemon socket. Defaults to the configuration's `socket`, then the
    /// daemon's discovery record or the per-user runtime path.
    #[arg(long, env = "OVVC_SOCKET", value_name = "FILE")]
    socket: Option<PathBuf>,

    /// Resolve the configuration, data directory and socket, print them as
    /// `data_dir=...` and `socket=...`, then exit without opening a terminal.
    #[arg(long)]
    print_paths: bool,

    /// Read configuration from this file instead of the user config file.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Apply this named profile from <config-dir>/profiles.
    #[arg(long, env = "OVVC_PROFILE", value_name = "NAME")]
    profile: Option<String>,

    /// Commune to open: name, name prefix, or id prefix
    #[arg(long)]
    guild: Option<String>,

    /// Address to listen on, e.g. /ip4/0.0.0.0/tcp/0 (repeatable)
    #[arg(long)]
    listen: Vec<String>,

    /// Peer to dial, e.g. /ip4/192.0.2.9/tcp/4001 (repeatable)
    #[arg(long)]
    dial: Vec<String>,

    /// Circuit-relay address used for NAT traversal
    #[arg(long)]
    relay: Option<String>,

    /// Try to map a port with UPnP
    #[arg(long)]
    upnp: bool,

    /// Try to map a port with NAT-PMP
    #[arg(long)]
    natpmp: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Load the whole layered configuration before choosing a store or socket.
    // A file, `OVVC_DATA_DIR`/`OVVC_SOCKET`, `--data-dir`/`--socket` and the
    // active profile are all resolved by the loader, so the TUI, GUI and CLI
    // agree on one data directory and one control socket.
    let loaded = load_effective_config(&cli)?;
    let data_dir = resolve_data_dir(&loaded.config)?;
    let socket = resolve_socket(&loaded.config, &data_dir);

    if cli.print_paths {
        println!("data_dir={}", data_dir.display());
        println!("socket={}", socket.display());
        return Ok(());
    }

    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating data directory {}", data_dir.display()))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the tokio runtime")?;

    runtime.block_on(run(cli, loaded.config, data_dir, socket))
}

/// Load every configuration layer with this process's explicit overrides.
fn load_effective_config(cli: &Cli) -> Result<LoadedConfig> {
    let paths = ovvc_config::ConfigPaths::discover().context("locating configuration")?;
    let overrides = ovvc_config::Overrides {
        config: cli.config.clone(),
        profile: cli.profile.clone(),
        data_dir: cli.data_dir_override(),
        socket: cli.socket.clone(),
        theme: None,
    };
    ovvc_config::load(&paths, overrides).context("loading configuration")
}

impl Cli {
    /// The data directory this process was explicitly given.
    ///
    /// `OVVC_DATA_DIR` is the documented name and is also read by the
    /// configuration loader. `OVVC_DATA` was the TUI's older name and is kept
    /// as a fallback so an existing environment is not silently ignored.
    fn data_dir_override(&self) -> Option<PathBuf> {
        self.data_dir.clone().or_else(|| {
            std::env::var_os("OVVC_DATA")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
    }
}

/// The resolved data directory: explicit override or config value, then the
/// platform default.
fn resolve_data_dir(config: &Config) -> Result<PathBuf> {
    if let Some(path) = &config.data_dir {
        return Ok(path.clone());
    }
    ovvc_ipc::default_data_dir().context("could not determine the platform data directory")
}

/// The resolved control socket: the config value, then the daemon's discovery
/// record, then the per-user runtime path.
fn resolve_socket(config: &Config, data_dir: &Path) -> PathBuf {
    config
        .socket
        .clone()
        .unwrap_or_else(|| ovvc_remote::resolve_socket_path(data_dir))
}

async fn run(cli: Cli, config: Config, data_dir: PathBuf, socket: PathBuf) -> Result<()> {
    // The palette is shared with the GUI. `client.theme_file` and
    // `client.theme` from the layered configuration win, and otherwise a
    // `theme.toml` or `OVVC_THEME` restyles both frontends from one file.
    theme::install(theme::load_configured(Some(&config.client)));

    // The daemon owns the store; the TUI only ever talks to it over the
    // socket. It is started on demand when nothing is listening.
    // The TUI is an interactive frontend, so a daemon it starts comes up with
    // the peer network on: the guilds that were online last time are resumed,
    // and the rest are connected on an ephemeral port. Without this, closing
    // and reopening the client quietly left the machine unreachable.
    let client: Arc<dyn Frontend> = Arc::new(
        RemoteClient::attach(
            &data_dir,
            ConnectOptions {
                online: true,
                // A daemon the TUI starts must read the same configuration and
                // profile the TUI was given, and listen on the data directory
                // and socket this process resolved, so the auto-spawn path
                // does not quietly fall back to the user defaults.
                socket: Some(socket),
                config: cli.config.clone(),
                profile: cli.profile.clone(),
                ..ConnectOptions::default()
            },
        )
        .with_context(|| format!("attaching to the daemon for {}", data_dir.display()))?,
    );

    let options = AppOptions {
        guild: cli.guild,
        listen: parse_addresses("listen", &cli.listen)?,
        dial: parse_addresses("dial", &cli.dial)?,
        relay: cli
            .relay
            .as_deref()
            .map(|value| parse_address(value, "--relay"))
            .transpose()?,
        upnp: cli.upnp,
        natpmp: cli.natpmp,
    };

    let (notice_tx, mut notice_rx) = unbounded_channel();
    let mut app = App::new(client.clone(), data_dir.clone(), options)?;

    let mut events = client.subscribe();
    let (input_tx, mut input_rx) = unbounded_channel();
    spawn_input_thread(input_tx);

    // Push-to-talk needs key-release events. They only arrive when the
    // terminal understands the enhanced keyboard protocol, so ask for it when
    // available and let the app fall back to press-to-toggle otherwise.
    let key_releases =
        ratatui::crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);

    // Restore the terminal before a panic message is printed; otherwise a
    // panic leaves the user's shell in raw mode with a hidden cursor.
    let panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        pop_keyboard_enhancement(key_releases);
        let _ = try_restore();
        panic_hook(info);
    }));

    let mut terminal = ratatui::try_init().context("initializing the terminal")?;
    push_keyboard_enhancement(key_releases);
    let result = event_loop(
        &mut terminal,
        &mut app,
        &mut input_rx,
        &mut notice_rx,
        &mut events,
        client,
        notice_tx,
    )
    .await;
    pop_keyboard_enhancement(key_releases);
    try_restore().context("restoring the terminal")?;

    result
}

/// Ask the terminal to report key-release events, which push-to-talk needs.
fn push_keyboard_enhancement(enabled: bool) {
    if !enabled {
        return;
    }
    use ratatui::crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
    );
}

/// Undo [`push_keyboard_enhancement`] so the shell keeps normal key handling.
fn pop_keyboard_enhancement(enabled: bool) {
    if !enabled {
        return;
    }
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::PopKeyboardEnhancementFlags
    );
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    input_rx: &mut UnboundedReceiver<Event>,
    notice_rx: &mut UnboundedReceiver<Notice>,
    events: &mut tokio::sync::broadcast::Receiver<ovvc_client::ClientEvent>,
    client: Arc<dyn Frontend>,
    notice_tx: UnboundedSender<Notice>,
) -> Result<()> {
    // Presence is a lease in the daemon's control session, so a guild that
    // stays online has to re-announce itself even when nothing happens.
    let mut presence = tokio::time::interval(Duration::from_millis(App::PRESENCE_KEEPALIVE_MS));
    presence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // The spinner only animates while a connect, sync or history load is on
    // screen. The select precondition disables this branch when idle, so a
    // quiet terminal is not redrawn ten times a second.
    let mut animation = tokio::time::interval(Duration::from_millis(SPINNER_FRAME_MS));
    animation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // Drain every client event that is already queued before drawing, so a
        // burst (joining a guild with a long history, or several peers syncing
        // at once) causes one reload and one redraw instead of one per event.
        loop {
            match events.try_recv() {
                Ok(event) => app.on_client_event_result(Ok(event)),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    app.on_client_event_result(Err(
                        tokio::sync::broadcast::error::RecvError::Lagged(skipped),
                    ));
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
        app.flush_dirty();

        terminal
            .draw(|frame| ui::draw(frame, app))
            .context("drawing the interface")?;

        // A mention rings the bell once. The draw above has flushed the
        // terminal backend, so writing the byte directly cannot tear a frame.
        if std::mem::take(&mut app.bell) {
            use std::io::Write;
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(b"\x07");
            let _ = stdout.flush();
        }

        if app.should_quit {
            break;
        }

        while let Some(action) = app.take_action() {
            dispatch(action, client.clone(), notice_tx.clone());
        }

        tokio::select! {
            _ = animation.tick(), if app.spinner_active() => app.spinner_tick(),
            _ = presence.tick() => app.presence_tick(ovvc_control::now_ms()),
            Some(event) = input_rx.recv() => app.on_event(event),
            Some(notice) = notice_rx.recv() => app.on_notice(notice),
            result = events.recv() => app.on_client_event_result(result),
            else => break,
        }
    }

    Ok(())
}

fn dispatch(action: Action, client: Arc<dyn Frontend>, notices: UnboundedSender<Notice>) {
    match action {
        Action::Connect(guild_id, options) => {
            tokio::spawn(async move {
                if let Err(error) = client.connect(guild_id, options).await {
                    let _ = notices.send(Notice::Error(format!("connect failed: {error}")));
                }
            });
        }
        Action::Join(code, options) => {
            tokio::spawn(async move {
                let outcome = match app::parse_invite(&code) {
                    Ok(invite) => client
                        .join_guild(&invite, options)
                        .await
                        .map_err(|error| anyhow::anyhow!("{error}")),
                    Err(error) => Err(error),
                };

                let _ = match outcome {
                    Ok(guild) => notices.send(Notice::Joined(guild)),
                    Err(error) => notices.send(Notice::Error(format!("join failed: {error}"))),
                };
            });
        }
        Action::LoadHistory {
            guild_id,
            channel_id,
            cursor,
        } => {
            // `history_before` is a blocking socket call, so it runs on the
            // blocking pool; the event loop keeps painting the spinner.
            tokio::spawn(async move {
                let page = tokio::task::spawn_blocking(move || {
                    client.history_before(guild_id, channel_id, Some(cursor), app::MESSAGE_PAGE)
                })
                .await;
                let page = match page {
                    Ok(Ok(page)) => Ok(page),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(error) => Err(format!("history task failed: {error}")),
                };
                let _ = notices.send(Notice::HistoryPage {
                    guild_id,
                    channel_id,
                    page,
                });
            });
        }
    }
}

fn spawn_input_thread(sender: UnboundedSender<Event>) {
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(250)) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        if sender.send(event).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
}

fn parse_addresses(flag: &str, values: &[String]) -> Result<Vec<Multiaddr>> {
    values
        .iter()
        .map(|value| parse_address(value, flag))
        .collect()
}

fn parse_address(value: &str, flag: &str) -> Result<Multiaddr> {
    value
        .parse::<Multiaddr>()
        .map_err(|error| anyhow::anyhow!("invalid --{flag} address {value}: {error}"))
}
