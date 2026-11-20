//! Calibration: before a solver that lists the `calibrate` feature takes its
//! first job, the coordinator sends it recent winning qblocks to tune on, and
//! holds its jobs back until it answers or its deadline and grace run out.
//!
//! The plan is built once at startup. Each miner id is calibrated at most once
//! per coordinator process, across reconnects and restarts.

use crate::coordinator::state::CoordinatorState;
use crate::miner::session::stream::coord;
use crate::miner::CALIBRATE_FEATURE;
use crate::types::calibration::ReferenceWinner;
use quip_proto::v1::{coord_msg, Calibrate, CalibrationInstance, FamilyDescriptor};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// How long the coordinator waits past a calibration deadline before it
/// gives the miner jobs without a `CalibrateDone`.
pub const CALIBRATION_GRACE: Duration = Duration::from_secs(30);

/// The calibration every capable miner receives, and which miners have had it.
#[derive(Debug, Clone)]
pub struct CalibrationPlan {
    family: FamilyDescriptor,
    topology_hash: [u8; 32],
    instances: Vec<CalibrationInstance>,
    deadline_s: u64,
    /// Miner ids sent a `Calibrate` this process.
    sent: HashSet<String>,
    /// Miner ids whose calibration ended: answered, timed out, or abandoned.
    finished: HashSet<String>,
}

impl CalibrationPlan {
    /// A plan over `winners`, newest first, for the miners that mine
    /// `topology_hash` with `family`. Each miner gets `deadline_s` seconds.
    #[must_use]
    pub fn prepare(
        family: FamilyDescriptor,
        topology_hash: [u8; 32],
        winners: &[ReferenceWinner],
        deadline_s: u64,
    ) -> Self {
        let instances = winners
            .iter()
            .map(|w| CalibrationInstance {
                id: w.qblock_id.to_string(),
                nonce: w.nonce.to_vec(),
                achieved_energy_milli: w.energy_milli,
                max_energy_milli: w.max_energy_milli,
            })
            .collect();
        Self {
            family,
            topology_hash,
            instances,
            deadline_s,
            sent: HashSet::new(),
            finished: HashSet::new(),
        }
    }

    /// The `Calibrate` to send `miner_id` now, or `None`.
    ///
    /// `Some` only when the plan has instances, the miner lists the
    /// `calibrate` feature, its session topology is the plan's, and it was
    /// never sent one. Marks the miner as sent. The deadline is
    /// `now_ms + deadline_s * 1000`, in Unix milliseconds.
    pub fn due_for(
        &mut self,
        miner_id: &str,
        features: &[String],
        session_topology_hash: &[u8],
        now_ms: u64,
    ) -> Option<Calibrate> {
        let due = !self.instances.is_empty()
            && features.iter().any(|f| f == CALIBRATE_FEATURE)
            && session_topology_hash == self.topology_hash.as_slice()
            && !self.sent.contains(miner_id);
        if !due {
            return None;
        }
        let _ = self.sent.insert(miner_id.to_owned());
        Some(Calibrate {
            family: Some(self.family.clone()),
            instances: self.instances.clone(),
            deadline_ms: now_ms.saturating_add(self.deadline_s.saturating_mul(1000)),
        })
    }

    /// End the calibration of `miner_id`. True the first time only, and only
    /// for a miner that was sent one.
    pub fn finish(&mut self, miner_id: &str) -> bool {
        self.sent.contains(miner_id) && self.finished.insert(miner_id.to_owned())
    }

    /// Whether `miner_id` was sent a `Calibrate` that has not ended.
    #[must_use]
    pub fn pending(&self, miner_id: &str) -> bool {
        self.sent.contains(miner_id) && !self.finished.contains(miner_id)
    }

    /// Seconds each miner has to calibrate.
    #[must_use]
    pub const fn deadline_s(&self) -> u64 {
        self.deadline_s
    }

