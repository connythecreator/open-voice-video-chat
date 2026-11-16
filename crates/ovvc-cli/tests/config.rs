//! Exercises the layered-configuration and doctor commands as separate
//! processes, so each invocation reads only the environment this test gives it.

use std::path::PathBuf;
use std::process::{Command, Output};

use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc");

struct Fixture {
    _dir: TempDir,
    config_dir: PathBuf,
    data_dir: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_dir = dir.path().join("config");
        let data_dir = dir.path().join("data");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        std::fs::create_dir_all(&home).expect("home dir");
        Self {
            _dir: dir,
            config_dir,
            data_dir,
            home,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
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
            .env_remove("OVVC_THEME");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("run ovvc")
    }

    fn write_user(&self, text: &str) {
        std::fs::write(self.config_dir.join("config.toml"), text).expect("write user config");
    }
}

fn describe(output: &Output) -> String {
    format!(
        "status: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn config_path_lists_the_three_paths() {
    let fixture = Fixture::new();
    let output = fixture.run(&["config", "path"]);
    assert!(output.status.success(), "{}", describe(&output));
    let text = stdout(&output);
    assert!(
        text.contains(&fixture.config_dir.join("config.toml").display().to_string()),
        "{text}"
    );
    assert!(
        text.contains(&fixture.config_dir.join("profiles").display().to_string()),
        "{text}"
    );
    assert!(
        text.contains(&fixture.config_dir.join("system.toml").display().to_string()),
        "{text}"
    );
}

#[test]
fn config_validate_accepts_defaults_and_rejects_a_bad_frame_size() {
    let fixture = Fixture::new();

    let output = fixture.run(&["config", "validate"]);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(stdout(&output).contains("configuration ok"), "{}", describe(&output));

    fixture.write_user("[audio]\nframe_ms = 25\n");
    let output = fixture.run(&["config", "validate"]);
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        stderr(&output).contains("audio.frame_ms"),
        "{}",
        describe(&output)
    );
}

#[test]
fn config_show_json_is_valid_json_and_names_the_active_profile() {
    let fixture = Fixture::new();
    let profiles = fixture.config_dir.join("profiles");
    std::fs::create_dir_all(&profiles).expect("profiles dir");
    std::fs::write(profiles.join("work.toml"), "[client]\ntheme = \"ice\"\n")
        .expect("profile");

    let output = fixture.run(&["config", "show", "--json", "--profile", "work"]);
    assert!(output.status.success(), "{}", describe(&output));
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON configuration");
    assert_eq!(value["profile"], "work");
    assert_eq!(value["client"]["theme"], "ice");
}

#[test]
fn global_data_dir_and_socket_flags_reach_the_config() {
    let fixture = Fixture::new();
    let flag_data = fixture._dir.path().join("flag-data");
    let flag_socket = fixture._dir.path().join("flag.sock");

    let output = fixture.run(&[
        "--data-dir",
        flag_data.to_str().expect("utf-8 data dir"),
        "--socket",
        flag_socket.to_str().expect("utf-8 socket"),
        "config",
        "show",
        "--json",
    ]);
    assert!(output.status.success(), "{}", describe(&output));
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON configuration");
    assert_eq!(
        value["data_dir"],
        flag_data.display().to_string(),
        "{}",
        describe(&output)
    );
    assert_eq!(
        value["socket"],
        flag_socket.display().to_string(),
        "{}",
        describe(&output)
    );
}

#[test]
fn config_init_writes_once_and_requires_force_to_replace() {
    let fixture = Fixture::new();
    let path = fixture.config_dir.join("config.toml");

    let output = fixture.run(&["config", "init"]);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(path.is_file(), "config init did not write {}", path.display());
    assert!(stdout(&output).contains("wrote"), "{}", describe(&output));

    let output = fixture.run(&["config", "init"]);
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(stderr(&output).contains("already exists"), "{}", describe(&output));

    let output = fixture.run(&["config", "init", "--force"]);
    assert!(output.status.success(), "{}", describe(&output));
}

