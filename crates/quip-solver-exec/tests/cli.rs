//! The `quip-solver-exec` binary: flags, configuration errors and exit codes.

use std::process::{Command, Output};

fn run(args: &[&str]) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_quip-solver-exec"))
        .args(args)
        .env_remove("QUIP_SESSION_TOKEN")
        .output()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// `--capabilities` needs no solver command: it names the backend and exits 0.
#[test]
fn capabilities_name_the_exec_backend_without_a_solver_command() {
    let out = run(&["--capabilities"]).expect("run");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let caps: serde_json::Value = serde_json::from_slice(&out.stdout).expect("capabilities JSON");
    assert_eq!(caps.get("backend"), Some(&serde_json::json!("exec")));
    assert_eq!(caps.get("algorithm"), Some(&serde_json::json!("external")));
}

/// `--check` validates the solver command: a missing command or a file-mode
/// command without `{model}` exits 69, a usable command exits 0.
#[test]
fn check_validates_the_solver_command() {
    for (label, args, code, fragment) in [
        (
            "no solver command",
            vec!["--check"],
            69,
            "--solver-cmd is required",
        ),
        (
            "file mode without the placeholder",
            vec!["--check", "--solver-cmd", "solver --in"],
            69,
            "{model}",
        ),
        (
            "file mode with the placeholder",
            vec!["--check", "--solver-cmd", "solver --model {model}"],
            0,
            "",
        ),
        (
            "stdin mode without the placeholder",
            vec!["--check", "--solver-cmd", "solver --in", "--solver-stdin"],
            0,
            "",
        ),
    ] {
        let out = run(&args).expect("run");
        assert_eq!(out.status.code(), Some(code), "{label}: {}", stderr(&out));
        assert!(stderr(&out).contains(fragment), "{label}: {}", stderr(&out));
    }
}

/// Session mode without a coordinator endpoint is a configuration error (64).
#[test]
fn session_mode_without_a_coordinator_exits_64() {
    let out = run(&["--solver-cmd", "solver --model {model}"]).expect("run");
    assert_eq!(out.status.code(), Some(64), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--quip-coordinator required"),
        "{}",
        stderr(&out)
    );
}
