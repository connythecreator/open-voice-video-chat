//! Runtime capability and backend-feature wiring, against real daemon
//! processes.
//!
//! The audit found that the daemon could not report which audio backends,
//! transports, codecs and features it compiled, so a default (null) build and
//! a PipeWire build were indistinguishable from the outside, and a configured
//! `pipewire` backend on a default build joined a call with no audio service.
//!
//! These tests start the shipped binary and assert its `Op::Capabilities`
//! report. Run the default and live forms:
//!
//! ```text
//! cargo test -p ovvc-daemon --test feature_wiring -- --test-threads=1
//! cargo test -p ovvc-daemon --test feature_wiring --features pipewire -- --test-threads=1
//! ```

#![cfg(unix)]
#![allow(clippy::await_holding_lock)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ovvc_ipc::{CAP_CAPABILITIES, DaemonCapabilities, DaemonClient, Op, Reply};
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

fn pipewire_build() -> bool {
    cfg!(all(target_os = "linux", feature = "pipewire"))
}

struct DaemonProcess {
    child: Child,
    _root: TempDir,
    socket: PathBuf,
}

impl DaemonProcess {
    /// Start a daemon whose config selects `backend`.
    fn start(backend: &str) -> Self {
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
                 backend = {:?}\n\
                 engine = \"opus\"\n\
                 sample_rate = 48000\n\
                 frame_ms = 20\n\
                 profile = \"voice-standard\"\n\
                 [media]\n\
                 transport = \"webrtc\"\n\
                 ice_servers = []\n",
                data_dir.display().to_string(),
                socket.display().to_string(),
                backend,
            ),
        )
        .expect("write config");

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
            if self.child.try_wait().ok().flatten().is_some() {
                return false;
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

    /// The daemon's stderr, for a process that has already exited.
    fn stderr(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    }

    async fn shutdown(&mut self, client: &DaemonClient) {
        let _ = client.call(Op::ShutdownDaemon).await;
        assert!(
            self.wait_exit(SHUTDOWN).await,
            "daemon did not exit after ShutdownDaemon"
        );
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

async fn connect(daemon: &mut DaemonProcess) -> DaemonClient {
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

async fn capabilities(client: &DaemonClient) -> DaemonCapabilities {
    match client.call(Op::Capabilities).await {
        Ok(Reply::Capabilities(capabilities)) => capabilities,
        other => panic!("unexpected capability reply: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_default_build_reports_null_and_a_pipewire_build_reports_auto() {
    let _guard = serial();
    let mut daemon = DaemonProcess::start("auto");
    let client = connect(&mut daemon).await;

    // The handshake must advertise the new op so an older daemon is never
    // sent a request it cannot parse.
    let state = client.state();
    assert!(
        state.capabilities.iter().any(|name| name == CAP_CAPABILITIES),
        "Welcome did not advertise the capability report: {:?}",
        state.capabilities
    );

    let report = capabilities(&client).await;
    assert!(
        report.audio_backends_compiled.contains(&"null".to_string()),
        "the null backend must always compile: {report:?}"
    );
    assert_eq!(
        report.audio_codecs,
        vec!["opus".to_string()],
        "only Opus is implemented, never Codec2 or the lower profiles"
    );
    assert_eq!(
        report.media_transports_compiled,
        vec!["webrtc".to_string(), "libp2p".to_string()],
        "both media transports must be reported"
    );
    assert_eq!(report.media_transport_selected, "webrtc");
    assert!(
        report.video_descriptor_only,
        "video has no live path and must say so: {report:?}"
    );
    assert!(
        report.features.iter().any(|feature| feature == "e2ee"),
        "per-call E2EE must be reported: {report:?}"
    );

    if pipewire_build() {
        assert!(
            report
                .audio_backends_compiled
                .contains(&"pipewire".to_string()),
            "a pipewire build must report its backend: {report:?}"
        );
        assert_eq!(
            report.audio_backend_selected, "auto",
            "auto may fall back to null when the device cannot open, so it must not claim \
             pipewire before a call: {report:?}"
        );
        assert!(
            report
                .features
                .iter()
                .any(|feature| feature == "live-audio"),
            "a pipewire build must report live audio: {report:?}"
        );
    } else {
        assert_eq!(
            report.audio_backends_compiled,
            vec!["null".to_string()],
            "a default build must not claim PipeWire: {report:?}"
        );
        assert_eq!(
            report.audio_backend_selected, "null",
            "auto must resolve to the silent null backend and say so"
        );
        assert!(
            !report
                .features
                .iter()
                .any(|feature| feature == "live-audio"),
            "a default build must not claim live audio: {report:?}"
        );
    }

    daemon.shutdown(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_pipewire_backend_without_the_feature_refuses_to_start() {
    let _guard = serial();
    let mut daemon = DaemonProcess::start("pipewire");

    if pipewire_build() {
        // The feature is compiled: the daemon starts and reports the
        // configured backend, without opening a device until a call.
        let client = connect(&mut daemon).await;
        let report = capabilities(&client).await;
        assert_eq!(report.audio_backend_selected, "pipewire");
        assert!(
            report
                .audio_backends_compiled
                .contains(&"pipewire".to_string())
        );
        daemon.shutdown(&client).await;
    } else {
        assert!(
            daemon.wait_exit(STARTUP).await,
            "a default daemon must refuse audio.backend = \"pipewire\" instead of starting"
        );
        let status = daemon
            .child
            .try_wait()
            .expect("daemon status")
            .expect("daemon exited");
        assert!(
            !status.success(),
            "the daemon must exit non-zero when the backend is not compiled"
        );
        let stderr = daemon.stderr();
        assert!(
            stderr.contains("without the pipewire feature"),
            "the failure must name the missing feature: {stderr}"
        );
        assert!(
            stderr.contains("pipewire"),
            "the failure must name the configured backend: {stderr}"
        );
    }
}
