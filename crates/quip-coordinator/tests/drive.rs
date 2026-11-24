//! Integration: drive `quip-mock-miner` end-to-end over UDS through the
//! `JobSource`-driven harness: an explicit JSONL list, salt leases, and the
//! deep-credit-pool flow-control regression.

#![expect(
    clippy::expect_used,
    reason = "helper builds mock miner outside #[test]"
)]
#![expect(clippy::unwrap_used, reason = "now_ms helper outside #[test]")]
#![expect(
    clippy::cast_possible_truncation,
    reason = "test wall-clock millis fit u64"
)]

mod common;

use common::mock_miner;
use quip_coordinator::config::LaunchEntry;
use quip_coordinator::drive::{
    aggregate, drain_all, DriveManyParams, JobRow, LeaseSource, ListSource,
};
use quip_proto::v1::{ising_problem, Configure, EdgeList, Job, JobKind, SetTarget};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test]
async fn drives_two_entry_list_end_to_end() {
    let miner = mock_miner();
    let deadline = now_ms() + 3_600_000;

    // Two explicit entries; mock-miner always returns an all-+1 solution.
    let list_text = "{\"h_milli\":[1000,-1000],\"j_milli\":[500],\"edges\":[[0,1]]}\n\
                      {\"h_milli\":[0,0,0],\"j_milli\":[100,100],\"edges\":[[0,1],[1,2]]}\n";
    let mut src = ListSource::parse(list_text, None, deadline).expect("parse list");
    let jobs = drain_all(&mut src);
    assert_eq!(jobs.len(), 2, "expected exactly 2 jobs from the list");

    let entry = LaunchEntry {
        miner_id: "cpu-0".into(),
        binary: miner.clone(),
        backend: "cpu".into(),
        device: None,
        configure: Configure {
            queue_depth: 3,
            idle_timeout_s: 30,
            heartbeat_s: 15,
            reconnect_window_s: 60,
            backend_toml: String::new(),
        },
    };

    let sock = format!("/tmp/quip-drive-e2e-{}.sock", std::process::id());
    let token = "drive-e2e-token";
    let report = quip_coordinator::drive::run_drive(DriveManyParams {
        miner_bin: &miner,
        sock_path: &sock,
        miner_id: "cpu-0",
        token,
        entry: &entry,
        topology: None,
        target: None,
        jobs,
        utilization: None,
        yielding: false,
        log_level: quip_coordinator::logging::LogLevel::Info,
    })
    .await;

    assert!(report.handshake_ok, "handshake failed");
    assert_eq!(
        report.rows.len(),
        2,
        "expected 2 result rows, got {:?}",
        report.rows
    );
    assert!(
        report.rows.iter().all(|r| r.passed && !r.rejected),
        "expected every job to pass with loose gates: {:?}",
        report.rows
    );

    let agg = aggregate(&report.rows, report.run_wall_ms);
    assert_eq!(agg.total_jobs, 2);
    assert_eq!(agg.passed, 2);
    assert_eq!(agg.rejected, 0);
    // A 2-job run against the mock miner can finish inside a single millisecond
    // now that job dispatch no longer shares the session's read path, and the
    // span is only measured to whole milliseconds — so 0 here means "too fast
    // to measure", not "nothing ran". The row assertions above already prove
    // both jobs completed. (`aggregate` reports 0 jobs/s for a sub-ms span; that
    // reporting floor predates this change and is not what this test covers.)
    if report.run_wall_ms > 0 {
        assert!(agg.throughput_per_s > 0.0);
    }
}

#[tokio::test]
async fn drives_leases_end_to_end() {
    let miner = mock_miner();
    let spec = quip_coordinator::drive::parse_topology_spec(
        quip_coordinator::presets::preset_spec("smoke").expect("preset"),
    )
    .expect("spec");
    let mut src = LeaseSource::new(&spec, [0; 32], 1, 10, 4);
    let jobs = drain_all(&mut src);
    let target = SetTarget {
        max_energy_milli: i64::MAX / 2,
        min_solutions: 1,
        max_proof_solutions: 32,
        ..Default::default()
    };
    let entry = LaunchEntry {
        miner_id: "cpu-0".into(),
        binary: miner.clone(),
        backend: "cpu".into(),
        device: None,
        configure: Configure {
            queue_depth: 3,
            idle_timeout_s: 30,
            heartbeat_s: 15,
            reconnect_window_s: 60,
            backend_toml: String::new(),
        },
    };
    let report = quip_coordinator::drive::run_drive(DriveManyParams {
        miner_bin: &miner,
        sock_path: &format!("/tmp/quip-drive-lease-{}.sock", std::process::id()),
        miner_id: "cpu-0",
        token: "tok",
        entry: &entry,
        topology: Some(spec.topology.clone()),
        target: Some(target),
        jobs,
        utilization: None,
        yielding: false,
        log_level: quip_coordinator::logging::LogLevel::Info,
    })
    .await;
    assert!(report.handshake_ok);
    assert_eq!(report.rows.len(), 3, "one row per lease");
    let agg = aggregate(&report.rows, report.run_wall_ms);
    assert_eq!(agg.salts_done, 10);
    assert_eq!(
        report.rows.iter().map(|r| r.winners).sum::<u32>(),
        10,
        "every salt wins an open target"
    );
}

