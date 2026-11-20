//! End-to-end tests of the production session: the shared loop, the
//! coordinator's events, and settlement.

use crate::coordinator::events::CoordinatorService;
use crate::coordinator::router::MinerCaps;
use crate::coordinator::settle::{
    handle_lease_done, handle_lease_result, settle_pow_result, PowOutcome,
};
use crate::coordinator::state::CoordinatorState;
use crate::miner::session::cancel_timing::CancelTiming;
use crate::miner::session::handlers::{dispatch_granted, run_session, GRANT_CHANNEL_DEPTH};
use crate::miner::session::stream::{outbound_stream, send_cancel, OutboundStream};
use crate::observe::logging::test_support::{drain, init_capture, Capture};
use crate::testing::fake_chain::FakeChain;
use crate::types::proof::{Proof, SubmitAction, SubmitReceipt};
use crate::types::topology::Topology;
use quip_proto::v1::{coord_msg, miner_msg, Capabilities, CoordMsg, MinerMsg};
use quip_proto::v1::{
    Job, JobKind, LeaseDone, Provenance, Result as WireResult, SetTarget, Solution,
};
use quip_protocol::lease::{LeaseSpec, TopologyView};
use quip_protocol::session::PROTOCOL_VERSION;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt as _;
use tonic::Status;

fn lease_snapshot() -> crate::types::snapshot::MiningSnapshot {
    let nodes = vec![0, 1, 2, 3];
    let edges = vec![(0, 1), (1, 2), (2, 3), (0, 3)];
    let (h, j, spin) = (vec![-1000, 0, 1000], vec![-1000, 1000], vec![-1000, 1000]);
    crate::types::snapshot::MiningSnapshot {
        head_hash: [0; 32],
        last_proof_block_hash: [7; 32],
        topology_hash: crate::types::topology::topology_hash_sets(&nodes, &edges, &h, &j, &spin)
            .to_vec(),
        nodes,
        edges,
        allowed_h_milli: h,
        allowed_j_milli: j,
        allowed_spin_milli: spin,
        min_solutions: 1,
        max_energy_milli: i64::MAX / 2,
        min_diversity_milli: 0,
        block_number: 42,
        spec_version: 117,
        target_proof_blocks: crate::types::snapshot::TARGET_PROOF_BLOCKS,
        family: None,
    }
}

/// State with one live lease of generation 1, its topology, and a target.
fn lease_state() -> (CoordinatorState, Job) {
    let snap = lease_snapshot();
    let mut st = CoordinatorState::new();
    st.router.register_miner("m", MinerCaps::default());
    st.generation = 1;
    st.set_topology(Some(Topology::from_nodes_edges(
        snap.nodes.clone(),
        snap.edges.clone(),
        &snap.allowed_h_milli,
        &snap.allowed_j_milli,
        &snap.allowed_spin_milli,
    )));
    st.target = Some(SetTarget {
        max_energy_milli: i64::MAX / 2,
        min_solutions: 1,
        min_diversity_milli: 0,
        max_proof_solutions: 32,
        ..Default::default()
    });
    let job = crate::coordinator::scoring::lease::build_lease_job(
        crate::coordinator::scoring::lease::lease_generator(snap.family.as_ref()),
        &snap.topology_hash,
        snap.last_proof_block_hash,
        [1; 32],
        100,
        4,
        1,
    );
    st.dispatch_inflight("m", job.clone());
    (st, job)
}

/// All-+1 winner for salt index `i` of `job`, scored on the drawn problem.
fn winner(job: &Job, i: u64) -> WireResult {
    let spec = LeaseSpec::from_proto(job.generator.as_ref().unwrap()).unwrap();
    let snap = lease_snapshot();
    let view = TopologyView {
        num_nodes: snap.nodes.len(),
        edges: snap
            .edges
            .iter()
            .map(|&(u, v)| (u as usize, v as usize))
            .collect(),
        allowed_h_milli: snap.allowed_h_milli.clone(),
        allowed_j_milli: snap.allowed_j_milli.clone(),
    };
    let nonce = spec.nonce(i).unwrap();
    let (h, j) = view.draw(nonce).unwrap();
    let spins = vec![1i8; view.num_nodes];
    let energy = quip_protocol::scoring::energy_from_milli(&spins, &h, &j, &view.edges);
    WireResult {
        job_id: job.job_id.clone(),
        solutions: vec![Solution {
            values: vec![],
            spins: quip_protocol::wire::encode_spins_packed(&spins),
            energy_milli: energy,
        }],
        meta: None,
        salt: spec.salt(i).unwrap().to_vec(),
        nonce: nonce.to_vec(),
    }
}

fn notify() -> Arc<Mutex<Option<oneshot::Sender<()>>>> {
    Arc::new(Mutex::new(None))
}

/// The production session events over `state` and `chain`.
fn service(
    state: &Arc<Mutex<CoordinatorState>>,
    chain: Arc<FakeChain>,
) -> Arc<CoordinatorService<FakeChain>> {
    Arc::new(CoordinatorService {
        state: Arc::clone(state),
        sink: chain,
        submit_notify: notify(),
    })
}

