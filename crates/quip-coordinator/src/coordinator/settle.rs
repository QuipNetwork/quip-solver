//! Settle validated results: submit a winning proof, stash a near miss,
//! answer a mempool order, and record every attempt.

use crate::coordinator::scoring::validate::{beats_current, validate_result, ResolvedTopo};
use crate::coordinator::state::CoordinatorState;
use crate::coordinator::ResultSink;
use crate::types::order::MAX_ORDER_SOLUTIONS;
use crate::types::proof::{Proof, SubmitAction, SubmitReceipt};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};

/// Record a stale drop without holding the coordinator mutex across an await.
pub(crate) async fn st_metrics_stale(state: &Arc<Mutex<CoordinatorState>>, miner_id: &str) {
    let metrics = Arc::clone(&state.lock().await.metrics);
    metrics.record_stale_drop(miner_id);
}

/// Render the leading bytes of a job id the way the miner does.
///
/// Eight bytes plus `..` is enough to correlate a stash decision with the
/// miner's per-attempt line.
pub(crate) fn short_job_id(job_id: &[u8]) -> String {
    let mut s = String::with_capacity(18);
    for b in job_id.iter().take(8) {
        let _ = write!(s, "{b:02x}");
    }
    if job_id.len() > 8 {
        s.push_str("..");
    }
    s
}

/// One validated proof-of-work outcome to settle.
pub(crate) struct PowOutcome<'a> {
    /// Miner that produced the result.
    pub miner_id: &'a str,
    /// The dispatched job: a plain job, or the lease the winner came from.
    pub job: &'a quip_proto::v1::Job,
    /// Attempt identifier: the nonce for proof-of-work.
    pub job_id: &'a [u8],
    /// Salt the nonce was derived from, required by live submit.
    pub salt: Option<[u8; 32]>,
    /// Scored rows and gate verdict.
    pub validated: crate::types::proof::Validated,
    /// Gates the verdict used.
    pub gates: crate::coordinator::scoring::validate::QualityGates,
    /// Round best before this outcome.
    pub current_best_milli: Option<i64>,
    /// Round context captured with the job, before validation or submit can wait.
    pub round_log: Option<Arc<Mutex<crate::coordinator::round::log::RoundLog>>>,
    /// Device time the miner reported.
    pub device_access_time_us: u64,
}

