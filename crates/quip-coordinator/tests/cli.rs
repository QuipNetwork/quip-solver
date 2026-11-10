//! CLI smoke tests for the `quip-coordinator` binary.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_quip-coordinator")
}

const MISSING: &str = "/nonexistent/quip-coordinator-config.toml";

#[test]
fn help_exits_zero() {
    let out = Command::new(bin())
        .arg("--help")
        .output()
        .expect("run --help");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A missing config is `ConfigInvalid` (64), and the error reaches stderr
/// through the installed `tracing` subscriber as structured plain text.
/// Before the subscriber was installed every `tracing` call in the binary was
/// a silent no-op and the output was empty.
#[test]
fn missing_config_exits_64_and_logs_the_error() {
    let out = Command::new(bin())
        .args(["--config", MISSING])
        .env_remove("RUST_LOG")
        .output()
        .expect("run --config");
    assert_eq!(out.status.code(), Some(64));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("ERROR"), "no level in stderr: {stderr:?}");
    assert!(
        stderr.contains("cannot read config"),
        "no event in stderr: {stderr:?}"
    );
    // Structured fields, not an interpolated blob.
    assert!(
        stderr.contains(&format!("path={MISSING}")),
        "no fields in stderr: {stderr:?}"
    );
    // Captured (non-tty) output must be plain text. The node manager redirects
    // this stream to a file, and ANSI escapes there corrupt every later grep.
    assert!(
        !stderr.contains('\u{1b}'),
        "ANSI escapes in piped stderr: {stderr:?}"
    );
}

/// `--log-level` is a global flag that parses ahead of a subcommand, it
/// overrides an ambient `RUST_LOG`, and logs never land on stdout: `drive`
/// prints its timing table there, and a caller parsing that report would
/// choke on interleaved log lines.
#[test]
fn log_level_flag_parses_and_logs_stay_on_stderr() {
    for level in ["debug", "trace", "error"] {
        let out = Command::new(bin())
            .args(["--log-level", level, "--config", MISSING])
            .env("RUST_LOG", "quip_coordinator=trace")
            .output()
            .expect("run");
        // 64 = ConfigInvalid (the missing file), not 2 (clap usage error).
        assert_eq!(out.status.code(), Some(64), "level: {level}");
        assert!(
            out.stdout.is_empty(),
            "stdout at {level}: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("ERROR"), "stderr at {level}: {stderr:?}");
    }
}

#[test]
fn log_level_rejects_unknown_value() {
    let out = Command::new(bin())
        .args(["--log-level", "verbose", "--config", MISSING])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2), "clap should reject the value");
}

#[test]
fn bad_toml_exits_64() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.toml");
    std::fs::write(&path, "not = [valid").unwrap();
    let out = Command::new(bin())
        .arg("--config")
        .arg(&path)
        .output()
        .expect("run bad config");
    assert_eq!(out.status.code(), Some(64));
}