#[tokio::test]
async fn session_lease_win_reports_the_round_receipt_once() {
    init_capture();
    let (mut st, job) = lease_state();
    st.qblock_id = Some(64);
    st.target.as_mut().unwrap().max_energy_milli = 1_000_000;
    st.round_log = Some(Arc::new(Mutex::new(
        crate::coordinator::round::log::RoundLog::new(Some(64), [1; 32]),
    )));
    let state = Arc::new(Mutex::new(st));
    let chain = FakeChain::new(lease_snapshot(), None);
    chain.set_submit_receipt(Ok(SubmitReceipt {
        action: SubmitAction::Success,
        extrinsic_hash: Some([3; 32]),
        block_hash: Some([4; 32]),
        block_number: Some(693),
    }));
    let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(Capture(Arc::clone(&buf)))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let result = winner(&job, 0);
    handle_lease_result(&chain, &state, &notify(), "m", result.clone()).await;
    handle_lease_result(&chain, &state, &notify(), "m", result).await;

    let output = drain(&buf);
    assert_eq!(output.matches("qblock won by us").count(), 1, "{output}");
    for field in ["qblock_id=65", "inclusion_block=693", "target_energy=1000"] {
        assert!(output.contains(field), "{output}");
    }
    assert!(
        output.contains("quip_coordinator::coordinator::round::log"),
        "{output}"
    );
    assert_eq!(chain.submitted_count(), 1);
}

#[tokio::test]
async fn late_session_receipt_does_not_supply_the_next_rounds_best() {
    init_capture();
    let (mut st, job) = lease_state();
    let result = winner(&job, 0);
    let target = st.target.unwrap();
    let (verified, validated) = crate::coordinator::scoring::lease::verify_and_select(
        job.generator.as_ref().unwrap(),
        st.lease_topology.as_ref().unwrap(),
        &target,
        &result,
    )
    .unwrap();
    let old_round = Arc::new(Mutex::new(crate::coordinator::round::log::RoundLog::new(
        Some(64),
        [1; 32],
    )));
    let next_round = Arc::new(Mutex::new(crate::coordinator::round::log::RoundLog::new(
        Some(65),
        [1; 32],
    )));
    st.generation = 2;
    st.qblock_id = Some(65);
    st.round_log = Some(Arc::clone(&next_round));
    let state = Arc::new(Mutex::new(st));
    let chain = FakeChain::new(lease_snapshot(), None);
    let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(Capture(Arc::clone(&buf)))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Resume settlement after the feeder has opened another round, using
    // the context captured with the old job before validation/submission.
    settle_pow_result(
        &chain,
        &state,
        &notify(),
        PowOutcome {
            miner_id: "m",
            job: &job,
            job_id: &verified.nonce,
            salt: Some(verified.salt),
            validated,
            gates: crate::coordinator::scoring::validate::gates_from_target(Some(&target)),
            current_best_milli: None,
            round_log: Some(old_round),
            device_access_time_us: 0,
        },
    )
    .await;
    assert_eq!(
        state.lock().await.current_best_milli,
        None,
        "a receipt for generation {} must not set the best of generation 2",
        job.generation
    );
    let mut log = next_round.lock().await;
    log.log_heartbeat(694, 2, 1, false);
    log.observe_pending(
        crate::coordinator::round::pool_watch::Clearing {
            account: [2; 32],
            nonce: 1.into(),
            best_energy_milli: -5_038_000,
        },
        -5_030_000,
    );
    log.log_pause(3, 694, 0, 0);

    let output = drain(&buf);
    assert!(output.contains("qblock won by us qblock_id=65"), "{output}");
    assert_eq!(
        output.matches("our_best_energy_this_round=none").count(),
        2,
        "{output}"
    );
}

/// Run one lease `Result` against a fresh chain. Returns the shared state
/// and the proofs the chain received.
async fn settle(
    st: CoordinatorState,
    result: WireResult,
) -> (Arc<Mutex<CoordinatorState>>, Vec<Proof>) {
    let state = Arc::new(Mutex::new(st));
    let chain = FakeChain::new(lease_snapshot(), None);
    handle_lease_result(&chain, &state, &notify(), "m", result).await;
    (state, chain.take_submitted())
}

fn target(max_energy_milli: i64) -> SetTarget {
    SetTarget {
        max_energy_milli,
        min_solutions: 1,
        min_diversity_milli: 0,
        max_proof_solutions: 32,
        ..Default::default()
    }
}

/// A decay model that eases past `energy` within the horizon, so the stash
/// holds a candidate at `energy` instead of rejecting it as never viable.
fn ease_stash(st: &mut CoordinatorState, generation: u64, energy: i64) {
    st.stash.reset(
        generation,
        Some(crate::coordinator::round::decay::DecayModel {
            base_max_energy_milli: energy,
            curve: Some(crate::coordinator::round::decay::EnergyCurve {
                min_milli: energy - 100_000,
                knee_milli: energy,
                max_milli: energy + 50_000,
            }),
            epoch_length: 10,
            algorithm: crate::coordinator::round::decay::DecayAlgorithm::Stepwise,
            target_proof_blocks: crate::types::snapshot::TARGET_PROOF_BLOCKS,
        }),
        0,
    );
}

#[tokio::test]
async fn verified_lease_winner_is_submitted_with_its_salt() {
    let (st, job) = lease_state();
    let r = winner(&job, 2);
    let (state, submitted) = settle(st, r.clone()).await;
    assert_eq!(submitted.len(), 1);
    assert_eq!(submitted.first().map(|proof| &proof.salt), Some(&r.salt));
    assert_eq!(
        submitted.first().map(|proof| &proof.job_id),
        Some(&r.nonce),
        "attempt id is the nonce"
    );
    assert!(
        state.lock().await.inflight.contains_key(&job.job_id),
        "a lease stays live until LeaseDone"
    );
    assert!(state.lock().await.round_mined());
}

