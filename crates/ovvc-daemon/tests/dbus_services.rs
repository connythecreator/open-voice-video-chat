//! The daemon's opt-in desktop integrations against a private session bus.
//!
//! `scripts/e2e-dbus-tray.sh` starts `dbus-run-session`, a mock
//! `org.kde.StatusNotifierWatcher`, and this test. It asserts the real
//! StatusNotifierItem properties, the dbusmenu labels, the "Open ovvc" error
//! path when no GUI binary exists, and the "Quit" item shutting the daemon
//! down cleanly.
//!
//! Run with:
//!
//! ```text
//! scripts/e2e-dbus-tray.sh
//! ```

#![cfg(unix)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const DAEMON: &str = env!("CARGO_BIN_EXE_ovvc-daemon");
const ITEM_PREFIX: &str = "org.kde.StatusNotifierItem-";

struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn enabled() -> bool {
    if std::env::var_os("OVVC_RUN_DBUS_TESTS").is_none() {
        eprintln!("skipping DBus tests; run scripts/e2e-dbus-tray.sh");
        return false;
    }
    assert!(
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some(),
        "OVVC_RUN_DBUS_TESTS is set but there is no private session bus"
    );
    true
}

fn wait_until<T>(what: &str, limit: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {limit:?} waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn gdbus(args: &[&str]) -> Output {
    Command::new("gdbus")
        .args(args)
        .output()
        .expect("gdbus is required by scripts/e2e-dbus-tray.sh")
}

fn gdbus_ok(args: &[&str]) -> String {
    let output = gdbus(args);
    assert!(
        output.status.success(),
        "gdbus {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn list_names() -> String {
    gdbus_ok(&[
        "call",
        "--session",
        "--dest",
        "org.freedesktop.DBus",
        "--object-path",
        "/org/freedesktop/DBus",
        "--method",
        "org.freedesktop.DBus.ListNames",
    ])
}

fn item_name() -> Option<String> {
    list_names()
        .split('\'')
        .find(|name| name.starts_with(ITEM_PREFIX))
        .map(str::to_string)
}

fn property(item: &str, name: &str) -> String {
    gdbus_ok(&[
        "call",
        "--session",
        "--dest",
        item,
        "--object-path",
        "/StatusNotifierItem",
        "--method",
        "org.freedesktop.DBus.Properties.Get",
        "org.kde.StatusNotifierItem",
        name,
    ])
}

fn menu_event(item: &str, id: &str) -> Output {
    gdbus(&[
        "call",
        "--session",
        "--dest",
        item,
        "--object-path",
        "/MenuBar",
        "--method",
        "com.canonical.dbusmenu.Event",
        "--",
        id,
        "clicked",
        "<'x'>",
        "0",
    ])
}

fn stderr_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn tray_publishes_status_notifier_item_and_menu_actions_work() {
    if !enabled() {
        return;
    }

    let dir: TempDir = tempfile::tempdir().expect("temp dir");
    let ready = dir.path().join("watcher.ready");
    let watcher_log = dir.path().join("watcher.log");
    let watcher_stderr = dir.path().join("watcher.err");
    let daemon_stderr = dir.path().join("daemon.err");
    let socket = dir.path().join("daemon.sock");

    let watcher_script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/status-notifier-watcher.py");
    assert!(
        watcher_script.is_file(),
        "missing watcher script at {}",
        watcher_script.display()
    );
    let watcher = Command::new("python3")
        .arg(&watcher_script)
        .env("OVVC_WATCHER_READY", &ready)
        .env("OVVC_WATCHER_LOG", &watcher_log)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            File::create(&watcher_stderr).expect("watcher stderr"),
        ))
        .spawn()
        .expect("start status notifier watcher");
    let _watcher = Guard(watcher);

    wait_until(
        "the watcher to take its name",
        Duration::from_secs(10),
        || ready.is_file().then_some(()),
    );

    let daemon = Command::new(DAEMON)
        .arg("--data-dir")
        .arg(dir.path())
        .arg("--socket")
        .arg(&socket)
        // No `--tray`: the isolated config leaves `daemon.tray` at its
        // default, which must be on. The GUI and TUI spawn the daemon without
        // `--tray`, so this is the path a real user takes.
        .arg("--config")
        .arg({
            let config = dir.path().join("daemon.toml");
            std::fs::write(
                &config,
                format!(
                    "data_dir = {:?}\nsocket = {:?}\n",
                    dir.path().display().to_string(),
                    socket.display().to_string(),
                ),
            )
            .expect("write daemon config");
            config
        })
        // Force the "Open ovvc" menu action down the spawn-error path; it
        // must log and leave the daemon running.
        .env("OVVC_GUI", "/nonexistent/ovvc-gui")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            File::create(&daemon_stderr).expect("daemon stderr"),
        ))
        .spawn()
        .expect("start ovvc-daemon");
    let mut daemon = Guard(daemon);

    wait_until("the daemon socket", Duration::from_secs(15), || {
        socket.exists().then_some(())
    });
    let item = wait_until(
        "the tray item to register",
        Duration::from_secs(15),
        item_name,
    );

    let title = property(&item, "Title");
    assert!(title.contains("ovvc-daemon"), "title was {title}");
    assert!(
        title.contains(env!("CARGO_PKG_VERSION")),
        "title did not include the daemon version: {title}"
    );
    let icon = property(&item, "IconName");
    assert!(icon.contains("utilities-terminal"), "icon was {icon}");
    let status = property(&item, "Status");
    assert!(status.contains("Active"), "status was {status}");

    let menu = gdbus_ok(&[
        "call",
        "--session",
        "--dest",
        &item,
        "--object-path",
        "/MenuBar",
        "--method",
        "com.canonical.dbusmenu.GetLayout",
        "--",
        "0",
        "-1",
        "[]",
    ]);
    assert!(menu.contains("Open ovvc"), "menu was {menu}");
    assert!(menu.contains("Quit"), "menu was {menu}");
    assert!(menu.contains("0 peers"), "menu was {menu}");
    assert!(
        menu.contains(env!("CARGO_PKG_VERSION")),
        "menu lacked the daemon version: {menu}"
    );

    // Activating "Open ovvc" with a missing binary only logs; the daemon must
    // keep serving.
    let activate = menu_event(&item, "1");
    assert!(
        activate.status.success(),
        "menu activate failed: {}",
        String::from_utf8_lossy(&activate.stderr)
    );
    wait_until(
        "the GUI spawn failure to be logged",
        Duration::from_secs(10),
        || {
            stderr_text(&daemon_stderr)
                .contains("could not open the GUI")
                .then_some(())
        },
    );
    assert!(
        daemon.0.try_wait().expect("daemon state").is_none(),
        "daemon exited after a failed GUI spawn"
    );

    // "Quit" asks the daemon to shut down through its watch channel. The
    // dbusmenu reply races the process exit, so a NoReply error is expected.
    let _ = menu_event(&item, "4");
    let status = wait_until(
        "the daemon to exit after Quit",
        Duration::from_secs(15),
        || daemon.0.try_wait().expect("daemon state"),
    );
    assert!(status.success(), "daemon exited with {status}");
    let stderr = stderr_text(&daemon_stderr);
    assert!(stderr.contains("ovvc-daemon stopped"), "stderr:\n{stderr}");

    // The watcher is torn down by its guard.
    let watcher_log = std::fs::read_to_string(&watcher_log).unwrap_or_default();
    assert!(
        watcher_log.contains("registered"),
        "the watcher did not see a registration:\n{watcher_log}"
    );
}

