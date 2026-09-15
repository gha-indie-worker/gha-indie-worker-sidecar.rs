#![forbid(unsafe_code)]

use std::process::{Command, Stdio};

#[test]
fn invalid_generated_bind_fails_closed_without_env_fallback() {
    let output = Command::new(env!("CARGO_BIN_EXE_ghaiw-sidecar"))
        .env("GHA_INDIE_WORKER_SIDECAR_BIND", "not-a-socket-address")
        .stdin(Stdio::null())
        .output()
        .expect("run product with invalid bind");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty(), "configuration errors keep stdout quiet");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "ghaiw-sidecar: invalid GHA_INDIE_WORKER_SIDECAR_BIND"
    );
}