/// None of these is submitted or marks the round. A report that fails
/// authenticity verification is a bad miner, not participation. A result
/// before the target cannot be judged. Reporting zero solutions is not
/// participation.
#[tokio::test]
async fn rejected_lease_results_are_not_submitted_and_do_not_mark_the_round() {
    type Mutate = fn(&mut CoordinatorState, &mut WireResult);
    let cases: [(&str, Mutate); 3] = [
        ("wrong energy", |_, r| {
            r.solutions
                .iter_mut()
                .for_each(|solution| solution.energy_milli -= 1);
        }),
        ("no target", |st, _| st.target = None),
        ("no solutions", |_, r| r.solutions.clear()),
    ];
    for (label, mutate) in cases {
        let (mut st, job) = lease_state();
        let mut r = winner(&job, 0);
        mutate(&mut st, &mut r);
        let (state, submitted) = settle(st, r).await;
        assert!(submitted.is_empty(), "{label}: submitted");
        assert!(!state.lock().await.round_mined(), "{label}: round mined");
    }
}

/// A verified lease Result whose energy does not clear the live target is
/// held in the decay-ratchet stash, not submitted, and still counts as
/// participation for the round — the same as a plain `PoW` Result that
/// misses the gate.
#[tokio::test]
async fn lease_result_above_target_is_stashed_and_marks_round_mined() {
    let (mut st, job) = lease_state();
    let r = winner(&job, 3);
    let energy = r.solutions.first().unwrap().energy_milli;
    // Ceiling set to exactly the reported energy: the gate is strict
    // less-than, so this candidate does not clear it.
    st.target = Some(target(energy));
    ease_stash(&mut st, job.generation, energy);
    let (expected_nonce, expected_salt) = (r.nonce.clone(), r.salt.clone());
    let (state, submitted) = settle(st, r).await;
    assert!(
        submitted.is_empty(),
        "energy equal to the ceiling does not clear it"
    );
    let st = state.lock().await;
    assert!(!st.stash.is_empty(), "held in the decay-ratchet stash");
    assert!(
        st.round_mined(),
        "a verified lease Result counts as participation whether or not it clears the target"
    );
    let candidate = st
        .stash
        .win_time_check(
            u64::MAX,
            u64::MAX,
            None,
            &crate::coordinator::scoring::validate::gates_from_target(None),
            0,
        )
        .map(|w| w.candidate)
        .expect("the single stashed candidate becomes due within the horizon");
    assert_eq!(
        candidate.job_id, expected_nonce,
        "stashed candidate keeps the result's nonce as its attempt id"
    );
    assert_eq!(
        candidate.salt.map(|s| s.to_vec()),
        Some(expected_salt),
        "stashed candidate keeps the result's salt for the later live submit"
    );
}

#[tokio::test]
async fn lease_result_with_more_reads_than_a_proof_holds_submits_a_capped_set() {
    let (st, job) = lease_state();
    let mut r = winner(&job, 1);
    let read = r.solutions.first().cloned().unwrap();
    r.solutions = vec![read; 40];
    let (_, submitted) = settle(st, r).await;
    assert_eq!(submitted.len(), 1, "an unfiltered read set still wins");
    assert!(submitted
        .first()
        .is_some_and(|proof| proof.solutions.len() <= crate::types::proof::MAX_PROOF_SOLUTIONS));
}

static FAMILY_TABLE: [ising_generators::descriptor::Contract; 1] =
    [quip_solver_conformance::families::layered::UNIFORM_V1];

/// A session holding one in-flight family lease, and an honest report for
/// the lease's first salt that yields an instance.
fn family_state() -> (CoordinatorState, Job, WireResult) {
    use ising_generators::descriptor::{admit, AdvertisedLimits};
    use ising_generators::family::{admit_topology, generate_instance, BaseTopology};

    let (mut st, legacy) = lease_state();
    let mut snap = lease_snapshot();
    let topology = Topology::from_nodes_edges(
        snap.nodes.clone(),
        snap.edges.clone(),
        &snap.allowed_h_milli,
        &snap.allowed_j_milli,
        &snap.allowed_spin_milli,
    );
    snap.family = Some(
        crate::types::family::ChallengeFamily::admit(
            &FAMILY_TABLE,
            "vectors/uniform-field",
            1,
            quip_solver_conformance::families::layered::P_SPEC.to_vec(),
            &topology,
        )
        .unwrap(),
    );
    let account: [u8; 32] = legacy
        .generator
        .as_ref()
        .unwrap()
        .miner_account
        .as_slice()
        .try_into()
        .unwrap();
    let mut job = crate::coordinator::scoring::lease::build_lease_job(
        crate::coordinator::scoring::lease::lease_generator(snap.family.as_ref()),
        &snap.topology_hash,
        snap.last_proof_block_hash,
        account,
        0,
        32,
        legacy.generation,
    );
    job.job_id.clone_from(&legacy.job_id);
    let _ = st.inflight.insert(job.job_id.clone(), job.clone());
    st.lease_topology = Some(Arc::new(
        crate::coordinator::scoring::lease::LeaseTopology::from_topology(&topology, &FAMILY_TABLE)
            .unwrap(),
    ));

    let generator = job.generator.as_ref().unwrap();
    let spec = LeaseSpec::from_proto(generator).unwrap();
    let descriptor = generator.family.as_ref().unwrap();
    let admitted = admit(
        &FAMILY_TABLE,
        descriptor.family_id.as_bytes(),
        descriptor.revision,
        &descriptor.params,
        &descriptor.params_digest,
    )
    .unwrap();
    let base = BaseTopology::new(topology.nodes.clone(), &topology.edge_pairs()).unwrap();
    let bounds = admit_topology(
        &admitted,
        &base,
        &generator.topology_order_digest,
        &AdvertisedLimits::default(),
    )
    .unwrap();
    let (index, nonce, instance) = (0..32)
        .find_map(|i| {
            let nonce = spec.nonce(i).unwrap();
            generate_instance(&admitted, &base, &bounds, nonce)
                .ok()
                .map(|instance| (i, nonce, instance))
        })
        .expect("a salt yields an instance");
    let spins = vec![1i8; instance.node_ids().len()];
    let edges: Vec<(usize, usize)> = instance
        .edges()
        .iter()
        .map(|&(u, v)| (u as usize, v as usize))
        .collect();
    let energy =
        quip_protocol::scoring::energy_from_milli(&spins, instance.h(), instance.j(), &edges)
            + instance.offset_milli();
    let report = WireResult {
        job_id: job.job_id.clone(),
        solutions: vec![Solution {
            values: vec![],
            spins: quip_protocol::wire::encode_spins_packed(&spins),
            energy_milli: energy,
        }],
        meta: None,
        salt: spec.salt(index).unwrap().to_vec(),
        nonce: nonce.to_vec(),
    };
    (st, job, report)
}

