//! Feeder rounds and the runtime lifecycle against `FakeChain`.
//!
//! The feeder tests drive `feeder_loop` directly and observe `CoordinatorState`
//! and the fake chain. The runtime test covers the process wiring: supervise a
//! real `quip-mock-miner`, feed it through a live session, submit what it
//! returns, and shut down on signal.

#![expect(
    clippy::expect_used,
    reason = "test fixtures: a failed setup should stop the test"
)]
#![expect(
    clippy::cast_possible_truncation,
    reason = "fixture drain rates fit u32"
)]

mod common;

use ising_generators::random::derive_nonce;
use quip_coordinator::chain::{FakeChain, JobOrder, MiningSnapshot, PendingProof};
use quip_coordinator::config::LaunchEntry;
use quip_coordinator::router::MinerCaps;
use quip_coordinator::runtime::{feeder_loop, run_runtime, FeederParams, RuntimeParams};
use quip_coordinator::session::CoordinatorState;
use quip_coordinator::supervisor::BackoffPolicy;
use quip_proto::v1::{coord_msg, Configure, CoordMsg, GeneratorAlgorithm, JobKind};
use sp_core::crypto::{AccountId32, Ss58Codec};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch, Mutex};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A four-node ring with real coefficient sets, so derived `PoW` jobs stage.
/// The gates are loose: one row, any energy, no diversity floor.
fn ising_snapshot() -> MiningSnapshot {
    let nodes = vec![0, 1, 2, 3];
    let edges = vec![(0, 1), (1, 2), (2, 3), (0, 3)];
    let h = vec![-1000, 0, 1000];
    let j = vec![-1000, 1000];
    let spin = vec![-1000, 1000];
    let topology_hash =
        quip_coordinator::topology::topology_hash_sets(&nodes, &edges, &h, &j, &spin).to_vec();
    MiningSnapshot {
        head_hash: [0u8; 32],
        last_proof_block_hash: [7u8; 32],
        topology_hash,
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
        family: None,
    }
}

fn snapshot_with_head(head: [u8; 32]) -> MiningSnapshot {
    let mut snap = ising_snapshot();
    snap.last_proof_block_hash = head;
    snap
}

fn snapshot_at(head: [u8; 32], block_number: u64) -> MiningSnapshot {
    let mut snap = snapshot_with_head(head);
    snap.block_number = block_number;
    snap
}

/// Difficulty gates that demand three mutually distant solutions, with the
/// energy ceiling the caller wants live on the chain.
fn gated_snapshot(max_energy_milli: i64) -> MiningSnapshot {
    let mut snap = snapshot_with_head([9u8; 32]);
    snap.min_solutions = 3;
    snap.min_diversity_milli = 300;
    snap.max_energy_milli = max_energy_milli;
    snap
}

/// An open order on the `ising_snapshot` graph with no energy gate, so any
/// sample the mock miner returns clears it.
fn mempool_order() -> JobOrder {
    JobOrder {
        order_id: 7u64.to_le_bytes().to_vec(),
        mode: quip_coordinator::chain::scale_types::JobMode::Open,
        nodes: vec![0, 1, 2, 3],
        edges: vec![(0, 1), (1, 2), (2, 3), (0, 3)],
        h_milli: vec![0; 4],
        j_milli: vec![-1000; 4],
        min_energy_milli: None,
        min_diversity_milli: None,
        min_solutions: Some(1),
        deadline_ms: 0,
    }
}

fn ising_caps() -> MinerCaps {
    MinerCaps {
        backend: "mock".into(),
        algorithm: "sa".into(),
        supported_kinds: vec![JobKind::IsingSample as i32],
        max_nodes: 0,
        max_edges: 0,
        ..MinerCaps::default()
    }
}

fn lease_caps() -> MinerCaps {
    MinerCaps {
        supported_kinds: vec![JobKind::IsingSample as i32, JobKind::IsingGenerate as i32],
        generators: vec![GeneratorAlgorithm::Blake3Chacha8V1 as i32],
        stream_width: 2,
        ..ising_caps()
    }
}

fn feeder_params(buffer_depth: usize, poll_ms: u64) -> FeederParams {
    FeederParams {
        max_submit_attempts: 5,
        miner_identity: [0u8; 32],
        miner_account: [0u8; 32],
        buffer_depth,
        poll_interval: Duration::from_millis(poll_ms),
        funding: quip_coordinator::funding::FundingParams::default(),
        descriptor: quip_coordinator::config::DescriptorParams::default(),
        descriptor_filed: Arc::new(AtomicBool::new(false)),
        miner_registered: Arc::new(AtomicBool::new(false)),
        solver_registered: Arc::new(AtomicBool::new(false)),
        metrics: Arc::new(quip_coordinator::metrics::CoordinatorMetrics::new(&[])),
    }
}

fn named_feeder_params(buffer_depth: usize, poll_ms: u64) -> FeederParams {
    let mut params = feeder_params(buffer_depth, poll_ms);
    params.descriptor = quip_coordinator::config::DescriptorParams {
        node_name: Some("Tesla".into()),
        public_host: Some("96.233.112.201".into()),
        rpc_endpoints: vec!["ws://127.0.0.1:9944".into()],
        miners: vec![
            quip_coordinator::chain::MinerSpecScale {
                kind: quip_coordinator::chain::MinerKind::Cpu,
                label: Some(b"cpu-0".to_vec()),
                backend: Some(b"cpu".to_vec()),
                device_id: None,
            },
            quip_coordinator::chain::MinerSpecScale {
                kind: quip_coordinator::chain::MinerKind::Metal,
                label: Some(b"metal-0".to_vec()),
                backend: Some(b"metal".to_vec()),
                device_id: None,
            },
        ],
        ..quip_coordinator::config::DescriptorParams::default()
    };
    params
}

