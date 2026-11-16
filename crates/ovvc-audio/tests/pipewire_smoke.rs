//! A manual smoke test for the native PipeWire backend.
//!
//! It is ignored because it needs a live PipeWire session with a default
//! source and sink, which CI does not have. Run it by hand after staging the
//! development headers:
//!
//! ```text
//! source scripts/audio-dev-env.sh
//! cargo test -p ovvc-audio --features pipewire --test pipewire_smoke -- --ignored --nocapture
//! ```
//!
//! The test runs the probe as a child process, so a panic in the backend would
//! surface as a non-zero exit and a "panicked" message rather than taking the
//! test harness down with it.

#![cfg(all(target_os = "linux", feature = "pipewire"))]

use std::process::Command;

#[test]
#[ignore = "needs a live PipeWire session"]
fn the_pipewire_probe_captures_without_panicking() {
    let output = Command::new(env!("CARGO_BIN_EXE_ovvc-audio-probe"))
        .args(["--backend", "pipewire", "--seconds", "1"])
        .output()
        .expect("run the ovvc-audio-probe binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stderr.contains("panicked"),
        "the PipeWire probe panicked\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        output.status.success(),
        "the PipeWire probe failed with {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
    assert!(
        stdout.contains("backend=pipewire"),
        "unexpected probe output: {stdout}"
    );
    assert!(
        stdout.contains("captured="),
        "the probe did not report captured frames: {stdout}"
    );
}
