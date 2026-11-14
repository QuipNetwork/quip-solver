//! Production runtime: bind the miner session server, spawn + supervise every
//! configured miner, and shut down cleanly.
//!
//! This module owns process wiring and the graceful-shutdown fan-out. The live
//! job-production loop (chain-head following + continuous feeder + decay-ratchet
//! stash) is layered on top by the live-mining epic; a coordinator built from
//! this alone serves sessions and supervises miners but stages no work yet.

use crate::chain::sync::{wait_until_synced, SyncOutcome, SyncSource};
use crate::chain::{ChainClient, JobOrder, MiningSnapshot};
use crate::config::{DescriptorParams, LaunchEntry};
use crate::decay::{DecayAlgorithm, DecayModel, EnergyCurve};
use crate::funding::{ensure_funded, BalanceSource, Faucet};
use crate::logging::LogLevel;
use crate::pool_watch::PoolObservation;
use crate::producer::{derive_pow_job, job_order_to_job};
use crate::readiness::{
    build_faucet, declare_round_participation, file_round_descriptor, register_round_miner,
    ReadinessError,
};
use crate::round::{RoundEvent, RoundState};
use crate::session::{coord, CoordinatorService, CoordinatorState};
use crate::supervisor::{supervise_miner, BackoffPolicy};
use crate::topology::Topology;
use crate::validate::QualityGates;
use quip_proto::v1::miner_service_server::MinerServiceServer;
use quip_proto::v1::{coord_msg, SetTarget};
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::sync::{watch, Mutex};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

/// Runtime knobs shared by every supervised miner.
pub struct RuntimeParams {
    /// Unix-domain socket the server binds and miners connect to.
    pub sock_path: String,
    /// Consecutive failed submissions of one proof, inside one quantum block,
    /// before the coordinator stops retrying it. From `[miner]` config.
    pub max_submit_attempts: u32,
    /// Grace period (ms) between an in-band `Shutdown` and a hard kill.
    pub grace_ms: u32,
    /// Restart backoff + failure budget applied to each miner.
    pub backoff: BackoffPolicy,
    /// `PoW` nonce input: `blake2_256(SCALE(account))`, matching the pallet's
    /// `account_to_bytes`. Not an address — see [`Self::miner_account`].
    pub miner_identity: [u8; 32],
    /// The signing `AccountId32`. Pays fees, holds the balance the funding path
    /// tops up, and keys every per-signer chain map.
    pub miner_account: [u8; 32],
    /// Floor for the adaptive staging window: minimum staged jobs per miner
    /// (0 disables feeding). The window grows above this with each miner's
    /// observed drain rate; there is no ceiling. See [`feeder_loop`].
    pub buffer_depth: usize,
    /// How often the feeder polls the chain head and tops up buffers.
    pub poll_interval_ms: u64,
    /// Optional attempt-dashboard: `(listen_addr, data_dir)`. `None` disables
    /// recording + the REST endpoint.
    pub dashboard: Option<(String, std::path::PathBuf)>,
    /// Log level forwarded to each miner child as `--log-level`. Children inherit
    /// coordinator stdio, so their own subscriber needs this flag to match
    /// coordinator verbosity.
    pub log_level: LogLevel,
    /// Account-funding knobs reused on every round, not only at process start.
    pub funding: crate::funding::FundingParams,
    /// Node identity and miner list for `set_descriptor`.
    pub descriptor: DescriptorParams,
    /// Set after the first `DescriptorFiled` walk in this process.
    pub descriptor_filed: Arc<AtomicBool>,
    /// Set once `QuantumPow.Miners` is known to hold the signing account.
    pub miner_registered: Arc<AtomicBool>,
    /// Set once `QuantumComputeMempool.Solvers` holds the signing account with
    /// the configured type. Mempool orders are staged only after this.
    pub solver_registered: Arc<AtomicBool>,
    /// Chain identity advertised on `/api/v1/status`. Empty when the process
    /// holds no usable signer key.
    pub identity: crate::metrics::Identity,
}

/// Inputs to the feeder loop.
pub struct FeederParams {
    /// `PoW` nonce input: `blake2_256(SCALE(account))`. Not an address.
    pub miner_identity: [u8; 32],
    /// The signing `AccountId32`. Pays fees and keys the chain maps.
    pub miner_account: [u8; 32],
    /// Floor for the adaptive staging window: the minimum staged jobs kept per
    /// miner (0 disables feeding). The window grows above this from the miner's
    /// observed drain rate — see [`feeder_loop`] — with no ceiling.
    pub buffer_depth: usize,
    /// How often the feeder polls the chain head and tops up buffers.
    pub poll_interval: Duration,
    /// Account-funding knobs reused on every round, not only at process start.
    pub funding: crate::funding::FundingParams,
    /// Node identity and miner list for `set_descriptor`.
    pub descriptor: DescriptorParams,
    /// Set after the first `DescriptorFiled` walk in this process.
    pub descriptor_filed: Arc<AtomicBool>,
    /// Set once `QuantumPow.Miners` is known to hold the signing account.
    pub miner_registered: Arc<AtomicBool>,
    /// Set once `QuantumComputeMempool.Solvers` holds the signing account with
    /// the configured type. Mempool orders are staged only after this.
    pub solver_registered: Arc<AtomicBool>,
    /// Consecutive failed submissions of one proof, inside one quantum block,
    /// before the coordinator stops retrying it.
    pub max_submit_attempts: u32,
    /// Live controller counters, for `heads_observed`.
    pub metrics: Arc<crate::metrics::CoordinatorMetrics>,
}

/// Poll interval while the miners are stopped for a pending win. The block
/// that includes the proof lands within one block time, so a tight poll wins
/// most of the configured interval back. Never slower than `poll_interval`.
const AWAIT_QBLOCK_POLL: Duration = Duration::from_millis(250);

/// EMA smoothing for the per-miner consumption signal. Lower reacts slower but
/// steadier; 0.3 favors stability over reactivity for a ~1s poll, so a single
/// slow or fast poll doesn't swing the window.
const CONSUMPTION_EMA_ALPHA: f64 = 0.3;

/// Staging headroom over smoothed consumption: keep ~2 poll-intervals of drain
/// staged so a fast miner never idles waiting for the next top-up.
const WINDOW_HEADROOM: f64 = 2.0;

/// Adaptive staging depth for one miner from its smoothed drain rate. The
/// `buffer_depth` floor keeps a small reserve for idle/slow miners. `ema`
/// tracks dispatches per poll (`Router::take_consumed`): a job leaves the
/// staged queue at dispatch, so that is the drain signal the feeder sizes
/// against. The feeder additionally clamps the depth to [`stage_ceiling`] so a
/// fast miner on a large topology cannot stage an unbounded-memory reserve.
fn adaptive_depth(ema: f64, floor: usize) -> usize {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "EMA*headroom is non-negative and small for operational drain rates"
    )]
    {
        floor.max((ema * WINDOW_HEADROOM).ceil() as usize)
    }
}

/// Per-miner budget on staged-job memory. The adaptive window sizes to drain
/// rate with no ceiling of its own, so a fast miner on a large topology could
/// stage an unbounded reserve — each staged `PoW` job holds a full materialized
/// Ising model (`h`/`j`), ~`4·(nodes+edges)` bytes. This bounds the peak
/// staging memory per miner regardless of topology size or drain rate.
const MAX_STAGE_BYTES_PER_MINER: usize = 64 * 1024 * 1024; // 64 MiB

/// Periodic liveness ping interval for [`crate::liveness::liveness_loop`].
const LIVENESS_PING_INTERVAL: Duration = Duration::from_secs(15);

/// How often the feeder emits its steady-state throughput line. The loop polls
/// once per `poll_interval` (1s in production), so narrating every poll at
/// `info` would bury everything else; the per-poll detail sits at `debug`.
const FEEDER_HEARTBEAT: Duration = Duration::from_mins(1);

/// Logging context shared by the feeder and session submissions for one root.
/// A receipt can arrive after the feeder has already opened the next round.
#[derive(Debug)]
pub(crate) struct RoundLog {
    qblock_id: Option<u64>,
    our_account: [u8; 32],
    started: std::time::Instant,
    our_best_milli: Option<i64>,
    pending: Option<(crate::pool_watch::Clearing, i64)>,
    submissions: usize,
    closed: bool,
    reported: bool,
}

impl RoundLog {
    pub(crate) fn new(latest_qblock_id: Option<u64>, our_account: [u8; 32]) -> Self {
        Self {
            // Like participation, outcomes name the candidate, not the latest
            // completed qblock used as the attempt-log key in CoordinatorState.
            qblock_id: latest_qblock_id.map(|id| id.saturating_add(1)),
            our_account,
            started: std::time::Instant::now(),
            our_best_milli: None,
            pending: None,
            submissions: 0,
            closed: false,
            reported: false,
        }
    }

    pub(crate) fn observe_pending(
        &mut self,
        proof: crate::pool_watch::Clearing,
        target_milli: i64,
    ) {
        self.pending = Some((proof, target_milli));
    }