/// A pending proof that clears `snap`. `ising_snapshot` sets the ceiling to
/// `i64::MAX / 2` with one required row and no diversity floor, so any
/// well-formed row clears once the nonce matches the root.
fn clearing_pending_proof(snap: &MiningSnapshot, account: [u8; 32]) -> PendingProof {
    use quantum_validation::packed::pack_solution;
    use quantum_validation::{derive_nonce, AllowedValueSpec};
    use quip_coordinator::chain::extrinsic::account_identity_bytes;
    use quip_coordinator::chain::scale_types::QuantumProof;

    let salt = [3u8; 32];
    let nonce = derive_nonce(
        &snap.last_proof_block_hash,
        &account_identity_bytes(&account),
        &salt,
    );
    let spins = vec![1000i32; snap.nodes.len()];
    let packed = pack_solution(
        &spins,
        &AllowedValueSpec::Set(snap.allowed_spin_milli.as_slice()),
    )
    .expect("pack");
    PendingProof {
        extrinsic_hash: account,
        account,
        proof: QuantumProof {
            topology_hash: sp_core::H256::from_slice(&snap.topology_hash),
            nonce,
            salt,
            solutions: vec![packed],
            device_access_time_us: 0,
        },
    }
}

/// Three mutually distant rows, each pair at symmetric Hamming distance 2 of 4
/// spins, so any two score 500 milli and all three score 500 milli.
fn distant_rows() -> Vec<quip_coordinator::validate::SpinRow> {
    [
        (vec![1, 1, 1, 1], -3000),
        (vec![1, 1, -1, -1], -2000),
        (vec![1, -1, 1, -1], -1000),
    ]
    .into_iter()
    .map(
        |(spins, energy_milli)| quip_coordinator::validate::SpinRow {
            spins,
            energy_milli,
        },
    )
    .collect()
}

/// A stashed candidate whose viability projection has already arrived, so the
/// only thing standing between it and a submission is the live difficulty.
fn due_candidate() -> quip_coordinator::stash::Candidate {
    quip_coordinator::stash::Candidate {
        job_id: vec![0xaa],
        salt: Some([1u8; 32]),
        generation: 1,
        best_energy_milli: -3000,
        diversity_milli: 500,
        n_valid: 3,
        solutions: distant_rows(),
        is_pow: true,
        order_id: vec![],
        device_access_time_us: 0,
        submitted: false,
    }
}

/// Poll `check` every `every_ms` until it holds or `polls` attempts pass.
async fn wait_until(polls: usize, every_ms: u64, mut check: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..polls {
        tokio::time::sleep(Duration::from_millis(every_ms)).await;
        if check().await {
            return true;
        }
    }
    false
}

fn new_generation_staged(st: &CoordinatorState, generation: u64) -> usize {
    if st.generation != generation {
        return 0;
    }
    st.router.staged_len("cpu-0")
}

type Outbound = mpsc::Receiver<Result<CoordMsg, tonic::Status>>;