#[tokio::test]
async fn an_honest_family_result_is_submitted() {
    let (st, _, report) = family_state();
    let salt = report.salt.clone();
    let (state, submitted) = settle(st, report).await;
    assert_eq!(submitted.len(), 1);
    assert_eq!(submitted.first().map(|proof| &proof.salt), Some(&salt));
    assert!(state.lock().await.round_mined());
}

#[tokio::test]
async fn a_forged_family_result_never_reaches_submission_or_the_stash() {
    let (mut st, job, mut report) = family_state();
    let energy = report.solutions.first().unwrap().energy_milli;
    ease_stash(&mut st, job.generation, energy);
    report
        .solutions
        .iter_mut()
        .for_each(|solution| solution.energy_milli -= 1);
    let (state, submitted) = settle(st, report).await;
    assert!(submitted.is_empty());
    let st = state.lock().await;
    assert!(st.stash.is_empty(), "a forgery is not a near miss");
    assert!(!st.round_mined(), "a forgery is not participation");
}

#[tokio::test]
async fn an_authentic_family_miss_is_stashed_and_counts_as_participation() {
    let (mut st, job, report) = family_state();
    let energy = report.solutions.first().unwrap().energy_milli;
    st.target = Some(target(energy));
    ease_stash(&mut st, job.generation, energy);
    let (state, submitted) = settle(st, report).await;
    assert!(submitted.is_empty());
    let st = state.lock().await;
    assert!(!st.stash.is_empty(), "held for the decay schedule");
    assert!(st.round_mined());
}

#[tokio::test]
async fn lease_result_after_cancel_is_dropped() {
    let (mut st, job) = lease_state();
    let _ = st.cancel_inflight(1);
    st.generation = 2;
    let (state, submitted) = settle(st, winner(&job, 1)).await;
    assert!(submitted.is_empty());
    let mut st = state.lock().await;
    handle_lease_done(
        &mut st,
        "m",
        &LeaseDone {
            job_id: job.job_id,
            salts_done: 3,
            best_energy_milli: 0,
        },
    );
    assert_eq!(
        st.router.take_lease_salts("m"),
        3,
        "cancelled salts still count toward the rate"
    );
    assert_eq!(
        st.router.round_salts("m"),
        0,
        "a cancelled round's salts do not count toward this round"
    );
    assert!(!st.round_mined());
}

/// A lease that finished salts marks the round mined; one that finished
/// none does not. Either way the lease completes and its salts count.
#[test]
fn lease_done_marks_the_round_mined_only_with_salts() {
    for (salts_done, best_energy_milli) in [(4, -5), (0, i64::MAX)] {
        let (mut st, job) = lease_state();
        assert!(st.lease_issued_at.contains_key(&job.job_id));
        handle_lease_done(
            &mut st,
            "m",
            &LeaseDone {
                job_id: job.job_id.clone(),
                salts_done,
                best_energy_milli,
            },
        );
        let label = format!("salts_done {salts_done}");
        assert_eq!(st.round_mined(), salts_done > 0, "{label}");
        assert!(!st.inflight.contains_key(&job.job_id), "{label}");
        // The issue-instant clock is consumed (and its entry cleared) as
        // part of reporting elapsed time, not left dangling.
        assert!(!st.lease_issued_at.contains_key(&job.job_id), "{label}");
        assert_eq!(st.router.take_lease_salts("m"), salts_done, "{label}");
        assert_eq!(st.router.round_salts("m"), salts_done, "{label}");
    }
}

#[test]
fn reclaimed_lease_restages_with_same_range() {
    let mut st = CoordinatorState::new();
    let caps = MinerCaps {
        supported_kinds: vec![JobKind::IsingGenerate as i32],
        generators: vec![quip_proto::v1::GeneratorAlgorithm::Blake3Chacha8V1 as i32],
        ..MinerCaps::default()
    };
    st.router.register_miner("m", caps);
    let snap = lease_snapshot();
    let job = crate::coordinator::scoring::lease::build_lease_job(
        crate::coordinator::scoring::lease::lease_generator(snap.family.as_ref()),
        &snap.topology_hash,
        snap.last_proof_block_hash,
        [0; 32],
        21,
        4,
        1,
    );

    st.dispatch_inflight("m", job.clone());
    let reclaimed = st.reclaim_miner("m");
    assert_eq!(reclaimed, vec![job.clone()]);
    assert_eq!(
        st.router
            .route(reclaimed.first().cloned().expect("one reclaimed lease"))
            .as_deref(),
        Some("m")
    );
    st.router.grant_credits("m", 1);
    let staged = st.router.next_job("m").expect("restaged lease");
    assert_eq!(staged.generator, job.generator);
}

