//! `ovvc-gui`: an egui client that attaches to the local daemon.

#[cfg(feature = "desktop")]
mod main {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use clap::Parser;
    use eframe::egui;
    use ovvc_config::Config;
    use ovvc_gui::app::App;
    use ovvc_ipc::{default_data_dir, default_socket_path, read_daemon_info};

    #[derive(Debug, Parser)]
    #[command(name = "ovvc-gui", version, about = "Desktop client for ovvc")]
    struct Args {
        /// Data directory used by the daemon. The GUI never opens the store.
        #[arg(long, env = "OVVC_DATA_DIR")]
        data_dir: Option<PathBuf>,

        /// Daemon socket. Defaults to the per-user runtime path.
        #[arg(long, env = "OVVC_SOCKET")]
        socket: Option<PathBuf>,

        /// Read configuration from this file instead of the user config file.
        /// Forwarded to a daemon this process starts.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,

        /// Apply this named profile from <config-dir>/profiles. Forwarded to a
        /// daemon this process starts.
        #[arg(long, env = "OVVC_PROFILE", value_name = "NAME")]
        profile: Option<String>,

        /// Resolve the configuration, data directory and socket, print them as
        /// `data_dir=...` and `socket=...`, then exit without opening a window.
        #[arg(long)]
        print_paths: bool,

        /// Do not start `ovvc-daemon` when nothing is listening.
        #[arg(long)]
        no_spawn_daemon: bool,

        /// Write one frame of the real window to this PNG, then exit. Useful
        /// for verifying the rendered UI on compositors that hide window
        /// pixels from external screen capture.
        #[arg(long)]
        screenshot: Option<PathBuf>,
    }

    /// Load every configuration layer with this process's explicit overrides.
    ///
    /// This runs before the window is created, because the same configuration
    /// chooses the store and socket the window attaches to. A broken config is
    /// reported instead of silently pointing the GUI at the platform default.
    fn load_effective_config(args: &Args) -> Result<ovvc_config::LoadedConfig> {
        let paths = ovvc_config::ConfigPaths::discover().context("locating configuration")?;
        let overrides = ovvc_config::Overrides {
            config: args.config.clone(),
            profile: args.profile.clone(),
            data_dir: args.data_dir.clone(),
            socket: args.socket.clone(),
            theme: None,
        };
        ovvc_config::load(&paths, overrides).context("loading configuration")
    }

    pub fn run() -> Result<()> {
        let args = Args::parse();
        let loaded = load_effective_config(&args)?;
        let config = loaded.config;
        let client_config = Some(config.client.clone());
        let data_dir = resolve_data_dir(&config)?;
        let socket = resolve_socket(&config, &data_dir);

        if args.print_paths {
            println!("data_dir={}", data_dir.display());
            println!("socket={}", socket.display());
            return Ok(());
        }

        if !args.no_spawn_daemon && !wait_for_socket(&socket, Duration::from_millis(600)) {
            spawn_daemon(
                &data_dir,
                &socket,
                args.config.as_deref(),
                args.profile.as_deref(),
            );
        }

        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title("ovvc")
                .with_inner_size([1180.0, 760.0])
                .with_min_inner_size([760.0, 480.0]),
            renderer: eframe::Renderer::Wgpu,
            ..Default::default()
        };
        let socket_for_app = socket.clone();
        let screenshot = args.screenshot;
        eframe::run_native(
            "ovvc",
            options,
            Box::new(move |context| {
                let mut app = App::new(context, socket_for_app, client_config);
                if let Some(path) = screenshot {
                    app.screenshot_to(path);
                }
                Ok(Box::new(app))
            }),
        )
        .map_err(|error| anyhow::anyhow!("eframe: {error}"))
    }

    /// The resolved data directory: explicit override or config value, then the
    /// platform default.
    fn resolve_data_dir(config: &Config) -> Result<PathBuf> {
        if let Some(path) = &config.data_dir {
            return Ok(path.clone());
        }
        default_data_dir().context("could not determine a per-user data directory")
    }

    /// The resolved control socket: the config value, then the daemon's
    /// discovery record, then the per-user runtime path.
    fn resolve_socket(config: &Config, data_dir: &Path) -> PathBuf {
        config.socket.clone().unwrap_or_else(|| {
            read_daemon_info(data_dir)
                .ok()
                .flatten()
                .map(|info| PathBuf::from(info.socket))
                .unwrap_or_else(|| default_socket_path(data_dir))
        })
    }

    fn wait_for_socket(socket: &Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if std::os::unix::net::UnixStream::connect(socket).is_ok() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Start the sibling daemon binary. The GUI stays usable if this fails; it
    /// simply shows the offline onboarding screen instead.
    fn spawn_daemon(data_dir: &Path, socket: &Path, config: Option<&Path>, profile: Option<&str>) {
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|parent| parent.join("ovvc-daemon")));
        let binary = match sibling.filter(|path| path.is_file()) {
            Some(path) => path,
            None => PathBuf::from("ovvc-daemon"),
        };
        let mut command = std::process::Command::new(&binary);
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--socket")
            .arg(socket);
        // A daemon started from this window reads the same configuration and
        // profile the window was given, so `--config` is not silently lost.
        if let Some(config) = config {
            command.arg("--config").arg(config);
        }
        if let Some(profile) = profile {
            command.arg("--profile").arg(profile);
        }
        match command
            // Opening the GUI means the person wants to be reachable. The
            // daemon resumes whatever was online last time, and connects any
            // guild it has no record for on an ephemeral port, so the status
            // bar comes up net:online on its own.
            .arg("--online")
            // The notifier is a runtime opt-in on the daemon, and the GUI's
            // "desktop notifications" toggle only has an effect when one is
            // running. The daemon still checks the stored preference per event,
            // so turning the toggle off stops the popups immediately.
            .arg("--notify")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => {
                let _ = wait_for_socket(socket, Duration::from_secs(3));
            }
            Err(error) => {
                eprintln!(
                    "ovvc-gui: could not start {}: {error}; start the daemon yourself",
                    binary.display()
                );
            }
        }
    }
}

#[cfg(feature = "desktop")]
fn main() -> anyhow::Result<()> {
    main::run()
}

#[cfg(not(feature = "desktop"))]
fn main() {
    eprintln!("ovvc-gui was built without the `desktop` feature");
}