/// Submit an accepted outcome that beats the round best, keep a transient
/// submit failure for the win-time retry, stash a sub-threshold outcome, and
/// record the attempt.
#[expect(
    clippy::too_many_lines,
    reason = "settling a proof preserves submit, stash, and attempt ordering"
)]
pub(crate) async fn settle_pow_result<R: ResultSink + ?Sized>(
    sink: &R,
    state: &Arc<Mutex<CoordinatorState>>,
    submit_notify: &Arc<Mutex<Option<oneshot::Sender<()>>>>,
    outcome: PowOutcome<'_>,
) {
    let PowOutcome {
        miner_id,
        job,
        job_id,
        salt,
        validated,
        gates,
        current_best_milli: best,
        round_log,
        device_access_time_us,
    } = outcome;
    let mut submitted = false;
    // Set when an accepted proof fails to submit for a
    // transient reason: keep it for the win-time retry loop
    // instead of dropping a genuine winner.
    let mut retain_for_retry = false;
    // Chain detail of the submit attempt, kept for the
    // attempt record below; `None` when this result never
    // reached the gate that triggers a submit.
    let mut receipt: Option<Result<SubmitReceipt, String>> = None;
    if validated.accepted && beats_current(validated.best_energy_milli, best) {
        let proof = Proof {
            job_id: job_id.to_vec(),
            best_energy_milli: validated.best_energy_milli,
            diversity_milli: validated.diversity_milli,
            n_valid: validated.n_valid,
            // Diverse gate-passing subset, capped at the
            // pallet's MAX_PROOF_SOLUTIONS (not all raw rows,
            // which would fail bounded-vec decode).
            solutions: validated.selected_solutions.clone(),
            is_pow: job.provenance.as_ref().is_some_and(|p| p.is_pow),
            order_id: job
                .provenance
                .as_ref()
                .map(|p| p.order_id.clone())
                .unwrap_or_default(),
            generation: job.generation,
            // Salt is chosen by the feeder when the PoW job
            // is derived and remembered by job_id; the live
            // RealChainClient submit requires 32 bytes.
            salt: salt.map_or_else(Vec::new, |s| s.to_vec()),
            device_access_time_us,
        };
        let job_hex = crate::types::hex::hex_encode(job_id);
        if let Some(log) = &round_log {
            log.lock().await.submission_started();
        }
        let submit_result = sink.submit_proof(&proof).await;
        if let Some(log) = &round_log {
            log.lock().await.submission_finished(
                submit_result.as_ref().ok(),
                validated.best_energy_milli,
                gates.min_energy_milli,
            );
        }
        match submit_result.as_ref().map(|r| r.action) {
            Ok(SubmitAction::Success) => {
                let mut st = state.lock().await;
                st.current_best_milli = Some(validated.best_energy_milli);
                st.metrics.record_proof_submitted(miner_id);
                if let Some(n) = submit_notify.lock().await.take() {
                    let _ = n.send(());
                }
                submitted = true;
            }
            Ok(SubmitAction::Retry) => {
                tracing::warn!(job = %job_hex, "session submit rejected (retryable); retaining accepted candidate for win-time retry");
                state.lock().await.metrics.record_submission_error(miner_id);
                retain_for_retry = true;
            }
            Ok(SubmitAction::StopRoundStale) => {
                tracing::info!(job = %job_hex, "session submit stale for round; dropping candidate");
                state.lock().await.metrics.record_stale_drop(miner_id);
            }
            Ok(SubmitAction::StopFatal) => {
                tracing::error!(job = %job_hex, "session submit fatally rejected by pallet; dropping candidate");
                state.lock().await.metrics.record_submission_error(miner_id);
            }
            Err(e) => {
                tracing::error!(job = %job_hex, error = %e, "session submit failed (transient); retaining accepted candidate for win-time retry");
                state.lock().await.metrics.record_submission_error(miner_id);
                retain_for_retry = true;
            }
        }
        receipt = Some(submit_result);
    }
    // Record the attempt; stash sub-threshold candidates so
    // the easing difficulty can still win them; refresh the
    // per-qblock summary when the stash or a submit changed.
    if let Some(log) = &round_log {
        log.lock()
            .await
            .observe_result(validated.raw_best_energy_milli);
    }
    {
        let mut st = state.lock().await;
        st.results_validated += 1;
        let qblock_id = st.qblock_id;
        let device_us = device_access_time_us;

        // A solution below the current threshold is submitted
        // immediately (above); one not yet viable is stashed
        // if the projection says the decay will clear it. An
        // accepted candidate whose submit failed transiently
        // (`retain_for_retry`) is also stashed so the win-time
        // loop resubmits it instead of losing a winner.
        let mut stash_changed = false;
        if !validated.accepted || retain_for_retry {
            let is_pow = job.provenance.as_ref().is_some_and(|p| p.is_pow);
            let order_id = job
                .provenance
                .as_ref()
                .map(|p| p.order_id.clone())
                .unwrap_or_default();
            stash_changed = st
                .stash
                .insert(crate::coordinator::round::stash::Candidate {
                    job_id: job_id.to_vec(),
                    salt,
                    generation: job.generation,
                    // Raw best (gate-agnostic) so the decay
                    // projection can decide when the easing gate
                    // admits it. The rows resubmitted with it are
                    // `stash_solutions`: the prefix-safe subset
                    // (≤ MAX_PROOF_SOLUTIONS) that keeps clearing
                    // the chain's diversity gate however tight the
                    // ceiling is when the proof lands.
                    best_energy_milli: validated.raw_best_energy_milli,
                    diversity_milli: validated.diversity_milli,
                    n_valid: validated.n_valid,
                    solutions: validated.stash_solutions.clone(),
                    is_pow,
                    order_id,
                    device_access_time_us: device_us,
                    submitted: false,
                });
            let decision = if stash_changed {
                "stashed"
            } else {
                "discarded"
            };
            let stash_txt = match st.stash.summary().retained_band_milli() {
                None => "empty".to_owned(),
                Some((worst, best)) => format!(
                    "{} -> {}",
                    crate::observe::logging::energy_units(worst),
                    crate::observe::logging::energy_units(best)
                ),
            };
            let target_txt = crate::observe::logging::display_energy(
                st.target.as_ref().map(|t| t.max_energy_milli),
            );
            let msg = format!(
                "[quip-solver-{miner_id}] attempt {}: {decision} (stash: {stash_txt}, target <= {target_txt})",
                short_job_id(job_id),
            );
            // A plain PoW job produces at most one Result per dispatched job,
            // so this stays at info. A lease Result now reports every salt a
            // miner chooses to, not only winners, so this fires once per
            // non-winning Result of a lease — at lease volume (one Result
            // per salt) an info line there would dominate the log, so lease
            // stays at debug. The stash/metrics counters below still record
            // every decision either way.
            if crate::coordinator::scoring::lease::is_lease(job) {
                tracing::debug!("{msg}");
            } else {
                tracing::info!("{msg}");
            }
        }

        let pow_sequence = submitted
            .then(|| st.metrics.chain().miner_info.map(|i| i.proofs_submitted))
            .flatten();
        let ctx = crate::observe::attempt::AttemptContext {
            miner_type: st.miner_types.get(miner_id).cloned().unwrap_or_default(),
            // The ceiling the coordinator checked this
            // result against, not the base difficulty: the
            // gate is what decided accept or reject.
            threshold_milli: gates.min_energy_milli,
            last_proof_block_hash: st.last_proof_block_hash.clone(),
            extrinsic_hash: receipt
                .as_ref()
                .and_then(|r| r.as_ref().ok())
                .and_then(|r| r.extrinsic_hash)
                .map(|h| crate::types::hex::hex_encode(&h)),
            chain_block_hash: receipt
                .as_ref()
                .and_then(|r| r.as_ref().ok())
                .and_then(|r| r.block_hash)
                .map(|h| crate::types::hex::hex_encode(&h)),
            chain_block_number: receipt
                .as_ref()
                .and_then(|r| r.as_ref().ok())
                .and_then(|r| r.block_number),
            pow_sequence,
            device_access_time_us: device_us,
        };
        let attempt = crate::observe::attempt::AttemptRecord::new(
            qblock_id, miner_id, job_id, job, &validated, submitted, &ctx,
        );
        let summary = (stash_changed || submitted).then(|| {
            crate::observe::attempt::summary_body(
                qblock_id,
                st.current_best_milli,
                st.results_validated,
                st.stash.summary(),
            )
        });
        if let Some(tx) = st.attempt_tx.as_ref() {
            let _ = tx.send(crate::observe::attempt::WriterMsg::Attempt(Box::new(
                attempt,
            )));
            if let Some(body) = summary {
                let _ = tx.send(crate::observe::attempt::WriterMsg::Summary { qblock_id, body });
            }
        }
    }
}

