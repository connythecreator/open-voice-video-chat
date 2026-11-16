//! The optional relay server's argument surface and startup validation.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-server");

#[test]
fn help_and_version_describe_the_optional_relay() {
    let help = Command::new(BIN).arg("--help").output().expect("run --help");
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("Optional relay server for ovvc"));
    assert!(stdout.contains("--listen"));
    assert!(stdout.contains("--identity"));

    let version = Command::new(BIN).arg("--version").output().expect("run --version");
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("ovvc-server"));
}

#[test]
fn an_invalid_listen_address_fails_before_serving() {
    let dir = std::env::temp_dir().join(format!("ovvc-server-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let output = Command::new(BIN)
        .arg("--identity")
        .arg(dir.join("server.key"))
        .arg("--listen")
        .arg("not-a-multiaddr")
        .output()
        .expect("run with a bad address");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid listen multiaddr"), "stderr: {stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}
