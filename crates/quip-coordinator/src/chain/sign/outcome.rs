//! Why a submitted proof did not win: the qblock winner, and the failure
//! explanation the submit path logs.

use crate::chain::client::storage::parse_block_number;
use crate::chain::client::RealChainClient;
use crate::chain::codec::extrinsic::qblocks_storage_key;
use crate::chain::sign::submit::Confirmation;
use crate::chain::ChainError;
use crate::types::hex::{hex_decode, hex_encode};
use serde_json::Value;

impl RealChainClient {
    /// Which account won the qblock recorded at `block_hex`, if any?
    ///
    /// `QuantumPow::QBlocks` is keyed by block number and its value begins with
    /// the winning account, so this answers the question exactly. `None` means
    /// no proof was accepted in this block at all.
    pub(in crate::chain) async fn qblock_winner_at(
        &self,
        block_hex: &str,
    ) -> Result<Option<[u8; 32]>, ChainError> {
        let header = self
            .rpc_call(
                "chain_getHeader",
                Value::Array(vec![Value::String(block_hex.to_string())]),
            )
            .await?;
        let block_number = u32::try_from(parse_block_number(&header)?)
            .map_err(|_| ChainError::Decode("block number exceeds u32".into()))?;

        let key = qblocks_storage_key(block_number);
        let raw = self
            .rpc_call(
                "state_getStorage",
                Value::Array(vec![
                    Value::String(hex_encode(&key)),
                    Value::String(block_hex.to_string()),
                ]),
            )
            .await?;
        let Some(hex) = raw.as_str() else {
            return Ok(None);
        };
        let bytes = hex_decode(hex).map_err(ChainError::Decode)?;
        // `miner` is the first field of `QBlock`, so it occupies the leading 32
        // bytes. Reading only those survives the migrations that appended
        // fields to the end of the struct.
        let Some(miner) = bytes.get(..32) else {
            return Err(ChainError::Decode(format!(
                "QBlocks value is {} bytes, too short to hold an account",
                bytes.len()
            )));
        };
        let mut account = [0u8; 32];
        account.copy_from_slice(miner);
        Ok(Some(account))
    }

    /// Did this account win the qblock recorded at `block_hex`?
    ///
    /// Comparing `LastProofBlock` instead would report a false success whenever
    /// another miner won the same block.
    pub(in crate::chain) async fn proof_win_at(
        &self,
        block_hex: &str,
        our_account: &[u8; 32],
    ) -> Result<bool, ChainError> {
        Ok(self.qblock_winner_at(block_hex).await? == Some(*our_account))
    }

    /// Best-effort reason for a dispatch that was included but did not take
    /// effect.
    ///
    /// The pallet's own error variant lives in this block's `System.Events`,
    /// and decoding those needs runtime metadata this client does not carry.
    /// Replaying the extrinsic through `system_dryRun` cannot stand in for it:
    /// at the head the nonce is already spent, and at the parent block the
    /// replay omits whatever was ordered ahead of us inside the same block,
    /// which is the very thing that beats a proof. So this reads the storage
    /// the call competes for and reports what is actually there.
    ///
    /// The caller must treat this as log text only.
    pub(in crate::chain) async fn explain_failure(
        &self,
        block_hex: &str,
        confirmation: &Confirmation,
    ) -> (String, bool) {
        // Only a proof competes with other miners, so only a proof can lose a
        // race. Every other call fails on its own merits.
        if let Confirmation::ProofWin { account } = confirmation {
            return match self.qblock_winner_at(block_hex).await {
                Ok(winner) => explain_proof_failure(account, winner),
                Err(e) => (
                    format!("dispatch failed and the qblock winner could not be read: {e}"),
                    false,
                ),
            };
        }
        let text = match confirmation {
            // Handled above.
            Confirmation::ProofWin { .. } => unreachable!(),
            Confirmation::MinerRegistered { .. } => {
                "dispatch failed and QuantumPow.Miners still has no entry for this account; \
                 the deposit may be unaffordable"
                    .to_string()
            }
            Confirmation::Descriptor { .. } => {
                "dispatch failed and MinerRegistry.NodeDescriptors still has no entry for this \
                 account"
                    .to_string()
            }
            Confirmation::Participation { qblock_id, .. } => format!(
                "dispatch failed and this account is absent from the participant set for qblock \
                 {qblock_id}"
            ),
            Confirmation::DefaultTopologyIs { .. } => {
                "dispatch failed and QuantumPow.DefaultTopology is not the requested topology"
                    .to_string()
            }
            Confirmation::MotionOpen { .. } => {
                "dispatch failed and Foundation.Voting holds no vote for the motion".to_string()
            }
            Confirmation::Voted { .. } => {
                "dispatch failed and Foundation.Voting does not list this account as an aye"
                    .to_string()
            }
            Confirmation::SolverRegistered { .. } => {
                "dispatch failed and QuantumComputeMempool.Solvers has no entry with the requested \
                 solver type for this account"
                    .to_string()
            }
            Confirmation::SolverDeregistered { .. } => {
                "dispatch failed and QuantumComputeMempool.Solvers still holds this account"
                    .to_string()
            }
            Confirmation::SolutionAccepted { order_id, .. } => format!(
                "dispatch failed and OrderSolutions for order {order_id} holds no submission from \
                 this block; the order may have closed, or the solutions missed its gates"
            ),
        };
        (text, false)
    }
}

/// Turn the qblock winner recorded at a block into a reason and a race verdict.
///
/// Split from the RPC read so the verdict can be tested without a chain. The
/// returned flag is true only when the qblock went to somebody else, which is
/// the one failure that says nothing is wrong with our proof.
pub(in crate::chain) fn explain_proof_failure(
    our_account: &[u8; 32],
    winner: Option<[u8; 32]>,
) -> (String, bool) {
    match winner {
        Some(w) if w != *our_account => (
            format!(
                "lost the qblock to miner {}; this proof was valid but arrived behind theirs",
                hex_encode(&w)
            ),
            true,
        ),
        Some(_) => (
            "we hold the qblock but the win was not observed at this block; treat as a read \
             race, not a rejection"
                .to_string(),
            false,
        ),
        None => (
            "no qblock was recorded at this block, so the pallet rejected the proof itself; \
             check diversity, energy, and registration"
                .to_string(),
            false,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::hex::hex_encode;

    /// Losing the qblock to another miner is the one included-but-failed case
    /// that must not be reported as a rejection: the proof itself was sound.
    #[test]
    fn only_a_rival_winner_counts_as_a_lost_race() {
        let ours = [1u8; 32];
        let rival = [2u8; 32];

        let (text, lost_race) = explain_proof_failure(&ours, Some(rival));
        assert!(lost_race, "a rival winner is a lost race");
        assert!(
            text.contains(&hex_encode(&rival)),
            "the reason must name the winner so the loss can be verified on chain, got: {text}"
        );

        // No qblock at all means the pallet threw the proof out. That is a real
        // rejection and must stay distinguishable from a lost race.
        let (_, lost_race) = explain_proof_failure(&ours, None);
        assert!(!lost_race, "an empty qblock slot is a rejection");

        // Seeing our own account means the read raced the write, not a loss.
        let (_, lost_race) = explain_proof_failure(&ours, Some(ours));
        assert!(!lost_race, "winning it ourselves is not a lost race");
    }
}