/// A running `feeder_loop` with the state and chain it works against.
struct Feeder {
    chain: Arc<FakeChain>,
    state: Arc<Mutex<CoordinatorState>>,
    rx: Option<Outbound>,
    stop_tx: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Feeder {
    /// Spawn the feeder with `miners` registered in the router.
    async fn spawn(chain: FakeChain, params: FeederParams, miners: Vec<(&str, MinerCaps)>) -> Self {
        Self::start(chain, params, miners, false).await
    }

    /// Like [`Self::spawn`], with `cpu-0` holding a live outbound channel so
    /// the test can read the feeder's broadcasts.
    async fn spawn_with_outbound(chain: FakeChain, params: FeederParams) -> Self {
        Self::start(chain, params, vec![("cpu-0", ising_caps())], true).await
    }

    async fn start(
        chain: FakeChain,
        params: FeederParams,
        miners: Vec<(&str, MinerCaps)>,
        outbound: bool,
    ) -> Self {
        let chain = Arc::new(chain);
        let state = Arc::new(Mutex::new(CoordinatorState::new()));
        let mut rx = None;
        {
            let mut st = state.lock().await;
            for (id, caps) in miners {
                st.router.register_miner(id, caps);
            }
            if outbound {
                let (tx, receiver) = mpsc::channel(64);
                st.register_outbound("cpu-0", tx);
                rx = Some(receiver);
            }
        }
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(feeder_loop(
            Arc::clone(&chain),
            Arc::clone(&state),
            params,
            stop_rx,
        ));
        Self {
            chain,
            state,
            rx,
            stop_tx,
            task,
        }
    }

    async fn stop(self) {
        let _ = self.stop_tx.send(true);
        tokio::time::timeout(Duration::from_secs(2), self.task)
            .await
            .expect("feeder did not stop")
            .expect("feeder task panicked");
    }

    async fn staged(&self) -> usize {
        self.state.lock().await.router.staged_len("cpu-0")
    }

    async fn generation(&self) -> u64 {
        self.state.lock().await.generation
    }

    async fn wait_staged(&self, n: usize) -> bool {
        wait_until(40, 40, async || self.staged().await >= n).await
    }

    /// Wait until `generation` is current and has work staged on `cpu-0`.
    async fn wait_generation(&self, generation: u64) -> bool {
        wait_until(40, 40, async || {
            new_generation_staged(&*self.state.lock().await, generation) > 0
        })
        .await
    }

    async fn wait_participations(&self, n: usize) -> bool {
        wait_until(40, 40, async || self.chain.participation_calls() >= n).await
    }

    fn rx(&mut self) -> &mut Outbound {
        self.rx.as_mut().expect("feeder spawned with an outbound")
    }

    async fn recv(&mut self) -> coord_msg::Msg {
        tokio::time::timeout(Duration::from_secs(2), self.rx().recv())
            .await
            .expect("no outbound")
            .expect("closed")
            .expect("status")
            .msg
            .expect("outbound message has a body")
    }

    /// True when nothing is waiting on the outbound.
    fn quiet(&mut self) -> bool {
        self.rx().try_recv().is_err()
    }

    /// Drive the first round to staged work and drain its broadcasts.
    async fn first_round_staged(&mut self) {
        assert!(
            self.wait_generation(1).await,
            "first round never staged jobs"
        );
        while self.rx().try_recv().is_ok() {}
    }

    /// Expect the reseed triple for `cancelled`, in order: `Cancel`, then
    /// `Topology`, then `SetTarget`.
    async fn expect_reseed(&mut self, cancelled: u64) {
        self.expect_cancel(cancelled).await;
        let msg = self.recv().await;
        assert!(
            matches!(msg, coord_msg::Msg::Topology(_)),
            "Topology must follow Cancel, got {msg:?}"
        );
        let msg = self.recv().await;
        assert!(
            matches!(msg, coord_msg::Msg::SetTarget(_)),
            "SetTarget must follow Topology, got {msg:?}"
        );
    }

    /// Expect `Cancel(cancelled)` with nothing else before it.
    async fn expect_cancel(&mut self, cancelled: u64) {
        let msg = self.recv().await;
        assert!(
            matches!(&msg, coord_msg::Msg::Cancel(c) if c.max_generation == cancelled),
            "expected Cancel({cancelled}), got {msg:?}"
        );
    }

    /// Play one Result for `generation` the way the session path does:
    /// dispatch a staged job to cpu-0 and complete it as this round's work.
    /// False when no job of that generation is staged in time.
    async fn return_one_result(&self, generation: u64) -> bool {
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let mut st = self.state.lock().await;
            if st.generation != generation || st.router.staged_len("cpu-0") == 0 {
                continue;
            }
            st.router.grant_credits("cpu-0", 1);
            let Some(job) = st.router.next_job("cpu-0") else {
                continue;
            };
            let job_id = job.job_id.clone();
            let job_generation = job.generation;
            st.dispatch_inflight("cpu-0", job);
            let _ = st.complete_inflight(&job_id);
            st.note_result(job_generation);
            return true;
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

/// The signing account and the `PoW` identity are two different 32-byte values
/// derived from one key, and each has exactly one correct destination. Funding
/// the identity leaves the signer broke, so every submit fails with "Inability
/// to pay some fees"; deriving nonces from the account makes every proof fail
/// `InvalidNonce`. Neither failure names the mix-up, so pin both routes here.
///
/// A per-salt miner is topped up to `buffer_depth` with one salt recorded per
/// job. A lease miner gets disjoint salt ranges that carry their own salts.
#[tokio::test]
async fn feeder_stages_per_salt_jobs_and_leases_from_the_identity() {
    const IDENTITY: [u8; 32] = [0x11; 32];
    const ACCOUNT: [u8; 32] = [0x22; 32];
    const HEAD: [u8; 32] = [0x33; 32];
    let depth = quip_coordinator::lease::LEASE_STAGE_DEPTH;

    let mut params = feeder_params(4, 30);
    params.miner_identity = IDENTITY;
    params.miner_account = ACCOUNT;
    let f = Feeder::spawn(
        FakeChain::new(snapshot_with_head(HEAD), None),
        params,
        vec![("cpu-0", ising_caps()), ("cpu-1", lease_caps())],
    )
    .await;

    let filled = wait_until(40, 50, async || {
        let st = f.state.lock().await;
        st.router.staged_len("cpu-0") == 4 && st.router.staged_len("cpu-1") == depth
    })
    .await;
    assert!(filled, "feeder never filled both buffers");

    let checked = f.chain.take_balance_accounts();
    assert!(!checked.is_empty(), "funding never read a balance");
    assert!(
        checked.iter().all(|a| *a == ACCOUNT),
        "funding must read the signing account, not the PoW identity: {checked:?}"
    );

    let mut st = f.state.lock().await;
    assert!(st.topology.is_some(), "reseed sets the topology");
    assert_eq!(st.target.map(|t| t.max_proof_solutions), Some(32));
    assert_eq!(
        st.salts.len(),
        4,
        "one salt per staged job; leases carry their own"
    );
    for (job_id, salt) in &st.salts {
        assert_eq!(
            job_id.as_slice(),
            derive_nonce(HEAD, IDENTITY, *salt).as_slice(),
            "job nonce must derive from the PoW identity"
        );
        assert_ne!(
            job_id.as_slice(),
            derive_nonce(HEAD, ACCOUNT, *salt).as_slice(),
            "job nonce must not derive from the signing account"
        );
    }

    st.router.grant_credits("cpu-1", 8);
    let mut ranges = Vec::new();
    while let Some(job) = st.router.next_job("cpu-1") {
        assert_eq!(job.kind, JobKind::IsingGenerate as i32);
        let g = job.generator.expect("lease generator");
        assert_eq!(g.miner_account, IDENTITY, "leases use the PoW identity");
        assert_ne!(g.miner_account, ACCOUNT);
        ranges.push((g.salt_start, g.salt_start + g.salt_count));
    }
    assert_eq!(ranges.len(), depth);
    ranges.sort_unstable();
    assert!(
        ranges.windows(2).all(|pair| {
            let [(_, left_end), (right_start, _)] = pair else {
                return false;
            };
            left_end <= right_start
        }),
        "overlapping ranges: {ranges:?}"
    );
    assert!(
        ranges.iter().all(|(s, e)| e - s == 9_000),
        "a first lease of width 2 covers 60 seconds at 75 salts per second per slot"
    );
    drop(st);
    f.stop().await;
}

/// Registration sits between funding and requirements. A coordinator that
/// cannot register must not mine: the pallet rejects every proof from an
/// unregistered account, and the account still pays for each attempt.
#[tokio::test]
async fn feeder_registers_once_and_holds_off_mining_until_it_succeeds() {
    let chain = FakeChain::new(ising_snapshot(), None);
    chain.set_registration_result(Err(quip_coordinator::chain::ChainError::Unavailable(
        "rpc down".into(),
    )));
    let f = Feeder::spawn(chain, feeder_params(2, 30), vec![("cpu-0", ising_caps())]).await;

    // Registration keeps failing, so the walk never reaches staging.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            f.staged().await,
            0,
            "an unregistered miner must not be fed work"
        );
    }

    f.chain
        .set_registration_result(Ok(quip_coordinator::chain::RegistrationOutcome::Registered));
    assert!(
        wait_until(60, 30, async || f.staged().await >= 2).await,
        "mining never started after registration succeeded"
    );

    let submits = f.chain.registration_submits();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        f.chain.registration_submits(),
        submits,
        "later rounds must not re-submit register_miner"
    );
    f.stop().await;
}