/// `daemon.tray = false` is the documented opt-out, so a daemon started from a
/// config that disables it must not register any StatusNotifierItem.
#[test]
fn a_daemon_with_the_tray_disabled_does_not_register() {
    if !enabled() {
        return;
    }

    let dir: TempDir = tempfile::tempdir().expect("temp dir");
    let ready = dir.path().join("watcher.ready");
    let watcher_log = dir.path().join("watcher.log");
    let watcher_stderr = dir.path().join("watcher.err");
    let daemon_stderr = dir.path().join("daemon.err");
    let socket = dir.path().join("daemon.sock");

    let watcher_script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/status-notifier-watcher.py");
    let watcher = Command::new("python3")
        .arg(&watcher_script)
        .env("OVVC_WATCHER_READY", &ready)
        .env("OVVC_WATCHER_LOG", &watcher_log)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            File::create(&watcher_stderr).expect("watcher stderr"),
        ))
        .spawn()
        .expect("start status notifier watcher");
    let _watcher = Guard(watcher);
    wait_until(
        "the watcher to take its name",
        Duration::from_secs(10),
        || ready.is_file().then_some(()),
    );

    let config = dir.path().join("daemon.toml");
    std::fs::write(
        &config,
        format!(
            "data_dir = {:?}\nsocket = {:?}\n[daemon]\ntray = false\n",
            dir.path().display().to_string(),
            socket.display().to_string(),
        ),
    )
    .expect("write daemon config");

    let daemon = Command::new(DAEMON)
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            File::create(&daemon_stderr).expect("daemon stderr"),
        ))
        .spawn()
        .expect("start ovvc-daemon");
    let mut daemon = Guard(daemon);

    wait_until("the daemon socket", Duration::from_secs(15), || {
        socket.exists().then_some(())
    });
    assert!(
        daemon.0.try_wait().expect("daemon state").is_none(),
        "the daemon exited with the tray disabled"
    );
    // Give a tray task the time it would need to register, then require that
    // no item exists and the watcher never saw one.
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        item_name().is_none(),
        "a StatusNotifierItem registered despite daemon.tray = false"
    );
    let log = std::fs::read_to_string(&watcher_log).unwrap_or_default();
    assert!(
        !log.contains("registered"),
        "the watcher recorded a registration despite tray = false:\n{log}"
    );
}
