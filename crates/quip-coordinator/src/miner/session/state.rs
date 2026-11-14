//! The session registry: who may connect, and how to reach each live session.

use super::cancel_timing::CancelTiming;
use super::stream::coord;
use quip_proto::v1::{coord_msg, Configure, CoordMsg};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tonic::Status;

/// The session side of the coordinator state: who may connect, and how to
/// reach each live session.
#[derive(Default)]
pub struct SessionState {
    /// Per-miner expected session tokens (`miner_id` → hex token).
    pub expected_tokens: HashMap<String, String>,
    /// Per-miner `Configure` messages to send after `Welcome`.
    pub configure: HashMap<String, Configure>,
    /// Live per-miner outbound channels, so the supervisor can push an in-band
    /// `Shutdown`/cancel to a running session. Registered on handshake success,
    /// removed when the session ends.
    pub outbound: HashMap<String, mpsc::Sender<Result<CoordMsg, Status>>>,
    /// Live per-miner dispatcher wakeups. A miner grants its credits as soon as
    /// it starts, which is usually before the first chain snapshot arrives, so
    /// the grant drains an empty queue and the dispatcher parks. Staging alone
    /// does not wake it, and the miner sends no further `JobRequest` until a
    /// job completes. The feeder sends a zero-credit grant here after it stages
    /// work, which re-runs the drain without changing the credit balance.
    pub wakeups: HashMap<String, mpsc::Sender<u32>>,
    /// Live per-session `Cancel` timing, registered beside `outbound`.
    pub(crate) cancel_timings: HashMap<String, Arc<CancelTiming>>,
}

impl SessionState {
    /// Register a live session's outbound channel for supervisor-initiated sends.
    pub fn register_outbound(
        &mut self,
        miner_id: &str,
        tx: mpsc::Sender<Result<CoordMsg, Status>>,
    ) {
        let _ = self.outbound.insert(miner_id.to_string(), tx);
    }

    /// Register a live session's dispatcher wakeup channel.
    pub fn register_wakeup(&mut self, miner_id: &str, grants: mpsc::Sender<u32>) {
        let _ = self.wakeups.insert(miner_id.to_string(), grants);
    }

    /// Wake a miner's dispatcher so it drains newly staged work against credits
    /// the miner already granted.
    ///
    /// The send is non-blocking on purpose. A full channel already holds an
    /// unread wakeup, and one wakeup drains the whole queue, so dropping this
    /// one loses nothing. Blocking here would stall the feeder for every other
    /// miner.
    pub fn wake_dispatcher(&self, miner_id: &str) {
        if let Some(grants) = self.wakeups.get(miner_id) {
            let _ = grants.try_send(0);
        }
    }

    /// Drop a session's outbound channel when it ends.
    pub fn deregister_outbound(&mut self, miner_id: &str) {
        let _ = self.outbound.remove(miner_id);
        let _ = self.wakeups.remove(miner_id);
        let _ = self.cancel_timings.remove(miner_id);
    }
}

/// The state a session loop runs against: the session registry, and what a
/// joining miner must receive after `Configure`.
pub trait SessionHost: Send + 'static {
    /// The session registry.
    fn sessions(&self) -> &SessionState;
    /// The session registry, for registration.
    fn sessions_mut(&mut self) -> &mut SessionState;
    /// The `Topology` and `SetTarget` a miner joining now must receive, in
    /// send order. Empty before the first round.
    fn requirements(&self) -> Vec<coord_msg::Msg>;
}

/// Fans one message out to every live session. Clones the outbound senders
/// under the lock, drops it, then awaits the sends, so the fan-out never holds
/// the state lock across I/O.
#[async_trait::async_trait]
impl<S: SessionHost> crate::miner::MinerBroadcast for Arc<Mutex<S>> {
    async fn broadcast(&self, msg: coord_msg::Msg) -> usize {
        let senders: Vec<_> = {
            let st = self.lock().await;
            st.sessions().outbound.values().cloned().collect()
        };
        let n = senders.len();
        for tx in senders {
            let _ = tx.send(Ok(coord(msg.clone()))).await;
        }
        n
    }
}
