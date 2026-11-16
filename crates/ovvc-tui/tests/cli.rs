//! The TUI binary's argument surface, exercised without a terminal.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-tui");

/// A configuration directory isolated from the developer's real environment.
struct Fixture {
    _dir: tempfile::TempDir,
    config_dir: PathBuf,
    system: PathBuf,
    data_dir: PathBuf,
    socket: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        Self {
            system: config_dir.join("system.toml"),
            data_dir: dir.path().join("custom-data"),
            socket: dir.path().join("custom.sock"),
            config_dir,
            _dir: dir,
        }
    }

    fn write_config(&self, data_dir: &Path, socket: &Path) {
        std::fs::write(
            self.config_dir.join("config.toml"),
            format!(
                "data_dir = \"{}\"\nsocket = \"{}\"\n",
                data_dir.display(),
                socket.display()
            ),
        )
        .expect("write config");
    }

    fn run(
        &self,
        extra: &[&str],
        data_dir_env: Option<&Path>,
        socket_env: Option<&Path>,
    ) -> Output {
        let config = self.config_dir.join("config.toml");
        let mut command = Command::new(BIN);
        command
            .env("OVVC_CONFIG_DIR", &self.config_dir)
            .env("OVVC_SYSTEM_CONFIG", &self.system)
            .env_remove("OVVC_DATA_DIR")
            .env_remove("OVVC_DATA")
            .env_remove("OVVC_SOCKET")
            .env_remove("OVVC_PROFILE");
        if let Some(data_dir) = data_dir_env {
            command.env("OVVC_DATA_DIR", data_dir);
        }
        if let Some(socket) = socket_env {
            command.env("OVVC_SOCKET", socket);
        }
        command
            .arg("--config")
            .arg(&config)
            .args(extra)
            .output()
            .expect("run ovvc-tui")
    }
}

fn printed(output: &Output, key: &str) -> Option<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")).map(str::to_string))
}

#[test]
fn help_and_version_are_available_without_a_terminal() {
    let help = Command::new(BIN).arg("--help").output().expect("run --help");
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("Terminal client for ovvc"));
    assert!(stdout.contains("--guild"));
    assert!(stdout.contains("--listen"));
    assert!(stdout.contains("--dial"));
    assert!(stdout.contains("--socket"));
    assert!(stdout.contains("--data-dir"));

    let version = Command::new(BIN).arg("--version").output().expect("run --version");
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("ovvc-tui"));
}

#[test]
fn an_unknown_flag_is_rejected() {
    let output = Command::new(BIN)
        .arg("--not-a-real-flag")
        .output()
        .expect("run with a bad flag");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unexpected argument") || stderr.contains("error"));
}

#[test]
fn print_paths_honours_the_config_data_dir_and_socket() {
    let fixture = Fixture::new();
    fixture.write_config(&fixture.data_dir, &fixture.socket);

    let output = fixture.run(&["--print-paths"], None, None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        printed(&output, "data_dir").as_deref(),
        Some(fixture.data_dir.display().to_string().as_str())
    );
    assert_eq!(
        printed(&output, "socket").as_deref(),
        Some(fixture.socket.display().to_string().as_str())
    );
}

#[test]
fn environment_overrides_the_config_data_dir_and_socket() {
    let fixture = Fixture::new();
    fixture.write_config(&fixture.data_dir, &fixture.socket);
    let env_data = fixture._dir.path().join("env-data");
    let env_socket = fixture._dir.path().join("env.sock");

    let output = fixture.run(&["--print-paths"], Some(&env_data), Some(&env_socket));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        printed(&output, "data_dir").as_deref(),
        Some(env_data.display().to_string().as_str())
    );
    assert_eq!(
        printed(&output, "socket").as_deref(),
        Some(env_socket.display().to_string().as_str())
    );
}
