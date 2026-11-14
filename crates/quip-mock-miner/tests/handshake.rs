//! CLI smoke tests for the mock miner binary (`--capabilities`, `--check`, exit codes).

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_quip-mock-miner")
}

#[test]
fn capabilities_prints_json_and_exits_zero() {
    let out = Command::new(bin()).arg("--capabilities").output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(
        s.contains("\"backend\""),
        "capabilities JSON must include backend, got: {s}"
    );
}

#[test]
fn version_exits_zero() {
    let out = Command::new(bin()).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8(out.stdout).unwrap().contains("protocol"));
}

#[test]
fn check_exits_zero_on_this_host() {
    assert!(Command::new(bin())
        .arg("--check")
        .status()
        .unwrap()
        .success());
}

#[test]
fn missing_configuration_exits_with_its_own_code() {
    // No --quip-coordinator and no --capabilities/--check is ConfigInvalid
    // (64); a coordinator without QUIP_SESSION_TOKEN is 77.
    for (label, expected, coordinator) in [
        ("no coordinator", 64, None),
        (
            "no session token",
            77,
            Some("unix:///tmp/quip-no-such-socket.sock"),
        ),
    ] {
        let mut command = Command::new(bin());
        let _ = match coordinator {
            Some(coordinator) => command
                .arg("--quip-coordinator")
                .arg(coordinator)
                .env_remove("QUIP_SESSION_TOKEN"),
            None => command.env("QUIP_SESSION_TOKEN", "tok"),
        };
        let out = command.output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(expected),
            "{label} (got {:?}, stderr={})",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
