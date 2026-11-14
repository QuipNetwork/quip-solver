//! Extrinsic submit + hybrid sign (CONFIRM) and groundable receipt classification.

use crate::types::proof::SubmitAction;

/// Outcome of a `MinerRegistry.set_descriptor` submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptorOutcome {
    /// The descriptor landed (create or replace).
    Filed,
    /// The pallet rejected the payload. Do not retry.
    Rejected,
}

const DESCRIPTOR_REJECT: [&str; 20] = [
    "EmptyNodeId",
    "EmptyNodeName",
    "EmptyPublicHost",
    "EmptyRpcEndpoint",
    "EmptyMinerLabel",
    "EmptyMinerBackend",
    "EmptyMinerDeviceId",
    "EmptyOsSystem",
    "EmptyCpuBrand",
    "EmptyCpuArch",
    "EmptyGpuVendor",
    "EmptyGpuName",
    "InvalidGpuUtilization",
    "EmptyPythonVersion",
    "EmptyQuipVersion",
    "EmptyDockerImage",
    "NoMiners",
    "InvalidPort",
    "InsufficientBalance",
    "LiquidityRestrictions",
];

/// Classify a `set_descriptor` pallet error. `None` is a successful dispatch.
/// Unknown strings stay `None` so the caller can treat them as transient.
#[must_use]
pub fn classify_descriptor(error: Option<&str>) -> Option<DescriptorOutcome> {
    let Some(e) = error else {
        return Some(DescriptorOutcome::Filed);
    };
    if DESCRIPTOR_REJECT.iter().any(|s| e.contains(s)) {
        return Some(DescriptorOutcome::Rejected);
    }
    None
}

/// Outcome of a `MinerRegistry.participate` submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticipationOutcome {
    /// The declaration landed.
    Declared,
    /// This account already declared this qblock. Treat as success.
    AlreadyDeclared,
    /// The candidate qblock moved under the call.
    StaleQBlock,
    /// No descriptor is stored for this account.
    DescriptorMissing,
}

/// Classify a `participate` pallet error. `None` is a successful dispatch.
/// Unknown strings stay `None` so the caller can treat them as transient.
#[must_use]
pub fn classify_participation(error: Option<&str>) -> Option<ParticipationOutcome> {
    let Some(e) = error else {
        return Some(ParticipationOutcome::Declared);
    };
    if e.contains("DuplicateParticipation") {
        return Some(ParticipationOutcome::AlreadyDeclared);
    }
    if e.contains("InvalidQBlockId") {
        return Some(ParticipationOutcome::StaleQBlock);
    }
    if e.contains("DescriptorRequired") {
        return Some(ParticipationOutcome::DescriptorMissing);
    }
    None
}

/// Outcome of a `QuantumPow.register_miner` or
/// `QuantumComputeMempool.register_solver` submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationOutcome {
    /// The registration landed (for a miner, the deposit is reserved).
    Registered,
    /// This account was already registered. Treat as success.
    AlreadyRegistered,
}

/// Classify a `register_miner` or `register_solver` pallet error. `None` is a
/// successful dispatch.
///
/// Only the already-registered race is benign. Everything else — a deposit the
/// account cannot cover above all — stays `None` so the caller reports it and
/// retries rather than marking the miner ready to submit proofs it cannot land.
#[must_use]
pub fn classify_registration(error: Option<&str>) -> Option<RegistrationOutcome> {
    let Some(e) = error else {
        return Some(RegistrationOutcome::Registered);
    };
    if e.contains("MinerAlreadyRegistered") || e.contains("SolverAlreadyRegistered") {
        return Some(RegistrationOutcome::AlreadyRegistered);
    }
    None
}

