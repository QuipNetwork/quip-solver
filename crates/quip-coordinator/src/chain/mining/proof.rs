//! Submitting proofs and mempool solutions.

use crate::chain::client::RealChainClient;
use crate::chain::codec::extrinsic::{miner_identity_bytes, signer_account_bytes};
use crate::chain::codec::proof_encode::{build_quantum_proof, ProofBuildContext};
use crate::chain::codec::scale_types::{encode_submit_proof_call, encode_submit_solution_call};
use crate::chain::mining::family::witness_format;
use crate::chain::sign::submit::{classify_receipt, Confirmation, SignedCallOutcome};
use crate::chain::ChainError;
use crate::types::hex::hex_decode;
use crate::types::proof::{Proof, SubmitAction, SubmitReceipt};
use quantum_validation::AllowedValueSpec;

/// The receipt that drops a proof mined on a topology other than `current`,
/// or `None` when the proof matches it. The chain refuses such a proof, so
/// submitting it would only start a retry loop.
fn stale_receipt(proof: &Proof, current: &[u8]) -> Option<SubmitReceipt> {
    if proof.topology_hash == current {
        return None;
    }
    tracing::warn!(
        job = %crate::types::hex::hex_encode(&proof.job_id),
        mined_on = %crate::types::hex::hex_encode(&proof.topology_hash),
        current = %crate::types::hex::hex_encode(current),
        "proof was mined on a topology the chain no longer uses; dropping it"
    );
    Some(SubmitReceipt {
        action: SubmitAction::StopRoundStale,
        extrinsic_hash: None,
        block_hash: None,
        block_number: None,
    })
}

impl RealChainClient {
    /// Submit a proof; see `ChainClient::submit_proof`.
    pub(in crate::chain) async fn send_proof(
        &self,
        proof: &Proof,
    ) -> Result<SubmitReceipt, ChainError> {
        let pair = self.pair()?;
        let snap = self
            .last_snapshot
            .lock()
            .map_err(|e| ChainError::Unavailable(e.to_string()))?
            .clone()
            .ok_or_else(|| {
                ChainError::Unavailable(
                    "submit_proof requires a prior fetch_mining_snapshot".into(),
                )
            })?;
        if let Some(stale) = stale_receipt(proof, &snap.topology_hash) {
            return Ok(stale);
        }
        let spin_spec = self
            .last_spin_spec
            .lock()
            .map_err(|e| ChainError::Unavailable(e.to_string()))?
            .clone()
            .unwrap_or_else(|| AllowedValueSpec::Set(snap.allowed_spin_milli.clone()));

        let salt = extract_salt(proof).ok_or_else(|| {
            ChainError::Submit(
                "proof.salt must be 32 bytes for live submit_proof (session does not yet thread salt through Job)".into(),
            )
        })?;

        let mut topo = [0u8; 32];
        if snap.topology_hash.len() == 32 {
            topo.copy_from_slice(&snap.topology_hash);
        }

        let ctx = ProofBuildContext {
            topology_hash: topo,
            last_proof_block_hash: snap.last_proof_block_hash,
            miner_identity: miner_identity_bytes(&pair),
            salt,
            witness: witness_format(&snap, spin_spec),
        };
        let quantum = build_quantum_proof(proof, &ctx)
            .map_err(|e| ChainError::Submit(format!("encode proof: {e}")))?;
        let call = encode_submit_proof_call(&quantum);
        let account = signer_account_bytes(&pair);

        let (outcome, extrinsic_hash) = self
            .submit_signed_call(&call, Confirmation::ProofWin { account })
            .await?;
        match outcome {
            SignedCallOutcome::Success { block, number } => {
                tracing::info!(block = %block, number, "proof included and dispatched successfully");
                let block_hash = hex_decode(&block)
                    .ok()
                    .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok());
                Ok(SubmitReceipt {
                    action: SubmitAction::Success,
                    extrinsic_hash: Some(extrinsic_hash),
                    block_hash,
                    block_number: Some(number),
                })
            }
            SignedCallOutcome::DispatchFailed {
                error,
                lost_race: true,
            } => {
                tracing::info!(reason = %error, "proof included but another miner took the qblock");
                Ok(SubmitReceipt {
                    action: SubmitAction::StopRoundStale,
                    extrinsic_hash: Some(extrinsic_hash),
                    block_hash: None,
                    block_number: None,
                })
            }
            SignedCallOutcome::DispatchFailed { error, .. } => {
                tracing::warn!(error = %error, "proof included but ExtrinsicFailed");
                Ok(SubmitReceipt {
                    action: classify_receipt(Some(&error)),
                    extrinsic_hash: Some(extrinsic_hash),
                    block_hash: None,
                    block_number: None,
                })
            }
            SignedCallOutcome::Invalid { message } => {
                tracing::warn!(%message, "proof rejected as invalid before inclusion");
                Ok(SubmitReceipt {
                    action: classify_receipt(Some(&message)),
                    extrinsic_hash: Some(extrinsic_hash),
                    block_hash: None,
                    block_number: None,
                })
            }
            SignedCallOutcome::Dropped { message } => {
                tracing::warn!(%message, "proof dropped by node before inclusion");
                Ok(SubmitReceipt {
                    action: SubmitAction::Retry,
                    extrinsic_hash: Some(extrinsic_hash),
                    block_hash: None,
                    block_number: None,
                })
            }
        }
    }

    /// Answer a mempool order; see `ChainClient::submit_solution`.
    pub(in crate::chain) async fn send_solution(
        &self,
        proof: &Proof,
    ) -> Result<SubmitReceipt, ChainError> {
        let order_id = <[u8; 8]>::try_from(proof.order_id.as_slice())
            .map(u64::from_le_bytes)
            .map_err(|_| {
                ChainError::Submit(format!(
                    "mempool order id is {} bytes, expected 8",
                    proof.order_id.len()
                ))
            })?;
        // Proof rows contain validated +1/-1 i8 spins.
        let rows: Vec<Vec<i8>> = proof.solutions.iter().map(|r| r.spins.clone()).collect();
        let account = signer_account_bytes(&self.pair()?);
        let call = encode_submit_solution_call(order_id, &rows);
        let (outcome, extrinsic_hash) = self
            .submit_signed_call(&call, Confirmation::SolutionAccepted { order_id, account })
            .await?;
        let action = match &outcome {
            SignedCallOutcome::Success { block, number } => {
                tracing::info!(order_id, block = %block, number, rows = rows.len(), "mempool solution accepted");
                let block_hash = hex_decode(block)
                    .ok()
                    .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok());
                return Ok(SubmitReceipt {
                    action: SubmitAction::Success,
                    extrinsic_hash: Some(extrinsic_hash),
                    block_hash,
                    block_number: Some(*number),
                });
            }
            // The failure text explains state; it carries no pallet error name
            // to classify, and a rejected solution is not worth resending.
            SignedCallOutcome::DispatchFailed { error: reason, .. }
            | SignedCallOutcome::Invalid { message: reason } => {
                tracing::warn!(order_id, reason = %reason, "mempool solution rejected");
                SubmitAction::StopFatal
            }
            SignedCallOutcome::Dropped { message } => {
                tracing::warn!(order_id, reason = %message, "mempool solution dropped before inclusion");
                SubmitAction::Retry
            }
        };
        Ok(SubmitReceipt {
            action,
            extrinsic_hash: Some(extrinsic_hash),
            block_hash: None,
            block_number: None,
        })
    }
}