// ---------------------------------------------------------------------------
// Flow control
// ---------------------------------------------------------------------------

/// Jobs shaped like production nonces: an Advantage2-sized ring (4577 nodes)
/// carries ~73 KB of inline `h`/`J`/edges, so a 480-deep pool puts ~35 MB on
/// the wire. Volume is the whole trigger — at 5 KB per job the transport
/// buffers absorb the burst and the deadlock does not appear.
fn ring_jobs(count: usize, nodes: usize) -> Vec<Job> {
    let deadline = now_ms() + 3_600_000;
    let h: Vec<i32> = (0..nodes)
        .map(|i| if i % 2 == 0 { 1000 } else { -1000 })
        .collect();
    let u: Vec<u32> = (0..nodes as u32).collect();
    let v: Vec<u32> = (0..nodes as u32).map(|i| (i + 1) % nodes as u32).collect();
    let j: Vec<i32> = vec![500; nodes];
    (0..count)
        .map(|n| Job {
            job_id: format!("job-{n}").into_bytes(),
            generator: None,
            kind: JobKind::IsingSample as i32,
            generation: 0,
            deadline_ms: deadline,
            ising: Some({
                let mut problem = quip_coordinator::producer::problem::milli_problem(
                    Some(ising_problem::Graph::Edges(EdgeList {
                        u: u.clone(),
                        v: v.clone(),
                    })),
                    &h,
                    &j,
                );
                problem.num_reads = 1;
                problem
            }),
            provenance: None,
        })
        .collect()
}

/// Regression: a deep credit pool must not deadlock the miner session.
///
/// Both peers speak one bidi stream. If either one writes on the same task that
/// reads, a full outbound channel stops it reading — and since each side's
/// reads are what free the other side's writes, both park permanently. The
/// trigger is simply enough bytes in flight: a large credit pool plus jobs
/// carrying inline `h`/`J`.
///
/// This reproduces it through the public drive path. Pre-fix (dispatch on the
/// coordinator's read loop) the run hangs and this test trips its timeout;
/// post-fix the dispatcher is a separate task, so results keep draining.
#[tokio::test]
async fn deep_credit_pool_does_not_deadlock_the_session() {
    let miner = mock_miner();
    let jobs = ring_jobs(480, 4577);
    let total = jobs.len();

    let entry = LaunchEntry {
        miner_id: "cpu-0".into(),
        binary: miner.clone(),
        backend: "cpu".into(),
        device: None,
        configure: Configure {
            // The lever: `seed_credits` is `queue_depth`, so this grants the
            // whole pool up front and the coordinator dispatches every staged
            // job in one uninterrupted burst — exactly the flood that used to
            // wedge the read path.
            queue_depth: 480,
            idle_timeout_s: 30,
            heartbeat_s: 15,
            reconnect_window_s: 60,
            backend_toml: String::new(),
        },
    };

    let sock = format!("/tmp/quip-flow-control-{}.sock", std::process::id());
    let run = quip_coordinator::drive::run_drive(DriveManyParams {
        miner_bin: &miner,
        sock_path: &sock,
        miner_id: "cpu-0",
        token: "flow-control-token",
        entry: &entry,
        topology: None,
        target: None,
        jobs,
        utilization: None,
        yielding: false,
        log_level: quip_coordinator::logging::LogLevel::Info,
    });

    // A generous bound: the mock miner answers instantly, so this only fires if
    // the session wedged. Without the timeout a regression hangs the suite
    // instead of failing it.
    let report = tokio::time::timeout(Duration::from_secs(90), run)
        .await
        .expect("session deadlocked: no progress within 90s");

    assert!(report.handshake_ok, "handshake failed");
    assert_eq!(
        report.rows.len(),
        total,
        "every job must reach a terminal outcome; got {} of {total}",
        report.rows.len()
    );
    let rejected = report.rows.iter().filter(|r: &&JobRow| r.rejected).count();
    assert_eq!(rejected, 0, "no job should be rejected by the mock miner");
}