/// Classify a pallet/dispatch error string into a fire-loop action.
///
/// Mirrors `substrate/submitter.py:_classify_receipt`. Unknown → fail loud.
#[must_use]
pub fn classify_receipt(error: Option<&str>) -> SubmitAction {
    const RETRY: [&str; 2] = ["InsufficientEnergy", "ProofLimitReached"];
    const STALE: [&str; 3] = ["InvalidNonce", "TopologyNotRegistered", "InvalidTopology"];
    const FATAL: [&str; 3] = [
        "InsufficientSolutions",
        "InsufficientDiversity",
        "MinerNotRegistered",
    ];
    let Some(e) = error else {
        return SubmitAction::Success;
    };
    if RETRY.iter().any(|s| e.contains(s)) {
        return SubmitAction::Retry;
    }
    if STALE.iter().any(|s| e.contains(s)) {
        return SubmitAction::StopRoundStale;
    }
    if FATAL.iter().any(|s| e.contains(s)) {
        return SubmitAction::StopFatal;
    }
    SubmitAction::StopFatal
}

use crate::chain::client::storage::{parse_block_number, parse_spec_version};
use crate::chain::client::watch::{parse_tx_status, TxStatus};
use crate::chain::client::RealChainClient;
use crate::chain::codec::extrinsic::{
    build_hybrid_signed_extrinsic, default_topology_storage_key, extrinsic_hash,
    foundation_voting_storage_key, load_hybrid_pair, miners_storage_key,
    node_descriptors_storage_key, order_solutions_storage_key, participants_by_qblock_storage_key,
    signer_account_bytes, solvers_storage_key, RuntimeTxContext, SignedExtensionContext,
};
use crate::chain::codec::quantum_compute_mempool::{JobSolutionScale, SolverInfoScale};
use crate::chain::ChainError;
use crate::types::hex::{hex_decode, hex_encode};
use crate::types::miner::SolverType;
use futures::StreamExt as _;
use quip_transaction_crypto::{account_id_from_public, HybridPair};
use serde_json::Value;
use sp_core::crypto::Ss58Codec;
use sp_core::Pair as _;

impl RealChainClient {
    pub(in crate::chain) fn pair(&self) -> Result<HybridPair, ChainError> {
        let mut guard = self
            .pair
            .lock()
            .map_err(|e| ChainError::Unavailable(e.to_string()))?;
        if guard.is_none() {
            let p = load_hybrid_pair(&self.signer_key)
                .map_err(|e| ChainError::Unavailable(format!("load signer: {e}")))?;
            *guard = Some(p);
        }
        // HybridPair is not Clone in all builds — re-load from key each time
        // if needed. Prefer re-load for simplicity.
        load_hybrid_pair(&self.signer_key)
            .map_err(|e| ChainError::Unavailable(format!("load signer: {e}")))
    }

    /// The account this client signs with.
    pub(crate) fn signer_account(&self) -> Result<[u8; 32], ChainError> {
        Ok(signer_account_bytes(&self.pair()?))
    }