fn job(id: &[u8]) -> Job {
    Job {
        iqm: None,
        job_id: id.to_vec(),
        generator: None,
        kind: JobKind::IsingSample as i32,
        generation: 1,
        deadline_ms: 0,
        ising: Some(crate::types::problem::milli_problem(None, &[0; 2], &[0; 1])),
        provenance: Some(Provenance {
            is_pow: true,
            order_id: vec![],
        }),
    }
}

fn caps() -> MinerCaps {
    MinerCaps {
        backend: "mock".into(),
        algorithm: "sa".into(),
        supported_kinds: vec![JobKind::IsingSample as i32],
        max_nodes: 0, // unlimited
        max_edges: 0,
        ..MinerCaps::default()
    }
}

#[test]
fn result_for_the_current_generation_marks_the_round_mined() {
    let mut st = CoordinatorState::new();
    st.generation = 3;
    assert!(!st.round_mined(), "no result yet");
    st.note_result(2);
    assert!(
        !st.round_mined(),
        "a stale-generation result is not this round's work"
    );
    st.note_result(3);
    assert!(st.round_mined());
    st.generation = 4;
    assert!(!st.round_mined(), "a new round starts unmined");
}

#[test]
fn reclaim_miner_returns_inflight_plus_staged_and_isolates_by_owner() {
    let mut st = CoordinatorState::new();
    st.router.register_miner("cpu-0", caps());
    st.router.register_miner("cpu-1", caps());

    // cpu-0 owns two in-flight jobs; cpu-1 owns one.
    st.dispatch_inflight("cpu-0", job(b"a"));
    st.dispatch_inflight("cpu-0", job(b"b"));
    st.dispatch_inflight("cpu-1", job(b"c"));
    // One job staged on cpu-0 (route picks the first capable miner, cpu-0).
    assert_eq!(st.router.route(job(b"d")).as_deref(), Some("cpu-0"));

    let mut reclaimed = st.reclaim_miner("cpu-0");
    reclaimed.sort_by(|x, y| x.job_id.cmp(&y.job_id));
    let ids: Vec<&[u8]> = reclaimed.iter().map(|j| j.job_id.as_slice()).collect();
    assert_eq!(ids, vec![&b"a"[..], &b"b"[..], &b"d"[..]]);

    // cpu-0's in-flight is cleared from both maps; cpu-1's is untouched.
    assert!(!st.inflight.contains_key(&b"a"[..]));
    assert!(!st.inflight_owner.contains_key(&b"a"[..]));
    assert!(st.inflight.contains_key(&b"c"[..]));
    assert_eq!(st.router.staged_len("cpu-0"), 0);
}

fn job_with_generation(id: &[u8], generation: u64) -> Job {
    let mut j = job(id);
    j.generation = generation;
    j.provenance = Some(Provenance {
        is_pow: generation != 0,
        order_id: vec![],
    });
    j
}

#[test]
fn cancel_inflight_drops_pow_keeps_mempool_and_newer() {
    let mut st = CoordinatorState::new();
    st.dispatch_inflight("cpu-0", job_with_generation(b"mempool", 0));
    st.dispatch_inflight("cpu-0", job_with_generation(b"old", 3));
    st.dispatch_inflight("cpu-0", job_with_generation(b"live", 5));
    assert_eq!(st.cancel_inflight(4), 1);
    assert!(st.inflight.contains_key(&b"mempool"[..]));
    assert!(!st.inflight.contains_key(&b"old"[..]));
    assert!(st.inflight.contains_key(&b"live"[..]));
    assert!(st.complete_inflight(b"old").is_none());
}

/// Ending a session drops its outbound channel and its wakeup: a dead
/// session must not be woken, and waking an unregistered miner is harmless.
#[test]
fn ending_a_session_drops_its_outbound_and_wakeup() {
    let mut st = CoordinatorState::new();
    let (tx, _rx) = mpsc::channel::<Result<CoordMsg, Status>>(1);
    let (grants, _grant_rx) = mpsc::channel::<u32>(1);
    st.sessions.register_outbound("cpu-0", tx);
    st.sessions.register_wakeup("cpu-0", grants);
    assert!(st.sessions.outbound.contains_key("cpu-0"));
    assert!(st.sessions.wakeups.contains_key("cpu-0"));
    st.sessions.deregister_outbound("cpu-0");
    assert!(!st.sessions.outbound.contains_key("cpu-0"));
    assert!(
        !st.sessions.wakeups.contains_key("cpu-0"),
        "a dead session must not be woken"
    );
    st.sessions.wake_dispatcher("cpu-0");
}

