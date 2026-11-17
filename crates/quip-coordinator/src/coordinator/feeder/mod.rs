//! The feeder: follow the head, walk each round's readiness steps, and keep
//! every miner's staged queue full. One file per job kind does the staging.

mod lease;
mod mempool;
mod pow;

use crate::coordinator::round::decay::{DecayAlgorithm, DecayModel, EnergyCurve};
use crate::coordinator::round::log::RoundLog;
use crate::coordinator::round::pool_watch::PoolObservation;
use crate::coordinator::round::{RoundEvent, RoundState};
use crate::coordinator::state::CoordinatorState;
use crate::coordinator::{ResultSink, WorkSource};
use crate::miner::session::stream::coord;
use crate::types::snapshot::MiningSnapshot;
use crate::types::topology::Topology;
use quip_proto::v1::{coord_msg, SetTarget};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Mutex};

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
    /// Consecutive failed submissions of one proof, inside one quantum block,
    /// before the coordinator stops retrying it.
    pub max_submit_attempts: u32,
    /// Live controller counters, for `heads_observed`.
    pub metrics: Arc<crate::observe::metrics::CoordinatorMetrics>,
}

/// Poll interval while the miners are stopped for a pending win. The block
/// that includes the proof lands within one block time, so a tight poll wins
/// most of the configured interval back. Never slower than `poll_interval`.
const AWAIT_QBLOCK_POLL: Duration = Duration::from_millis(250);

/// EMA smoothing for the per-miner consumption signal. Lower reacts slower but
/// steadier; 0.3 favors stability over reactivity for a ~1s poll, so a single
/// slow or fast poll doesn't swing the window.
const CONSUMPTION_EMA_ALPHA: f64 = 0.3;

/// How often the feeder emits its steady-state throughput line. The loop polls
/// once per `poll_interval` (1s in production), so narrating every poll at
/// `info` would bury everything else; the per-poll detail sits at `debug`.
const FEEDER_HEARTBEAT: Duration = Duration::from_mins(1);

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