#[tokio::test]
async fn feeder_grows_window_for_drainer_and_holds_floor_for_idle() {
    // Floor of 2; the fast miner drains a fixed ~8/interval so its adaptive
    // window converges to ~2x that (headroom), well above the floor. The idle
    // miner never consumes, so it stays pinned at the floor.
    const FLOOR: usize = 2;
    const DRAIN_RATE: usize = 8;

    let f = Feeder::spawn(
        FakeChain::new(ising_snapshot(), None),
        feeder_params(FLOOR, 30),
        vec![("cpu-fast", ising_caps()), ("cpu-idle", ising_caps())],
    )
    .await;

    // Simulate a fixed-throughput miner: each interval, grant credits and pull
    // up to DRAIN_RATE staged jobs (min with what's available, so an unramped
    // buffer can't force runaway growth). Watch the window climb past the floor.
    let grew = wait_until(80, 30, async || {
        let mut st = f.state.lock().await;
        st.router.grant_credits("cpu-fast", DRAIN_RATE as u32);
        for _ in 0..DRAIN_RATE {
            if st.router.next_job("cpu-fast").is_none() {
                break;
            }
        }
        drop(st);
        f.state.lock().await.router.staged_len("cpu-fast") >= DRAIN_RATE
    })
    .await;
    assert!(
        grew,
        "adaptive window never grew to the drain rate under sustained consumption"
    );
    assert_eq!(
        f.state.lock().await.router.staged_len("cpu-idle"),
        FLOOR,
        "idle miner should stay pinned at the buffer_depth floor"
    );
    f.stop().await;
}

// ---------------------------------------------------------------------------
// Rounds and broadcasts
// ---------------------------------------------------------------------------

/// The first reseed pushes `Topology` then `SetTarget` to the live miner, and
/// an unchanged head does not push them again. A new qblock is not a target
/// refresh: the feeder stops the dead generation, pushes the new round's
/// `Topology` and `SetTarget`, and only then stages jobs of the new
/// generation. Same topology hash and same gates still require that push.
#[tokio::test]
async fn feeder_sends_requirements_before_staging_each_round() {
    let mut f = Feeder::spawn_with_outbound(
        FakeChain::new(snapshot_with_head([1u8; 32]), None),
        feeder_params(2, 40),
    )
    .await;

    // Generation 0 has nothing to cancel, so the first round skips Cancel.
    let first = f.recv().await;
    assert!(
        matches!(first, coord_msg::Msg::Topology(_)),
        "first message must be Topology, got {first:?}"
    );
    let second = f.recv().await;
    let snap = ising_snapshot();
    match second {
        coord_msg::Msg::SetTarget(target) => {
            assert_eq!(target.max_energy_milli, snap.max_energy_milli);
            assert_eq!(target.min_solutions, snap.min_solutions);
            assert_eq!(target.min_diversity_milli, snap.min_diversity_milli);
        }
        other => panic!("second message must be SetTarget, got {other:?}"),
    }
    assert!(f.wait_staged(2).await, "first round never staged jobs");

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        f.quiet(),
        "unchanged head and difficulty must not re-broadcast"
    );

    // Same topology, same difficulty, new head: still a new round.
    f.chain.set_snapshot(Some(snapshot_with_head([2u8; 32])));
    f.expect_reseed(1).await;
    assert!(
        f.wait_generation(2).await,
        "new generation was never staged after requirements"
    );
    f.stop().await;
}

/// A proof in the pool that clears the round stops the miners: the current
/// generation is cancelled and nothing is staged until the block that
/// includes the proof arrives. That block's hash is the next root.
#[tokio::test]
async fn feeder_stops_on_a_clearing_pending_proof_and_mines_the_new_root() {
    use quip_coordinator::chain::extrinsic::hex_encode;

    let mut f = Feeder::spawn_with_outbound(
        FakeChain::new(snapshot_with_head([1u8; 32]), None),
        feeder_params(2, 40),
    )
    .await;
    f.first_round_staged().await;

    f.chain.set_pending_proofs(vec![clearing_pending_proof(
        &snapshot_with_head([1u8; 32]),
        [5u8; 32],
    )]);
    f.expect_cancel(1).await;
    assert_eq!(f.generation().await, 2);
    assert_eq!(f.staged().await, 0, "nothing stays staged");

    // Waiting: no requirements, no staging, while the root is unchanged.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(f.quiet(), "no broadcast while awaiting the qblock");
    assert_eq!(f.staged().await, 0);

    // The block that includes the proof arrives: its hash is the new root.
    f.chain.set_pending_proofs(Vec::new());
    f.chain.set_snapshot(Some(snapshot_with_head([2u8; 32])));
    f.expect_reseed(2).await;
    assert_eq!(
        f.state.lock().await.last_proof_block_hash,
        hex_encode(&[2u8; 32])
    );
    assert!(f.wait_generation(3).await, "new root was never staged");
    f.stop().await;
}

/// The pending proof leaves the pool and no qblock follows: the round
/// resumes on the same root with a fresh generation, and a resubmission of
/// the same proof does not stop it again.
#[tokio::test]
async fn feeder_resumes_when_the_pending_proof_leaves_the_pool_without_a_qblock() {
    let mut f = Feeder::spawn_with_outbound(
        FakeChain::new(snapshot_with_head([1u8; 32]), None),
        feeder_params(2, 40),
    )
    .await;
    f.first_round_staged().await;

    f.chain.set_pending_proofs(vec![clearing_pending_proof(
        &snapshot_with_head([1u8; 32]),
        [5u8; 32],
    )]);
    f.expect_cancel(1).await;

    f.chain.set_pending_proofs(Vec::new());
    f.expect_reseed(2).await;
    assert!(
        f.wait_generation(3).await,
        "the round never resumed on the same root"
    );
    assert_eq!(
        f.state.lock().await.last_proof_block_hash,
        quip_coordinator::chain::extrinsic::hex_encode(&[1u8; 32]),
        "resume keeps the root"
    );

    let mut resubmitted = clearing_pending_proof(&snapshot_with_head([1u8; 32]), [5u8; 32]);
    resubmitted.extrinsic_hash = [9u8; 32];
    f.chain.set_pending_proofs(vec![resubmitted]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        f.quiet(),
        "a resubmitted proof must not stop the resumed round"
    );
    f.stop().await;
}

/// The proof stays pending but two blocks pass with no qblock: resume.
#[tokio::test]
async fn feeder_resumes_after_two_blocks_when_the_pending_proof_never_lands() {
    let mut f = Feeder::spawn_with_outbound(
        FakeChain::new(snapshot_with_head([1u8; 32]), None),
        feeder_params(2, 40),
    )
    .await;
    f.first_round_staged().await;

    f.chain.set_pending_proofs(vec![clearing_pending_proof(
        &snapshot_with_head([1u8; 32]),
        [5u8; 32],
    )]);
    f.expect_cancel(1).await;

    // Same root, two blocks later, proof still pending.
    f.chain.set_snapshot(Some(snapshot_at([1u8; 32], 44)));
    f.expect_reseed(2).await;
    assert!(
        f.wait_generation(3).await,
        "the round never resumed on the same root"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        f.quiet(),
        "the timed-out proof must not stop the resumed round"
    );
    f.stop().await;
}

