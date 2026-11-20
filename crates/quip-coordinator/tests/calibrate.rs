//! Offline calibration against `FakeChain` and the real `quip-mock-miner`:
//! every way the run can end, with no validator.

#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers build fixtures outside #[test]"
)]

mod common;

use common::mock_miner;
use quip_coordinator::app::local_env::{
    calibrate_miner, CalibrateError, CalibrateReport, CalibrateRequest, CalibrateResult,
};
use quip_coordinator::cli::calibrate::render_outcome;
use quip_coordinator::testing::fake_chain::FakeChain;
use quip_coordinator::types::calibration::ReferenceWinner;
use quip_coordinator::types::family::ChallengeFamily;
use quip_coordinator::types::log::LogLevel;
use quip_coordinator::types::miner::MinerKind;
use quip_coordinator::types::snapshot::{MiningSnapshot, TARGET_PROOF_BLOCKS};
use quip_coordinator::types::topology::Topology;
use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, Instant};

static FAMILY_TABLE: [ising_generators::descriptor::Contract; 1] =
    [quip_solver_conformance::families::layered::UNIFORM_V1];

/// A four-node ring snapshot, with an admitted family when `family` is set.
fn ring_snapshot(family: bool) -> MiningSnapshot {
    let nodes = vec![0, 1, 2, 3];
    let edges = vec![(0, 1), (1, 2), (2, 3), (0, 3)];
    let (h, j, spin) = (vec![-1000, 0, 1000], vec![-1000, 1000], vec![-1000, 1000]);
    let topology = Topology::from_nodes_edges(nodes.clone(), edges.clone(), &h, &j, &spin);
    MiningSnapshot {
        head_hash: [0; 32],
        last_proof_block_hash: [1; 32],
        topology_hash: topology.hash.clone(),
        nodes,
        edges,
        allowed_h_milli: h,
        allowed_j_milli: j,
        allowed_spin_milli: spin,
        min_solutions: 1,
        max_energy_milli: 0,
        min_diversity_milli: 0,
        block_number: 1,
        spec_version: 120,
        target_proof_blocks: TARGET_PROOF_BLOCKS,
        family: family.then(|| {
            ChallengeFamily::admit(
                &FAMILY_TABLE,
                "vectors/uniform-field",
                1,
                quip_solver_conformance::families::layered::P_SPEC.to_vec(),
                &topology,
            )
            .expect("family admits on the ring")
        }),
    }
}

/// A chain with `winners` qblocks scripted on the snapshot topology.
fn chain_with(family: bool, winners: u8) -> FakeChain {
    let snapshot = ring_snapshot(family);
    let topology_hash: [u8; 32] = snapshot.topology_hash.clone().try_into().unwrap();
    let chain = FakeChain::new(snapshot, None);
    for id in 1..=winners {
        chain.script_qblock(
            u64::from(id),
            ReferenceWinner {
                qblock_id: u64::from(id),
                miner: [id; 32],
                nonce: [id; 32],
                energy_milli: -5000,
                max_energy_milli: -4000,
                topology_hash,
                kind: Some(MinerKind::Cpu),
            },
        );
    }
    chain.set_qblock_id(Some(u64::from(winners)));
    chain
}

/// An executable that runs the mock miner with `extra` after the session
/// flags the coordinator passes. The run takes a binary path only.
fn mock_with(dir: &tempfile::TempDir, extra: &str) -> String {
    let path = dir.path().join("mock-miner.sh");
    std::fs::write(
        &path,
        format!("#!/bin/sh\nexec \"{}\" \"$@\" {extra}\n", mock_miner()),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

async fn run(
    chain: &FakeChain,
    miner: &str,
    name: &str,
    deadline_s: u64,
    grace: Duration,
) -> Result<CalibrateReport, CalibrateError> {
    let sock = format!("/tmp/quip-calibrate-{}-{name}.sock", std::process::id());
    calibrate_miner(
        chain,
        &CalibrateRequest {
            miner_bin: miner,
            sock_path: &sock,
            qblocks: 40,
            kinds: &[],
            deadline_s,
            grace,
            log_level: LogLevel::Info,
        },
    )
    .await
}

#[tokio::test]
async fn calibrate_done_reports_done_and_submits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let chain = chain_with(true, 3);
    let report = run(
        &chain,
        &mock_with(&dir, "--calibrate done"),
        "done",
        60,
        Duration::from_secs(30),
    )
    .await
    .expect("calibration runs");
    assert_eq!(report.result, CalibrateResult::Done);
    assert_eq!(report.result.label(), "DONE");
    assert!(report.summary.contains("Done"), "{}", report.summary);
    assert_eq!(chain.submitted_count(), 0);
}

#[tokio::test]
async fn calibrate_failed_and_deadline_passed_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let chain = chain_with(true, 2);
    for (flag, expected) in [
        ("failed", CalibrateResult::Failed),
        ("deadline", CalibrateResult::DeadlinePassed),
    ] {
        let report = run(
            &chain,
            &mock_with(&dir, &format!("--calibrate {flag}")),
            flag,
            60,
            Duration::from_secs(30),
        )
        .await
        .expect("calibration runs");
        assert_eq!(report.result, expected, "{flag}");
    }
}

