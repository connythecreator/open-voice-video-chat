//! `ovvc doctor` must report the daemon's runtime capability report rather
//! than guessing audio from PATH or `/dev/snd`, and must never report video as
//! healthy while it is only a descriptor.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc");
const STARTUP: Duration = Duration::from_secs(15);

static DAEMON_BINARY: OnceLock<PathBuf> = OnceLock::new();

/// The daemon binary next to the CLI binary. `cargo test -p ovvc-cli` does not
/// build sibling packages, so build it once if the workspace build has not
/// already produced it.
fn daemon_binary() -> PathBuf {
    DAEMON_BINARY
        .get_or_init(|| {
            if let Some(path) = std::env::var_os("OVVC_DAEMON") {
                return PathBuf::from(path);
            }
            let cli = PathBuf::from(BIN);
            let sibling = cli.with_file_name("ovvc-daemon");
            if sibling.is_file() {
                return sibling;
            }
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "ovvc-daemon"])
                .status()
                .expect("run cargo to build ovvc-daemon");
            assert!(
                status.success(),
                "could not build ovvc-daemon for the doctor test"
            );
            sibling
        })
        .clone()
}

struct Fixture {
    _dir: TempDir,
    config_dir: PathBuf,
    data_dir: PathBuf,
    home: PathBuf,
    config_file: PathBuf,
    socket: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_dir = dir.path().join("config");
        let data_dir = dir.path().join("data");
        let run_dir = dir.path().join("run");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        std::fs::create_dir_all(&run_dir).expect("run dir");
        std::fs::create_dir_all(&home).expect("home dir");

        let socket = run_dir.join("daemon.sock");
        let config_file = dir.path().join("daemon.toml");
        std::fs::write(
            &config_file,
            format!(
                "data_dir = {:?}\nsocket = {:?}\n\
                 [audio]\n\
                 backend = \"auto\"\n\
                 [media]\n\
                 transport = \"webrtc\"\n\
                 ice_servers = []\n",
                data_dir.display().to_string(),
                socket.display().to_string(),
            ),
        )
        .expect("write config");

        Self {
            _dir: dir,
            config_dir,
            data_dir,
            home,
            config_file,
            socket,
        }
    }

    fn apply_env<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command
            .env("OVVC_CONFIG_DIR", &self.config_dir)
            .env("OVVC_SYSTEM_CONFIG", self.config_dir.join("system.toml"))
            .env("OVVC_DATA_DIR", &self.data_dir)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_RUNTIME_DIR", self.home.join("runtime"))
            .env_remove("OVVC_PROFILE")
            .env_remove("OVVC_SOCKET")
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.apply_env(&mut Command::new(BIN))
            .args(args)
            .output()
            .expect("run ovvc")
    }

    fn start_daemon(&self) -> Child {
        self.apply_env(&mut Command::new(daemon_binary()))
            .arg("--config")
            .arg(&self.config_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ovvc-daemon")
    }

    fn wait_for_socket(&self, child: &mut Child) {
        self.wait_for_path(&self.socket, child);
    }

    fn wait_for_path(&self, path: &Path, child: &mut Child) {
        let deadline = Instant::now() + STARTUP;
        loop {
            if path.exists() {
                return;
            }
            if let Ok(Some(status)) = child.try_wait() {
                panic!("daemon exited before binding its socket: {status}");
            }
            assert!(Instant::now() < deadline, "daemon did not bind its socket");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

struct DaemonGuard {
    child: Child,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn doctor_reports_runtime_capabilities_and_calls_video_a_descriptor() {
    let fixture = Fixture::new();
    let mut child = fixture.start_daemon();
    fixture.wait_for_socket(&mut child);
    let _guard = DaemonGuard { child };

    let config = fixture.config_file.to_str().expect("config path");
    let output = fixture.run(&["--config", config, "doctor", "--json"]);
    assert!(
        output.status.success(),
        "doctor failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid doctor JSON");
    let capabilities = &report["capabilities"];
    assert!(
        capabilities.is_object(),
        "doctor did not include the daemon capability report: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let backends = capabilities["audio_backends_compiled"]
        .as_array()
        .expect("compiled backend list");
    assert!(
        backends.iter().any(|name| name == "null"),
        "the null backend must be reported: {capabilities}"
    );
    let codecs = capabilities["audio_codecs"]
        .as_array()
        .expect("codec list");
    assert!(
        codecs.iter().any(|name| name == "opus"),
        "Opus must be reported: {capabilities}"
    );
    assert_eq!(
        capabilities["video_descriptor_only"], true,
        "video must be reported as descriptor only: {capabilities}"
    );
    assert!(
        capabilities["media_transports_compiled"]
            .as_array()
            .is_some_and(|transports| transports.iter().any(|name| name == "webrtc")),
        "the media transports must be reported: {capabilities}"
    );

    let checks = report["checks"].as_array().expect("check list");
    let detail = |name: &str| -> String {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .and_then(|check| check["detail"].as_str())
            .unwrap_or_else(|| panic!("no {name} check: {checks:?}"))
            .to_string()
    };
    assert!(
        detail("audio").contains("compiled backends"),
        "audio check did not use the runtime report: {}",
        detail("audio")
    );
    assert!(
        detail("media").contains("compiled transports"),
        "media check did not use the runtime report: {}",
        detail("media")
    );
    assert!(
        detail("video").contains("descriptor only"),
        "video check was not honest: {}",
        detail("video")
    );

    // The human output carries the same fields, not only the JSON.
    let human = fixture.run(&["--config", config, "doctor"]);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("compiled backends"), "{text}");
    assert!(text.contains("compiled transports"), "{text}");
    assert!(text.contains("warn: video: descriptor only"), "{text}");
}

/// A daemon started with an explicit `--socket` that the config does not name
/// must still be probed through its `daemon.json` record. Otherwise `doctor`
/// falls back to the derived default, fails to attach, and reports a running
/// daemon as stale or unreachable.
#[test]
fn doctor_probes_the_socket_recorded_by_an_explicit_socket_daemon() {
    let fixture = Fixture::new();
    // Deliberately omit `socket`: the daemon is launched with --socket below.
    std::fs::write(
        &fixture.config_file,
        format!(
            "data_dir = {:?}\n\
             [audio]\n\
             backend = \"auto\"\n\
             [media]\n\
             transport = \"webrtc\"\n\
             ice_servers = []\n",
            fixture.data_dir.display().to_string(),
        ),
    )
    .expect("write config without a socket");

    let explicit = fixture._dir.path().join("run").join("explicit.sock");
    let mut child = fixture
        .apply_env(&mut Command::new(daemon_binary()))
        .arg("--config")
        .arg(&fixture.config_file)
        .arg("--socket")
        .arg(&explicit)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ovvc-daemon");
    fixture.wait_for_path(&explicit, &mut child);
    let _guard = DaemonGuard { child };

    let config = fixture.config_file.to_str().expect("config path");
    let output = fixture.run(&["--config", config, "doctor", "--json"]);
    assert!(
        output.status.success(),
        "doctor failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid doctor JSON");
    let daemon_check = report["checks"]
        .as_array()
        .expect("check list")
        .iter()
        .find(|check| check["name"] == "daemon")
        .expect("daemon check");
    assert_eq!(
        daemon_check["level"], "ok",
        "doctor did not attach through the daemon record: {daemon_check}"
    );
    let detail = daemon_check["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains(&explicit.display().to_string()),
        "doctor probed the wrong socket: {detail}"
    );
    assert!(
        report["capabilities"].is_object(),
        "doctor lost the capability report: {report}"
    );
}