/// Verify one reported lease salt and settle it like a plain proof-of-work
/// result: submit it if it clears the target, else offer it to the stash.
///
/// The lease stays in flight: only `LeaseDone` completes it. A result for an
/// unknown or cancelled lease, a result that arrives before a target exists,
/// and a result that fails authenticity verification (a bad miner, not a
/// near miss) are dropped. A dropped result never marks the round mined;
/// `note_result` fires only after verification succeeds.
pub(crate) async fn handle_lease_result<R: ResultSink + ?Sized>(
    sink: &R,
    state: &Arc<Mutex<CoordinatorState>>,
    submit_notify: &Arc<Mutex<Option<oneshot::Sender<()>>>>,
    miner_id: &str,
    result: quip_proto::v1::Result,
) {
    let (job, view, target, best, round_log) = {
        let st = state.lock().await;
        st.metrics.record_result_received(miner_id);
        let job = st.inflight.get(&result.job_id).cloned();
        if job.is_none() {
            st.metrics.record_duplicate_result_drop(miner_id);
        }
        (
            job,
            st.lease_topology.clone(),
            st.target,
            st.current_best_milli,
            st.round_log.clone(),
        )
    };
    let Some(job) = job else { return };
    let (Some(generator), Some(view), Some(target)) = (job.generator.as_ref(), view, target) else {
        tracing::warn!(miner = %miner_id, "lease result before topology or target; dropping");
        return;
    };
    if result.solutions.is_empty() {
        // Not a protocol violation (a miner may report zero rows for a salt
        // it drew but chose not to send), so this is debug, not warn. It is
        // not participation either: the authenticity-only target accepts an
        // empty set, so without this check `note_result` would fire for it.
        tracing::debug!(miner = %miner_id, "lease result reports no solutions; dropping");
        return;
    }
    let started = tokio::time::Instant::now();
    let verification =
        crate::coordinator::scoring::lease::verify_and_select(generator, &view, &target, &result);
    log_validation(miner_id, &result.job_id, started.elapsed());
    let (verified, validated) = match verification {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(miner = %miner_id, error = %e, "lease result failed verification; dropping");
            return;
        }
    };
    state.lock().await.note_result(job.generation);
    settle_pow_result(
        sink,
        state,
        submit_notify,
        PowOutcome {
            miner_id,
            job: &job,
            job_id: &verified.nonce,
            salt: Some(verified.salt),
            validated,
            gates: crate::coordinator::scoring::validate::gates_from_target(Some(&target)),
            current_best_milli: best,
            round_log,
            device_access_time_us: result.meta.as_ref().map_or(0, |m| m.device_access_time_us),
        },
    )
    .await;
}