/// A pending proof derived from another root is not this round's win.
#[tokio::test]
async fn feeder_ignores_a_pending_proof_for_another_round() {
    let mut f = Feeder::spawn_with_outbound(
        FakeChain::new(snapshot_with_head([1u8; 32]), None),
        feeder_params(2, 40),
    )
    .await;
    f.first_round_staged().await;

    f.chain.set_pending_proofs(vec![clearing_pending_proof(
        &snapshot_with_head([9u8; 32]),
        [5u8; 32],
    )]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(f.quiet(), "no Cancel for another round's proof");
    assert_eq!(f.generation().await, 1);
    assert!(f.staged().await > 0);
    f.stop().await;
}

/// The readiness walk re-reads sync state and balance on every round, not
/// only at process start. A mid-run funding failure must not exit: it holds
/// off staging the new generation and retries. Startup still exits 64; this
/// path must not.
#[tokio::test]
async fn feeder_re_checks_readiness_each_round_and_holds_off_when_underfunded() {
    let f = Feeder::spawn(
        FakeChain::new(snapshot_with_head([1u8; 32]), None),
        feeder_params(2, 40),
        vec![("cpu-0", ising_caps())],
    )
    .await;

    assert!(f.wait_generation(1).await, "first round never became ready");
    let sync_after_first = f.chain.sync_calls();
    let balance_after_first = f.chain.balance_calls();
    assert!(
        sync_after_first >= 1,
        "first round must read sync status, got {sync_after_first}"
    );
    assert!(
        balance_after_first >= 1,
        "first round must read balance, got {balance_after_first}"
    );

    f.chain.set_snapshot(Some(snapshot_with_head([2u8; 32])));
    assert!(f.wait_generation(2).await, "second round never started");
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        f.chain.sync_calls() > sync_after_first,
        "second round must re-read sync status ({} then {})",
        sync_after_first,
        f.chain.sync_calls()
    );
    assert!(
        f.chain.balance_calls() > balance_after_first,
        "second round must re-read balance ({} then {})",
        balance_after_first,
        f.chain.balance_calls()
    );

    // The next round cannot pay fees. No faucet is configured on the default
    // FundingParams, so ensure_funded fails immediately.
    f.chain.set_balance(Ok(0));
    f.chain.set_snapshot(Some(snapshot_with_head([3u8; 32])));
    tokio::time::sleep(Duration::from_millis(250)).await;
    let st = f.state.lock().await;
    assert_eq!(
        new_generation_staged(&st, 3),
        0,
        "must not stage the new generation while the account is underfunded"
    );
    assert_eq!(
        st.router.staged_len("cpu-0"),
        0,
        "prior-generation staged jobs must have been cancelled"
    );
    drop(st);
    f.stop().await;
}

/// A job already dispatched under the dead generation is dropped from
/// in-flight on reseed. A late `Result` cannot be scored or submitted, and the
/// job is not re-queued into the new round.
#[tokio::test]
async fn feeder_drops_dead_generation_inflight_and_does_not_submit_it() {
    let f = Feeder::spawn(
        FakeChain::new(snapshot_with_head([1u8; 32]), None),
        feeder_params(2, 40),
        vec![("cpu-0", ising_caps())],
    )
    .await;

    let mut job_id = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        let mut st = f.state.lock().await;
        if st.router.staged_len("cpu-0") == 0 {
            continue;
        }
        st.router.grant_credits("cpu-0", 1);
        if let Some(job) = st.router.next_job("cpu-0") {
            job_id = Some(job.job_id.clone());
            st.dispatch_inflight("cpu-0", job);
            break;
        }
    }
    let job_id = job_id.expect("never dispatched a first-round job");
    assert!(f.state.lock().await.inflight.contains_key(&job_id));

    f.chain.set_snapshot(Some(snapshot_with_head([2u8; 32])));
    let dropped = wait_until(40, 40, async || {
        !f.state.lock().await.inflight.contains_key(&job_id)
    })
    .await;
    assert!(dropped, "dead-generation in-flight job was not dropped");
    assert_eq!(
        f.chain.submitted_count(),
        0,
        "cancelled job must not be submitted"
    );
    assert!(
        f.state.lock().await.complete_inflight(&job_id).is_none(),
        "complete_inflight on a cancelled id must be a no-op"
    );
    f.stop().await;
}

// ---------------------------------------------------------------------------
// Mempool orders
// ---------------------------------------------------------------------------