/// A miner that grants its credits before any job is staged must still
/// receive work once the feeder stages it. The grant drains an empty queue,
/// so only the feeder's wakeup can start the dispatch. Without that wakeup
/// the miner waits for a job and the coordinator waits for a request, and
/// neither side ever moves.
///
/// C7 wiring check: `contexts_dispatched` moves at `dispatch_granted`'s real
/// call site, not just through the metrics API directly.
#[tokio::test]
async fn credits_granted_before_staging_still_dispatch() {
    let state = Arc::new(Mutex::new(CoordinatorState::new()));
    let (tx, mut rx) = mpsc::channel::<Result<CoordMsg, Status>>(8);
    let (grants, mut grant_rx) = mpsc::channel::<u32>(GRANT_CHANNEL_DEPTH);
    {
        let mut st = state.lock().await;
        st.router.register_miner("cpu-0", caps());
        st.sessions.register_wakeup("cpu-0", grants);
    }

    let dispatched = |st: &CoordinatorState| st.metrics.global().contexts_dispatched;
    assert_eq!(dispatched(&*state.lock().await), 0);

    // The miner grants credits first. Nothing is staged, so nothing is sent.
    let events = service(&state, Arc::new(FakeChain::new(lease_snapshot(), None)));
    assert!(dispatch_granted(events.as_ref(), "cpu-0", 32, &tx).await);
    assert!(
        rx.try_recv().is_err(),
        "no job exists yet, so none can dispatch"
    );
    // This catches a metrics bump placed outside the `while let` drain.
    assert_eq!(dispatched(&*state.lock().await), 0);

    // The feeder stages work and wakes the dispatcher.
    {
        let mut st = state.lock().await;
        assert!(st.router.stage_on("cpu-0", job(b"late")));
        st.sessions.wake_dispatcher("cpu-0");
    }

    // The wakeup carries no credits: the balance from the first grant stands.
    assert_eq!(grant_rx.try_recv().expect("wakeup was queued"), 0);
    assert!(dispatch_granted(events.as_ref(), "cpu-0", 0, &tx).await);
    let sent = rx
        .try_recv()
        .expect("the staged job dispatches on the wakeup");
    let Ok(CoordMsg {
        msg: Some(coord_msg::Msg::Job(j)),
    }) = sent
    else {
        panic!("expected a Job message");
    };
    assert_eq!(j.job_id, b"late".to_vec());
    assert_eq!(dispatched(&*state.lock().await), 1);
}

#[tokio::test]
async fn capabilities_frames_replace_the_envelope_and_ignore_wrong_protocol() {
    use quip_proto::v1::{Algorithm, Backend, Capabilities, Hello};

    let mut st = CoordinatorState::new();
    let _ = st
        .sessions
        .expected_tokens
        .insert("m".to_owned(), "token".to_owned());
    st.dispatch_inflight("m", job(b"already-dispatched"));
    let state = Arc::new(Mutex::new(st));
    let hello_caps = Capabilities {
        protocol_version: PROTOCOL_VERSION,
        max_nodes: 100,
        ..Capabilities::default()
    };
    let updated = Capabilities {
        protocol_version: PROTOCOL_VERSION,
        backend: Backend::Cuda as i32,
        algorithm: Algorithm::Msa as i32,
        max_nodes: 64,
        max_edges: 128,
        max_degree: Some(20),
        stream_width: 4,
        native_topology_hash: Some(vec![9; 32]),
        features: vec!["streaming".to_owned()],
        ..Capabilities::default()
    };
    let wrong = Capabilities {
        protocol_version: 1,
        max_nodes: 1,
        ..Capabilities::default()
    };
    let frame = |msg: miner_msg::Msg| MinerMsg { msg: Some(msg) };
    let mut inbound = tokio_stream::iter(
        [
            frame(miner_msg::Msg::Hello(Hello {
                miner_id: "m".to_owned(),
                session_token: "token".to_owned(),
                capabilities: Some(hello_caps),
            })),
            frame(miner_msg::Msg::Capabilities(updated.clone())),
            frame(miner_msg::Msg::Capabilities(wrong)),
        ]
        .map(Ok),
    );
    let (tx, _rx) = mpsc::channel(8);
    run_session(
        &mut inbound,
        &tx,
        Arc::clone(&state),
        service(&state, Arc::new(FakeChain::new(lease_snapshot(), None))),
        Arc::new(CancelTiming::default()),
    )
    .await;
    let st = state.lock().await;
    let actual = st.router.caps("m");
    assert_eq!(actual.map(|c| c.max_nodes), Some(64));
    assert_eq!(actual.map(|c| c.max_edges), Some(128));
    assert_eq!(actual.and_then(|c| c.max_degree), Some(20));
    assert_eq!(actual.map(|c| c.stream_width), Some(4));
    assert_eq!(
        actual.and_then(|c| c.native_topology_hash.as_ref()),
        updated.native_topology_hash.as_ref()
    );
    assert_eq!(
        actual.map(|c| c.features.as_slice()),
        Some(updated.features.as_slice())
    );
    assert!(st.inflight.contains_key(b"already-dispatched".as_slice()));
}

#[tokio::test]
async fn calibrate_done_without_request_is_ignored() {
    use quip_proto::v1::{CalibrateDone, CalibrateOutcome, Hello, JobRequest};

    let mut st = CoordinatorState::new();
    let _ = st
        .sessions
        .expected_tokens
        .insert("m".to_owned(), "token".to_owned());
    let state = Arc::new(Mutex::new(st));
    let frame = |msg: miner_msg::Msg| MinerMsg { msg: Some(msg) };
    let (in_tx, in_rx) = mpsc::channel::<Result<MinerMsg, Status>>(8);
    let mut inbound = ReceiverStream::new(in_rx);
    let (tx, mut rx) = mpsc::channel::<Result<CoordMsg, Status>>(8);
    let session = {
        let state = Arc::clone(&state);
        let events = service(&state, Arc::new(FakeChain::new(lease_snapshot(), None)));
        tokio::spawn(async move {
            run_session(
                &mut inbound,
                &tx,
                state,
                events,
                Arc::new(CancelTiming::default()),
            )
            .await;
        })
    };
    let send = |msg: miner_msg::Msg| {
        let in_tx = in_tx.clone();
        async move { in_tx.send(Ok(frame(msg))).await.expect("session reads") }
    };
    send(miner_msg::Msg::Hello(Hello {
        miner_id: "m".to_owned(),
        session_token: "token".to_owned(),
        capabilities: Some(Capabilities {
            protocol_version: PROTOCOL_VERSION,
            max_nodes: 100,
            ..Capabilities::default()
        }),
    }))
    .await;
    // Welcome arrives once the handshake passes.
    let welcome = rx.recv().await.expect("welcome").expect("ok");
    assert!(matches!(welcome.msg, Some(coord_msg::Msg::Welcome(_))));
    // Configure follows Welcome; the job is the first message after both.
    let configure = rx.recv().await.expect("configure").expect("ok");
    assert!(matches!(configure.msg, Some(coord_msg::Msg::Configure(_))));
    {
        let mut st = state.lock().await;
        assert!(st.router.stage_on("m", job(b"after-calibrate")));
    }

    send(miner_msg::Msg::CalibrateDone(CalibrateDone {
        outcome: CalibrateOutcome::Done as i32,
        summary: None,
    }))
    .await;
    send(miner_msg::Msg::JobRequest(JobRequest { credits: 1 })).await;

    let sent = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the job dispatches after the stray CalibrateDone")
        .expect("session still open")
        .expect("ok");
    let Some(coord_msg::Msg::Job(j)) = sent.msg else {
        panic!("expected a Job message");
    };
    assert_eq!(j.job_id, b"after-calibrate".to_vec());
    assert!(!session.is_finished(), "the session stays open");
    drop(in_tx);
    session.await.expect("session ends cleanly");
}

