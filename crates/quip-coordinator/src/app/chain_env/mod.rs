//! The production environment: work comes from the chain head, results go
//! to the chain, and each round walks the readiness steps on chain.

pub mod readiness;

use crate::chain::sync::{wait_until_synced, SyncOutcome, SyncSource};
use crate::chain::ChainClient;
use crate::config::DescriptorParams;
use crate::coordinator::{ResultSink, WorkSource};
use crate::funding::{ensure_funded, BalanceSource, FundingParams, HttpFaucet};
use crate::types::miner::MinerInfo;
use crate::types::order::JobOrder;
use crate::types::proof::{Proof, SubmitReceipt};
use crate::types::snapshot::{DecayParams, MiningSnapshot, PendingProof, Verdict};
use async_trait::async_trait;
use readiness::{
    build_faucet, declare_round_participation, file_round_descriptor, register_round_miner,
    register_round_solver, ReadinessError,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What the chain environment needs besides the client.
pub struct ChainEnvParams {
    /// The signing `AccountId32`. Pays fees and keys the chain maps.
    pub miner_account: [u8; 32],
    /// Account-funding knobs reused on every round, not only at process start.
    pub funding: FundingParams,
    /// Node identity and miner list for `set_descriptor`.
    pub descriptor: DescriptorParams,
    /// Set after the first `DescriptorFiled` walk in this process.
    pub descriptor_filed: Arc<AtomicBool>,
    /// Set once `QuantumPow.Miners` is known to hold the signing account.
    pub miner_registered: Arc<AtomicBool>,
    /// Set once `QuantumComputeMempool.Solvers` holds the signing account with
    /// the configured type. Mempool orders are staged only after this.
    pub solver_registered: Arc<AtomicBool>,
}

/// The chain as the coordinator's [`WorkSource`] and [`ResultSink`].
pub struct ChainEnv<C> {
    chain: Arc<C>,
    params: ChainEnvParams,
    faucet: Option<HttpFaucet>,
}

impl<C> ChainEnv<C> {
    /// Wrap `chain`. Builds the faucet client from `params.funding`.
    #[must_use]
    pub fn new(chain: Arc<C>, params: ChainEnvParams) -> Self {
        let faucet = build_faucet(params.funding.faucet_url.as_deref());
        Self {
            chain,
            params,
            faucet,
        }
    }
}

#[async_trait]
impl<C: ChainClient + SyncSource + BalanceSource + 'static> WorkSource for ChainEnv<C> {
    async fn mining_snapshot(&self) -> Result<Option<MiningSnapshot>, String> {
        self.chain
            .fetch_mining_snapshot(None, self.params.miner_account, None)
            .await
            .map_err(|e| e.to_string())
    }

    async fn miner_info(&self) -> Result<Option<MinerInfo>, String> {
        self.chain
            .fetch_miner_info(self.params.miner_account)
            .await
            .map_err(|e| e.to_string())
    }

    async fn pending_proofs(&self) -> Result<Vec<PendingProof>, String> {
        self.chain
            .fetch_pending_proofs()
            .await
            .map_err(|e| e.to_string())
    }

    fn judge(&self, pending: &PendingProof, snap: &MiningSnapshot) -> Verdict {
        crate::chain::pool::judge_pending_proof(pending, snap)
    }

    async fn latest_qblock_id(&self) -> Result<Option<u64>, String> {
        self.chain
            .fetch_latest_qblock_id()
            .await
            .map_err(|e| e.to_string())
    }

    async fn decay_params(&self, topology_hash: [u8; 32]) -> Result<Option<DecayParams>, String> {
        self.chain
            .fetch_decay_params(topology_hash)
            .await
            .map_err(|e| e.to_string())
    }

    async fn mempool_orders(&self) -> Result<Vec<JobOrder>, String> {
        self.chain
            .fetch_mempool_orders(self.params.miner_account)
            .await
            .map_err(|e| e.to_string())
    }

    async fn wait_synced(&self) -> Result<(), String> {
        match wait_until_synced(self.chain.as_ref(), tokio::time::sleep).await {
            SyncOutcome::Synced => Ok(()),
            SyncOutcome::Unknown(reason) => Err(reason),
        }
    }

    async fn ensure_funded(&self) -> Result<(), String> {
        ensure_funded(
            self.chain.as_ref(),
            self.faucet.as_ref(),
            self.params.miner_account,
            &self.params.funding,
            tokio::time::sleep,
        )
        .await
        .map(|_| ())
        .map_err(|e| ReadinessError::Funding(e).to_string())
    }

    async fn ensure_miner_registered(&self) -> bool {
        register_round_miner(self.chain.as_ref(), self.params.miner_registered.as_ref()).await
    }

    async fn file_descriptor(&self) {
        file_round_descriptor(
            self.chain.as_ref(),
            self.params.descriptor_filed.as_ref(),
            &self.params.descriptor,
            self.params.miner_account,
        )
        .await;
    }

    async fn declare_participation(&self, last_declared: &mut Option<u64>) -> bool {
        declare_round_participation(
            self.chain.as_ref(),
            last_declared,
            self.params.miner_account,
        )
        .await
    }

    async fn ensure_solver_registered(&self) {
        let _ = register_round_solver(self.chain.as_ref(), self.params.solver_registered.as_ref())
            .await;
    }

    fn solver_registered(&self) -> bool {
        self.params.solver_registered.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl<C: ChainClient + 'static> ResultSink for ChainEnv<C> {
    async fn submit_proof(&self, proof: &Proof) -> Result<SubmitReceipt, String> {
        ResultSink::submit_proof(self.chain.as_ref(), proof).await
    }

    async fn submit_solution(&self, proof: &Proof) -> Result<SubmitReceipt, String> {
        ResultSink::submit_solution(self.chain.as_ref(), proof).await
    }
}

/// Any chain client takes results directly: submission needs no round state.
#[async_trait]
impl<C: ChainClient> ResultSink for C {
    async fn submit_proof(&self, proof: &Proof) -> Result<SubmitReceipt, String> {
        ChainClient::submit_proof(self, proof)
            .await
            .map_err(|e| e.to_string())
    }

    async fn submit_solution(&self, proof: &Proof) -> Result<SubmitReceipt, String> {
        ChainClient::submit_solution(self, proof)
            .await
            .map_err(|e| e.to_string())
    }
}
