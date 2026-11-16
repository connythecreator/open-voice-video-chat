//! The daemon's system tray icon.
//!
//! The tray uses the freedesktop/KDE StatusNotifierItem protocol through
//! [`ksni`], which talks over the session DBus. It is on by default
//! (`daemon.tray` defaults to true) so a daemon started directly, or spawned
//! by the GUI or TUI, has an Open GUI entry and a Quit daemon entry without
//! needing a task manager. `daemon.tray = false` opts out, and a daemon whose
//! configuration has no tray host only logs that the tray is unavailable and
//! keeps running.
//!
//! GNOME's stock shell does not render StatusNotifierItem icons; a user who
//! wants the tray there needs a StatusNotifier/AppIndicator shell extension
//! (for example `gnome-shell-extension-appindicator`). Nothing in the daemon
//! depends on the extension: without it the tray is simply not displayed,
//! while IPC shutdown, SIGTERM and Ctrl-C still stop the daemon.
//!
//! The icon is an embedded 3x3 loading-grid pixmap as well as the
//! `utilities-terminal` icon name, so a host with no matching theme icon still
//! shows the ovvc mark.

#[cfg(feature = "tray")]
mod enabled {
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::Arc;

    use ksni::TrayMethods;
    use tokio::sync::{broadcast, watch};

    use crate::daemon::Daemon;

    /// Side length of the embedded tray icon, in pixels.
    const ICON_SIZE: i32 = 16;

    /// Neon green used by the loading grid, as red, green and blue bytes.
    const NEON_GREEN: [u8; 3] = [0x39, 0xFF, 0x14];

    /// The tray menu's disabled status line.
    fn status_label(peers: usize) -> String {
        let noun = if peers == 1 { "peer" } else { "peers" };
        format!(
            "{peers} {noun}  |  ovvc-daemon {}",
            env!("CARGO_PKG_VERSION")
        )
    }

    /// Build the embedded 3x3 loading-grid icon for the tray.
    ///
    /// The mark is baked into the binary instead of relying on a theme icon
    /// name, so the same icon appears on every desktop. It is a 16x16
    /// transparent canvas with nine neon-green 4x4 cells, one pixel apart.
    /// `ksni::Icon::data` is ARGB32 in network byte order, which is the byte
    /// order written here: alpha, red, green, blue.
    fn loading_grid_icon() -> ksni::Icon {
        let size = ICON_SIZE as usize;
        let mut data = vec![0u8; size * size * 4];
        // Cells start at 1, 6 and 11: a one-pixel margin, four pixels of
        // cell, then a one-pixel gap.
        let cell_start = |index: usize| 1 + index * 5;
        for row in 0..3 {
            for column in 0..3 {
                for y in cell_start(row)..cell_start(row) + 4 {
                    for x in cell_start(column)..cell_start(column) + 4 {
                        let offset = (y * size + x) * 4;
                        data[offset] = 0xFF;
                        data[offset + 1] = NEON_GREEN[0];
                        data[offset + 2] = NEON_GREEN[1];
                        data[offset + 3] = NEON_GREEN[2];
                    }
                }
            }
        }
        ksni::Icon {
            width: ICON_SIZE,
            height: ICON_SIZE,
            data,
        }
    }

    pub struct OvvcTray {
        peers: usize,
        shutdown: watch::Sender<bool>,
        data_dir: PathBuf,
        socket: PathBuf,
    }