    /// Read the genesis hash and the runtime versions from the validator.
    pub(in crate::chain) async fn fetch_runtime_tx_context(
        &self,
    ) -> Result<RuntimeTxContext, ChainError> {
        let genesis = self
            .rpc_call(
                "chain_getBlockHash",
                Value::Array(vec![Value::Number(0.into())]),
            )
            .await?;
        let genesis_hex = genesis
            .as_str()
            .ok_or_else(|| ChainError::Decode("genesis hash not a string".into()))?;
        let genesis_bytes = hex_decode(genesis_hex).map_err(ChainError::Decode)?;
        let mut genesis_hash = [0u8; 32];
        if genesis_bytes.len() != 32 {
            return Err(ChainError::Decode("genesis hash length".into()));
        }
        genesis_hash.copy_from_slice(&genesis_bytes);

        let rv = self
            .rpc_call("state_getRuntimeVersion", Value::Array(vec![]))
            .await?;
        let spec_version = parse_spec_version(&rv)?;
        let transaction_version = u32::try_from(
            rv.get("transactionVersion")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    ChainError::Decode("runtime transactionVersion missing/not a u64".into())
                })?,
        )
        .map_err(|_| ChainError::Decode("transactionVersion exceeds u32".into()))?;

        Ok(RuntimeTxContext {
            genesis_hash,
            spec_version,
            transaction_version,
        })
    }

    /// The runtime context, reused while it matches the last snapshot's
    /// `spec_version`. A runtime upgrade moves `spec_version` and forces a read.
    pub(in crate::chain) async fn runtime_tx_context(
        &self,
    ) -> Result<RuntimeTxContext, ChainError> {
        let current_spec = self
            .last_snapshot
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.spec_version));
        let cached = self.tx_context.lock().ok().and_then(|g| *g);
        if let (Some(ctx), Some(spec)) = (cached, current_spec) {
            if ctx.spec_version == spec {
                return Ok(ctx);
            }
        }
        let ctx = self.fetch_runtime_tx_context().await?;
        if let Ok(mut g) = self.tx_context.lock() {
            *g = Some(ctx);
        }
        Ok(ctx)
    }

    /// Nonce, genesis hash, and runtime versions for a signed extrinsic.
    pub(in crate::chain) async fn signed_extension_context(
        &self,
        pair: &HybridPair,
    ) -> Result<SignedExtensionContext, ChainError> {
        let account_ss58 = account_id_from_public(&pair.public()).to_ss58check();
        let nonce_val = self
            .rpc_call(
                "system_accountNextIndex",
                Value::Array(vec![Value::String(account_ss58)]),
            )
            .await?;
        let account_nonce = u32::try_from(
            nonce_val
                .as_u64()
                .ok_or_else(|| ChainError::Decode("system_accountNextIndex not a u64".into()))?,
        )
        .map_err(|_| ChainError::Decode("account nonce exceeds u32".into()))?;

        let runtime = self.fetch_runtime_tx_context().await?;
        Ok(SignedExtensionContext {
            account_nonce,
            genesis_hash: runtime.genesis_hash,
            spec_version: runtime.spec_version,
            transaction_version: runtime.transaction_version,
            tip: 0,
        })
    }

    /// Hybrid-sign `call`, submit it, and confirm the result from chain state.
    ///
    /// The status subscription reports pool and inclusion progress only. It
    /// cannot say whether the dispatch inside the block succeeded, because that
    /// lives in the block's events and decoding those needs runtime metadata.
    /// So each call names the storage entry its own success writes. Inclusion
    /// is confirmed from the block body. Both reads are metadata-free.
    pub(crate) async fn submit_signed_call(
        &self,
        call: &[u8],
        confirmation: Confirmation,
    ) -> Result<(SignedCallOutcome, [u8; 32]), ChainError> {
        let pair = self.pair()?;
        let signed_ctx = self.signed_extension_context(&pair).await?;
        let ext = build_hybrid_signed_extrinsic(&pair, call, &signed_ctx);
        let want_hash = extrinsic_hash(&ext);

        let url = self.primary_url()?.to_string();
        let mut stream = match self
            .transport
            .subscribe(
                &url,
                "author_submitAndWatchExtrinsic",
                Value::Array(vec![Value::String(hex_encode(&ext))]),
                "author_unwatchExtrinsic",
            )
            .await
        {
            Ok(s) => s,
            // A node that answers the submit with a rejection is delivering a
            // verdict about this extrinsic, not reporting a transport failure.
            // Resubmitting the same bytes cannot change it. Returning an error
            // here would let the caller read a permanent rejection as transient
            // and retry the same proof forever, which is how a lost win hides.
            Err(ChainError::Submit(message)) => {
                return Ok((SignedCallOutcome::Invalid { message }, want_hash))
            }
            Err(e) => return Err(e),
        };

        while let Some(item) = stream.next().await {
            let value = item?;
            match parse_tx_status(&value) {
                TxStatus::InBlock(block) | TxStatus::Finalized(block) => {
                    let outcome = self
                        .confirm_in_block(&block, &want_hash, confirmation)
                        .await?;
                    return Ok((outcome, want_hash));
                }
                TxStatus::Invalid(message) => {
                    return Ok((SignedCallOutcome::Invalid { message }, want_hash))
                }
                TxStatus::Dropped(message) => {
                    return Ok((SignedCallOutcome::Dropped { message }, want_hash))
                }
                TxStatus::Other(s) => {
                    tracing::debug!(status = %s, "unmodelled transaction status");
                }
                TxStatus::Ready | TxStatus::Broadcast | TxStatus::Future => {}
            }
        }
        Err(ChainError::Unavailable(
            "transaction status stream ended before inclusion".into(),
        ))
    }

    /// Read the block that claimed to include our extrinsic and decide.
    pub(in crate::chain) async fn confirm_in_block(
        &self,
        block_hex: &str,
        want_hash: &[u8; 32],
        confirmation: Confirmation,
    ) -> Result<SignedCallOutcome, ChainError> {
        let included = self.block_contains(block_hex, want_hash).await?;
        let confirmed = self.confirmation_present(block_hex, &confirmation).await?;
        match classify_state_outcome(included, confirmed) {
            StateOutcome::Won => {
                let header = self
                    .rpc_call(
                        "chain_getHeader",
                        Value::Array(vec![Value::String(block_hex.to_string())]),
                    )
                    .await?;
                Ok(SignedCallOutcome::Success {
                    block: block_hex.to_string(),
                    number: parse_block_number(&header)?,
                })
            }
            StateOutcome::IncludedButNotWon => {
                let (error, lost_race) = self.explain_failure(block_hex, &confirmation).await;
                Ok(SignedCallOutcome::DispatchFailed { error, lost_race })
            }
            StateOutcome::NotIncluded => Ok(SignedCallOutcome::Dropped {
                message: format!("extrinsic absent from block {block_hex}"),
            }),
        }
    }

    /// Is our extrinsic in this block?
    ///
    /// `chain_getBlock` is in the safe RPC set. Each extrinsic comes back as a
    /// hex blob, so inclusion is a hash comparison and needs no metadata.
    pub(in crate::chain) async fn block_contains(
        &self,
        block_hex: &str,
        want_hash: &[u8; 32],
    ) -> Result<bool, ChainError> {
        let body = self
            .rpc_call(
                "chain_getBlock",
                Value::Array(vec![Value::String(block_hex.to_string())]),
            )
            .await?;
        let Some(exts) = body
            .get("block")
            .and_then(|b| b.get("extrinsics"))
            .and_then(Value::as_array)
        else {
            return Err(ChainError::Decode(
                "chain_getBlock has no block.extrinsics array".into(),
            ));
        };
        for e in exts {
            let Some(hex) = e.as_str() else { continue };
            let bytes = hex_decode(hex).map_err(ChainError::Decode)?;
            if extrinsic_hash(&bytes) == *want_hash {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Is the storage entry this call should have written present at `block_hex`?
    pub(in crate::chain) async fn confirmation_present(
        &self,
        block_hex: &str,
        confirmation: &Confirmation,
    ) -> Result<bool, ChainError> {
        match *confirmation {
            Confirmation::ProofWin { account } => self.proof_win_at(block_hex, &account).await,
            Confirmation::Descriptor { account } => {
                self.storage_value_present(&node_descriptors_storage_key(&account), block_hex)
                    .await
            }
            Confirmation::MinerRegistered { account } => {
                self.storage_value_present(&miners_storage_key(&account), block_hex)
                    .await
            }
            Confirmation::Participation { qblock_id, account } => {
                self.storage_value_present(
                    &participants_by_qblock_storage_key(qblock_id, &account),
                    block_hex,
                )
                .await
            }
            Confirmation::DefaultTopologyIs { hash } => Ok(self
                .read_storage::<[u8; 32]>(&default_topology_storage_key(), block_hex)
                .await?
                == Some(hash)),
            Confirmation::MotionOpen { proposal } => {
                self.storage_value_present(&foundation_voting_storage_key(&proposal), block_hex)
                    .await
            }
            Confirmation::Voted { proposal, account } => Ok(self
                .foundation_votes(&proposal, block_hex)
                .await?
                .is_some_and(|votes| votes.ayes.contains(&account))),
            Confirmation::SolverRegistered { account } => {
                let stored = self
                    .read_storage::<SolverInfoScale>(&solvers_storage_key(&account), block_hex)
                    .await?;
                let registered = stored
                    .filter(|info| info.solver_type == SolverType::from(self.participate_kind))
                    .map(|info| (account, info.solver_type));
                *self
                    .registered_solver
                    .lock()
                    .map_err(|e| ChainError::Unavailable(e.to_string()))? = registered;
                Ok(registered.is_some())
            }
            Confirmation::SolverDeregistered { account } => Ok(!self
                .storage_value_present(&solvers_storage_key(&account), block_hex)
                .await?),
            Confirmation::SolutionAccepted { order_id, account } => {
                let header = self
                    .rpc_call(
                        "chain_getHeader",
                        Value::Array(vec![Value::String(block_hex.to_string())]),
                    )
                    .await?;
                let number = parse_block_number(&header)?;
                let stored = self
                    .read_storage::<JobSolutionScale>(
                        &order_solutions_storage_key(order_id, &account),
                        block_hex,
                    )
                    .await?;
                Ok(stored.is_some_and(|s| u64::from(s.submitted_at) == number))
            }
        }
    }
}

/// What chain state proves a submitted call succeeded.
///
/// The status subscription reports pool and inclusion progress only. It cannot
/// say whether the dispatch inside the block succeeded, because that lives in
/// the block's events and decoding those needs runtime metadata. So each call
/// names the storage entry its own success writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Confirmation {
    /// `QuantumPow.QBlocks[block_number].miner` equals this account.
    ProofWin { account: [u8; 32] },
    /// `MinerRegistry.NodeDescriptors[account]` is present.
    Descriptor { account: [u8; 32] },
    /// `QuantumPow.Miners[account]` is present.
    MinerRegistered { account: [u8; 32] },
    /// `MinerRegistry.ParticipantsByQBlock[qblock_id][account]` is present.
    Participation { qblock_id: u64, account: [u8; 32] },
    /// `QuantumPow.DefaultTopology` equals this hash. Presence alone is not
    /// enough on a chain that already had a default.
    DefaultTopologyIs { hash: [u8; 32] },
    /// `Foundation.Voting[proposal]` is present: the motion is open for votes.
    MotionOpen { proposal: [u8; 32] },
    /// `Foundation.Voting[proposal]` lists this account among the ayes.
    Voted {
        proposal: [u8; 32],
        account: [u8; 32],
    },
    /// `QuantumComputeMempool.Solvers[account]` holds the configured solver type.
    SolverRegistered { account: [u8; 32] },
    /// `QuantumComputeMempool.Solvers[account]` is absent.
    SolverDeregistered { account: [u8; 32] },
    /// `QuantumComputeMempool.OrderSolutions[order_id][account]` was written by
    /// this block. Presence alone is not enough: an earlier submission for the
    /// same order would already be there.
    SolutionAccepted { order_id: u64, account: [u8; 32] },
}

/// On-chain result of a hybrid-signed extrinsic, before pallet-specific classify.
pub(crate) enum SignedCallOutcome {
    /// Included and the dispatch succeeded.
    Success {
        /// `0x`-prefixed hash of the including block.
        block: String,
        /// Height of that block.
        number: u64,
    },
    /// Included but the dispatch failed.
    DispatchFailed {
        error: String,
        /// The call was sound and another miner simply took the qblock first.
        /// Distinguished so a lost race is not reported as a rejection.
        lost_race: bool,
    },
    /// The transaction pool rejected the extrinsic.
    Invalid { message: String },
    /// Dropped or errored before inclusion.
    Dropped { message: String },
}

/// What chain state says about a submitted extrinsic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::chain) enum StateOutcome {
    /// Our extrinsic is in the block and the expected storage write is present.
    Won,
    /// Our extrinsic is in the block but the expected storage write is absent.
    /// The dispatch failed, and state does not say why.
    IncludedButNotWon,
    /// Our extrinsic is not in the block.
    NotIncluded,
}