    pub(crate) fn log_pause(
        &self,
        generation: u64,
        block: u64,
        cancelled_jobs: usize,
        miners_told: usize,
    ) {
        let Some((proof, target_milli)) = self.pending else {
            return;
        };
        tracing::info!(
            generation,
            qblock_id = %crate::logging::display_option(self.qblock_id),
            block,
            proof_owner = %(if proof.account == self.our_account { "ours" } else { "another_miner" }),
            signer = %crate::chain::extrinsic::hex_encode(&proof.account),
            proof_energy = crate::logging::energy_units(proof.best_energy_milli),
            target_energy = crate::logging::energy_units(target_milli),
            our_best_energy_this_round = %crate::logging::display_energy(self.our_best_milli),
            cancelled_jobs,
            miners_told,
            "pausing coordinator: a pending proof clears this round; awaiting the qblock"
        );
    }

    fn resume(&mut self) {
        self.pending = None;
    }

    fn close(&mut self) {
        self.closed = true;
        self.report_other_win();
    }

    /// Fold one validated miner result into our best for the round, so the
    /// pause, heartbeat and loss lines report what we found even when we
    /// never submitted. `i64::MAX` marks a result with no rows.
    pub(crate) fn observe_result(&mut self, energy_milli: i64) {
        if energy_milli == i64::MAX {
            return;
        }
        self.our_best_milli = Some(
            self.our_best_milli
                .map_or(energy_milli, |best| best.min(energy_milli)),
        );
    }

    pub(crate) fn submission_started(&mut self) {
        self.submissions += 1;
    }

    pub(crate) fn submission_finished(
        &mut self,
        receipt: Option<&crate::chain::SubmitReceipt>,
        proof_energy_milli: i64,
        target_milli: i64,
    ) {
        self.submissions = self.submissions.saturating_sub(1);
        if let Some(receipt) = receipt.filter(|r| r.action == crate::chain::SubmitAction::Success) {
            // A late receipt can write current_best_milli after a reseed. Only
            // its captured round owns that energy, so never import shared bests.
            self.our_best_milli = Some(
                self.our_best_milli
                    .map_or(proof_energy_milli, |old| old.min(proof_energy_milli)),
            );
            if !self.reported {
                self.reported = true;
                tracing::info!(
                    qblock_id = %crate::logging::display_option(self.qblock_id),
                    inclusion_block = %crate::logging::display_option(receipt.block_number),
                    proof_energy = crate::logging::energy_units(proof_energy_milli),
                    target_energy = crate::logging::energy_units(target_milli),
                    round_elapsed_s = self.started.elapsed().as_secs(),
                    "qblock won by us"
                );
            }
        }
        self.report_other_win();
    }

    fn report_other_win(&mut self) {
        if !self.closed || self.reported || self.submissions != 0 {
            return;
        }
        let Some((proof, target_milli)) = self.pending else {
            return;
        };
        if proof.account == self.our_account {
            return;
        }
        // Pool proofs can vanish without inclusion, so wait for a new root.
        // The chain seam exposes no foreign winning receipt: attribute this
        // closure to the last observed clearing proof and state that source.
        // Outstanding own receipts settle first, preventing two outcomes.
        self.reported = true;
        tracing::info!(
            qblock_id = %crate::logging::display_option(self.qblock_id),
            winner = %crate::chain::extrinsic::hex_encode(&proof.account),
            proof_energy = crate::logging::energy_units(proof.best_energy_milli),
            target_energy = crate::logging::energy_units(target_milli),
            our_best_energy_this_round = %crate::logging::display_energy(self.our_best_milli),
            winner_source = %"pending_pool",
            "qblock won by another miner"
        );
    }

    pub(crate) fn log_heartbeat(
        &self,
        block: u64,
        generation: u64,
        results_validated: u64,
        paused: bool,
    ) {
        tracing::info!(
            block,
            generation,
            qblock_id = %crate::logging::display_option(self.qblock_id),
            results_validated,
            our_best_energy_this_round = %crate::logging::display_energy(self.our_best_milli),
            round_state = %(if paused { "paused_awaiting_qblock" } else { "mining" }),
            "coordinator alive"
        );
    }
}

/// Outcome of one snapshot poll. The feeder logs *transitions* between these
/// at `info`/`warn` rather than one line per poll, so a coordinator that cannot
/// mine says so once and loudly instead of either spamming or staying silent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FeedState {
    /// The chain returned a usable mining snapshot.
    Ready,
    /// The chain answered, but has no mining snapshot: no default topology is
    /// registered, or none is mineable yet. Nothing can be staged.
    Empty,
    /// The snapshot fetch itself failed (RPC down, runtime API absent, decode
    /// mismatch). Nothing can be staged.
    Failed,
}

/// Estimated materialized-model bytes for one staged job on this topology: one
/// i32 field per node (`h`) and one per edge (`j`). The 32-byte `topology_hash`
/// and `job_id` are negligible next to these. Floored at 1 to avoid a zero
/// divisor on an empty topology.
fn est_job_bytes(num_nodes: usize, num_edges: usize) -> usize {
    4usize
        .saturating_mul(num_nodes.saturating_add(num_edges))
        .max(1)
}

/// Memory-aware ceiling on the staging window: the most jobs that fit the
/// per-miner byte budget, but never below `floor` — a miner must not idle for
/// lack of a staged job even on a topology where the floor alone exceeds the
/// budget.
fn stage_ceiling(num_nodes: usize, num_edges: usize, floor: usize) -> usize {
    floor.max(MAX_STAGE_BYTES_PER_MINER / est_job_bytes(num_nodes, num_edges))
}

/// Difficulty gates advertised to miners, from the snapshot.
fn target_from_snapshot(snap: &MiningSnapshot) -> SetTarget {
    SetTarget {
        max_proof_solutions: crate::validate::MAX_PROOF_SOLUTIONS_WIRE,
        max_energy_milli: snap.max_energy_milli,
        min_solutions: snap.min_solutions,
        min_diversity_milli: snap.min_diversity_milli,
        num_reads: 0,
        num_sweeps: 0,
        anneal_time_us: 0,
    }
}

/// One miner's line on the throughput heartbeat. Each count has a window,
/// the change since the last heartbeat, and a total since registration.
#[derive(Debug)]
struct MinerThroughput {
    id: String,
    /// Jobs finished with a `Result` or `LeaseDone`.
    completed: u64,
    completed_total: u64,
    /// Jobs the miner refused. Not mining work, so counted apart.
    rejected: u64,
    rejected_total: u64,
    window: usize,
    staged: usize,
    round_salts: u64,
    salts_per_s: f64,
}

/// Broadcast `target` to every live miner off-lock: clone the outbound senders
/// under the state lock, drop it, then await the sends so the fan-out never
/// holds the state lock across I/O.
async fn broadcast_set_target(state: &Arc<Mutex<CoordinatorState>>, target: SetTarget) {
    let senders: Vec<_> = {
        let st = state.lock().await;
        st.outbound.values().cloned().collect()
    };
    for tx in senders {
        let _ = tx.send(Ok(coord(coord_msg::Msg::SetTarget(target)))).await;
    }
}

/// Push a refreshed topology to every live miner off-lock. Live sessions only
/// receive `Topology` at handshake, so a topology that first becomes available
/// (miner connected before the first snapshot) or changes on reseed must be
/// fanned out here — otherwise those miners reject subsequent hash-based jobs as
/// topology-missing/mismatch.
async fn broadcast_topology(state: &Arc<Mutex<CoordinatorState>>, topo: quip_proto::v1::Topology) {
    let senders: Vec<_> = {
        let st = state.lock().await;
        st.outbound.values().cloned().collect()
    };
    for tx in senders {
        let _ = tx
            .send(Ok(coord(coord_msg::Msg::Topology(topo.clone()))))
            .await;
    }
}

/// Fan out a protocol `Cancel(max_generation)` to every live miner on reseed so
/// they abandon stale-generation work promptly. Belt-and-suspenders with the
/// miner-side cooperative-cancel guard and the cleared salts (which already make
/// a late stale-generation result unsubmittable). Returns how many miners were
/// told.
///
/// Each session times its `Cancel` until the miner's `Status` acknowledges
/// it, so the returned count says only that the `Cancel`s were queued.
async fn broadcast_cancel(state: &Arc<Mutex<CoordinatorState>>, max_generation: u64) -> usize {
    let senders: Vec<_> = {
        let st = state.lock().await;
        st.outbound
            .iter()
            .map(|(id, tx)| (tx.clone(), st.cancel_timings.get(id).cloned()))
            .collect()
    };
    let n = senders.len();
    for (tx, timing) in senders {
        let _ = crate::session::send_cancel(&tx, timing.as_deref(), max_generation).await;
    }
    n
}

/// Enter `generation`: drop every staged and in-flight job of the previous
/// generation, forget its salts, and reset the round's best and stash.
/// Returns the count of staged jobs dropped. The caller broadcasts `Cancel`.
async fn stop_mining(coord: &Arc<Mutex<CoordinatorState>>, generation: u64) -> usize {
    let mut st = coord.lock().await;
    st.generation = generation;
    st.current_best_milli = None;
    st.stash.reset(generation, None, 0);
    let dropped = st.router.cancel(generation.saturating_sub(1));
    let _ = st.cancel_inflight(generation.saturating_sub(1));
    st.clear_salts();
    st.router.reset_round_salts();
    dropped
}