/// Complete a lease. Its salts count toward the miner's rate even when the
/// lease was cancelled, and a lease that finished salts in the current round
/// counts as mining, the same as a `Result`.
pub(crate) fn handle_lease_done(
    st: &mut CoordinatorState,
    miner_id: &str,
    done: &quip_proto::v1::LeaseDone,
) {
    st.router.record_lease_salts(miner_id, done.salts_done);
    let issued_at = st.lease_issued_at.get(&done.job_id).copied();
    if let Some(job) = st.complete_inflight(&done.job_id) {
        st.router.record_completion(miner_id);
        let counted_toward_round = job.generation == st.generation;
        if counted_toward_round {
            st.router.record_round_salts(miner_id, done.salts_done);
        }
        if done.salts_done > 0 {
            st.note_result(job.generation);
        }
        // `display_energy` handles the `i64::MAX` "no solution" sentinel that
        // a bare `energy_units` call would render as a nonsense whole-unit
        // figure, so it is used here instead.
        tracing::info!(
            miner = %miner_id,
            job_generation = job.generation,
            current_generation = st.generation,
            salts_done = done.salts_done,
            salt_count = job.generator.as_ref().map_or(0, |g| g.salt_count),
            best_energy = %crate::observe::logging::display_energy((done.best_energy_milli != i64::MAX).then_some(done.best_energy_milli)),
            elapsed_secs = issued_at.map(|t| t.elapsed().as_secs_f64()),
            counted_toward_round,
            "lease done"
        );
    } else {
        st.metrics.record_duplicate_result_drop(miner_id);
        tracing::debug!(
            miner = %miner_id,
            salts_done = done.salts_done,
            "lease done for unknown or already-completed job"
        );
    }
}

/// Report how long one result took to validate. Debug, because lease
/// results arrive once per salt.
pub(crate) fn log_validation(miner_id: &str, job_id: &[u8], elapsed: Duration) {
    tracing::debug!(
        miner = %miner_id,
        job = %short_job_id(job_id),
        validation_us = elapsed.as_micros(),
        "result validated"
    );
}

/// Validate a mempool result against its order's gates and answer the order
/// with `submit_solution`.
///
/// Mempool results stay out of the `PoW` round: they never move the best
/// energy, enter the win-time stash, or count as stale. A result that misses
/// the gates, or a solution the chain refuses, leaves the order answered. Only
/// a delivery failure forgets it, so the feeder stages it again while it is
/// still open.
pub(crate) async fn submit_mempool_result<R: ResultSink + ?Sized>(
    sink: &R,
    state: &Arc<Mutex<CoordinatorState>>,
    job: &quip_proto::v1::Job,
    solutions: &[quip_proto::v1::Solution],
    device_access_time_us: u64,
) {
    let order = crate::types::hex::hex_encode(&job.job_id);
    let gates = state.lock().await.mempool_orders.get(&job.job_id).copied();
    let (Some(ising), Some(gates)) = (job.ising.as_ref(), gates) else {
        tracing::warn!(order = %order, "mempool result has no problem or no staged order; dropping");
        return;
    };
    let validated = validate_result(ising, solutions, &gates, &ResolvedTopo::default());
    if !validated.accepted {
        tracing::info!(
            order = %order,
            n_valid = validated.n_valid,
            diversity_milli = validated.diversity_milli,
            min_solutions = gates.min_solutions,
            min_diversity_milli = gates.min_diversity_milli,
            "mempool result misses the order's gates; not submitting"
        );
        return;
    }
    let proof = Proof {
        job_id: job.job_id.clone(),
        best_energy_milli: validated.best_energy_milli,
        diversity_milli: validated.diversity_milli,
        n_valid: validated.n_valid,
        solutions: validated
            .selected_solutions
            .into_iter()
            .take(MAX_ORDER_SOLUTIONS)
            .collect(),
        is_pow: false,
        order_id: job
            .provenance
            .as_ref()
            .map(|p| p.order_id.clone())
            .unwrap_or_default(),
        generation: 0,
        salt: Vec::new(),
        device_access_time_us,
    };
    match sink.submit_solution(&proof).await.map(|r| r.action) {
        Ok(SubmitAction::Success) => tracing::info!(
            order = %order,
            best_energy_milli = proof.best_energy_milli,
            rows = proof.solutions.len(),
            "mempool solution submitted"
        ),
        Ok(SubmitAction::StopRoundStale | SubmitAction::StopFatal) => {
            tracing::warn!(order = %order, "mempool solution refused; not retrying this order");
        }
        Ok(SubmitAction::Retry) => {
            tracing::warn!(order = %order, "mempool solution not delivered; staging the order again");
            let _ = state.lock().await.mempool_orders.remove(&job.job_id);
        }
        Err(e) => {
            tracing::warn!(order = %order, error = %e, "mempool solution not delivered; staging the order again");
            let _ = state.lock().await.mempool_orders.remove(&job.job_id);
        }
    }
}
