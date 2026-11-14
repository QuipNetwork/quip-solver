//! The per-miner staged queue, its credits, and the drop log.

use super::{FamilyCase, MinerCaps};
use quip_proto::v1::{Job, RejectReason};
use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(super) struct MinerQueue {
    pub(super) caps: MinerCaps,
    pub(super) staged: VecDeque<Job>,
    /// Consumable credit pool: each `JobRequest` adds credits, each dispatch
    /// spends one. The miner grants `width` up front and one per terminal
    /// event (Result or Reject), so dispatch stays 1:1 with completion and
    /// in-flight is bounded — never a cumulative ceiling that ack also frees
    /// (that double-count let in-flight grow without bound and deadlocked the
    /// miner's channels). In-flight itself is tracked by the coordinator's
    /// `inflight` map, so the router needs no separate outstanding counter.
    pub(super) granted_credits: u32,
    /// Jobs dispatched (drained from `staged`) since the last `take_consumed`.
    /// The feeder reads-and-resets this each poll to size the adaptive staging
    /// window from the miner's observed drain rate (see `feeder_loop`).
    pub(super) consumed_since_poll: u32,
    /// Lease salts the miner finished since the last `take_lease_salts`. The feeder turns this into a salts-per-second rate for lease sizing.
    pub(super) salts_since_poll: u64,
    /// Lease salts the miner finished for the current round. Reset when a new
    /// round starts, not when the miner re-registers.
    pub(super) round_salts: u64,
    /// Jobs this miner finished with a `Result` since it registered.
    /// The heartbeat reads this total. Polls do not reset it.
    pub(super) completed: u64,
    /// Jobs this miner answered with a `Reject` since it registered. Kept
    /// apart from `completed` so a miner that refuses everything does not
    /// read as one that mines.
    pub(super) rejected: u64,
    pub(super) unsupported_kinds: HashSet<i32>,
    /// Family cases this miner rejected. A capability snapshot clears them.
    pub(super) rejected_families: HashSet<FamilyCase>,
}

/// How often one miner's dropped-job total may reach the log.
///
/// A miner that rejects everything it is offered (a QPU with an exhausted
/// budget, say) drops one job per dispatch, and at credit speed that is
/// thousands of lines a second. The count is what an operator needs, not the
/// individual jobs.
pub(super) const DROP_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Dropped-job accounting for one miner, for the rate-limited log.
#[derive(Debug)]
pub(super) struct DropLog {
    /// Jobs dropped since the last line reached the log.
    pub(super) since_log: u64,
    /// When that line was emitted.
    pub(super) last_log: Instant,
}

pub(super) fn reject_reason_name(reason: i32) -> String {
    RejectReason::try_from(reason).map_or_else(
        |_| format!("Unknown({reason})"),
        |reason| format!("{reason:?}"),
    )
}