/// A unique 32-byte salt from a monotonic counter: distinct salts derive
/// distinct nonces (job ids), so each attempt is a fresh `PoW` draw.
fn salt_from_counter(ctr: u64) -> [u8; 32] {
    let mut salt = [0u8; 32];
    salt[..8].copy_from_slice(&ctr.to_le_bytes());
    salt
}

/// A state that is held this long is no longer a healthy walk.
const SLOW_STATE: Duration = Duration::from_secs(10);

/// Per-entry timer so a slow or retrying state warns once, not every poll.
struct StateHold {
    entered: std::time::Instant,
    warned: bool,
}

impl StateHold {
    fn new() -> Self {
        Self {
            entered: std::time::Instant::now(),
            warned: false,
        }
    }

    fn reset(&mut self) {
        self.entered = std::time::Instant::now();
        self.warned = false;
    }

    fn warn_once(&mut self, machine: RoundState, generation: u64) {
        if self.warned {
            return;
        }
        machine.log_unhealthy(generation);
        self.warned = true;
    }
}

/// Wait for `work`. Warn once if this state stays active longer than 10 seconds.
/// Returns `None` when shutdown is requested.
async fn await_round_step<T>(
    machine: RoundState,
    generation: u64,
    hold: &mut StateHold,
    stop: &mut watch::Receiver<bool>,
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::pin!(work);
    loop {
        let remaining = SLOW_STATE.saturating_sub(hold.entered.elapsed());
        tokio::select! {
            biased;
            _ = stop.changed() => return None,
            () = tokio::time::sleep(remaining), if !hold.warned => {
                hold.warn_once(machine, generation);
            }
            out = &mut work => return Some(out),
        }
    }
}

/// Drive [`RoundState`] from `machine` until [`RoundState::StartMining`].
///
/// The caller logs the entry into the first state and bumps `generation`.
/// This function does the I/O for each state, applies the pure transition,
/// and logs each new state once. A failed step stays in that state and
/// retries after `poll_interval`. Returns `None` when shutdown is requested.
#[expect(
    clippy::too_many_lines,
    reason = "one match arm per round state plus retry"
)]
async fn drive_to_mining<C, F>(
    mut machine: RoundState,
    chain: &C,
    coord: &Arc<Mutex<CoordinatorState>>,
    params: &FeederParams,
    faucet: Option<&F>,
    generation: u64,
    stop: &mut watch::Receiver<bool>,
) -> Option<(MiningSnapshot, usize, usize)>
where
    C: ChainClient + SyncSource + BalanceSource,
    F: Faucet + ?Sized,
{
    let mut cancelled_jobs = 0usize;
    let mut miners_told = 0usize;
    let mut snap: Option<MiningSnapshot> = None;
    let mut hold = StateHold::new();

    loop {
        let event = match machine {
            RoundState::StopMining => {
                cancelled_jobs = stop_mining(coord, generation).await;
                miners_told = if generation > 1 {
                    broadcast_cancel(coord, generation.saturating_sub(1)).await
                } else {
                    0
                };
                RoundEvent::Succeeded
            }
            RoundState::ValidatorSynced => {
                let outcome = await_round_step(
                    machine,
                    generation,
                    &mut hold,
                    stop,
                    wait_until_synced(chain, tokio::time::sleep),
                )
                .await?;
                match outcome {
                    SyncOutcome::Synced => RoundEvent::Succeeded,
                    SyncOutcome::Unknown(reason) => {
                        tracing::warn!(
                            reason = %reason,
                            "cannot confirm the validator has caught up; continuing, but funding and \
                             mining may fail until it does"
                        );
                        RoundEvent::Succeeded
                    }
                }
            }
            RoundState::AccountFunded => {
                let result = await_round_step(
                    machine,
                    generation,
                    &mut hold,
                    stop,
                    ensure_funded(
                        chain,
                        faucet,
                        params.miner_account,
                        &params.funding,
                        tokio::time::sleep,
                    ),
                )
                .await?;
                match result {
                    Ok(_) => RoundEvent::Succeeded,
                    Err(e) => {
                        tracing::warn!(
                            error = %ReadinessError::Funding(e),
                            generation,
                            "round readiness failed; holding off mining"
                        );
                        RoundEvent::Failed
                    }
                }
            }
            RoundState::MinerRegistered => {
                let registered = await_round_step(
                    machine,
                    generation,
                    &mut hold,
                    stop,
                    register_round_miner(chain, params.miner_registered.as_ref()),
                )
                .await?;
                if registered {
                    RoundEvent::Succeeded
                } else {
                    // Mining without registration is guaranteed waste: the
                    // pallet rejects every proof and the account still pays.
                    RoundEvent::Failed
                }
            }
            RoundState::RequirementsDownloaded => {
                let result = await_round_step(
                    machine,
                    generation,
                    &mut hold,
                    stop,
                    chain.fetch_mining_snapshot(None, params.miner_account, None),
                )
                .await?;
                match result {
                    Ok(Some(s)) => {
                        snap = Some(s);
                        RoundEvent::Succeeded
                    }
                    Ok(None) => {
                        tracing::warn!(
                            error = %ReadinessError::Snapshot(
                                "chain has no mining snapshot".into()
                            ),
                            generation,
                            "round readiness failed; holding off mining"
                        );
                        RoundEvent::Failed
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %ReadinessError::Snapshot(e.to_string()),
                            generation,
                            "round readiness failed; holding off mining"
                        );
                        RoundEvent::Failed
                    }
                }
            }
            RoundState::DescriptorFiled => {
                file_round_descriptor(
                    chain,
                    params.descriptor_filed.as_ref(),
                    &params.descriptor,
                    params.miner_account,
                )
                .await;
                RoundEvent::Succeeded
            }
            RoundState::StartMining => {
                return snap.map(|s| (s, cancelled_jobs, miners_told));
            }
            RoundState::AwaitingQBlock => {
                // The feeder holds this state itself and never starts a walk
                // here. If one does start here, restart the round.
                RoundEvent::Resume
            }
        };

        if event == RoundEvent::Failed {
            hold.warn_once(machine, generation);
            tokio::select! {
                () = tokio::time::sleep(params.poll_interval) => {}
                _ = stop.changed() => {
                    let _ = machine.transition(RoundEvent::Shutdown);
                    return None;
                }
            }
        }

        let next = machine.transition(event)?;
        if next != machine {
            next.log_entry(generation);
            hold.reset();
        }
        machine = next;
    }
}

/// Validation gates for results on `order`.
///
/// The pallet keeps a row when `energy <= min_energy_milli`, but
/// [`crate::validate::validate_result`] admits only `energy < min_energy_milli`,
/// so the ceiling moves up one milli. An order without `min_solutions` keeps
/// every submitted row; asking for two keeps a diversity gate reachable, since
/// a single row scores zero diversity.
fn order_gates(order: &JobOrder) -> QualityGates {
    let min_diversity_milli = order.min_diversity_milli.unwrap_or(0);
    QualityGates {
        min_energy_milli: order
            .min_energy_milli
            .map_or(i64::MAX, |e| e.saturating_add(1)),
        min_diversity_milli,
        min_solutions: order
            .min_solutions
            .unwrap_or(if min_diversity_milli > 0 { 2 } else { 1 }),
    }
}

/// Stage each open mempool order this process has not staged before.
///
/// Every order goes to one capable miner as a generation-0 job, which a round
/// turnover never cancels. Its gates are remembered by order id for the session
/// that validates the result.
///
/// Returns whether the orders were read. With no miner connected, nothing is
/// read: routing would drop every order for good.
async fn stage_mempool_orders<C: ChainClient>(
    chain: &C,
    state: &Arc<Mutex<CoordinatorState>>,
    miner_account: [u8; 32],
) -> bool {
    if state.lock().await.router.miner_ids().is_empty() {
        return false;
    }
    let orders = match chain.fetch_mempool_orders(miner_account).await {
        Ok(orders) => orders,
        Err(e) => {
            tracing::debug!(error = %e, "feeder: mempool order read failed");
            return false;
        }
    };
    let mut st = state.lock().await;
    for order in orders {
        if st.mempool_orders.contains_key(&order.order_id) {
            continue;
        }
        let _ = st
            .mempool_orders
            .insert(order.order_id.clone(), order_gates(&order));
        let id = crate::chain::extrinsic::hex_encode(&order.order_id);
        let Some(job) = job_order_to_job(&order) else {
            tracing::warn!(
                order = %id,
                nodes = order.nodes.len(),
                edges = order.edges.len(),
                "feeder: mempool order's node list cannot index its own edges; skipping it"
            );
            continue;
        };
        if let Some(miner) = st.router.route(job) {
            tracing::info!(order = %id, miner = %miner, nodes = order.nodes.len(), "feeder: staged mempool order");
            st.wake_dispatcher(&miner);
        } else {
            tracing::warn!(
                order = %id,
                nodes = order.nodes.len(),
                edges = order.edges.len(),
                "feeder: no connected miner can take this mempool order; skipping it"
            );
        }
    }
    true
}