#[test]
fn set_topology_populates_the_router_graph_metrics() {
    let topology = Topology::from_nodes_edges(
        vec![10, 20, 30],
        vec![(10, 20), (10, 30)],
        &[0],
        &[1000],
        &[-1000, 1000],
    );
    let problem = crate::types::problem::milli_problem(
        Some(quip_proto::v1::ising_problem::Graph::TopologyHash(
            topology.hash.clone(),
        )),
        &[0; 3],
        &[0; 2],
    );
    let mut st = CoordinatorState::new();
    st.router.register_miner(
        "m",
        MinerCaps {
            max_degree: Some(1),
            ..caps()
        },
    );
    st.set_topology(Some(topology));
    assert!(!st.router.stage_on(
        "m",
        Job {
            kind: JobKind::IsingSample as i32,
            ising: Some(problem),
            ..Job::default()
        }
    ));
}

/// One in-process session. The test plays the miner on `miner` and the
/// transport on `outbound`.
struct LiveSession {
    miner: mpsc::Sender<MinerMsg>,
    outbound: OutboundStream,
    tx: mpsc::Sender<Result<CoordMsg, Status>>,
    state: Arc<Mutex<CoordinatorState>>,
}

fn frame(msg: miner_msg::Msg) -> MinerMsg {
    MinerMsg { msg: Some(msg) }
}

async fn next_msg(outbound: &mut OutboundStream) -> coord_msg::Msg {
    let item = outbound.next().await.expect("session is open");
    item.expect("no error frames").msg.expect("message body")
}

/// Start a session for miner `m` with an outbound channel of
/// `outbound_depth`, and complete its handshake. No `Configure` is set,
/// so the coordinator sends `queue_depth = 0`, which the miner SDKs read
/// as 3: the depth the removed `Ready` seed used to grant.
async fn start_session(chain: Arc<FakeChain>, outbound_depth: usize) -> LiveSession {
    use quip_proto::v1::Hello;

    let mut st = CoordinatorState::new();
    let _ = st
        .sessions
        .expected_tokens
        .insert("m".to_owned(), "token".to_owned());
    let state = Arc::new(Mutex::new(st));
    let (miner, miner_rx) = mpsc::channel(16);
    let (tx, rx) = mpsc::channel(outbound_depth);
    let timing = Arc::new(CancelTiming::default());
    let mut outbound = outbound_stream(rx, Arc::clone(&timing));
    {
        let state = Arc::clone(&state);
        let tx = tx.clone();
        drop(tokio::spawn(async move {
            let mut inbound = ReceiverStream::new(miner_rx).map(Ok);
            let events = service(&state, chain);
            run_session(&mut inbound, &tx, state, events, timing).await;
        }));
    }
    miner
        .send(frame(miner_msg::Msg::Hello(Hello {
            miner_id: "m".to_owned(),
            session_token: "token".to_owned(),
            capabilities: Some(Capabilities {
                protocol_version: PROTOCOL_VERSION,
                supported_kinds: vec![JobKind::IsingSample as i32],
                ..Capabilities::default()
            }),
        })))
        .await
        .expect("session reads Hello");
    assert!(matches!(
        next_msg(&mut outbound).await,
        coord_msg::Msg::Welcome(_)
    ));
    assert!(matches!(
        next_msg(&mut outbound).await,
        coord_msg::Msg::Configure(_)
    ));
    LiveSession {
        miner,
        outbound,
        tx,
        state,
    }
}

/// Stage `jobs` for `m` and wake its dispatcher, as the feeder does.
async fn stage(state: &Arc<Mutex<CoordinatorState>>, jobs: &[&[u8]]) {
    let mut st = state.lock().await;
    for id in jobs {
        assert!(st.router.stage_on("m", job(id)));
    }
    st.sessions.wake_dispatcher("m");
}

