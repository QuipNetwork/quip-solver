//! The coordinator's shared state: routing, in-flight work, the round, and
//! the session registry, under one lock.

use crate::coordinator::router::{MinerCaps, Router};
use crate::coordinator::scoring::validate::ResolvedTopo;
use crate::miner::session::service::gen_session_token;
use crate::miner::session::state::{SessionHost, SessionState};
use crate::miner::session::stream::shutdown_msg;
use crate::types::topology::Topology;
use quip_proto::v1::{coord_msg, Capabilities};
use quip_protocol::session::PROTOCOL_VERSION;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

impl SessionHost for CoordinatorState {
    fn sessions(&self) -> &SessionState {
        &self.sessions
    }

    fn sessions_mut(&mut self) -> &mut SessionState {
        &mut self.sessions
    }

    fn requirements(&self) -> Vec<coord_msg::Msg> {
        let mut msgs = Vec::new();
        if let Some(topo) = self.topology.as_ref() {
            msgs.push(coord_msg::Msg::Topology(topo.to_proto()));
        }
        // The current difficulty, so a miner joining mid-generation tracks it
        // immediately, rather than waiting for the next reseed broadcast.
        if let Some(target) = self.target {
            msgs.push(coord_msg::Msg::SetTarget(target));
        }
        msgs
    }
}

/// How many top candidates the win-time stash retains per generation.
const WIN_STASH_K: usize = 8;

/// Shared coordinator runtime state for one or more miner sessions.
pub struct CoordinatorState {
    /// Who may connect and how to reach each live session.
    pub sessions: SessionState,
    /// Active graph topology advertised to miners, if any.
    pub topology: Option<Topology>,
    /// Dense-position view of `topology` that lease verification draws on.
    /// Built once in [`CoordinatorState::set_topology`]. `None` without a
    /// topology or when the topology cannot map its edges.
    pub lease_topology: Option<Arc<crate::coordinator::scoring::lease::LeaseTopology>>,
    /// Position-indexed scoring form of `topology`, resolved once via
    /// [`CoordinatorState::set_topology`]. A run constant every result borrows
    /// by `Arc`, so the graph is never rebuilt per result.
    pub resolved_topo: Arc<ResolvedTopo>,
    /// Difficulty target advertised to miners via `SetTarget`.
    pub target: Option<quip_proto::v1::SetTarget>,
    /// Work router: credits, staging queues, and miner caps.
    pub router: Router,
    /// `job_id` → `Job`, for validation context.
    pub inflight: HashMap<Vec<u8>, quip_proto::v1::Job>,
    /// `job_id` → owning `miner_id`, so a crashed miner's in-flight jobs can be
    /// isolated and re-routed. Maintained on the production path via
    /// [`CoordinatorState::dispatch_inflight`]/[`CoordinatorState::complete_inflight`];
    /// the drive harness leaves it empty (it never crash-requeues).
    pub inflight_owner: HashMap<Vec<u8>, String>,
    /// `job_id` → the instant a lease job was dispatched, so
    /// [`handle_lease_done`] can report how long the lease was outstanding.
    /// Populated in [`CoordinatorState::dispatch_inflight`] for lease jobs
    /// only; cleared in [`CoordinatorState::complete_inflight`].
    pub lease_issued_at: HashMap<Vec<u8>, std::time::Instant>,
    /// `job_id` (nonce) → the 32-byte salt it was derived from, so the winning
    /// `Proof` carries the salt live submit requires. Recorded when the feeder
    /// stages a job, consumed on its `Result`, cleared on reseed.
    pub salts: HashMap<Vec<u8>, [u8; 32]>,
    /// Best accepted energy so far this generation, if any.
    pub current_best_milli: Option<i64>,
    /// Count of results that passed through validation.
    pub results_validated: u64,
    /// Highest abandoned generation reported by any miner via `Status`.
    pub last_abandoned_generation: u64,
    /// Sink for mining-attempt records + summaries (dashboard). `None` disables
    /// recording.
    pub attempt_tx: Option<std::sync::mpsc::Sender<crate::observe::attempt::WriterMsg>>,
    /// Current chain quantum-block id, refreshed by the feeder on reseed; keys
    /// the per-qblock attempt logs.
    pub qblock_id: Option<u64>,
    /// Logging context retained across pauses and outstanding submit receipts.
    pub(crate) round_log: Option<Arc<Mutex<crate::coordinator::round::log::RoundLog>>>,
    /// Win-time stash of the most-viable sub-threshold candidates for the
    /// current generation, armed with the decay projection on reseed.
    pub stash: crate::coordinator::round::stash::WinStash,
    /// Block-interval/lag tracker for current-block estimation.
    pub timing: crate::coordinator::round::timing::TimingTracker,
    /// Feeder's current round; set on reseed. Compared against a miner's
    /// self-reported generation to detect stale-round mining.
    pub generation: u64,
    /// Generation of the newest `Result` a miner returned for a live job.
    /// Equal to `generation` once some miner has mined this round; the
    /// feeder declares participation on that evidence and nothing weaker.
    mined_generation: Option<u64>,
    /// Per-miner last-known self-reported liveness (mining/paused + round),
    /// updated from ping-reply `Status` messages.
    pub miner_liveness: crate::miner::liveness::LivenessTable,
    /// Live controller counters. Shared with the dashboard router, so the HTTP
    /// handlers never take this mutex.
    pub metrics: Arc<crate::observe::metrics::CoordinatorMetrics>,
    /// `LastProofBlockHash` the current round's jobs were built on, as
    /// `0x`-prefixed hex. Set by the feeder on reseed.
    pub last_proof_block_hash: String,
    /// `miner_id` to its backend label (`CPU`, `GPU-CUDA`). Seeded from the
    /// launch plan so the session can stamp every attempt.
    pub miner_types: HashMap<String, String>,
    /// Mempool order id to the gates its result must clear. The feeder stages
    /// an order only while its id is absent, so each order is answered once;
    /// a transient submit failure removes the id to stage it again.
    pub mempool_orders: HashMap<Vec<u8>, crate::coordinator::scoring::validate::QualityGates>,
    /// The calibration miners that list the `calibrate` feature receive before
    /// their first job. Set once at startup, before any session starts.
    /// `None` turns calibration off.
    pub calibration: Option<crate::coordinator::calibration::CalibrationPlan>,
}