/// The replenished `PoW` feeder: follow the chain head, and keep each registered
/// miner's staged queue topped up to `buffer_depth` with fresh-salt jobs.
///
/// On a new `last_proof_block_hash` it drives the round state machine: stop
/// mining, wait until the validator is synced, confirm the miner account can
/// pay fees, download the next qblock's requirements, file a node descriptor
/// on the first walk, declare participation, then start mining under the new
/// seed. While mining, it also reads the transaction pool. When a pending proof
/// clears the round, it stops the miners and waits for the block that includes it.
/// Runs until `stop` flips. `pub` so it can be exercised directly in tests without
/// a gRPC server.
#[expect(
    clippy::too_many_lines,
    reason = "single feeder loop: reseed, top-up, win-time submit"
)]
pub async fn feeder_loop<C>(
    chain: Arc<C>,
    state: Arc<Mutex<CoordinatorState>>,
    params: FeederParams,
    mut stop: watch::Receiver<bool>,
) where
    C: ChainClient + SyncSource + BalanceSource,
{
    let mut current_head: Option<[u8; 32]> = None;
    let mut last_block_number: u64 = 0;
    let mut generation: u64 = 0;
    let mut salt_ctr: u64 = 0;
    // Monotonic anchor for block-time estimation (win-time submission).
    let start = std::time::Instant::now();
    // Per-miner smoothed jobs-consumed-per-poll, driving the adaptive window.
    let mut consumption_ema: HashMap<String, f64> = HashMap::new();
    // Per-miner lease throughput, for lease sizing.
    let mut salt_rates: HashMap<String, crate::lease::SaltRate> = HashMap::new();
    // When lease rates were last updated, so each update covers the real
    // time since the previous one rather than the nominal poll interval.
    let mut last_rate_update = std::time::Instant::now();
    // Last difficulty triple broadcast to miners, so we only re-push on change
    // within a round. A reseed always pushes Topology and SetTarget again.
    let mut last_broadcast: Option<(i64, u32, u32)> = None;

    // Last snapshot-poll outcome, so the feeder narrates transitions instead of
    // one line per poll. `None` until the first poll completes.
    let mut feed_state: Option<FeedState> = None;
    let mut last_heartbeat = std::time::Instant::now();
    // Completion and reject totals printed on the last heartbeat, per miner.
    // The next heartbeat reports the delta from this map so the line does not
    // depend on which poll it lands on.
    let mut last_finished: HashMap<String, (u64, u64)> = HashMap::new();
    let faucet = build_faucet(params.funding.faucet_url.as_deref());
    let mut round: Option<RoundState> = None;
    // Participation bookkeeping. `last_declared` is the candidate qblock the
    // chain has an answer for; `declared_generation` is the round that answer
    // was obtained in, so the qblock id is read once per mined round.
    let mut last_declared: Option<u64> = None;
    let mut declared_generation: Option<u64> = None;
    // Consecutive failed submits of one proof in one quantum block.
    let mut submit_ledger = crate::chain::SubmitLedger::new(params.max_submit_attempts);
    // Round of the last solver-registration attempt, and the block the open
    // mempool orders were last read at.
    let mut solver_attempted: Option<u64> = None;
    let mut orders_read_at: u64 = 0;
    // Pool scan memory and the wait for the block that includes a pending win.
    let mut pool_watch = crate::pool_watch::PoolWatch::new();
    let mut round_log: Option<Arc<Mutex<RoundLog>>> = None;

    loop {
        let (snap, state_now) = match chain
            .fetch_mining_snapshot(None, params.miner_account, None)
            .await
        {
            Ok(Some(s)) => (Some(s), FeedState::Ready),
            Ok(None) => (None, FeedState::Empty),
            Err(e) => {
                // Repeat the detail every poll at debug: the transition line
                // below carries it once, but a changing error matters while
                // diagnosing (e.g. RPC refused → runtime API missing).
                tracing::debug!(error = %e, "feeder: snapshot fetch failed");
                if feed_state != Some(FeedState::Failed) {
                    tracing::warn!(
                        error = %e,
                        "feeder: cannot fetch mining snapshot; staging nothing until this clears"
                    );
                }
                (None, FeedState::Failed)
            }
        };

        // Narrate the transition. Without this, a coordinator that never mines
        // is indistinguishable from one that mines fine.
        if feed_state != Some(state_now) {
            match state_now {
                FeedState::Ready => tracing::info!("feeder: mining snapshot available"),
                FeedState::Empty => tracing::warn!(
                    "feeder: chain has no mining snapshot (no registered/mineable topology); \
                     staging nothing"
                ),
                // Already reported above, with the error attached.
                FeedState::Failed => {}
            }
            feed_state = Some(state_now);
        }

        if let Some(mut snap) = snap {
            // The chain advances every block; `last_proof_block_hash` changes
            // only on a win. Counting block-number advances is what makes
            // `heads_observed` track chain height, which is what the dashboard
            // charts against.
            if snap.block_number > last_block_number {
                params.metrics.record_head_observed();
                last_block_number = snap.block_number;
            }

            // Refresh the dashboard's chain view. `miner_info` is a storage read,
            // so it is refreshed on a round turnover only, not on every poll.
            let refresh_miner_info = current_head != Some(snap.last_proof_block_hash);
            let miner_info = if refresh_miner_info {
                match chain.fetch_miner_info(params.miner_account).await {
                    Ok(info) => info,
                    Err(e) => {
                        tracing::debug!(error = %e, "feeder: miner info read failed");
                        params.metrics.chain().miner_info
                    }
                }
            } else {
                params.metrics.chain().miner_info
            };
            params.metrics.set_chain(crate::metrics::ChainView {
                head_hash: crate::chain::extrinsic::hex_encode(&snap.head_hash),
                head_number: snap.block_number,
                is_mining: round != Some(RoundState::AwaitingQBlock),
                miner_registered: miner_info.is_some(),
                miner_info,
            });

            let head = snap.last_proof_block_hash;
            // Pool scan. A proof that clears the round means the next block
            // mints a qblock. Scanned only on an unchanged root, in the two
            // states that care: mining (to stop) and awaiting (to resume).
            let (mut clearing, observed) = if current_head == Some(head)
                && matches!(
                    round,
                    Some(RoundState::StartMining | RoundState::AwaitingQBlock)
                ) {
                match chain.fetch_pending_proofs().await {
                    Ok(pending) => {
                        let clearing = pool_watch.scan(&pending, &snap);
                        let observed = if clearing.is_some() {
                            PoolObservation::Clearing
                        } else {
                            PoolObservation::Clear
                        };
                        (clearing, observed)
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "feeder: pool read failed");
                        (None, PoolObservation::Unknown)
                    }
                }
            } else {
                (None, PoolObservation::Unknown)
            };
            let event = if current_head != Some(head) {
                Some(RoundEvent::NewHead)
            } else if round == Some(RoundState::AwaitingQBlock) {
                pool_watch
                    .resume_reason(observed, snap.block_number)
                    .map(|reason| {
                        pool_watch.expire_clearing();
                        clearing = None;
                        tracing::info!(
                            generation,
                            block = snap.block_number,
                            reason = %reason,
                            "resuming the round on the same root"
                        );
                        RoundEvent::Resume
                    })
            } else {
                None
            };
            if let Some(event) = event {
                if let Some(log) = &round_log {
                    let mut log = log.lock().await;
                    if event == RoundEvent::NewHead {
                        log.close();
                    } else {
                        log.resume();
                    }
                }
                let next = match round {
                    Some(s) => s.transition(event),
                    None => Some(RoundState::start()),
                };
                let Some(machine) = next else {
                    return;
                };
                generation = generation.saturating_add(1);
                machine.log_entry(generation);

                let Some((fresh, cancelled_jobs, miners_told)) = drive_to_mining(
                    machine,
                    chain.as_ref(),
                    &state,
                    &params,
                    faucet.as_ref(),
                    generation,
                    &mut stop,
                )
                .await
                else {
                    return;
                };
                snap = fresh;
                round = Some(RoundState::StartMining);

                let mut topo = Topology::from_nodes_edges(
                    snap.nodes.clone(),
                    snap.edges.clone(),
                    &snap.allowed_h_milli,
                    &snap.allowed_j_milli,
                    &snap.allowed_spin_milli,
                );
                // The miners look the topology up by the hash a lease names,
                // which is the chain's. A family topology hash also covers the
                // descriptor, so the value sets alone cannot rebuild it.
                topo.hash.clone_from(&snap.topology_hash);
                let qblock_id = match chain.fetch_latest_qblock_id().await {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::warn!(error = %e, "feeder: qblock id read failed; attempt logs lose the qblock key");
                        None
                    }
                };
                let decay = match <[u8; 32]>::try_from(snap.topology_hash.as_slice()) {
                    Ok(h) => match chain.fetch_decay_params(h).await {
                        Ok(dp) => dp,
                        Err(e) => {
                            tracing::warn!(error = %e, "feeder: decay params read failed; running without decay projection");
                            None
                        }
                    },
                    Err(_) => None,
                };
                let (model, last_proof_block) = match &decay {
                    Some(dp) => {
                        let curve = match dp.energy_band_milli {
                            Some((hard, easy)) => EnergyCurve::from_band(hard, easy),
                            None => Some(EnergyCurve::from_topology(
                                snap.nodes.len() as u64,
                                snap.edges.len() as u64,
                                dp.c_easy_milli,
                                dp.c_knee_milli,
                                dp.c_hard_milli,
                                &snap.allowed_h_milli,
                                &snap.allowed_j_milli,
                            )),
                        };
                        (
                            Some(DecayModel {
                                base_max_energy_milli: dp.base_max_energy_milli,
                                curve,
                                epoch_length: dp.epoch_length,
                                algorithm: DecayAlgorithm::for_spec_version(snap.spec_version),
                                target_proof_blocks: snap.target_proof_blocks,
                            }),
                            dp.last_proof_block,
                        )
                    }
                    None => (None, 0),
                };
                let topo_proto = topo.to_proto();
                let target = target_from_snapshot(&snap);
                let target_key = (
                    target.max_energy_milli,
                    target.min_solutions,
                    target.min_diversity_milli,
                );
                if current_head != Some(snap.last_proof_block_hash) {
                    round_log = Some(Arc::new(Mutex::new(RoundLog::new(
                        qblock_id,
                        params.miner_account,
                    ))));
                }
                tracing::info!(
                    generation,
                    qblock_id = %crate::logging::display_option(qblock_id.map(|id| id.saturating_add(1))),
                    block = snap.block_number,
                    spec_version = snap.spec_version,
                    decay_algorithm = DecayAlgorithm::for_spec_version(snap.spec_version).name(),
                    target_proof_blocks = snap.target_proof_blocks,
                    cancelled_jobs,
                    miners_told,
                    topology = %crate::chain::extrinsic::hex_encode(&snap.topology_hash),
                    nodes = snap.nodes.len(),
                    edges = snap.edges.len(),
                    max_energy = crate::logging::energy_units(target.max_energy_milli),
                    min_solutions = target.min_solutions,
                    min_diversity_milli = target.min_diversity_milli,
                    allowed_h_milli = ?snap.allowed_h_milli,
                    allowed_j_milli = ?snap.allowed_j_milli,
                    allowed_spin_milli = ?snap.allowed_spin_milli,
                    "new round"
                );
                {
                    let mut st = state.lock().await;
                    st.set_topology(Some(topo));
                    st.target = Some(target);
                    st.qblock_id = qblock_id;
                    st.round_log.clone_from(&round_log);
                    st.last_proof_block_hash =
                        crate::chain::extrinsic::hex_encode(&snap.last_proof_block_hash);
                    st.stash.reset(generation, model, last_proof_block);
                }
                // Requirements before any new-generation Job: Topology and
                // SetTarget go out on every reseed, even when the values match
                // the last round. Staging happens only after these sends.
                broadcast_topology(&state, topo_proto).await;
                broadcast_set_target(&state, target).await;
                last_broadcast = Some(target_key);
                current_head = Some(snap.last_proof_block_hash);
            }

            // A clearing proof is pending: stop the miners and wait for the
            // block that includes it. That block's hash is the next root.
            if round == Some(RoundState::StartMining) {
                if let Some(win) = clearing {
                    // Decide before stop_mining moves the state generation.
                    let owed =
                        declared_generation != Some(generation) && state.lock().await.round_mined();
                    if let Some(log) = &round_log {
                        log.lock().await.observe_pending(win, snap.max_energy_milli);
                    }
                    round = round.and_then(|s| s.transition(RoundEvent::WinPending));
                    generation = generation.saturating_add(1);
                    RoundState::AwaitingQBlock.log_entry(generation);
                    let cancelled_jobs = stop_mining(&state, generation).await;
                    let miners_told = broadcast_cancel(&state, generation.saturating_sub(1)).await;
                    pool_watch.stopped(snap.block_number);
                    if let Some(log) = &round_log {
                        log.lock().await.log_pause(
                            generation,
                            snap.block_number,
                            cancelled_jobs,
                            miners_told,
                        );
                    }
                    // Declare after Cancel: inclusion can wait, but miners must stop first.
                    if owed {
                        let settled = declare_round_participation(
                            chain.as_ref(),
                            &mut last_declared,
                            params.miner_account,
                        )
                        .await;
                        if !settled {
                            tracing::warn!(
                                generation,
                                "participation for the stopped round did not settle"
                            );
                        }
                    }
                }
            }
            if round == Some(RoundState::AwaitingQBlock) {
                if let Some(log) = &round_log {
                    if let Some(win) = clearing {
                        log.lock().await.observe_pending(win, snap.max_energy_milli);
                    }
                    if last_heartbeat.elapsed() >= FEEDER_HEARTBEAT {
                        let validated = state.lock().await.results_validated;
                        log.lock().await.log_heartbeat(
                            snap.block_number,
                            generation,
                            validated,
                            true,
                        );
                        last_heartbeat = std::time::Instant::now();
                    }
                }
                let mut view = params.metrics.chain();
                view.is_mining = false;
                params.metrics.set_chain(view);
                tokio::select! {
                    () = tokio::time::sleep(AWAIT_QBLOCK_POLL.min(params.poll_interval)) => {}
                    _ = stop.changed() => {
                        let _ = RoundState::AwaitingQBlock.transition(RoundEvent::Shutdown);
                        break;
                    }
                }
                continue;
            }

            // Declare participation only once a miner has returned a Result
            // for this round. Staged work is not evidence: a QPU sits a round
            // out by withholding credits and rejecting what it is sent, and a
            // miner that is down returns nothing. Declaring on the first
            // Result records the rounds that were actually mined and nothing
            // else. `declare_round_participation` deduplicates per candidate
            // qblock, so a reseed of the same qblock does not submit again.
            if declared_generation != Some(generation) && state.lock().await.round_mined() {
                let settled = declare_round_participation(
                    chain.as_ref(),
                    &mut last_declared,
                    params.miner_account,
                )
                .await;
                if settled {
                    declared_generation = Some(generation);
                }
            }

            // Push the refreshed difficulty to live miners when it changed, so
            // they adapt their sampling budget as the chain difficulty moves.
            let target = target_from_snapshot(&snap);
            let key = (
                target.max_energy_milli,
                target.min_solutions,
                target.min_diversity_milli,
            );
            if last_broadcast != Some(key) {
                // Difficulty eases every block via the decay ratchet, so this is
                // debug: at info it would fire nearly every poll.
                tracing::debug!(
                    max_energy = crate::logging::energy_units(key.0),
                    min_solutions = key.1,
                    min_diversity_milli = key.2,
                    "difficulty target changed; broadcasting to miners"
                );
                broadcast_set_target(&state, target).await;
                last_broadcast = Some(key);
            }

            // Top every registered miner up to its adaptive window with fresh
            // jobs. The window tracks each miner's drain rate: sample the
            // jobs-consumed counter (read-and-reset), smooth it, and size the
            // staged reserve to ~headroom poll-intervals of that rate.
            let mut st = state.lock().await;
            // Persist the current difficulty so session-path validation gates on
            // the same target the miners were just told. Within one proof-block
            // the decay ratchet eases `max_energy` each block without a reseed;
            // without this, `st.target` would stay pinned at the last reseed's
            // (harder) gate and under-accept solutions viable at the eased one.
            st.target = Some(target);
            // A runtime upgrade or a council target change takes effect at
            // one block. The decay rule follows the spec version of the head
            // and the gate follows its target, so the projection switches on
            // the poll that reports either, with every stashed candidate kept.
            let algorithm = DecayAlgorithm::for_spec_version(snap.spec_version);
            if st.stash.set_rule(algorithm, snap.target_proof_blocks) {
                tracing::info!(
                    spec_version = snap.spec_version,
                    block = snap.block_number,
                    decay_algorithm = algorithm.name(),
                    target_proof_blocks = snap.target_proof_blocks,
                    "decay rule or target changed; projection switched"
                );
            }
            // Per-miner drain/staging stats for the heartbeat, collected here
            // and emitted off-lock below.
            let mut stats: Vec<MinerThroughput> = Vec::new();
            let rate_secs = last_rate_update.elapsed().as_secs_f64();
            last_rate_update = std::time::Instant::now();
            for id in st.router.miner_ids() {
                let consumed_raw = st.router.take_consumed(&id);
                let consumed = f64::from(consumed_raw);
                let ema = match consumption_ema.get(&id) {
                    Some(&prev) => {
                        CONSUMPTION_EMA_ALPHA * consumed + (1.0 - CONSUMPTION_EMA_ALPHA) * prev
                    }
                    // First sample seeds the EMA directly so a fast miner reaches
                    // full depth in one poll instead of ramping over several.
                    None => consumed,
                };
                let _ = consumption_ema.insert(id.clone(), ema);
                let lease_width = st
                    .router
                    .caps(&id)
                    .filter(|c| snap.family.is_some() || c.accepts_leases())
                    .map(|c| c.stream_width);
                // Lease salts per second, for the heartbeat. Zero for a miner
                // that takes plain jobs.
                let mut salts_per_s = 0.0;
                let depth = if let Some(stream_width) = lease_width {
                    let finished = st.router.take_lease_salts(&id);
                    // A miner's window opens at its first lease. Time before
                    // that, such as the wait for registration, is not its rate.
                    let rate = if let Some(r) = salt_rates.get_mut(&id) {
                        r.observe(finished, rate_secs)
                    } else {
                        let _ = salt_rates.insert(id.clone(), crate::lease::SaltRate::default());
                        0.0
                    };
                    salts_per_s = rate;
                    let count = crate::lease::lease_salt_count(rate, stream_width);
                    while st.router.staged_len(&id) < crate::lease::LEASE_STAGE_DEPTH {
                        let start = salt_ctr.saturating_add(1);
                        let job = crate::lease::build_lease_job(
                            &snap,
                            params.miner_identity,
                            start,
                            count,
                            generation,
                        );
                        if !st.router.stage_on(&id, job) {
                            break;
                        }
                        tracing::info!(
                            miner = %id,
                            generation,
                            salt_start = start,
                            salt_count = count,
                            salts_per_sec = rate,
                            "lease issued"
                        );
                        salt_ctr = start.saturating_add(count - 1);
                    }
                    crate::lease::LEASE_STAGE_DEPTH
                } else {
                    let depth = adaptive_depth(ema, params.buffer_depth).min(stage_ceiling(
                        snap.nodes.len(),
                        snap.edges.len(),
                        params.buffer_depth,
                    ));
                    while st.router.staged_len(&id) < depth {
                        salt_ctr = salt_ctr.saturating_add(1);
                        let salt = salt_from_counter(salt_ctr);
                        let job = match derive_pow_job(
                            &snap,
                            params.miner_identity,
                            salt,
                            generation,
                            0,
                        ) {
                            Ok(job) => job,
                            Err(e) => {
                                // The snapshot's allowed-value sets are validated at
                                // fetch (`require_set_values`), so an empty set here is
                                // an invariant violation, not routine. Stop topping up
                                // this miner rather than crash the feeder.
                                tracing::error!(error = %e, miner = %id, "feeder: cannot draw PoW job");
                                break;
                            }
                        };
                        let job_id = job.job_id.clone();
                        if st.router.stage_on(&id, job) {
                            st.record_salt(&job_id, salt);
                        } else {
                            break; // not capable for this shape — stop topping up
                        }
                    }
                    depth
                };
                // Wake the dispatcher now that work is staged. A miner grants
                // its credits when it starts, which is normally before the
                // first snapshot arrives, so that grant drained an empty queue
                // and the dispatcher parked. Without this the miner waits for a
                // job and the coordinator waits for a request, forever.
                st.wake_dispatcher(&id);
                let completed_total = st.router.jobs_completed(&id);
                let rejected_total = st.router.jobs_rejected(&id);
                let (completed_before, rejected_before) =
                    last_finished.get(&id).copied().unwrap_or_default();
                stats.push(MinerThroughput {
                    id: id.clone(),
                    completed: completed_total.saturating_sub(completed_before),
                    completed_total,
                    rejected: rejected_total.saturating_sub(rejected_before),
                    rejected_total,
                    window: depth,
                    staged: st.router.staged_len(&id),
                    round_salts: st.router.round_salts(&id),
                    salts_per_s,
                });
            }
            if stats.is_empty() {
                // Miners are configured but none has completed a handshake, so
                // there is nowhere to stage work. Silent otherwise.
                tracing::debug!("feeder: no registered miners; nothing to stage");
            }

            // Win-time submission: observe the head for block estimation, pick
            // the best stashed candidate whose projected viability block has
            // arrived (and still improves on the current best), then submit it
            // off-lock. `mark_submitted` + the `beats_current` guard keep this
            // from double-submitting or regressing what the session path sent.
            let now_mono = start.elapsed().as_secs_f64();
            let now_wall = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |d| d.as_secs_f64());
            st.timing
                .observe_head(snap.block_number, now_wall, now_mono, now_wall);
            let current_block = st
                .timing
                .estimate_block(now_mono)
                .unwrap_or(snap.block_number);
            let best = st.current_best_milli;
            let due = st.stash.due_improving(current_block, best).cloned();
            let validated = st.results_validated;
            drop(st);

            // Mempool work rides alongside `PoW`. Solver registration is tried
            // once per round and never holds mining off. Orders are read once
            // per block after it succeeds: `submit_solution` refuses anyone else.
            if solver_attempted != Some(generation) {
                solver_attempted = Some(generation);
                let _ = crate::readiness::register_round_solver(
                    chain.as_ref(),
                    params.solver_registered.as_ref(),
                )
                .await;
            }
            if params.solver_registered.load(Ordering::Relaxed)
                && snap.block_number > orders_read_at
                && stage_mempool_orders(chain.as_ref(), &state, params.miner_account).await
            {
                orders_read_at = snap.block_number;
            }

            // Steady-state narration. Every poll at debug for diagnosis; once a
            // minute at info so an operator watching the log sees the
            // coordinator is alive and how fast each miner is draining work.
            tracing::debug!(
                block = current_block,
                generation,
                miners = ?stats,
                "feeder: poll"
            );
            if last_heartbeat.elapsed() >= FEEDER_HEARTBEAT {
                for m in &stats {
                    tracing::info!(
                        miner = %m.id,
                        jobs_completed = m.completed,
                        jobs_completed_total = m.completed_total,
                        jobs_rejected = m.rejected,
                        jobs_rejected_total = m.rejected_total,
                        staged = m.staged,
                        window = m.window,
                        round_salts = m.round_salts,
                        salts_per_s = %format!("{:.0}", m.salts_per_s),
                        "miner throughput"
                    );
                    let _ =
                        last_finished.insert(m.id.clone(), (m.completed_total, m.rejected_total));
                }
                if let Some(log) = &round_log {
                    log.lock()
                        .await
                        .log_heartbeat(current_block, generation, validated, false);
                }
                last_heartbeat = std::time::Instant::now();
            }

            // The chain re-runs its gates against the difficulty live at the
            // inclusion block: it filters the proof to the rows under that
            // ceiling and scores diversity over only those. The ceiling moves
            // between stashing and submission, so replay the gates against the
            // freshest target and leave a candidate that does not clear them
            // stashed for a later window instead of spending a rejection on it.
            let live_gates = crate::validate::gates_from_target(Some(&target));
            let due = due.filter(|cand| {
                let check = crate::validate::check_proof_gates(&cand.solutions, &live_gates);
                if !check.accepted {
                    tracing::debug!(
                        job = %crate::chain::extrinsic::hex_encode(&cand.job_id),
                        n_valid = check.n_valid,
                        diversity_milli = check.diversity_milli,
                        min_solutions = live_gates.min_solutions,
                        min_diversity_milli = live_gates.min_diversity_milli,
                        max_energy = %crate::logging::energy_units(live_gates.min_energy_milli),
                        "win-time candidate does not clear the live difficulty; leaving stashed"
                    );
                }
                check.accepted
            });

            if let Some(cand) = due {
                use crate::chain::SubmitAction;

                let proof = crate::chain::Proof {
                    job_id: cand.job_id.clone(),
                    best_energy_milli: cand.best_energy_milli,
                    diversity_milli: cand.diversity_milli,
                    n_valid: cand.n_valid,
                    solutions: cand.solutions.clone(),
                    is_pow: cand.is_pow,
                    order_id: cand.order_id.clone(),
                    generation: cand.generation,
                    salt: cand.salt.map(|s| s.to_vec()).unwrap_or_default(),
                    device_access_time_us: cand.device_access_time_us,
                };
                let job_hex = crate::chain::extrinsic::hex_encode(&cand.job_id);
                if let Some(log) = &round_log {
                    log.lock().await.submission_started();
                }
                let receipt = chain.submit_proof(&proof).await;
                if let Some(log) = &round_log {
                    log.lock().await.submission_finished(
                        receipt.as_ref().ok(),
                        cand.best_energy_milli,
                        target.max_energy_milli,
                    );
                }
                match receipt.as_ref().map(|r| r.action) {
                    Ok(SubmitAction::Success) => {
                        let mut st = state.lock().await;
                        st.stash.mark_submitted(&cand.job_id);
                        let qblock_key = st.qblock_id.unwrap_or(0);
                        submit_ledger.record_success(qblock_key, &cand.job_id);
                        st.current_best_milli = Some(cand.best_energy_milli);
                        let qblock_id = st.qblock_id;
                        let body = crate::attempt::summary_body(
                            qblock_id,
                            st.current_best_milli,
                            st.results_validated,
                            st.stash.summary(),
                        );
                        if let Some(tx) = st.attempt_tx.as_ref() {
                            let _ = tx.send(crate::attempt::WriterMsg::Summary { qblock_id, body });
                        }
                    }
                    Ok(SubmitAction::Retry) => {
                        let qblock_key = state.lock().await.qblock_id.unwrap_or(0);
                        let decided = submit_ledger.record_failure(qblock_key, &cand.job_id);
                        let attempts = submit_ledger.attempts(qblock_key, &cand.job_id);
                        if decided == SubmitAction::StopFatal {
                            tracing::error!(
                                job = %job_hex,
                                attempts,
                                "win-time submit refused repeatedly; stopping this proof"
                            );
                            state.lock().await.stash.mark_submitted(&cand.job_id);
                        } else {
                            tracing::warn!(
                                job = %job_hex,
                                attempts,
                                "win-time submit rejected (retryable); leaving stashed"
                            );
                        }
                    }
                    Ok(SubmitAction::StopRoundStale) => {
                        // Stale for this round (e.g. InvalidNonce / topology moved);
                        // the next reseed clears the stash. Stop resubmitting it now.
                        tracing::info!(job = %job_hex, "win-time submit stale for round; dropping candidate");
                        state.lock().await.stash.mark_submitted(&cand.job_id);
                    }
                    Ok(SubmitAction::StopFatal) => {
                        // Fatally rejected by the pallet (InsufficientSolutions/
                        // InsufficientDiversity/MinerNotRegistered/...): never
                        // retry it, or the same candidate loops every window.
                        tracing::error!(job = %job_hex, "win-time submit fatally rejected by pallet; dropping candidate");
                        state.lock().await.stash.mark_submitted(&cand.job_id);
                    }
                    Err(e) => {
                        // Transient chain error (RPC down / unreachable): keep the
                        // winning candidate stashed for the next due window.
                        tracing::error!(job = %job_hex, error = %e, "win-time submit failed (transient); leaving stashed for retry");
                    }
                }
            }
        } else {
            let mut view = params.metrics.chain();
            view.is_mining = false;
            params.metrics.set_chain(view);
        }

        tokio::select! {
            () = tokio::time::sleep(params.poll_interval) => {}
            _ = stop.changed() => {
                if let Some(s) = round {
                    let _ = s.transition(RoundEvent::Shutdown);
                }
                break;
            }
        }
    }
}

