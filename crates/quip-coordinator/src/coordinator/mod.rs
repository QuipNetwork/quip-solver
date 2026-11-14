//! Solver logic: what work goes where. The coordinator reaches the outside
//! world only through the two traits here, which the environment supplies.

pub mod events;
pub mod feeder;
pub mod ledger;
/// `PoW` and mempool job construction helpers.
pub mod producer;
pub mod round;
pub mod router;
pub mod scoring;
pub mod settle;
pub mod state;

#[cfg(test)]
mod session_tests;

use crate::types::miner::MinerInfo;
use crate::types::order::JobOrder;
use crate::types::proof::{Proof, SubmitReceipt};
use crate::types::snapshot::{DecayParams, MiningSnapshot, PendingProof, Verdict};
use async_trait::async_trait;

/// Where the feeder's work comes from: the head it follows, the pool it
/// watches, and the readiness steps each round walks. Errors are the
/// environment's own message, which the feeder logs as is.
#[async_trait]
pub trait WorkSource: Send + Sync {
    /// The mining snapshot at the head. `Ok(None)` when there is none yet.
    async fn mining_snapshot(&self) -> Result<Option<MiningSnapshot>, String>;

    /// The mining account's on-chain record. `Ok(None)` when it has none.
    async fn miner_info(&self) -> Result<Option<MinerInfo>, String>;

    /// Proofs waiting to be included.
    async fn pending_proofs(&self) -> Result<Vec<PendingProof>, String>;

    /// Whether a pending proof ends the round of `snap`.
    fn judge(&self, pending: &PendingProof, snap: &MiningSnapshot) -> Verdict;

    /// The latest quantum-block id, which keys the attempt logs.
    async fn latest_qblock_id(&self) -> Result<Option<u64>, String>;

    /// The difficulty-decay inputs for `topology_hash`.
    async fn decay_params(&self, topology_hash: [u8; 32]) -> Result<Option<DecayParams>, String>;

    /// Open mempool orders this miner may answer.
    async fn mempool_orders(&self) -> Result<Vec<JobOrder>, String>;

    /// Wait until the source has caught up with its network. `Err` carries
    /// why that could not be confirmed; the round goes on regardless.
    async fn wait_synced(&self) -> Result<(), String>;

    /// Make sure the mining account can pay for the round.
    async fn ensure_funded(&self) -> Result<(), String>;

    /// Make sure the mining account is registered as a miner. `false` holds
    /// mining off.
    async fn ensure_miner_registered(&self) -> bool;

    /// File the node descriptor, at most once per process.
    async fn file_descriptor(&self);

    /// Declare participation in the round that `last_declared` does not
    /// already cover. Returns whether the declaration settled.
    async fn declare_participation(&self, last_declared: &mut Option<u64>) -> bool;

    /// Try once to register the mining account as a mempool solver.
    async fn ensure_solver_registered(&self);

    /// Whether the mining account is a registered mempool solver.
    fn solver_registered(&self) -> bool;
}

/// Where validated results go.
#[async_trait]
pub trait ResultSink: Send + Sync {
    /// Submit a proof-of-work proof.
    async fn submit_proof(&self, proof: &Proof) -> Result<SubmitReceipt, String>;

    /// Answer a mempool order.
    async fn submit_solution(&self, proof: &Proof) -> Result<SubmitReceipt, String>;
}
