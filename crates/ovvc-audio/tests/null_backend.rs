//! The mandatory CI probe test: the null backend runs the whole engine with no
//! hardware, and the probe must report usable energy and exit zero.

use std::process::Command;

#[test]
fn the_null_backend_probe_reports_passing_energy() {
    let output = Command::new(env!("CARGO_BIN_EXE_ovvc-audio-probe"))
        .args(["--backend", "null", "--seconds", "0.5"])
        .output()
        .expect("run the ovvc-audio-probe binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "null probe failed with {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
    assert!(
        stdout.contains("backend=null"),
        "unexpected probe output: {stdout}"
    );
    assert!(
        stdout.contains("energy=PASS"),
        "the synthetic tone did not survive the engine: {stdout}"
    );
}