/// Serve the UDS session server, supervise every miner in `launch`, and return
/// once `shutdown` resolves — fanning an in-band `Shutdown` to each live miner
/// and killing after grace. Generic over [`ChainClient`] so tests drive it with
/// `FakeChain`; `main` passes `RealChainClient`. `state` is shared with the
/// Optional mining-attempt dashboard: a single writer thread records every
/// solved model to `<data_dir>/<qblock_id>/attempts.jsonl`, and an HTTP task
/// serves those files plus the three `/api/v1` endpoints.
///
/// Logs which way it went either way. A silent skip here used to surface only
/// as a refused connection from whatever proxies the port, which points the
/// operator at the proxy instead of at the config that disabled the server.
async fn spawn_dashboard(
    params: &RuntimeParams,
    state: &Arc<Mutex<CoordinatorState>>,
    metrics: &Arc<crate::metrics::CoordinatorMetrics>,
) -> Option<tokio::task::JoinHandle<()>> {
    let Some((listen, data_dir)) = params.dashboard.clone() else {
        // Reached both when the config carries no [dashboard] section and when
        // it carries an unusable one; `parse_dashboard` warns about the latter
        // and names the missing key.
        tracing::info!("dashboard disabled: no usable [dashboard] section in the config");
        return None;
    };
    tracing::info!(%listen, data_dir = %data_dir.display(), "dashboard enabled");
    let tx = crate::attempt::spawn_writer(data_dir.clone());
    state.lock().await.attempt_tx = Some(tx);
    Some(tokio::spawn(crate::dashboard::serve(
        listen,
        crate::dashboard::DashboardState {
            data_dir,
            metrics: Arc::clone(metrics),
        },
    )))
}