/// `Ready` grants nothing. A budgeted miner, such as a QPU that sends no
/// `JobRequest` until it decides to spend, receives no jobs while work
/// is staged for it, and its first grant dispatches exactly that many.
#[tokio::test(start_paused = true)]
async fn a_budgeted_miner_receives_no_jobs_before_its_first_grant() {
    let mut s = start_session(Arc::new(FakeChain::new(lease_snapshot(), None)), 8).await;
    stage(&s.state, &[b"a", b"b"]).await;
    s.miner
        .send(frame(miner_msg::Msg::Ready(quip_proto::v1::Ready {})))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_mins(1), s.outbound.next())
            .await
            .is_err(),
        "Ready must not dispatch work"
    );
    assert!(s.state.lock().await.inflight.is_empty());

    s.miner
        .send(frame(miner_msg::Msg::JobRequest(
            quip_proto::v1::JobRequest { credits: 1 },
        )))
        .await
        .unwrap();
    let coord_msg::Msg::Job(first) = next_msg(&mut s.outbound).await else {
        panic!("the grant dispatches a job");
    };
    assert_eq!(first.job_id, b"a".to_vec());
    assert!(
        tokio::time::timeout(Duration::from_mins(1), s.outbound.next())
            .await
            .is_err(),
        "one credit dispatches one job"
    );
}

/// A reject frees the job and re-routes it, and counts as a reject, not
/// as completed mining work. The miner's replacement credit still flows.
#[tokio::test(start_paused = true)]
async fn a_reject_counts_apart_from_completed_work_and_keeps_flow_control() {
    let mut s = start_session(Arc::new(FakeChain::new(lease_snapshot(), None)), 8).await;
    stage(&s.state, &[b"a", b"b"]).await;
    s.miner
        .send(frame(miner_msg::Msg::JobRequest(
            quip_proto::v1::JobRequest { credits: 1 },
        )))
        .await
        .unwrap();
    assert!(matches!(next_msg(&mut s.outbound).await, coord_msg::Msg::Job(j) if j.job_id == b"a"));
    s.miner
        .send(frame(miner_msg::Msg::Reject(quip_proto::v1::Reject {
            job_id: b"a".to_vec(),
            reason: quip_proto::v1::RejectReason::Overloaded as i32,
        })))
        .await
        .unwrap();
    s.miner
        .send(frame(miner_msg::Msg::JobRequest(
            quip_proto::v1::JobRequest { credits: 1 },
        )))
        .await
        .unwrap();
    assert!(matches!(next_msg(&mut s.outbound).await, coord_msg::Msg::Job(j) if j.job_id == b"b"));
    let st = s.state.lock().await;
    assert!(
        !st.inflight.contains_key(b"a".as_slice()),
        "the rejected job is cleaned up"
    );
    assert_eq!(st.router.jobs_rejected("m"), 1);
    assert_eq!(st.router.jobs_completed("m"), 0);
}

/// The `Cancel` queues behind the jobs already in the outbound channel,
/// and the miner's acknowledging `Status` queues behind a `Result` whose
/// submit is slow. The timing puts each delay in its own interval.
///
/// The transport reads one message every 10 ms from a full channel of 4
/// jobs. The `Cancel` waits 10 ms for a slot, then 40 ms for the 4 jobs
/// ahead of it. The miner then sends a `Result` and the `Status`; the
/// `Result`'s 50 ms submit holds the `Status` in the read loop.
#[tokio::test(start_paused = true)]
async fn cancel_timing_locates_queued_jobs_and_a_slow_result_reader() {
    init_capture();
    let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(Capture(Arc::clone(&buf)))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let chain = Arc::new(FakeChain::new(lease_snapshot(), None));
    chain.set_submit_delay(Duration::from_millis(50));
    let mut s = start_session(Arc::clone(&chain), 4).await;
    stage(&s.state, &[b"j1", b"j2", b"j3", b"j4"]).await;
    s.miner
        .send(frame(miner_msg::Msg::JobRequest(
            quip_proto::v1::JobRequest { credits: 4 },
        )))
        .await
        .unwrap();
    while s.tx.capacity() > 0 {
        tokio::task::yield_now().await;
    }

    let timing = s
        .state
        .lock()
        .await
        .sessions
        .cancel_timings
        .get("m")
        .cloned()
        .unwrap();
    let start = tokio::time::Instant::now();
    let cancel = {
        let tx = s.tx.clone();
        tokio::spawn(async move { send_cancel(&tx, Some(&timing), 1).await })
    };

    let mut jobs = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(10)).await;
        match next_msg(&mut s.outbound).await {
            coord_msg::Msg::Job(_) => jobs += 1,
            coord_msg::Msg::Cancel(c) => {
                assert_eq!(c.max_generation, 1);
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(
        jobs, 4,
        "the Cancel leaves the channel after the queued jobs"
    );
    assert_eq!(start.elapsed(), Duration::from_millis(50));
    cancel.await.unwrap().unwrap();

    s.miner
        .send(frame(miner_msg::Msg::Result(WireResult {
            job_id: b"j1".to_vec(),
            solutions: vec![Solution {
                values: vec![],
                spins: quip_protocol::wire::encode_spins_packed(&[1, 1]),
                energy_milli: 0,
            }],
            ..WireResult::default()
        })))
        .await
        .unwrap();
    s.miner
        .send(frame(miner_msg::Msg::Status(quip_proto::v1::Status {
            miner_id: "m".to_owned(),
            abandoned_generation: 1,
            ..quip_proto::v1::Status::default()
        })))
        .await
        .unwrap();
    while !drain_peek(&buf).contains("miner acknowledged cancel") {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let output = drain(&buf);
    assert_eq!(chain.submitted_count(), 1, "the slow submit ran: {output}");
    for field in [
        "max_generation=1",
        "send_wait_ms=10.0",
        "queue_delay_ms=40.0",
        "ack_wait_ms=50.0",
        "total_ms=100.0",
    ] {
        assert!(output.contains(field), "{field} missing: {output}");
    }
}

fn drain_peek(buf: &Arc<std::sync::Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&buf.lock().unwrap()).into_owned()
}
