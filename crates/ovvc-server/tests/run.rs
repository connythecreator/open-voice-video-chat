//! Start the real relay server, wait for its listen address, then stop it with
//! SIGINT. The CLI tests only cover argument parsing; this covers the swarm
//! setup, the stdout status lines and the graceful shutdown path.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-server");

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn starts_listening_and_stops_cleanly_on_sigint() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut child = Command::new(BIN)
        .arg("--listen")
        .arg("/ip4/127.0.0.1/tcp/0")
        .arg("--identity")
        .arg(dir.path().join("server.key"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start ovvc-server");
    let stdout = child.stdout.take().expect("stdout pipe");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let mut child = KillOnDrop(child);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut lines = Vec::new();
    let listen_line = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "server never printed a listen address; got {lines:?}"
        );
        match rx.recv_timeout(remaining.min(Duration::from_secs(1))) {
            Ok(line) => {
                let listening = line.starts_with("listening:");
                lines.push(line.clone());
                if listening {
                    break line;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                assert!(
                    child.0.try_wait().expect("server state").is_none(),
                    "server exited before listening"
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("server stdout closed before a listen address; got {lines:?}");
            }
        }
    };
    assert!(
        listen_line.contains("/ip4/127.0.0.1/tcp/"),
        "unexpected listen line: {listen_line}"
    );
    assert!(
        lines.iter().any(|line| line.contains("relay peer id:")),
        "missing relay peer id in {lines:?}"
    );

    let signal = Command::new("kill")
        .arg("-INT")
        .arg(child.0.id().to_string())
        .status()
        .expect("send SIGINT");
    assert!(signal.success(), "kill -INT failed");

    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("server state") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "server did not exit after SIGINT"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "server exited with {status}");

    let mut tail = Vec::new();
    while let Ok(line) = rx.recv_timeout(Duration::from_millis(250)) {
        tail.push(line);
    }
    assert!(
        tail.iter().any(|line| line.contains("shutting down")),
        "missing shutdown line; tail was {tail:?}"
    );
}