/// Difficulty gates advertised to miners, from the snapshot.
fn target_from_snapshot(snap: &MiningSnapshot) -> SetTarget {
    SetTarget {
        max_proof_solutions: crate::types::proof::MAX_PROOF_SOLUTIONS_WIRE,
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
        st.sessions.outbound.values().cloned().collect()
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
        st.sessions.outbound.values().cloned().collect()
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
        st.sessions
            .outbound
            .iter()
            .map(|(id, tx)| (tx.clone(), st.sessions.cancel_timings.get(id).cloned()))
            .collect()
    };
    let n = senders.len();
    for (tx, timing) in senders {
        let _ = crate::miner::session::stream::send_cancel(&tx, timing.as_deref(), max_generation)
            .await;
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

/// Why the next qblock's requirements could not be loaded, as the readiness
/// walk reports it.
fn requirements_error(reason: &str) -> String {
    format!("cannot load next qblock requirements: {reason}")
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
async fn drive_to_mining<W: WorkSource + ?Sized>(
    mut machine: RoundState,
    work: &W,
    coord: &Arc<Mutex<CoordinatorState>>,
    params: &FeederParams,
    generation: u64,
    stop: &mut watch::Receiver<bool>,
) -> Option<(MiningSnapshot, usize, usize)> {
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
                let outcome =
                    await_round_step(machine, generation, &mut hold, stop, work.wait_synced())
                        .await?;
                match outcome {
                    Ok(()) => RoundEvent::Succeeded,
                    Err(reason) => {
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
                let result =
                    await_round_step(machine, generation, &mut hold, stop, work.ensure_funded())
                        .await?;
                match result {
                    Ok(()) => RoundEvent::Succeeded,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
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
                    work.ensure_miner_registered(),
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
                let result =
                    await_round_step(machine, generation, &mut hold, stop, work.mining_snapshot())
                        .await?;
                match result {
                    Ok(Some(s)) => {
                        snap = Some(s);
                        RoundEvent::Succeeded
                    }
                    Ok(None) => {
                        tracing::warn!(
                            error = %requirements_error("chain has no mining snapshot"),
                            generation,
                            "round readiness failed; holding off mining"
                        );
                        RoundEvent::Failed
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %requirements_error(&e),
                            generation,
                            "round readiness failed; holding off mining"
                        );
                        RoundEvent::Failed
                    }
                }
            }
            RoundState::DescriptorFiled => {
                work.file_descriptor().await;
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

/// Per-poll inputs every staging step shares.
struct Draw<'a> {
    /// The snapshot jobs are drawn from.
    snap: &'a MiningSnapshot,
    /// `PoW` nonce input: `blake2_256(SCALE(account))`.
    miner_identity: [u8; 32],
    /// The round the jobs belong to.
    generation: u64,
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
pub async fn feeder_loop<E: WorkSource + ResultSink + ?Sized>(
    env: Arc<E>,
    state: Arc<Mutex<CoordinatorState>>,
    params: FeederParams,
    mut stop: watch::Receiver<bool>,
) {
    let mut current_head: Option<[u8; 32]> = None;
    let mut last_block_number: u64 = 0;
    let mut generation: u64 = 0;
    let mut salt_ctr: u64 = 0;
    // Monotonic anchor for block-time estimation (win-time submission).
    let start = std::time::Instant::now();
    // Per-miner smoothed jobs-consumed-per-poll, driving the adaptive window.
    let mut consumption_ema: HashMap<String, f64> = HashMap::new();
    // Per-miner lease throughput, for lease sizing.
    let mut salt_rates: HashMap<String, crate::coordinator::scoring::lease::SaltRate> =
        HashMap::new();
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
    let mut round: Option<RoundState> = None;
    // Participation bookkeeping. `last_declared` is the candidate qblock the
    // chain has an answer for; `declared_generation` is the round that answer
    // was obtained in, so the qblock id is read once per mined round.
    let mut last_declared: Option<u64> = None;
    let mut declared_generation: Option<u64> = None;
    // Consecutive failed submits of one proof in one quantum block.
    let mut submit_ledger =
        crate::coordinator::ledger::SubmitLedger::new(params.max_submit_attempts);
    // Round of the last solver-registration attempt, and the block the open
    // mempool orders were last read at.
    let mut solver_attempted: Option<u64> = None;
    let mut orders_read_at: u64 = 0;
    // Pool scan memory and the wait for the block that includes a pending win.
    let mut pool_watch = crate::coordinator::round::pool_watch::PoolWatch::new();
    let mut round_log: Option<Arc<Mutex<RoundLog>>> = None;

    loop {
        let (snap, state_now) = match env.mining_snapshot().await {
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
                match env.miner_info().await {
                    Ok(info) => info,
                    Err(e) => {
                        tracing::debug!(error = %e, "feeder: miner info read failed");
                        params.metrics.chain().miner_info
                    }
                }
            } else {
                params.metrics.chain().miner_info
            };
            params
                .metrics
                .set_chain(crate::observe::metrics::ChainView {
                    head_hash: crate::types::hex::hex_encode(&snap.head_hash),
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
                match env.pending_proofs().await {
                    Ok(pending) => {
                        let clearing = pool_watch.scan(&pending, &snap, |p, s| env.judge(p, s));
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
                    env.as_ref(),
                    &state,
                    &params,
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
                let qblock_id = match env.latest_qblock_id().await {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::warn!(error = %e, "feeder: qblock id read failed; attempt logs lose the qblock key");
                        None
                    }
                };
                let decay = match <[u8; 32]>::try_from(snap.topology_hash.as_slice()) {
                    Ok(h) => match env.decay_params(h).await {
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
                    qblock_id = %crate::observe::logging::display_option(qblock_id.map(|id| id.saturating_add(1))),
                    block = snap.block_number,
                    spec_version = snap.spec_version,
                    decay_algorithm = DecayAlgorithm::for_spec_version(snap.spec_version).name(),
                    target_proof_blocks = snap.target_proof_blocks,
                    cancelled_jobs,
                    miners_told,
                    topology = %crate::types::hex::hex_encode(&snap.topology_hash),
                    nodes = snap.nodes.len(),
                    edges = snap.edges.len(),
                    max_energy = crate::observe::logging::energy_units(target.max_energy_milli),
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
                        crate::types::hex::hex_encode(&snap.last_proof_block_hash);
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
                        let settled = env.declare_participation(&mut last_declared).await;
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
                let settled = env.declare_participation(&mut last_declared).await;
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
                    max_energy = crate::observe::logging::energy_units(key.0),
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
            let draw = Draw {
                snap: &snap,
                miner_identity: params.miner_identity,
                generation,
            };
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
                        let _ = salt_rates.insert(
                            id.clone(),
                            crate::coordinator::scoring::lease::SaltRate::default(),
                        );
                        0.0
                    };
                    salts_per_s = rate;
                    lease::stage_leases(&mut st, &id, stream_width, rate, &draw, &mut salt_ctr)
                } else {
                    let depth = pow::window(ema, params.buffer_depth, &snap);
                    pow::stage_pow(&mut st, &id, depth, &draw, &mut salt_ctr);
                    depth
                };
                // Wake the dispatcher now that work is staged. A miner grants
                // its credits when it starts, which is normally before the
                // first snapshot arrives, so that grant drained an empty queue
                // and the dispatcher parked. Without this the miner waits for a
                // job and the coordinator waits for a request, forever.
                st.sessions.wake_dispatcher(&id);
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
            // the best stashed candidate whose projected viability block is the
            // block a proof sent now lands in (and still improves on the
            // current best), then submit it off-lock. `mark_submitted` + the
            // `beats_current` guard keep this from double-submitting or
            // regressing what the session path sent.
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
            // The chain re-runs its gates against the difficulty of the block
            // that includes the proof: it filters the proof to the rows under
            // that ceiling and scores diversity over only those. The live
            // target is the head's; a proof sent now lands at the head plus
            // `WIN_TIME_LEAD_BLOCKS` at the earliest, so replay the gates
            // against the projected ceiling of that block and leave a
            // candidate that does not clear them stashed for a later window
            // instead of spending a rejection on it.
            let live_gates =
                crate::coordinator::scoring::validate::gates_from_target(Some(&target));
            let win_time = st.stash.win_time_check(
                snap.block_number,
                current_block,
                best,
                &live_gates,
                crate::coordinator::round::stash::WIN_TIME_LEAD_BLOCKS,
            );
            let validated = st.results_validated;
            drop(st);

            // Mempool work rides alongside `PoW`. Solver registration is tried
            // once per round and never holds mining off. Orders are read once
            // per block after it succeeds: `submit_solution` refuses anyone else.
            if solver_attempted != Some(generation) {
                solver_attempted = Some(generation);
                env.ensure_solver_registered().await;
            }
            if env.solver_registered()
                && snap.block_number > orders_read_at
                && mempool::stage_mempool_orders(env.as_ref(), &state).await
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

            let due = win_time.and_then(|w| {
                if !w.check.accepted {
                    tracing::debug!(
                        job = %crate::types::hex::hex_encode(&w.candidate.job_id),
                        head = snap.block_number,
                        n_valid = w.check.n_valid,
                        diversity_milli = w.check.diversity_milli,
                        min_solutions = w.gates.min_solutions,
                        min_diversity_milli = w.gates.min_diversity_milli,
                        max_energy = %crate::observe::logging::energy_units(w.gates.min_energy_milli),
                        "win-time candidate does not clear the next block's difficulty; leaving stashed"
                    );
                }
                w.check.accepted.then_some(w.candidate)
            });

            if let Some(cand) = due {
                use crate::types::proof::SubmitAction;

                let proof = crate::types::proof::Proof {
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
                let job_hex = crate::types::hex::hex_encode(&cand.job_id);
                if let Some(log) = &round_log {
                    log.lock().await.submission_started();
                }
                let receipt = env.submit_proof(&proof).await;
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
                        let body = crate::observe::attempt::summary_body(
                            qblock_id,
                            st.current_best_milli,
                            st.results_validated,
                            st.stash.summary(),
                        );
                        if let Some(tx) = st.attempt_tx.as_ref() {
                            let _ = tx.send(crate::observe::attempt::WriterMsg::Summary {
                                qblock_id,
                                body,
                            });
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