    /// The reference instances, newest first.
    #[must_use]
    pub fn instances(&self) -> &[CalibrationInstance] {
        &self.instances
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Start the calibration of `miner_id`, if one is due: mark it sent, hold its
/// jobs back in the router, and arm the deadline timer. Returns the message
/// to send. The caller holds the state lock and sends after releasing it.
pub(crate) fn start_due(
    state: &Arc<Mutex<CoordinatorState>>,
    st: &mut CoordinatorState,
    miner_id: &str,
    features: &[String],
    session_topology_hash: &[u8],
) -> Option<Calibrate> {
    let plan = st.calibration.as_mut()?;
    let calibrate = plan.due_for(miner_id, features, session_topology_hash, now_unix_ms())?;
    let deadline_s = plan.deadline_s();
    st.router.begin_calibration(miner_id);
    tracing::info!(
        miner = %miner_id,
        instances = calibrate.instances.len(),
        deadline_s,
        "calibration sent; the miner takes jobs after it answers"
    );
    spawn_deadline_timer(
        Arc::clone(state),
        miner_id.to_owned(),
        Duration::from_secs(deadline_s).saturating_add(CALIBRATION_GRACE),
    );
    Some(calibrate)
}

/// End the calibration of `miner_id` and wake its dispatcher, so its granted
/// credits drain the jobs staged meanwhile. True the first time only.
pub(crate) fn end(st: &mut CoordinatorState, miner_id: &str) -> bool {
    if !st.calibration.as_mut().is_some_and(|p| p.finish(miner_id)) {
        return false;
    }
    st.router.end_calibration(miner_id);
    st.sessions.wake_dispatcher(miner_id);
    true
}

/// After `after`, end a calibration the miner never answered. Harmless when
/// it already ended or the session is gone: [`end`] then does nothing, and a
/// missing dispatcher wakes nobody.
fn spawn_deadline_timer(state: Arc<Mutex<CoordinatorState>>, miner_id: String, after: Duration) {
    drop(tokio::spawn(async move {
        tokio::time::sleep(after).await;
        let mut st = state.lock().await;
        if end(&mut st, &miner_id) {
            tracing::warn!(
                miner = %miner_id,
                waited_s = after.as_secs(),
                "miner did not answer Calibrate by its deadline and grace; giving it jobs"
            );
        }
    }));
}

/// Send each live session the `Calibrate` now due for it on the topology
/// `topology_hash`. The feeder calls this right after a topology broadcast,
/// so a miner that joined before the first topology still calibrates, and
/// its `Calibrate` follows the `Topology` on the same channel.
pub(crate) async fn send_due(state: &Arc<Mutex<CoordinatorState>>, topology_hash: &[u8]) {
    let sends: Vec<_> = {
        let mut st = state.lock().await;
        if st.calibration.is_none() {
            return;
        }
        let mut ids: Vec<String> = st.sessions.outbound.keys().cloned().collect();
        ids.sort();
        let mut sends = Vec::new();
        for id in ids {
            let Some(features) = st.router.caps(&id).map(|c| c.features.clone()) else {
                continue;
            };
            let Some(tx) = st.sessions.outbound.get(&id).cloned() else {
                continue;
            };
            if let Some(calibrate) = start_due(state, &mut st, &id, &features, topology_hash) {
                sends.push((tx, calibrate));
            }
        }
        sends
    };
    for (tx, calibrate) in sends {
        let _ = tx
            .send(Ok(coord(coord_msg::Msg::Calibrate(calibrate))))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn winner(id: u64) -> ReferenceWinner {
        ReferenceWinner {
            qblock_id: id,
            miner: [1; 32],
            nonce: [u8::try_from(id).unwrap_or(0); 32],
            energy_milli: -5000,
            max_energy_milli: -4000,
            topology_hash: [7; 32],
            kind: None,
        }
    }

    fn features() -> Vec<String> {
        vec![CALIBRATE_FEATURE.to_owned()]
    }

    #[test]
    fn calibrate_feature_matches_the_solver_contract() {
        assert_eq!(CALIBRATE_FEATURE, quip_solver_core::CALIBRATE_FEATURE);
    }

    #[test]
    fn calibration_due_once_per_miner_on_the_plan_topology() {
        let mut plan =
            CalibrationPlan::prepare(FamilyDescriptor::default(), [7; 32], &[winner(9)], 2);
        assert!(plan.due_for("m", &[], &[7; 32], 0).is_none(), "no feature");
        assert!(
            plan.due_for("m", &features(), &[8; 32], 0).is_none(),
            "other topology"
        );
        let sent = plan.due_for("m", &features(), &[7; 32], 1000).expect("due");
        assert_eq!(sent.deadline_ms, 3000);
        assert_eq!(sent.instances.first().map(|i| i.id.as_str()), Some("9"));
        assert!(
            plan.due_for("m", &features(), &[7; 32], 0).is_none(),
            "once"
        );
        assert!(plan.pending("m"));
        assert!(plan.finish("m"));
        assert!(!plan.finish("m"), "finish is true the first time only");
        assert!(!plan.finish("never-sent"));
    }

    #[test]
    fn calibration_without_instances_is_never_due() {
        let mut plan = CalibrationPlan::prepare(FamilyDescriptor::default(), [7; 32], &[], 2);
        assert!(plan.due_for("m", &features(), &[7; 32], 0).is_none());
    }
}
