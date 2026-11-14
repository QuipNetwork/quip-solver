//! End-to-end `ExecSampler` tests driving a real subprocess. Unix-only: the
//! fake solvers are `sh` one-liners.
#![cfg(unix)]

use quip_solver_core::{IsingGraph, SampleError, SampleParams, Sampler};
use quip_solver_exec::ExecSampler;

/// A 2-node problem, so valid solutions have `spins.len() == 2`.
fn graph() -> IsingGraph {
    IsingGraph::new(vec![1.0, -1.0], vec![1.0], vec![(0, 1)])
}

const ONE_SOLUTION: &str = r#"[{\"spins\":[1,-1],\"energy_milli\":-5}]"#;

#[test]
fn file_mode_delivers_model_and_parses_solutions() {
    // The solver only emits a solution when a non-empty model file is present at
    // the substituted path, proving file delivery.
    let cmd =
        format!("sh -c 'test -s \"$1\" && echo \"{ONE_SOLUTION}\" || echo \"[]\"' _ {{model}}");
    let sampler = ExecSampler::new(&cmd, 5_000, false).expect("valid cmd");
    let out = sampler
        .sample(&graph(), &SampleParams::default())
        .expect("ok");
    assert_eq!(out.len(), 1);
    #[expect(
        clippy::indexing_slicing,
        reason = "len asserted to 1; single solution at index 0"
    )]
    {
        assert_eq!(out[0].spins, vec![1i8, -1]);
        assert_eq!(out[0].energy_milli, -5);
    }
}

#[test]
fn file_mode_substitutes_every_model_token_including_an_embedded_one() {
    // `$1` is `--model=<path>` and `$2` is `<path>`: both tokens must carry the
    // same existing, non-empty model file.
    let cmd = format!(
        "sh -c 'f=${{1#--model=}}; test -s \"$f\" && test \"$f\" = \"$2\" && echo \"{ONE_SOLUTION}\" || echo \"[]\"' _ --model={{model}} {{model}}"
    );
    let sampler = ExecSampler::new(&cmd, 5_000, false).expect("valid cmd");
    let out = sampler
        .sample(&graph(), &SampleParams::default())
        .expect("ok");
    assert_eq!(
        out.len(),
        1,
        "an embedded {{model}} token was not substituted"
    );
}

#[test]
fn stdin_mode_delivers_model_on_stdin() {
    // `cat` drains stdin to EOF (newline-independent); a non-empty capture
    // proves the model arrived on stdin.
    let cmd =
        format!("sh -c 'in=$(cat); test -n \"$in\" && echo \"{ONE_SOLUTION}\" || echo \"[]\"'");
    let sampler = ExecSampler::new(&cmd, 5_000, true).expect("valid cmd");
    let out = sampler
        .sample(&graph(), &SampleParams::default())
        .expect("ok");
    assert_eq!(out.len(), 1);
}

/// Every way the solver process can fail maps to its own error class.
#[test]
fn solver_failures_map_to_their_error_class() {
    // (label, command, timeout in ms, expected error, detail fragment)
    let cases = [
        (
            "nonzero exit",
            "sh -c 'exit 1' _ {model}",
            5_000,
            SampleError::DeviceFault(String::new()),
            "exited",
        ),
        (
            // The shell kills itself with SIGKILL, which is indistinguishable
            // from an external OOM kill. A transient host condition must not
            // end the session.
            "killed by a signal",
            "sh -c 'kill -9 $$' _ {model}",
            5_000,
            SampleError::DeviceBusy,
            "",
        ),
        (
            "timeout",
            "sh -c 'sleep 10' _ {model}",
            150,
            SampleError::DeviceBusy,
            "",
        ),
        (
            "missing binary",
            "quip-no-such-solver-xyz {model}",
            5_000,
            SampleError::DeviceFault(String::new()),
            "spawn",
        ),
    ];
    for (label, cmd, timeout_ms, expected, fragment) in cases {
        let sampler = ExecSampler::new(cmd, timeout_ms, false).expect("valid cmd");
        match (sampler.sample(&graph(), &SampleParams::default()), expected) {
            (Err(SampleError::DeviceFault(detail)), SampleError::DeviceFault(_)) => {
                assert!(detail.contains(fragment), "{label}: {detail}");
            }
            (Err(SampleError::DeviceBusy), SampleError::DeviceBusy) => {}
            (other, expected) => panic!("{label}: expected {expected:?}, got {other:?}"),
        }
    }
}