pub(in crate::chain) fn extract_salt(proof: &Proof) -> Option<[u8; 32]> {
    if proof.salt.len() != 32 {
        return None;
    }
    let mut s = [0u8; 32];
    s.copy_from_slice(&proof.salt);
    Some(s)
}

#[cfg(test)]
mod tests {
    use crate::chain::client::transport::{BoxStream, RpcTransport};
    use crate::chain::{ChainError, RealChainClient};
    use crate::types::miner::MinerKind;
    use crate::types::proof::{Proof, SubmitAction};
    use serde_json::Value as JsonValue;

    /// Fails the test on any RPC: a stale proof must never reach the wire.
    struct NoRpc;

    #[async_trait::async_trait]
    impl RpcTransport for NoRpc {
        async fn request(
            &self,
            _url: &str,
            _method: &str,
            _params: JsonValue,
        ) -> Result<JsonValue, ChainError> {
            panic!("no RPC expected");
        }

        async fn subscribe(
            &self,
            _url: &str,
            _sub: &str,
            _params: JsonValue,
            _unsub: &str,
        ) -> Result<BoxStream<'static, Result<JsonValue, ChainError>>, ChainError> {
            panic!("no RPC expected");
        }
    }

    #[tokio::test]
    async fn a_proof_mined_on_another_topology_is_dropped_as_stale() {
        let client = RealChainClient::with_transport(
            vec!["ws://scripted".to_string()],
            "//Alice".to_string(),
            MinerKind::Cpu,
            std::sync::Arc::new(NoRpc),
        );
        let snap = crate::types::snapshot::MiningSnapshot {
            head_hash: [0; 32],
            last_proof_block_hash: [0; 32],
            topology_hash: vec![1; 32],
            nodes: vec![0, 1],
            edges: vec![(0, 1)],
            allowed_h_milli: vec![0],
            allowed_j_milli: vec![-1000, 1000],
            allowed_spin_milli: vec![-1000, 1000],
            min_solutions: 1,
            max_energy_milli: 0,
            min_diversity_milli: 0,
            block_number: 1,
            spec_version: 100,
            target_proof_blocks: crate::types::snapshot::TARGET_PROOF_BLOCKS,
            family: None,
        };
        *client.last_snapshot.lock().unwrap() = Some(snap);
        let proof = Proof {
            job_id: vec![1],
            best_energy_milli: -1,
            diversity_milli: 0,
            n_valid: 1,
            solutions: vec![],
            is_pow: true,
            order_id: vec![],
            generation: 1,
            salt: vec![0; 32],
            device_access_time_us: 0,
            topology_hash: vec![2; 32],
        };
        let receipt = client
            .send_proof(&proof)
            .await
            .expect("dropped, not failed");
        assert_eq!(receipt.action, SubmitAction::StopRoundStale);
        assert_eq!(receipt.extrinsic_hash, None);
    }
}