impl CoordinatorState {
    /// Empty coordinator state with default stash size and timing tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: SessionState::default(),
            topology: None,
            lease_topology: None,
            resolved_topo: Arc::new(ResolvedTopo::default()),
            target: None,
            router: Router::new(),
            inflight: HashMap::new(),
            inflight_owner: HashMap::new(),
            lease_issued_at: HashMap::new(),
            salts: HashMap::new(),
            current_best_milli: None,
            mined_generation: None,
            results_validated: 0,
            last_abandoned_generation: 0,
            attempt_tx: None,
            qblock_id: None,
            round_log: None,
            stash: crate::coordinator::round::stash::WinStash::new(WIN_STASH_K),
            timing: crate::coordinator::round::timing::TimingTracker::with_defaults(),
            generation: 0,
            miner_liveness: crate::miner::liveness::LivenessTable::default(),

            metrics: Arc::new(crate::observe::metrics::CoordinatorMetrics::new(&[])),
            last_proof_block_hash: String::new(),
            miner_types: HashMap::new(),
            mempool_orders: HashMap::new(),
            calibration: None,
        }
    }

    /// Set the session topology and resolve its position-indexed scoring form
    /// once. Both drive and production go through here so `validate_result`
    /// never rebuilds the graph per result.
    pub fn set_topology(&mut self, topology: Option<Topology>) {
        self.router.set_topology(topology.as_ref());
        self.lease_topology = topology.as_ref().and_then(|t| {
            crate::coordinator::scoring::lease::LeaseTopology::from_topology(
                t,
                crate::types::family::FAMILY_CONTRACTS,
            )
            .map_err(|e| tracing::error!(error = %e, "topology cannot back lease verification"))
            .ok()
            .map(Arc::new)
        });
        self.resolved_topo = Arc::new(
            topology
                .as_ref()
                .map(|t| ResolvedTopo::new(&t.nodes, &t.edge_pairs()))
                .unwrap_or_default(),
        );
        self.topology = topology;
    }

    /// Remember the salt a staged job was derived from (feeder path).
    pub fn record_salt(&mut self, job_id: &[u8], salt: [u8; 32]) {
        let _ = self.salts.insert(job_id.to_vec(), salt);
    }

    /// Consume the salt for a completed job, for its Proof.
    pub fn take_salt(&mut self, job_id: &[u8]) -> Option<[u8; 32]> {
        self.salts.remove(job_id)
    }

    /// Drop all remembered salts on reseed — the prior generation is cancelled.
    pub fn clear_salts(&mut self) {
        self.salts.clear();
    }

    /// Drop in-flight `PoW` jobs whose generation is at most `max_generation`.
    /// Mempool jobs (`generation == 0`) stay. Returns how many jobs were dropped.
    ///
    /// A late `Result` for a dropped id then sees `complete_inflight` return
    /// `None`, so the coordinator neither scores nor submits it.
    pub fn cancel_inflight(&mut self, max_generation: u64) -> usize {
        let ids: Vec<Vec<u8>> = self
            .inflight
            .iter()
            .filter(|(_, job)| job.generation != 0 && job.generation <= max_generation)
            .map(|(id, _)| id.clone())
            .collect();
        let n = ids.len();
        for id in ids {
            let _ = self.complete_inflight(&id);
        }
        n
    }

    /// Record a dispatched job as in-flight, attributing it to `miner_id`.
    /// A lease job also starts its issue-instant clock, read back in
    /// [`handle_lease_done`].
    pub fn dispatch_inflight(&mut self, miner_id: &str, job: quip_proto::v1::Job) {
        let _ = self
            .inflight_owner
            .insert(job.job_id.clone(), miner_id.to_string());
        if crate::coordinator::scoring::lease::is_lease(&job) {
            let _ = self
                .lease_issued_at
                .insert(job.job_id.clone(), std::time::Instant::now());
        }
        let _ = self.inflight.insert(job.job_id.clone(), job);
    }

    /// Clear an in-flight job on a terminal event (Result/Reject/LeaseDone);
    /// returns the job for validation context. Also drops any lease
    /// issue-instant, so a cancelled or reclaimed lease never leaks an entry.
    pub fn complete_inflight(&mut self, job_id: &[u8]) -> Option<quip_proto::v1::Job> {
        let _ = self.inflight_owner.remove(job_id);
        let _ = self.lease_issued_at.remove(job_id);
        self.inflight.remove(job_id)
    }

    /// A miner returned a `Result` for a live job of `job_generation`.
    ///
    /// Only a Result for the current round counts: a late Result from a
    /// cancelled generation is not this round's work. A `Reject` never
    /// reaches here; a QPU that sits a round out answers every dispatch with
    /// one, and that is exactly what must not read as participation.
    pub fn note_result(&mut self, job_generation: u64) {
        if job_generation == self.generation {
            self.mined_generation = Some(job_generation);
        }
    }

    /// Whether some miner has returned a Result for the current round.
    #[must_use]
    pub fn round_mined(&self) -> bool {
        self.mined_generation == Some(self.generation)
    }

    /// Reclaim every job a miner owned — its in-flight jobs plus its staged
    /// queue — so they can be re-routed after a crash. Also drops its outbound
    /// channel. The miner stays registered in the router.
    pub fn reclaim_miner(&mut self, miner_id: &str) -> Vec<quip_proto::v1::Job> {
        let mut jobs = self.drain_inflight(miner_id);
        jobs.extend(self.router.reclaim(miner_id));
        jobs
    }

    /// Like [`Self::reclaim_miner`], but for a miner the supervisor gave up
    /// on: the router forgets it too, so no job is staged onto a dead queue.
    pub fn retire_miner(&mut self, miner_id: &str) -> Vec<quip_proto::v1::Job> {
        let mut jobs = self.drain_inflight(miner_id);
        jobs.extend(self.router.remove_miner(miner_id));
        jobs
    }

    /// Take the in-flight jobs `miner_id` owns and drop its session channels.
    fn drain_inflight(&mut self, miner_id: &str) -> Vec<quip_proto::v1::Job> {
        let owned: Vec<Vec<u8>> = self
            .inflight_owner
            .iter()
            .filter(|(_, m)| m.as_str() == miner_id)
            .map(|(id, _)| id.clone())
            .collect();
        let mut jobs = Vec::with_capacity(owned.len());
        for id in owned {
            let _ = self.inflight_owner.remove(&id);
            let _ = self.lease_issued_at.remove(&id);
            if let Some(job) = self.inflight.remove(&id) {
                jobs.push(job);
            }
        }
        let _ = self.sessions.outbound.remove(miner_id);
        let _ = self.sessions.cancel_timings.remove(miner_id);
        jobs
    }

    /// Apply a compatible snapshot and wake dispatchers without granting credits.
    pub(crate) fn apply_capabilities(&mut self, miner_id: &str, caps: &Capabilities) -> bool {
        if caps.protocol_version != PROTOCOL_VERSION {
            tracing::warn!(
                miner = %miner_id,
                offered_protocol = caps.protocol_version,
                "ignoring capability snapshot with incompatible protocol"
            );
            return false;
        }
        if !self
            .router
            .update_capabilities(miner_id, MinerCaps::from_capabilities(caps))
        {
            return false;
        }
        tracing::debug!(
            miner = %miner_id,
            max_nodes = caps.max_nodes,
            max_edges = caps.max_edges,
            max_degree = ?caps.max_degree,
            stream_width = caps.stream_width,
            "miner capability snapshot applied"
        );
        for id in self.router.miner_ids() {
            self.sessions.wake_dispatcher(&id);
        }
        true
    }
}