/// `PoW` does not wait on solver registration, and no order is staged while
/// it keeps failing. A failure is retried once per round, not once per poll.
/// Once registered, the feeder stages an open order exactly once as a
/// generation-0 job, however many blocks it sees.
#[tokio::test]
async fn feeder_registers_as_solver_once_per_round_and_stages_each_order_once() {
    let chain = FakeChain::new(ising_snapshot(), Some(mempool_order()));
    chain.set_solver_registration_result(Err(quip_coordinator::chain::ChainError::Unavailable(
        "rpc down".into(),
    )));
    let f = Feeder::spawn(chain, feeder_params(2, 30), vec![("cpu-0", ising_caps())]).await;

    assert!(
        wait_until(100, 30, async || f.staged().await >= 2).await,
        "PoW must mine while solver registration fails"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(f.state.lock().await.mempool_orders.is_empty());
    assert_eq!(
        f.chain.solver_registration_submits(),
        1,
        "one attempt per round, not one per poll"
    );

    // The next round retries, succeeds, and reads the order map.
    f.chain.set_solver_registration_result(Ok(
        quip_coordinator::chain::RegistrationOutcome::Registered,
    ));
    f.chain.set_snapshot(Some(snapshot_at([2u8; 32], 43)));
    assert!(
        wait_until(100, 30, async || !f
            .state
            .lock()
            .await
            .mempool_orders
            .is_empty())
        .await,
        "the open order was never staged"
    );
    assert_eq!(f.chain.solver_registration_submits(), 2);

    // Later blocks re-read the order map; the order must not be staged again.
    f.chain.set_snapshot(Some(snapshot_at([2u8; 32], 44)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let jobs = f.state.lock().await.router.reclaim("cpu-0");
    let orders: Vec<_> = jobs.iter().filter(|j| j.generation == 0).collect();
    assert_eq!(orders.len(), 1, "the order must be staged exactly once");
    assert_eq!(
        orders.first().map(|j| j.job_id.clone()),
        Some(mempool_order().order_id)
    );
    f.stop().await;
}

// ---------------------------------------------------------------------------
// Participation and descriptors
// ---------------------------------------------------------------------------

/// Staging work is not participating. The declaration waits for a miner to
/// return a Result for the round: a QPU that sits a round out by withholding
/// credits (or any miner that is down) must not be recorded as a participant.
/// Two rounds on the same minted qblock send one participate call, and a new
/// qblock sends another.
#[tokio::test]
async fn feeder_declares_participation_once_per_qblock_after_a_result() {
    let chain = FakeChain::new(snapshot_with_head([1u8; 32]), None);
    chain.set_qblock_id(Some(10));
    let f = Feeder::spawn(chain, feeder_params(1, 40), vec![("cpu-0", ising_caps())]).await;

    assert!(f.wait_generation(1).await, "first round never staged");
    // Several polls with work staged and nothing returned.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        f.chain.participation_calls(),
        0,
        "staged work alone must not declare participation"
    );

    assert!(f.return_one_result(1).await, "no job to complete");
    assert!(
        f.wait_participations(1).await,
        "a Result for the round must declare participation"
    );
    assert_eq!(f.chain.take_participations(), vec![11]);

    // Same candidate qblock on the next round: no second declaration.
    f.chain.set_snapshot(Some(snapshot_with_head([2u8; 32])));
    assert!(f.return_one_result(2).await, "second round never mined");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        f.chain.take_participations().is_empty(),
        "same candidate must not be declared twice"
    );

    // A new minted qblock: declare again.
    f.chain.set_qblock_id(Some(11));
    f.chain.set_snapshot(Some(snapshot_with_head([3u8; 32])));
    assert!(f.return_one_result(3).await, "third round never mined");
    assert!(f.wait_participations(1).await, "third round not declared");
    assert_eq!(f.chain.take_participations(), vec![12]);
    f.stop().await;
}

/// A pending win must not erase participation owed for the stopped round.
#[tokio::test]
async fn feeder_declares_participation_for_a_round_stopped_by_a_pending_win() {
    let chain = FakeChain::new(snapshot_with_head([1u8; 32]), None);
    chain.set_qblock_id(Some(10));
    let mut f = Feeder::spawn_with_outbound(chain, feeder_params(2, 40)).await;
    f.first_round_staged().await;

    // No await between the returned Result and setting the proof: on this
    // current-thread runtime, the feeder's next poll sees both together.
    assert!(f.return_one_result(1).await, "no job to complete");
    f.chain.set_pending_proofs(vec![clearing_pending_proof(
        &snapshot_with_head([1u8; 32]),
        [5u8; 32],
    )]);
    f.expect_cancel(1).await;
    assert!(
        f.wait_participations(1).await,
        "a round mined before the stop must be declared"
    );
    assert_eq!(f.chain.take_participations(), vec![11]);
    f.stop().await;
}

/// Pallet participation errors must not hold off mining.
#[tokio::test]
async fn feeder_keeps_mining_when_participation_pallet_errors() {
    use quip_coordinator::chain::ParticipationOutcome;

    let chain = FakeChain::new(snapshot_with_head([1u8; 32]), None);
    chain.set_qblock_id(Some(4));
    chain.set_participation_result(Ok(ParticipationOutcome::AlreadyDeclared));
    let f = Feeder::spawn(chain, feeder_params(1, 40), vec![("cpu-0", ising_caps())]).await;

    assert!(f.return_one_result(1).await, "first round never mined");
    assert!(
        f.wait_participations(1).await,
        "DuplicateParticipation must not block mining"
    );

    f.chain.set_qblock_id(Some(5));
    f.chain
        .set_participation_result(Ok(ParticipationOutcome::StaleQBlock));
    f.chain.set_snapshot(Some(snapshot_with_head([2u8; 32])));
    assert!(f.return_one_result(2).await, "second round never mined");
    assert!(
        f.wait_participations(2).await,
        "InvalidQBlockId must not block mining"
    );

    f.chain.set_qblock_id(Some(6));
    f.chain
        .set_participation_result(Ok(ParticipationOutcome::DescriptorMissing));
    f.chain.set_snapshot(Some(snapshot_with_head([3u8; 32])));
    assert!(f.return_one_result(3).await, "third round never mined");
    assert!(
        f.wait_participations(3).await,
        "DescriptorRequired must not block mining"
    );
    assert_eq!(f.chain.participation_calls(), 3);
    f.stop().await;
}

/// Two rounds in one process file the descriptor once.
#[tokio::test]
async fn feeder_files_descriptor_once_across_two_rounds() {
    let chain = FakeChain::new(snapshot_with_head([1u8; 32]), None);
    chain.set_qblock_id(Some(10));
    let f = Feeder::spawn(
        chain,
        named_feeder_params(1, 40),
        vec![("cpu-0", ising_caps())],
    )
    .await;

    assert!(f.wait_generation(1).await, "first round never staged");
    f.chain.set_snapshot(Some(snapshot_with_head([2u8; 32])));
    assert!(f.wait_generation(2).await, "second round never staged");
    assert_eq!(f.chain.descriptor_calls(), 1);
    let filed = f.chain.take_descriptors();
    let desc = filed.first().expect("one descriptor");
    assert_eq!(desc.node_name, b"Tesla");
    assert_eq!(
        desc.miners.iter().map(|m| m.kind).collect::<Vec<_>>(),
        vec![
            quip_coordinator::chain::MinerKind::Cpu,
            quip_coordinator::chain::MinerKind::Metal
        ]
    );
    f.stop().await;
}

/// Missing `[miner].node_name` files the SS58 address and starts mining.
#[tokio::test]
async fn feeder_reaches_mining_when_node_name_is_missing() {
    let chain = FakeChain::new(snapshot_with_head([1u8; 32]), None);
    chain.set_qblock_id(Some(10));
    let mut params = named_feeder_params(1, 40);
    params.descriptor.node_name = None;
    params.miner_account = [0xab; 32];
    let f = Feeder::spawn(chain, params, vec![("cpu-0", ising_caps())]).await;

    assert!(
        f.wait_generation(1).await,
        "missing node_name must not block mining"
    );
    let descriptors = f.chain.take_descriptors();
    assert_eq!(descriptors.len(), 1);
    let descriptor = descriptors.first().expect("one descriptor");
    let name = std::str::from_utf8(&descriptor.node_name).expect("UTF-8 name");
    assert_eq!(
        AccountId32::from_ss58check(name).expect("SS58 address"),
        AccountId32::from([0xab; 32])
    );
    assert!(f.return_one_result(1).await, "first round never mined");
    assert!(
        f.wait_participations(1).await,
        "the default node name must not stop participation"
    );
    f.stop().await;
}

/// Transient descriptor or participation errors must not hold off mining.
#[tokio::test]
async fn feeder_keeps_mining_when_descriptor_or_participation_is_transient() {
    let chain = FakeChain::new(snapshot_with_head([1u8; 32]), None);
    chain.set_qblock_id(Some(10));
    chain.set_descriptor_result(Err(quip_coordinator::chain::ChainError::Unavailable(
        "rpc down".into(),
    )));
    chain.set_participation_result(Err(quip_coordinator::chain::ChainError::Unavailable(
        "rpc down".into(),
    )));
    let f = Feeder::spawn(
        chain,
        named_feeder_params(1, 40),
        vec![("cpu-0", ising_caps())],
    )
    .await;

    assert!(
        f.wait_generation(1).await,
        "transient errors must not block mining"
    );
    assert_eq!(f.chain.descriptor_calls(), 3);
    // Participation retries on later polls while the error persists, and
    // mining carries on around it.
    assert!(f.return_one_result(1).await, "first round never mined");
    assert!(
        f.wait_participations(2).await,
        "a transient participation error must be retried"
    );
    assert!(
        f.staged().await >= 1,
        "participation retries must not stop staging"
    );
    f.stop().await;
}

/// Pallet rejection of the descriptor must not hold off mining.
#[tokio::test]
async fn feeder_keeps_mining_when_descriptor_is_rejected() {
    let chain = FakeChain::new(snapshot_with_head([1u8; 32]), None);
    chain.set_qblock_id(Some(10));
    chain.set_descriptor_result(Ok(quip_coordinator::chain::DescriptorOutcome::Rejected));
    let f = Feeder::spawn(
        chain,
        named_feeder_params(1, 40),
        vec![("cpu-0", ising_caps())],
    )
    .await;

    assert!(
        f.wait_generation(1).await,
        "descriptor rejection must not block mining"
    );
    assert_eq!(f.chain.descriptor_calls(), 1);
    assert!(f.return_one_result(1).await, "first round never mined");
    assert!(
        f.wait_participations(1).await,
        "a rejected descriptor must not stop participation"
    );
    f.stop().await;
}

// ---------------------------------------------------------------------------
// The stash
// ---------------------------------------------------------------------------

/// The decay projection says a stashed candidate is due, but the chain gates it
/// would meet at the inclusion block are the ones live *now*, not the ones it
/// was stashed under. While the ceiling admits only two of its three rows the
/// chain would reject it for want of solutions, so the feeder has to hold it;
/// once the ceiling eases to admit all three, it submits.
#[tokio::test]
async fn feeder_holds_a_stashed_candidate_until_the_live_difficulty_admits_it() {
    let f = Feeder::spawn(
        FakeChain::new(gated_snapshot(-1500), None),
        feeder_params(2, 40),
        vec![],
    )
    .await;

    // Wait for the first reseed to arm the round, then plant the candidate. The
    // head never changes below, so no later reseed clears the stash.
    assert!(
        wait_until(40, 40, async || f.state.lock().await.target.is_some()).await,
        "feeder never reseeded the round"
    );
    {
        let mut st = f.state.lock().await;
        // A flat model at i64::MAX clears every energy at elapsed 0, so the
        // projection is never what withholds the candidate.
        let generation = st.generation;
        st.stash.reset(
            generation,
            Some(quip_coordinator::decay::DecayModel {
                base_max_energy_milli: i64::MAX,
                curve: None,
                epoch_length: 1,
                algorithm: quip_coordinator::decay::DecayAlgorithm::Stepwise,
            }),
            0,
        );
        assert!(
            st.stash.insert(due_candidate()),
            "candidate must be stashed"
        );
    }

    // The ceiling admits two of the three rows: below `min_solutions`.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        f.chain.submitted_count(),
        0,
        "a candidate the live difficulty rejects must not be submitted"
    );
    assert!(
        !f.state.lock().await.stash.is_empty(),
        "the withheld candidate must stay stashed for a later window"
    );

    // Ease the ceiling past every row. Same head, so this is a difficulty
    // refresh rather than a new round.
    f.chain.set_snapshot(Some(gated_snapshot(0)));
    assert!(
        wait_until(40, 40, async || f.chain.submitted_count() > 0).await,
        "the eased ceiling must let the candidate through"
    );
    let sent = f.chain.take_submitted();
    assert_eq!(sent.len(), 1);
    let proof = sent.first().expect("one recorded proof");
    assert_eq!(proof.job_id, vec![0xaa]);
    assert_eq!(proof.solutions.len(), 3);
    f.stop().await;
}