#[test]
fn config_diff_lists_an_overridden_key() {
    let fixture = Fixture::new();
    fixture.write_user("[audio]\nframe_ms = 40\n");

    let output = fixture.run(&["config", "diff"]);
    assert!(output.status.success(), "{}", describe(&output));
    let text = stdout(&output);
    assert!(text.contains("audio.frame_ms = 40"), "{text}");
}

#[test]
fn doctor_json_reports_checks_and_is_ok_without_a_daemon() {
    let fixture = Fixture::new();
    let output = fixture.run(&["doctor", "--json"]);
    assert!(output.status.success(), "{}", describe(&output));

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON doctor report");
    assert_eq!(value["ok"], true, "{}", describe(&output));
    let checks = value["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("checks is not an array: {}", describe(&output)));
    let names: Vec<&str> = checks
        .iter()
        .filter_map(|check| check["name"].as_str())
        .collect();
    assert!(names.contains(&"configuration"), "{names:?}");
    assert!(names.contains(&"data-dir"), "{names:?}");
    assert!(
        value["config_sources"].is_array(),
        "{}",
        describe(&output)
    );
}

/// A valid TURN configuration, so `media.turn_credential` is actually in
/// force rather than only named in a file the loader would reject.
const TURN_SECRET: &str = "turn-password-9c1f";

fn write_turn_config(fixture: &Fixture) {
    fixture.write_user(&format!(
        "[media]\n\
         ice_servers = [\"turn:turn.example.org:3478\"]\n\
         turn_username = \"alice\"\n\
         turn_credential = \"{TURN_SECRET}\"\n"
    ));
}

#[test]
fn config_show_masks_the_turn_credential_by_default() {
    let fixture = Fixture::new();
    write_turn_config(&fixture);

    let output = fixture.run(&["config", "show"]);
    assert!(output.status.success(), "{}", describe(&output));
    let text = stdout(&output);
    assert!(
        !text.contains(TURN_SECRET),
        "the TURN credential leaked into config show:\n{text}"
    );
    assert!(
        text.contains("turn_credential = \"***\""),
        "the masked credential is not visible:\n{text}"
    );

    let output = fixture.run(&["config", "show", "--reveal-secrets"]);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(
        stdout(&output).contains(TURN_SECRET),
        "--reveal-secrets did not print the credential:\n{}",
        describe(&output)
    );

    let output = fixture.run(&["config", "show", "--json"]);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(
        !stdout(&output).contains(TURN_SECRET),
        "the TURN credential leaked into config show --json:\n{}",
        describe(&output)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON configuration");
    assert_eq!(
        value["media"]["turn_credential"],
        "***",
        "{}",
        describe(&output)
    );

    let output = fixture.run(&["config", "show", "--json", "--reveal-secrets"]);
    assert!(output.status.success(), "{}", describe(&output));
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON configuration");
    assert_eq!(
        value["media"]["turn_credential"],
        TURN_SECRET,
        "--reveal-secrets did not print the credential: {}",
        describe(&output)
    );
}

#[test]
fn config_diff_masks_the_turn_credential_by_default() {
    let fixture = Fixture::new();
    write_turn_config(&fixture);

    let output = fixture.run(&["config", "diff"]);
    assert!(output.status.success(), "{}", describe(&output));
    let text = stdout(&output);
    assert!(
        !text.contains(TURN_SECRET),
        "the TURN credential leaked into config diff:\n{text}"
    );
    assert!(
        text.contains("media.turn_credential = ***"),
        "the masked credential is not listed:\n{text}"
    );

    let output = fixture.run(&["config", "diff", "--reveal-secrets"]);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(
        stdout(&output).contains(&format!("media.turn_credential = {TURN_SECRET}")),
        "--reveal-secrets did not print the credential:\n{}",
        describe(&output)
    );
}