    impl OvvcTray {
        /// Start the GUI against this daemon. If the binary cannot be found or
        /// spawned, the menu item is a no-op apart from a log line.
        fn open_gui(&self) {
            let binary = gui_binary();
            match Command::new(&binary)
                .arg("--data-dir")
                .arg(&self.data_dir)
                .arg("--socket")
                .arg(&self.socket)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(_) => {}
                Err(error) => eprintln!(
                    "ovvc-daemon: could not open the GUI ({}): {error}",
                    binary.display()
                ),
            }
        }
    }

    impl ksni::Tray for OvvcTray {
        fn id(&self) -> String {
            "ovvc-daemon".to_string()
        }

        fn title(&self) -> String {
            format!("ovvc-daemon {}", env!("CARGO_PKG_VERSION"))
        }

        fn icon_name(&self) -> String {
            // Kept as a fallback for hosts that only render theme icons.
            "utilities-terminal".to_string()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            vec![loading_grid_icon()]
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            self.open_gui();
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::StandardItem;
            vec![
                StandardItem {
                    // Keep "Open ovvc" in the label: the GUI opens the ovvc
                    // window, and the existing DBus tray test looks for it.
                    label: "Open ovvc GUI".to_string(),
                    icon_name: "utilities-terminal".to_string(),
                    activate: Box::new(|tray: &mut Self| tray.open_gui()),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: status_label(self.peers),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: "Quit daemon".to_string(),
                    icon_name: "application-exit".to_string(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.shutdown.send(true);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    /// Resolve the GUI binary: `OVVC_GUI`, then a sibling of this executable,
    /// then `ovvc-gui` on `PATH`.
    fn gui_binary() -> PathBuf {
        if let Some(path) = std::env::var_os("OVVC_GUI") {
            return PathBuf::from(path);
        }
        if let Some(sibling) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|parent| parent.join("ovvc-gui")))
            .filter(|path| path.is_file())
        {
            return sibling;
        }
        PathBuf::from("ovvc-gui")
    }

    /// Create the tray in a background task and keep the handle alive for the
    /// daemon's lifetime.
    pub fn start(
        data_dir: PathBuf,
        socket: PathBuf,
        shutdown: watch::Sender<bool>,
        daemon: Arc<Daemon>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let mut shutdown_rx = shutdown.subscribe();
        Some(tokio::spawn(async move {
            let tray = OvvcTray {
                peers: daemon.peer_count(),
                shutdown,
                data_dir,
                socket,
            };
            let handle = match tray.spawn().await {
                Ok(handle) => handle,
                Err(error) => {
                    eprintln!(
                        "ovvc-daemon: system tray unavailable: {error}; \
                         continuing without a tray"
                    );
                    return;
                }
            };

            let mut events = daemon.subscribe();
            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow_and_update() {
                            let _ = handle.shutdown().await;
                            return;
                        }
                    }
                    event = events.recv() => match event {
                        Ok(
                            ovvc_ipc::DaemonEvent::PeerConnected { .. }
                            | ovvc_ipc::DaemonEvent::PeerDisconnected { .. }
                            | ovvc_ipc::DaemonEvent::Synced { .. },
                        ) => {
                            let peers = daemon.peer_count();
                            let _ = handle.update(move |tray| tray.peers = peers).await;
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                }
            }
        }))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn tray(peers: usize) -> OvvcTray {
            let (shutdown, _) = watch::channel(false);
            OvvcTray {
                peers,
                shutdown,
                data_dir: PathBuf::from("/tmp/ovvc"),
                socket: PathBuf::from("/tmp/ovvc/daemon.sock"),
            }
        }

        #[test]
        fn status_label_reports_peer_count_and_version() {
            assert!(status_label(0).contains("0 peers"));
            assert!(status_label(1).contains("1 peer"));
            assert!(status_label(1).contains(env!("CARGO_PKG_VERSION")));
        }

        #[test]
        fn menu_has_open_a_disabled_status_line_and_quit_daemon() {
            let tray = tray(3);
            let menu = ksni::Tray::menu(&tray);
            assert_eq!(menu.len(), 4);

            match &menu[0] {
                ksni::MenuItem::Standard(item) => {
                    assert_eq!(item.label, "Open ovvc GUI");
                    assert!(item.enabled);
                }
                _ => panic!("the first item should be the Open entry"),
            }
            match &menu[1] {
                ksni::MenuItem::Standard(item) => {
                    assert_eq!(item.label, status_label(3));
                    assert!(!item.enabled, "the status line must not be clickable");
                }
                _ => panic!("the second item should be the status line"),
            }
            assert!(matches!(&menu[2], ksni::MenuItem::Separator));
            match &menu[3] {
                ksni::MenuItem::Standard(item) => {
                    assert_eq!(item.label, "Quit daemon");
                    assert!(item.enabled);
                }
                _ => panic!("the last item should be Quit daemon"),
            }
        }

        #[test]
        fn quit_sends_the_shutdown_signal() {
            let mut tray = tray(0);
            let mut shutdown = tray.shutdown.subscribe();
            let activate = match ksni::Tray::menu(&tray).pop() {
                Some(ksni::MenuItem::Standard(item)) => item.activate,
                _ => panic!("the last item should be Quit daemon"),
            };
            activate(&mut tray);
            assert!(*shutdown.borrow_and_update());
        }

        #[test]
        fn the_tray_advertises_the_daemon_identity() {
            let tray = tray(0);
            assert_eq!(ksni::Tray::id(&tray), "ovvc-daemon");
            assert_eq!(
                ksni::Tray::title(&tray),
                format!("ovvc-daemon {}", env!("CARGO_PKG_VERSION"))
            );
            assert_eq!(ksni::Tray::icon_name(&tray), "utilities-terminal");
        }

        #[test]
        fn the_icon_is_a_non_empty_loading_grid_pixmap() {
            let icon = loading_grid_icon();
            assert_eq!(icon.width, ICON_SIZE);
            assert_eq!(icon.height, ICON_SIZE);
            assert_eq!(icon.data.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);

            // A transparent corner, and one opaque neon-green pixel from the
            // first cell. The alpha byte comes first in ARGB32 network order.
            assert_eq!(&icon.data[0..4], &[0, 0, 0, 0]);
            let first_cell = ((ICON_SIZE + 1) * 4) as usize;
            assert_eq!(icon.data[first_cell], 0xFF);
            assert_eq!(&icon.data[first_cell + 1..first_cell + 4], &NEON_GREEN);

            // Nine 4x4 cells and nothing else is opaque.
            let opaque = icon
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[0] == 0xFF)
                .count();
            assert_eq!(opaque, 9 * 4 * 4);

            // The trait method exposes it to the tray host.
            let pixmaps = ksni::Tray::icon_pixmap(&tray(0));
            assert_eq!(pixmaps.len(), 1);
            assert!(!pixmaps[0].data.is_empty());
            assert_eq!(pixmaps[0].data.len(), icon.data.len());
        }

        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        #[test]
        fn gui_binary_uses_the_override_and_reports_spawn_failures() {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
            // SAFETY: this test holds ENV_LOCK and is the only test that
            // mutates the environment.
            unsafe {
                std::env::remove_var("OVVC_GUI");
            }
            assert!(gui_binary().to_string_lossy().ends_with("ovvc-gui"));

            unsafe {
                std::env::set_var("OVVC_GUI", "/bin/true");
            }
            assert_eq!(gui_binary(), PathBuf::from("/bin/true"));
            let mut tray = tray(0);
            ksni::Tray::activate(&mut tray, 0, 0);

            unsafe {
                std::env::set_var("OVVC_GUI", "/nonexistent/ovvc-gui");
            }
            ksni::Tray::activate(&mut tray, 0, 0);
            assert_eq!(gui_binary(), PathBuf::from("/nonexistent/ovvc-gui"));

            unsafe {
                std::env::remove_var("OVVC_GUI");
            }
        }
    }
}

#[cfg(not(feature = "tray"))]
pub fn start(
    _data_dir: std::path::PathBuf,
    _socket: std::path::PathBuf,
    _shutdown: tokio::sync::watch::Sender<bool>,
    _daemon: std::sync::Arc<crate::daemon::Daemon>,
) -> Option<tokio::task::JoinHandle<()>> {
    eprintln!(
        "ovvc-daemon: system tray unavailable: this build has no `tray` feature; \
         continuing without a tray"
    );
    None
}

#[cfg(feature = "tray")]
pub use enabled::start;