/// A runtime upgrade takes effect mid-round. The feeder switches the stash
/// projection on the poll that reports the new spec version (continuous at
/// 118, gated at 119), keeps the stashed candidate, and reprojects it.
#[tokio::test]
async fn feeder_switches_the_decay_projection_when_the_runtime_upgrades() {
    use quip_coordinator::decay::{DecayAlgorithm, DecayModel, EnergyCurve};

    let f = Feeder::spawn(
        FakeChain::new(gated_snapshot(-1500), None),
        feeder_params(2, 40),
        vec![],
    )
    .await;
    assert!(
        wait_until(40, 40, async || f.state.lock().await.target.is_some()).await,
        "feeder never reseeded the round"
    );

    // Plant a stepwise model far in the future so the candidate is never due
    // during the test: last proof at 1,000,000, epoch 10, thresholds -50000,
    // -48775, ... so -49500 clears at block 1,000,010 under stepwise.
    let stepwise = DecayModel {
        base_max_energy_milli: -50_000,
        curve: Some(EnergyCurve {
            min_milli: -100_000,
            knee_milli: -50_000,
            max_milli: -1_000,
        }),
        epoch_length: 10,
        algorithm: DecayAlgorithm::Stepwise,
    };
    {
        let mut st = f.state.lock().await;
        let generation = st.generation;
        st.stash.reset(generation, Some(stepwise), 1_000_000);
        let mut cand = due_candidate();
        cand.best_energy_milli = -49_500;
        assert!(st.stash.insert(cand), "candidate must be stashed");
        let summary = st.stash.summary();
        assert_eq!(summary.decay_algorithm, "stepwise");
        assert_eq!(
            summary.candidates.first().and_then(|c| c.viability_block),
            Some(1_000_010)
        );
    }

    // The upgrade takes effect: same head, spec 118.
    let block = viability_after(&f, 118, DecayAlgorithm::Continuous, "continuous").await;
    assert!(
        1_000_000 < block && block < 1_000_010,
        "continuous projection must land inside the first epoch, got {block}"
    );

    // The next upgrade, spec 119, gates easing behind the 100-block target.
    let block = viability_after(&f, 119, DecayAlgorithm::Gated, "gated").await;
    assert!(
        1_000_100 < block && block < 1_000_110,
        "gated projection must land in the first overdue epoch, got {block}"
    );
    f.stop().await;
}

