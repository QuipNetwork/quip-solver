//! The production work behind a miner session: the coordinator's
//! [`SessionEvents`] implementation.

use crate::coordinator::router::MinerCaps;
use crate::coordinator::scoring::validate::validate_result;
use crate::coordinator::settle::{
    handle_lease_done, handle_lease_result, log_validation, settle_pow_result, st_metrics_stale,
    submit_mempool_result, PowOutcome,
};
use crate::coordinator::state::CoordinatorState;
use crate::coordinator::ResultSink;
use crate::miner::session::handlers::{Flow, SessionLink};
use crate::miner::SessionEvents;
use quip_proto::v1::{Calibrate, CalibrateDone, CalibrateOutcome, Capabilities};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

/// Info-log one miner liveness transition. Stale-round logs carry the
/// coordinator's current qblock for operators.
fn log_liveness(
    miner_id: &str,
    qblock: Option<u64>,
    event: &crate::miner::liveness::LivenessEvent,
) {
    use crate::miner::liveness::LivenessEvent;
    match event {
        LivenessEvent::EnteredPaused { reason } => tracing::info!(
            miner = %miner_id,
            reason = reason.as_deref().unwrap_or("unspecified"),
            "miner paused"
        ),
        LivenessEvent::Resumed => tracing::info!(miner = %miner_id, "miner resumed mining"),
        LivenessEvent::StaleRound { reported, current } => tracing::info!(
            miner = %miner_id,
            reported_generation = reported,
            current_generation = current,
            qblock = qblock.unwrap_or(0),
            "miner reports mining a stale round"
        ),
    }
}

/// The production session work: shared state, and where results go.
pub struct CoordinatorService<R: ResultSink + ?Sized + 'static> {
    /// Shared session state.
    pub state: Arc<Mutex<CoordinatorState>>,
    /// Where accepted proofs and mempool solutions go.
    pub sink: Arc<R>,
    /// Notifies waiters when a proof is submitted successfully.
    pub submit_notify: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