impl Default for CoordinatorState {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl crate::miner::SessionGate for Arc<Mutex<CoordinatorState>> {
    async fn expect(&self, miner_id: &str) -> String {
        let token = gen_session_token();
        let _ = self
            .lock()
            .await
            .sessions
            .expected_tokens
            .insert(miner_id.to_owned(), token.clone());
        token
    }

    async fn request_shutdown(&self, miner_id: &str, grace_ms: u32) {
        let tx = self.lock().await.sessions.outbound.get(miner_id).cloned();

        if let Some(tx) = tx {
            let _ = tx.send(Ok(shutdown_msg(grace_ms))).await;
        }
    }

    async fn reclaim(&self, miner_id: &str) {
        let mut st = self.lock().await;
        let jobs = st.reclaim_miner(miner_id);
        for job in jobs {
            let _ = st.router.route(job);
        }
    }
    async fn retire(&self, miner_id: &str) {
        let mut st = self.lock().await;
        let jobs = st.retire_miner(miner_id);
        for job in jobs {
            let job_id = job.job_id.clone();
            if st.router.route(job).is_some() {
                continue;
            }
            if st.mempool_orders.remove(&job_id).is_some() {
                tracing::warn!(
                    miner = %miner_id,
                    job = %crate::types::hex::hex_encode(&job_id),
                    "no miner can take a reclaimed mempool order; releasing it for restaging"
                );
            } else {
                tracing::debug!(
                    miner = %miner_id,
                    job = %crate::types::hex::hex_encode(&job_id),
                    "no miner can take a reclaimed job; parked as unroutable"
                );
            }
        }
    }
}