/// Report `spec_version` on the same head, wait for the stash to switch to
/// `algorithm`, and return the surviving candidate's viability block.
async fn viability_after(
    f: &Feeder,
    spec_version: u32,
    algorithm: quip_coordinator::decay::DecayAlgorithm,
    name: &str,
) -> u64 {
    let mut upgraded = gated_snapshot(-1500);
    upgraded.spec_version = spec_version;
    f.chain.set_snapshot(Some(upgraded));
    let switched = wait_until(40, 40, async || {
        f.state.lock().await.stash.algorithm() == Some(algorithm)
    })
    .await;
    assert!(switched, "feeder never switched the projection to {name}");
    let summary = f.state.lock().await.stash.summary();
    assert_eq!(summary.decay_algorithm, name);
    assert_eq!(
        summary.candidates.len(),
        1,
        "the candidate survives the switch"
    );
    summary
        .candidates
        .first()
        .and_then(|c| c.viability_block)
        .unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// The runtime with a real mock miner
// ---------------------------------------------------------------------------

/// The supervisor spawns the lease-capable mock miner, the session hands it
/// leases and the open order, and the coordinator verifies a winning salt and
/// submits it with the salt from inside the lease. The order is answered with
/// `submit_solution`, never `submit_proof`. On signal, `run_runtime` fans out
/// Shutdown, drains the supervisors, and returns Ok within the grace margin.
#[tokio::test]
async fn runtime_submits_a_lease_winner_answers_an_order_and_shuts_down_clean() {
    let chain = Arc::new(FakeChain::new(ising_snapshot(), Some(mempool_order())));
    let params = RuntimeParams {
        max_submit_attempts: 5,
        sock_path: format!("/tmp/quip-rt-{}.sock", std::process::id()),
        grace_ms: 500,
        backoff: BackoffPolicy::default(),
        miner_identity: [0u8; 32],
        miner_account: [0u8; 32],
        buffer_depth: 1,
        poll_interval_ms: 50,
        dashboard: None,
        log_level: quip_coordinator::logging::LogLevel::Info,
        funding: quip_coordinator::funding::FundingParams::default(),
        descriptor: quip_coordinator::config::DescriptorParams::default(),
        descriptor_filed: Arc::new(AtomicBool::new(false)),
        miner_registered: Arc::new(AtomicBool::new(false)),
        solver_registered: Arc::new(AtomicBool::new(false)),
        identity: quip_coordinator::metrics::Identity::default(),
    };
    let entry = LaunchEntry {
        miner_id: "cpu-0".into(),
        binary: common::mock_miner(),
        backend: "cpu".into(),
        device: None,
        configure: Configure {
            // Long idle so the miner stays connected until we shut it down.
            queue_depth: 3,
            idle_timeout_s: 60,
            heartbeat_s: 15,
            reconnect_window_s: 60,
            backend_toml: String::new(),
        },
    };
    let (trigger_tx, trigger_rx) = oneshot::channel::<()>();
    let run = tokio::spawn(run_runtime(
        vec![entry],
        Arc::clone(&chain),
        Arc::new(Mutex::new(CoordinatorState::new())),
        params,
        async move {
            let _ = trigger_rx.await;
        },
    ));

    let done = wait_until(100, 30, async || {
        chain.submitted_count() > 0
            && chain.participation_calls() > 0
            && !chain.solutions.lock().expect("solutions lock").is_empty()
    })
    .await;

    trigger_tx.send(()).expect("send shutdown trigger");
    tokio::time::timeout(Duration::from_secs(6), run)
        .await
        .expect("run_runtime did not return after shutdown")
        .expect("run_runtime task panicked")
        .expect("run_runtime returned an error");

    assert!(
        done,
        "expected a lease win, a participation, and an answered order; got {} proofs, {} participations, {} solutions",
        chain.submitted_count(),
        chain.participation_calls(),
        chain.solutions.lock().expect("solutions lock").len()
    );
    let order_id = mempool_order().order_id;
    let proofs = chain.take_submitted();
    assert!(
        proofs.iter().all(|p| p.order_id != order_id),
        "a mempool result must never reach submit_proof"
    );
    let proof = proofs.first().expect("one proof");
    assert_eq!(proof.salt.len(), 32);
    let counter_bytes: [u8; 8] = proof
        .salt
        .get(..8)
        .expect("8 salt counter bytes")
        .try_into()
        .expect("8 bytes");
    assert!(
        u64::from_le_bytes(counter_bytes) >= 1,
        "lease counters start after the feeder's first increment"
    );
    assert!(
        proof
            .salt
            .get(8..)
            .expect("base salt bytes")
            .iter()
            .all(|&b| b == 0),
        "zero base salt"
    );

    let solutions = chain.take_solutions();
    let first = solutions.first().expect("one captured solution");
    assert_eq!(first.order_id, order_id);
    assert!(!first.is_pow);
    assert!(!first.solutions.is_empty());
}
