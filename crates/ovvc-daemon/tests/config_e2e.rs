//! Proves that the daemon takes its data directory and socket from a config
//! file passed with `--config`, using a real process and the real IPC client.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-daemon --test config_e2e -- --test-threads=1
//! ```

#![cfg(unix)]
// The serial guard deliberately spans the whole (async) test so one daemon
// scenario runs at a time; the test runtime is single-threaded, so the guard
// cannot block the executor it is running on.
#![allow(clippy::await_holding_lock)]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ovvc_ipc::{DaemonClient, Op, Reply};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-daemon");
const STARTUP: Duration = Duration::from_secs(15);
const SHUTDOWN: Duration = Duration::from_secs(10);

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct DaemonProcess {
    child: Child,
    _root: TempDir,
    data_dir: PathBuf,
    socket: PathBuf,
}

impl DaemonProcess {
    fn start() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        // The explicit file lives outside the auto-discovered config directory,
        // and no user config file exists, so only `--config` can supply these
        // paths. HOME and the XDG variables point into the temp root so a
        // regression that ignores the flag cannot reach the developer's own
        // data directory or socket.
        let config_dir = root.path().join("config");
        let inputs = root.path().join("inputs");
        let data_dir = root.path().join("data");
        let run_dir = root.path().join("run");
        let home = root.path().join("home");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&inputs).expect("inputs dir");
        std::fs::create_dir_all(&run_dir).expect("run dir");
        std::fs::create_dir_all(home.join("runtime")).expect("home runtime dir");

        let socket = run_dir.join("daemon.sock");
        let config_file = inputs.join("explicit.toml");
        std::fs::write(
            &config_file,
            format!(
                "data_dir = {:?}\nsocket = {:?}\n",
                data_dir.display().to_string(),
                socket.display().to_string()
            ),
        )
        .expect("write config");

        // Clear the environment bindings so the config file is the only thing
        // choosing the data directory and socket.
        let child = Command::new(BIN)
            .arg("--config")
            .arg(&config_file)
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
            data_dir,
            socket,
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

#[tokio::test(flavor = "multi_thread")]
async fn daemon_reads_data_dir_and_socket_from_config() {
    let _guard = serial();
    let mut daemon = DaemonProcess::start();
    assert!(
        daemon.wait_for_socket(STARTUP).await,
        "daemon did not bind the configured socket {}",
        daemon.socket.display()
    );

    let client = DaemonClient::new(&daemon.socket);
    client
        .wait_connected(STARTUP)
        .await
        .expect("daemon handshake");

    let status = match client.call(Op::Status).await {
        Ok(Reply::Status(status)) => status,
        Ok(other) => panic!("unexpected reply: {other:?}"),
        Err(error) => panic!("daemon returned an error: {error:?}"),
    };
    assert_eq!(status.data_dir, daemon.data_dir.display().to_string());
    assert_eq!(status.socket, daemon.socket.display().to_string());

    match client.call(Op::ShutdownDaemon).await {
        Ok(Reply::Unit) => {}
        other => panic!("unexpected shutdown reply: {other:?}"),
    }
    assert!(
        daemon.wait_exit(SHUTDOWN).await,
        "daemon did not stop after ShutdownDaemon"
    );
}