/// Fold the two state reads into an outcome.
///
/// The second argument is true when the storage entry this call should have
/// written is present. That is not the same as winning a quantum block. A
/// descriptor or a participation write never touches `QBlocks`. Confirmation
/// only counts when our own extrinsic is in the block. Another account's
/// extrinsic can write the same entry, and claiming that would mark a failed
/// call as successful.
pub(in crate::chain) const fn classify_state_outcome(
    included: bool,
    won_here: bool,
) -> StateOutcome {
    match (included, won_here) {
        (true, true) => StateOutcome::Won,
        (true, false) => StateOutcome::IncludedButNotWon,
        (false, _) => StateOutcome::NotIncluded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_pallet_errors() {
        assert!(matches!(classify_receipt(None), SubmitAction::Success));
        assert!(matches!(
            classify_receipt(Some("InsufficientEnergy")),
            SubmitAction::Retry
        ));
        assert!(matches!(
            classify_receipt(Some("ProofLimitReached")),
            SubmitAction::Retry
        ));
        assert!(matches!(
            classify_receipt(Some("InvalidNonce")),
            SubmitAction::StopRoundStale
        ));
        assert!(matches!(
            classify_receipt(Some("TopologyNotRegistered")),
            SubmitAction::StopRoundStale
        ));
        assert!(matches!(
            classify_receipt(Some("InsufficientSolutions")),
            SubmitAction::StopFatal
        ));
        assert!(matches!(
            classify_receipt(Some("SomethingUnknown")),
            SubmitAction::StopFatal
        ));
    }

    #[test]
    fn classifies_descriptor_errors() {
        assert_eq!(classify_descriptor(None), Some(DescriptorOutcome::Filed));
        assert_eq!(
            classify_descriptor(Some("MinerRegistry: EmptyNodeName")),
            Some(DescriptorOutcome::Rejected)
        );
        assert_eq!(
            classify_descriptor(Some("NoMiners")),
            Some(DescriptorOutcome::Rejected)
        );
        assert_eq!(
            classify_descriptor(Some("InsufficientBalance")),
            Some(DescriptorOutcome::Rejected)
        );
        assert_eq!(classify_descriptor(Some("SomethingUnknown")), None);
    }

    #[test]
    fn classifies_registration_errors() {
        assert_eq!(
            classify_registration(None),
            Some(RegistrationOutcome::Registered)
        );
        assert_eq!(
            classify_registration(Some("QuantumPow: MinerAlreadyRegistered")),
            Some(RegistrationOutcome::AlreadyRegistered)
        );
        // A deposit the account cannot reserve must not read as success.
        assert_eq!(classify_registration(Some("InsufficientBalance")), None);
        assert_eq!(classify_registration(Some("SomethingUnknown")), None);
    }

    #[test]
    fn classifies_participation_errors() {
        assert_eq!(
            classify_participation(None),
            Some(ParticipationOutcome::Declared)
        );
        assert_eq!(
            classify_participation(Some("MinerRegistry: DuplicateParticipation")),
            Some(ParticipationOutcome::AlreadyDeclared)
        );
        assert_eq!(
            classify_participation(Some("InvalidQBlockId")),
            Some(ParticipationOutcome::StaleQBlock)
        );
        assert_eq!(
            classify_participation(Some("DescriptorRequired")),
            Some(ParticipationOutcome::DescriptorMissing)
        );
        assert_eq!(classify_participation(Some("SomethingUnknown")), None);
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

    /// The submit path must not treat inclusion as success. A block that
    /// includes the extrinsic but does not advance `LastProofBlock` is a failed
    /// dispatch, and reporting it as success would mark a losing proof as won.
    #[test]
    fn inclusion_without_a_win_is_not_success() {
        for (label, included, won, expected) in [
            (
                "included, not won",
                true,
                false,
                StateOutcome::IncludedButNotWon,
            ),
            ("included and won", true, true, StateOutcome::Won),
            ("neither", false, false, StateOutcome::NotIncluded),
            // A win recorded while our extrinsic never made the block belongs
            // to another miner and must not be claimed.
            (
                "won without our extrinsic",
                false,
                true,
                StateOutcome::NotIncluded,
            ),
        ] {
            assert_eq!(classify_state_outcome(included, won), expected, "{label}");
        }
    }
}