#[tokio::test]
async fn silent_miner_times_out_after_deadline_and_grace() {
    let dir = tempfile::tempdir().unwrap();
    let chain = chain_with(true, 2);
    let started = Instant::now();
    let report = run(
        &chain,
        &mock_with(&dir, "--calibrate silent"),
        "silent",
        1,
        Duration::from_secs(1),
    )
    .await
    .expect("calibration runs");
    assert_eq!(report.result, CalibrateResult::TimedOut);
    assert!(started.elapsed() >= Duration::from_secs(2));
}

#[tokio::test]
async fn miner_without_feature_is_a_usage_error_before_anything_is_sent() {
    // No `--calibrate`: the mock lists no `calibrate` feature, and a Job or
    // Calibrate sent to it would be visible as a different error.
    let chain = chain_with(true, 2);
    // Resolve the binary first: the first call in the process runs
    // `cargo build`, which is not part of the shutdown being timed.
    let miner = mock_miner();
    let started = Instant::now();
    let error = run(&chain, &miner, "nofeature", 60, Duration::from_secs(30))
        .await
        .expect_err("a miner without the feature cannot calibrate");
    assert!(
        matches!(&error, CalibrateError::Usage(m) if m.contains("calibrate feature")),
        "{error}"
    );
    // The miner must leave on `Shutdown`, not on the 10 s kill.
    assert!(started.elapsed() < Duration::from_secs(5), "slow shutdown");
}

#[tokio::test]
async fn no_family_no_winners_and_no_snapshot_are_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    let miner = mock_with(&dir, "--calibrate done");
    for (name, chain) in [
        ("nofamily", chain_with(false, 2)),
        ("nowinners", chain_with(true, 0)),
    ] {
        let error = run(&chain, &miner, name, 60, Duration::from_secs(30))
            .await
            .expect_err(name);
        assert!(matches!(error, CalibrateError::Usage(_)), "{name}: {error}");
    }
    let chain = chain_with(true, 2);
    chain.set_snapshot(None);
    let error = run(&chain, &miner, "nosnapshot", 60, Duration::from_secs(30))
        .await
        .expect_err("no snapshot");
    assert!(matches!(error, CalibrateError::Usage(_)), "{error}");
}

#[tokio::test]
async fn bad_request_values_are_usage_errors_before_any_read() {
    let chain = chain_with(true, 2);
    let sock = format!("/tmp/quip-calibrate-{}-bad.sock", std::process::id());
    for (qblocks, deadline_s, kinds) in [
        (0, 60, vec![]),
        (1001, 60, vec![]),
        (40, 0, vec![]),
        (40, 60, vec!["tpu".to_owned()]),
    ] {
        let error = calibrate_miner(
            &chain,
            &CalibrateRequest {
                miner_bin: "/nonexistent/miner",
                sock_path: &sock,
                qblocks,
                kinds: &kinds,
                deadline_s,
                grace: Duration::from_secs(30),
                log_level: LogLevel::Info,
            },
        )
        .await
        .expect_err("bad request");
        assert!(matches!(error, CalibrateError::Usage(_)), "{error}");
    }
    assert_eq!(chain.qblock_reads(), 0);
}

/// The outcome line and exit code a run prints, from one real run.
async fn rendered(
    chain: &FakeChain,
    miner: &str,
    name: &str,
) -> (std::process::ExitCode, String, String) {
    let result = run(chain, miner, name, 60, Duration::from_secs(30)).await;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = render_outcome(&result, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[tokio::test]
async fn calibrate_done_prints_the_line_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let chain = chain_with(true, 2);
    let (code, out, err) = rendered(&chain, &mock_with(&dir, "--calibrate done"), "r-done").await;
    assert_eq!(code, std::process::ExitCode::from(0));
    assert_eq!(out, "outcome=DONE summary=mock calibrate Done\n");
    assert!(err.is_empty(), "{err}");
}

/// A summary with line breaks and other control characters still prints one
/// line, with each control character escaped.
#[test]
fn a_multi_line_summary_prints_one_line() {
    let result = Ok(CalibrateReport {
        result: CalibrateResult::Done,
        summary: "tuned 3 of 4\r\nsweeps=8\tbeta=2.5\u{1b}[0m".to_owned(),
    });
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = render_outcome(&result, &mut out, &mut err);
    assert_eq!(code, std::process::ExitCode::from(0));
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "outcome=DONE summary=tuned 3 of 4\\r\\nsweeps=8\\tbeta=2.5\\u{1b}[0m\n"
    );
    assert!(err.is_empty());
}

#[tokio::test]
async fn calibrate_failed_prints_the_line_and_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    let chain = chain_with(true, 2);
    let (code, out, _) = rendered(&chain, &mock_with(&dir, "--calibrate failed"), "r-failed").await;
    assert_eq!(code, std::process::ExitCode::from(1));
    assert_eq!(out, "outcome=FAILED summary=mock calibrate Failed\n");
}

#[tokio::test]
async fn miner_without_feature_prints_an_error_and_exits_64() {
    let chain = chain_with(true, 2);
    let (code, out, err) = rendered(&chain, &mock_miner(), "r-nofeature").await;
    assert_eq!(code, std::process::ExitCode::from(64));
    assert!(out.is_empty(), "{out}");
    assert_eq!(
        err,
        "error: the miner does not list the calibrate feature\n"
    );
}