/// The production work behind a session: route jobs, validate results, and
/// settle proofs on the chain.
#[async_trait::async_trait]
impl<R: ResultSink + ?Sized + 'static> SessionEvents for CoordinatorService<R> {
    async fn joined(&self, miner_id: &str, caps: &Capabilities) {
        let miner_caps = MinerCaps::from_capabilities(caps);
        {
            let mut st = self.state.lock().await;
            st.router
                .register_miner(miner_id.to_owned(), miner_caps.clone());
        }
        tracing::info!(
            miner = %miner_id,
            backend = %miner_caps.backend,
            algorithm = %miner_caps.algorithm,
            max_nodes = miner_caps.max_nodes,
            max_edges = miner_caps.max_edges,
            stream_width = miner_caps.stream_width,
            "miner registered"
        );
    }

    // Ready says the session is established, not that the miner wants work. A
    // miner that has nothing to spend, such as a QPU with an exhausted budget,
    // sends no JobRequest, so it receives no jobs.
    async fn ready(&self, miner_id: &str) {
        tracing::debug!(miner = %miner_id, "miner ready; dispatch waits for its JobRequest");
    }

    async fn job_request(&self, miner_id: &str, credits: u32) -> Vec<quip_proto::v1::Job> {
        let mut st = self.state.lock().await;
        st.router.grant_credits(miner_id, credits);
        let mut jobs = Vec::new();
        while let Some(job) = st.router.next_job(miner_id) {
            st.dispatch_inflight(miner_id, job.clone());
            jobs.push(job);
        }
        tracing::debug!(
            miner = %miner_id,
            added = credits,
            dispatched = jobs.len(),
            staged = st.router.staged_len(miner_id),
            "dispatch drain"
        );
        st.metrics
            .record_contexts_dispatched(miner_id, jobs.len() as u64);
        jobs
    }

    async fn result(&self, link: &SessionLink, result: quip_proto::v1::Result) -> Flow {
        let (state, sink, submit_notify) = (&self.state, self.sink.as_ref(), &self.submit_notify);
        let miner_id = link.miner_id.as_str();
        let is_lease = state
            .lock()
            .await
            .inflight
            .get(&result.job_id)
            .is_some_and(crate::coordinator::scoring::lease::is_lease);
        if is_lease {
            handle_lease_result(sink, state, submit_notify, miner_id, result).await;
            return Flow::Continue;
        }
        let (job, salt, topo, best, gates, generation, round_log) = {
            let mut st = state.lock().await;
            st.metrics.record_result_received(miner_id);
            let job = st.complete_inflight(&result.job_id);
            if let Some(job) = &job {
                st.router.record_completion(miner_id);
                st.note_result(job.generation);
            } else {
                // The job id is unknown or already completed: either the
                // miner sent the same result twice, or the job was
                // cancelled when the round turned over.
                st.metrics.record_duplicate_result_drop(miner_id);
            }
            let salt = st.take_salt(&result.job_id);
            let topo = Arc::clone(&st.resolved_topo);
            let best = st.current_best_milli;
            let gates =
                crate::coordinator::scoring::validate::gates_from_target(st.target.as_ref());
            (
                job,
                salt,
                topo,
                best,
                gates,
                st.generation,
                st.round_log.clone(),
            )
        };
        let job = match job {
            // A mempool order is judged by its own gates and answered
            // with `submit_solution`. None of the `PoW` handling below
            // applies to it.
            Some(job) if job.provenance.as_ref().is_some_and(|p| !p.is_pow) => {
                submit_mempool_result(
                    sink,
                    state,
                    &job,
                    &result.solutions,
                    result.meta.as_ref().map_or(0, |m| m.device_access_time_us),
                )
                .await;
                None
            }
            other => other,
        };
        if let Some(job) = job {
            if job.generation < generation {
                st_metrics_stale(state, miner_id).await;
            }
            if let Some(ising) = job.ising.as_ref() {
                let started = tokio::time::Instant::now();
                let validated = validate_result(ising, &result.solutions, &gates, &topo);
                log_validation(miner_id, &result.job_id, started.elapsed());
                settle_pow_result(
                    sink,
                    state,
                    submit_notify,
                    PowOutcome {
                        miner_id,
                        job: &job,
                        job_id: &result.job_id,
                        salt,
                        validated,
                        gates,
                        current_best_milli: best,
                        round_log,
                        device_access_time_us: result
                            .meta
                            .as_ref()
                            .map_or(0, |m| m.device_access_time_us),
                    },
                )
                .await;
            }
        }
        Flow::Continue
    }

    async fn reject(&self, link: &SessionLink, rej: quip_proto::v1::Reject) -> Flow {
        let mut st = self.state.lock().await;
        // The rejecting miner grants its own replacement credit (see the
        // miner's reject path), so the coordinator only re-routes the
        // job to a capable miner; an unknown job_id needs nothing.
        if let Some(job) = st.complete_inflight(&rej.job_id) {
            st.router.record_reject(&link.miner_id);
            st.router.on_reject(&link.miner_id, job, rej.reason);
        }
        Flow::Continue
    }

    async fn lease_done(&self, link: &SessionLink, done: quip_proto::v1::LeaseDone) -> Flow {
        let mut st = self.state.lock().await;
        handle_lease_done(&mut st, &link.miner_id, &done);
        Flow::Continue
    }

    async fn status(&self, miner_id: &str, s: &quip_proto::v1::Status) {
        let mut st = self.state.lock().await;
        if s.abandoned_generation > 0 {
            st.last_abandoned_generation = s.abandoned_generation;
        }
        let generation = st.generation;
        let events = st
            .miner_liveness
            .observe(miner_id, &s.sampler_stats, generation);
        let qblock = st.qblock_id;
        drop(st);
        for ev in &events {
            log_liveness(miner_id, qblock, ev);
        }
    }

    async fn calibration(
        &self,
        miner_id: &str,
        caps: &Capabilities,
        topology_hash: &[u8],
    ) -> Option<Calibrate> {
        let mut st = self.state.lock().await;
        // The miner reconnected while its calibration was open. The new
        // process never saw that `Calibrate`, and calibration runs once per
        // miner id, so end it rather than hold the miner back until the timer.
        if st.calibration.as_ref().is_some_and(|p| p.pending(miner_id))
            && crate::coordinator::calibration::end(&mut st, miner_id)
        {
            tracing::warn!(
                miner = %miner_id,
                "miner reconnected during calibration; giving it jobs without one"
            );
            return None;
        }
        crate::coordinator::calibration::start_due(
            &self.state,
            &mut st,
            miner_id,
            &caps.features,
            topology_hash,
        )
    }

    async fn calibrate_done(&self, miner_id: &str, done: &CalibrateDone) {
        let ended = crate::coordinator::calibration::end(&mut *self.state.lock().await, miner_id);
        if !ended {
            tracing::warn!(
                miner = %miner_id,
                "miner sent CalibrateDone with no calibration outstanding; ignoring"
            );
            return;
        }
        let summary = done.summary.as_deref().unwrap_or("");
        match CalibrateOutcome::try_from(done.outcome) {
            Ok(CalibrateOutcome::Done) => tracing::info!(
                miner = %miner_id,
                outcome = "DONE",
                summary,
                "miner finished calibration"
            ),
            outcome => tracing::warn!(
                miner = %miner_id,
                outcome = %outcome.map_or_else(|_| format!("Unknown({})", done.outcome), |o| o.as_str_name().to_owned()),
                summary,
                "miner finished calibration without new tuning; it keeps its current tuning"
            ),
        }
    }

    async fn capabilities(&self, miner_id: &str, caps: &Capabilities) {
        let _ = self.state.lock().await.apply_capabilities(miner_id, caps);
    }

    // A miner that gives up says so here. Dropping this silently is how a
    // backend failure reads as an idle miner: the process stays alive, the
    // supervisor sees no exit, and the queue never drains.
    async fn fatal(&self, link: &SessionLink, f: &quip_proto::v1::Fatal) -> Flow {
        tracing::error!(
            miner = %link.miner_id,
            exit_code = f.exit_code,
            restart_required = f.restart_required,
            reason = %f.reason,
            "miner reported a fatal error"
        );
        Flow::Continue
    }

    async fn settled(&self, _link: &SessionLink) -> Flow {
        Flow::Continue
    }
}