/// caller so a live coordinator (and tests) can inspect routing/inflight.
///
/// # Errors
/// Returns an I/O error if the UDS socket cannot be bound.
pub async fn run_runtime<C, S>(
    launch: Vec<LaunchEntry>,
    chain: Arc<C>,
    state: Arc<Mutex<CoordinatorState>>,
    params: RuntimeParams,
    shutdown: S,
) -> std::io::Result<()>
where
    C: ChainClient + SyncSource + BalanceSource + 'static,
    S: Future<Output = ()>,
{
    let _ = std::fs::remove_file(&params.sock_path);
    if let Some(parent) = Path::new(&params.sock_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let uds = UnixListener::bind(&params.sock_path)?;
    let incoming = UnixListenerStream::new(uds);

    // Seed each miner's Configure so the server can answer its handshake; the
    // per-spawn session token is set by the supervisor before each spawn.
    {
        let mut st = state.lock().await;
        for e in &launch {
            let _ = st.configure.insert(e.miner_id.clone(), e.configure.clone());
            let _ = st.miner_types.insert(
                e.miner_id.clone(),
                crate::metrics::miner_type_label(&e.backend),
            );
        }
    }

    // Live counters and identity, shared by the session path, the feeder, and
    // the dashboard router. Built from the launch plan so `modes` carries one
    // entry per backend group.
    let metrics = Arc::new(crate::metrics::CoordinatorMetrics::new(
        &launch
            .iter()
            .map(|e| (e.miner_id.clone(), e.backend.clone()))
            .collect::<Vec<_>>(),
    ));
    metrics.set_identity(params.identity.clone());
    state.lock().await.metrics = Arc::clone(&metrics);

    let dashboard_server = spawn_dashboard(&params, &state, &metrics).await;

    let svc = CoordinatorService {
        state: Arc::clone(&state),
        chain: Arc::clone(&chain),
        submit_notify: Arc::new(Mutex::new(None)),
    };
    let server = tokio::spawn(
        Server::builder()
            .add_service(MinerServiceServer::new(svc))
            .serve_with_incoming(incoming),
    );

    let (stop_tx, stop_rx) = watch::channel(false);
    let sock_uri = format!("unix://{}", params.sock_path);
    let mut supervisors = Vec::with_capacity(launch.len());
    for entry in launch {
        supervisors.push(tokio::spawn(supervise_miner(
            entry,
            sock_uri.clone(),
            Arc::clone(&state),
            params.backoff,
            params.grace_ms,
            params.log_level,
            stop_rx.clone(),
        )));
    }

    // The replenished feeder follows the chain head and keeps buffers full.
    let feeder = tokio::spawn(feeder_loop(
        Arc::clone(&chain),
        Arc::clone(&state),
        FeederParams {
            miner_identity: params.miner_identity,
            miner_account: params.miner_account,
            buffer_depth: params.buffer_depth,
            poll_interval: Duration::from_millis(params.poll_interval_ms),
            funding: params.funding,
            descriptor: params.descriptor,
            descriptor_filed: params.descriptor_filed,
            miner_registered: params.miner_registered,
            solver_registered: params.solver_registered,
            max_submit_attempts: params.max_submit_attempts,
            metrics: Arc::clone(&metrics),
        },
        stop_rx.clone(),
    ));

    let liveness = tokio::spawn(crate::liveness::liveness_loop(
        Arc::clone(&state),
        LIVENESS_PING_INTERVAL,
        stop_rx.clone(),
    ));
    drop(stop_rx);

    // Run until asked to stop.
    shutdown.await;

    // Fan shutdown out; drain the feeder + supervisors before dropping the
    // server so in-flight Shutdown/kill completes.
    let _ = stop_tx.send(true);
    let _ = feeder.await;
    let _ = liveness.await;
    for h in supervisors {
        let _ = h.await;
    }
    server.abort();
    if let Some(h) = dashboard_server {
        h.abort();
    }
    let _ = std::fs::remove_file(&params.sock_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::RoundLog;
    use super::{
        adaptive_depth, order_gates, stage_ceiling, MAX_STAGE_BYTES_PER_MINER, WINDOW_HEADROOM,
    };
    use crate::chain::JobOrder;
    use crate::chain::{SubmitAction, SubmitReceipt};
    use crate::logging::test_support::capture_logs;
    use crate::pool_watch::Clearing;

    fn pending(account: [u8; 32]) -> Clearing {
        Clearing {
            account,
            nonce: 1.into(),
            best_energy_milli: -5_038_000,
        }
    }

    fn won() -> SubmitReceipt {
        SubmitReceipt {
            action: SubmitAction::Success,
            extrinsic_hash: Some([3; 32]),
            block_hash: Some([4; 32]),
            block_number: Some(693),
        }
    }

    #[test]
    fn round_log_pause_names_the_owner_target_and_our_best() {
        for (account, best, owner, best_text) in [
            ([1; 32], Some(-4_999_000), "ours", "-4999"),
            ([2; 32], None, "another_miner", "none"),
        ] {
            let mut log = RoundLog::new(Some(64), [1; 32]);
            log.our_best_milli = best;
            log.observe_pending(pending(account), -5_030_000);
            let output = capture_logs(|| log.log_pause(23, 692, 2, 1));
            for field in [
                "qblock_id=65".to_owned(),
                format!("proof_owner={owner}"),
                "proof_energy=-5038".to_owned(),
                "target_energy=-5030".to_owned(),
                format!("our_best_energy_this_round={best_text}"),
            ] {
                assert!(output.contains(&field), "{output}");
            }
            assert!(output.contains("pausing coordinator"), "{output}");
            assert!(!output.contains(" best_energy="), "{output}");
            assert!(!output.contains("qblock won"), "{output}");
        }
    }

    #[test]
    fn round_log_reports_our_best_result_without_a_submission() {
        let mut log = RoundLog::new(Some(64), [1; 32]);
        log.observe_result(-5_020_000);
        log.observe_result(i64::MAX); // a result with no rows changes nothing
        log.observe_result(-5_038_000);
        log.observe_result(-5_001_000);
        log.observe_pending(pending([2; 32]), -5_030_000);
        let output = capture_logs(|| {
            log.log_pause(23, 692, 2, 1);
            log.close();
        });
        assert_eq!(
            output.matches("our_best_energy_this_round=-5038").count(),
            2,
            "{output}"
        );
        assert!(output.contains("qblock won by another miner"), "{output}");
    }

    #[test]
    fn round_log_reports_our_confirmed_win_only_once() {
        let mut log = RoundLog::new(Some(64), [1; 32]);
        log.observe_pending(pending([2; 32]), -5_030_000);
        let output = capture_logs(|| {
            for _ in 0..2 {
                log.submission_started();
                log.submission_finished(Some(&won()), -5_038_000, -5_030_000);
            }
            log.close();
            log.close();
        });
        assert_eq!(output.matches("qblock won by us").count(), 1, "{output}");
        assert!(!output.contains("another miner"), "{output}");
        for field in [
            "qblock_id=65",
            "inclusion_block=693",
            "proof_energy=-5038",
            "target_energy=-5030",
            "round_elapsed_s=",
        ] {
            assert!(output.contains(field), "{output}");
        }
    }

    #[test]
    fn round_log_reports_only_foreign_pending_proofs_after_closure() {
        for (account, best, expected) in [
            ([2; 32], Some(-4_999_000), Some("-4999")),
            ([2; 32], None, Some("none")),
            ([1; 32], None, None),
        ] {
            let mut log = RoundLog::new(Some(64), [1; 32]);
            let output = capture_logs(|| {
                log.our_best_milli = best;
                log.observe_pending(pending(account), -5_030_000);
            });
            assert!(output.is_empty(), "pending is not included: {output}");
            let output = capture_logs(|| {
                log.close();
                log.close();
            });
            if let Some(best) = expected {
                assert_eq!(
                    output.matches("qblock won by another miner").count(),
                    1,
                    "{output}"
                );
                for field in [
                    "qblock_id=65".to_owned(),
                    format!("winner=0x{}", "02".repeat(32)),
                    "proof_energy=-5038".to_owned(),
                    "target_energy=-5030".to_owned(),
                    format!("our_best_energy_this_round={best}"),
                    "winner_source=pending_pool".to_owned(),
                ] {
                    assert!(output.contains(&field), "{output}");
                }
                assert!(!output.contains("qblock won by us"), "{output}");
            } else {
                assert!(
                    output.is_empty(),
                    "our pending proof needs a receipt: {output}"
                );
            }
        }
    }

    #[test]
    fn round_log_resume_does_not_report_an_unincluded_proof() {
        let mut log = RoundLog::new(Some(64), [1; 32]);
        let output = capture_logs(|| {
            log.our_best_milli = Some(-4_999_000);
            log.observe_pending(pending([2; 32]), -5_030_000);
            log.resume();
            log.close();
        });
        assert!(output.is_empty(), "{output}");
    }

    #[test]
    fn round_log_late_own_receipt_prevents_a_foreign_report() {
        let mut log = RoundLog::new(Some(64), [1; 32]);
        log.submission_started();
        log.observe_pending(pending([2; 32]), -5_030_000);
        let output = capture_logs(|| log.close());
        assert!(
            output.is_empty(),
            "wait for our outstanding receipt: {output}"
        );
        let output = capture_logs(|| {
            log.submission_finished(Some(&won()), -5_038_000, -5_030_000);
            log.close();
        });
        assert_eq!(output.matches("qblock won by us").count(), 1, "{output}");
        assert!(!output.contains("another miner"), "{output}");
    }

    #[test]
    fn round_log_failed_submit_releases_the_foreign_report() {
        for receipt in [
            None,
            Some(SubmitReceipt::action_only(SubmitAction::Retry)),
            Some(SubmitReceipt::action_only(SubmitAction::StopRoundStale)),
            Some(SubmitReceipt::action_only(SubmitAction::StopFatal)),
        ] {
            let mut log = RoundLog::new(Some(64), [1; 32]);
            log.submission_started();
            log.observe_pending(pending([2; 32]), -5_030_000);
            let output = capture_logs(|| log.close());
            assert!(output.is_empty(), "{output}");
            let output = capture_logs(|| {
                log.submission_finished(receipt.as_ref(), -5_038_000, -5_030_000);
                log.close();
            });
            assert_eq!(
                output.matches("qblock won by another miner").count(),
                1,
                "{output}"
            );
            assert!(!output.contains("qblock won by us"), "{output}");
        }
    }

    #[test]
    fn round_log_does_not_invent_an_unobserved_winner() {
        let mut log = RoundLog::new(None, [1; 32]);
        let output = capture_logs(|| log.close());
        assert!(output.is_empty(), "{output}");
    }

    #[test]
    fn round_log_heartbeat_keeps_our_best_while_paused() {
        let mut log = RoundLog::new(Some(64), [1; 32]);
        let output = capture_logs(|| {
            log.our_best_milli = Some(-4_999_000);
            log.log_heartbeat(691, 22, 2, false);
            log.observe_pending(pending([2; 32]), -5_030_000);
            log.log_heartbeat(692, 23, 2, true);
        });
        assert!(output.contains("round_state=mining"), "{output}");
        assert!(
            output.contains("round_state=paused_awaiting_qblock"),
            "{output}"
        );
        assert_eq!(
            output.matches("our_best_energy_this_round=-4999").count(),
            2,
            "{output}"
        );
        assert!(!output.contains(" best_energy="), "{output}");
    }

    fn order(min_energy: Option<i64>, min_diversity: Option<u32>) -> JobOrder {
        JobOrder {
            order_id: 5u64.to_le_bytes().to_vec(),
            mode: crate::chain::scale_types::JobMode::Open,
            nodes: vec![0, 1],
            edges: vec![(0, 1)],
            h_milli: vec![0, 0],
            j_milli: vec![1000],
            min_energy_milli: min_energy,
            min_diversity_milli: min_diversity,
            min_solutions: None,
            deadline_ms: 0,
        }
    }

    /// The pallet keeps `energy == min_energy`; the local gate is strict.
    #[test]
    fn order_gates_admit_the_pallets_inclusive_energy_bound() {
        let gates = order_gates(&order(Some(-1000), None));
        assert_eq!(gates.min_energy_milli, -999);
        assert_eq!(gates.min_solutions, 1);
        assert_eq!(order_gates(&order(None, None)).min_energy_milli, i64::MAX);
    }

    /// One row scores zero diversity, so an order with a diversity gate and
    /// no `min_solutions` needs at least two rows to pass.
    #[test]
    fn order_gates_ask_for_two_rows_when_diversity_is_gated() {
        let gates = order_gates(&order(None, Some(300)));
        assert_eq!(gates.min_diversity_milli, 300);
        assert_eq!(gates.min_solutions, 2);
    }

    /// depth = ceil(ema * headroom), floor when that is smaller.
    #[test]
    fn adaptive_depth_is_the_ceiled_headroom_product_above_the_floor() {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "test fixture: exact integer headroom product"
        )]
        let headroom_10 = (10.0 * WINDOW_HEADROOM) as usize;
        let cases = [
            ("idle holds the floor", 0.0, 8, 8),
            ("idle with a zero floor", 0.0, 0, 0),
            ("grows with consumption", 10.0, 8, headroom_10),
            ("a many-core miner has no upper cap", 250.0, 8, 500),
            ("2.5 * 2.0 = 5.0 exactly", 2.5, 2, 5),
            ("0.8 ceils to 1, but floor 4 wins", 0.4, 4, 4),
        ];
        for (label, ema, floor, expected) in cases {
            assert_eq!(adaptive_depth(ema, floor), expected, "{label}");
        }
    }

    #[test]
    fn stage_ceiling_binds_only_on_large_topologies_and_never_below_the_floor() {
        // 2 nodes / 1 edge -> ~12 bytes/job -> the byte budget allows millions
        // of staged jobs, so the ceiling never clamps a real drain-driven depth.
        let cap = stage_ceiling(2, 1, 8);
        assert!(cap > 1_000_000, "dev topology must not be capped: {cap}");
        assert!(cap >= MAX_STAGE_BYTES_PER_MINER / 12);

        // Zephyr-scale: ~4577 nodes / 41515 edges -> ~184 KB/job. A fast miner
        // (ema 250 -> uncapped depth 500) is clamped to the byte budget.
        let cap = stage_ceiling(4577, 41515, 8);
        assert!(cap >= 8, "never below floor: {cap}");
        assert!(cap < 500, "large topology is memory-capped: {cap}");

        // Pathological topology where even one job exceeds the budget: the floor
        // still wins so the miner never idles for lack of a staged job.
        assert_eq!(stage_ceiling(usize::MAX / 8, 0, 8), 8);
    }
}
