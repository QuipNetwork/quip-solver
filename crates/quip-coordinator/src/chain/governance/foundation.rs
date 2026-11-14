//! Foundation motions for root-origin changes, and the chain reads they need:
//! members, votes, call weights, and the topology whitelist.
//!
//! Runtime 120 has no `Sudo` pallet. The calls go in one `Utility::batch_all`
//! that `ValidatorAdmission::dispatch_as_root` runs with the Root origin. That
//! call needs two thirds of the Foundation, so it is proposed as a Foundation
//! motion with threshold ⌈2N/3⌉ of N members. A threshold below 2, which is a
//! one-member Foundation such as the `dev` preset (`//Alice`), executes at
//! once. A larger Foundation votes: each member runs the same command, which
//! finds the open motion by its hash, votes aye, and closes it once the ayes
//! reach the threshold.

use crate::chain::client::RealChainClient;
use crate::chain::codec::extrinsic::{
    foundation_members_storage_key, foundation_voting_storage_key, mineable_topologies_prefix,
    registered_topologies_storage_key,
};
use crate::chain::codec::foundation::{FoundationVotesScale, WeightScale};
use crate::chain::governance::seed::SeedParams;
use crate::chain::governance::set_family::FamilyOutcome;
use crate::chain::ChainError;
use crate::types::hex::{hex_decode, hex_encode};
use parity_scale_codec::Decode;
use serde_json::Value;

use crate::chain::codec::foundation::{
    CLOSE_CALL_INDEX, FOUNDATION_PALLET_INDEX, PROPOSE_CALL_INDEX, VOTE_CALL_INDEX,
};
use crate::chain::codec::utility::{BATCH_ALL_CALL_INDEX, UTILITY_PALLET_INDEX};
use crate::chain::codec::validator_admission::{
    DISPATCH_AS_ROOT_CALL_INDEX, VALIDATOR_ADMISSION_PALLET_INDEX,
};
use crate::chain::sign::submit::{Confirmation, SignedCallOutcome};

use parity_scale_codec::{Compact, Encode};
use sp_crypto_hashing::blake2_256;

/// One Root call of the batch, with a label for the dry run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootCall {
    /// What the call does, as `Pallet.call(args)`.
    pub label: String,
    /// The SCALE-encoded `RuntimeCall`.
    pub bytes: Vec<u8>,
}

/// Ayes a Foundation motion needs for `dispatch_as_root`: two thirds of the
/// members, rounded up.
#[must_use]
pub fn foundation_threshold(members: usize) -> u32 {
    let members = u32::try_from(members).unwrap_or(u32::MAX);
    members.saturating_mul(2).saturating_add(2) / 3
}

/// `ValidatorAdmission.dispatch_as_root(Utility.batch_all(calls))`: the
/// proposal a Foundation motion carries.
///
/// A `Box<RuntimeCall>` encodes as the call itself, and `Vec<RuntimeCall>` as
/// a compact count followed by the calls.
#[must_use]
pub fn root_dispatch_call(calls: &[RootCall]) -> Vec<u8> {
    let mut root = vec![
        VALIDATOR_ADMISSION_PALLET_INDEX,
        DISPATCH_AS_ROOT_CALL_INDEX,
        UTILITY_PALLET_INDEX,
        BATCH_ALL_CALL_INDEX,
    ];
    let count = u32::try_from(calls.len()).unwrap_or(u32::MAX);
    Compact(count).encode_to(&mut root);
    for call in calls {
        root.extend_from_slice(&call.bytes);
    }
    root
}

/// The hash `Foundation.Voting` keys a proposal by: BLAKE2-256 of the call.
#[must_use]
pub fn proposal_hash(proposal: &[u8]) -> [u8; 32] {
    blake2_256(proposal)
}

/// `Foundation.propose(threshold, proposal, length_bound)`.
#[must_use]
pub fn foundation_propose_call(threshold: u32, proposal: &[u8]) -> Vec<u8> {
    let mut motion = vec![FOUNDATION_PALLET_INDEX, PROPOSE_CALL_INDEX];
    Compact(threshold).encode_to(&mut motion);
    motion.extend_from_slice(proposal);
    let length_bound = u32::try_from(proposal.len()).unwrap_or(u32::MAX);
    Compact(length_bound).encode_to(&mut motion);
    motion
}

/// `Foundation.vote(proposal, index, approve)`.
#[must_use]
pub fn foundation_vote_call(proposal: [u8; 32], index: u32, approve: bool) -> Vec<u8> {
    let mut call = vec![FOUNDATION_PALLET_INDEX, VOTE_CALL_INDEX];
    call.extend_from_slice(&proposal);
    Compact(index).encode_to(&mut call);
    approve.encode_to(&mut call);
    call
}

/// `Foundation.close(proposal, index, proposal_weight_bound, length_bound)`.
#[must_use]
pub fn foundation_close_call(
    proposal: [u8; 32],
    index: u32,
    weight_bound: WeightScale,
    length_bound: u32,
) -> Vec<u8> {
    let mut call = vec![FOUNDATION_PALLET_INDEX, CLOSE_CALL_INDEX];
    call.extend_from_slice(&proposal);
    Compact(index).encode_to(&mut call);
    weight_bound.encode_to(&mut call);
    Compact(length_bound).encode_to(&mut call);
    call
}

/// The open vote on `proposal`, which must exist.
pub(super) async fn open_votes(
    client: &RealChainClient,
    proposal: &[u8; 32],
) -> Result<FoundationVotesScale, ChainError> {
    client
        .foundation_votes(proposal, &client.head_hex().await?)
        .await?
        .ok_or_else(|| {
            ChainError::Submit(format!(
                "the motion {} closed before this member could vote; run the command again \
                 to see where the chain stands",
                hex_encode(proposal)
            ))
        })
}

/// Read `DefaultTopology` back at the head and report the switch.
///
/// The motion and the Root dispatch both succeed as extrinsics even when the
/// batch inside fails; only the events carry that result. The batch is
/// all-or-nothing and sets the default, so the stored default equal to the
/// local hash shows that every call landed.
pub(super) async fn applied(
    client: &RealChainClient,
    params: &SeedParams,
    topology_hash: [u8; 32],
    block: String,
) -> Result<FamilyOutcome, ChainError> {
    let stored = client.default_topology().await?;
    if stored != Some(topology_hash) {
        return Err(ChainError::Submit(format!(
            "the motion executed in {block}, but DefaultTopology is {} instead of {}; read \
             the Foundation.Executed and ValidatorAdmission.RootDispatched events of that \
             block for the inner error",
            stored.map_or_else(|| "unset".to_string(), |h| hex_encode(&h)),
            hex_encode(&topology_hash)
        )));
    }
    Ok(FamilyOutcome::Applied {
        topology_hash,
        nodes: params.topology.nodes.len(),
        edges: params.topology.edges.len(),
        block,
    })
}

/// Submit `call` and return the block that included it once `confirmation`
/// holds there.
pub(super) async fn dispatch(
    client: &RealChainClient,
    call: &[u8],
    what: &str,
    confirmation: Confirmation,
) -> Result<String, ChainError> {
    tracing::info!(call = what, "submitting Foundation call");
    let (outcome, _) = client.submit_signed_call(call, confirmation).await?;
    match outcome {
        SignedCallOutcome::Success { block, .. } => {
            tracing::info!(call = what, block = %block, "Foundation call included");
            Ok(block)
        }
        SignedCallOutcome::DispatchFailed { error, .. } => Err(ChainError::Submit(format!(
            "{what} was included but did not take effect: {error}; read the \
             Foundation and ValidatorAdmission events of that block for the inner error"
        ))),
        SignedCallOutcome::Invalid { message } => Err(ChainError::Submit(format!(
            "{what} was rejected by the transaction pool: {message}"
        ))),
        SignedCallOutcome::Dropped { message } => Err(ChainError::Submit(format!(
            "{what} was dropped before inclusion: {message}"
        ))),
    }
}

impl RealChainClient {
    /// Every key of `QuantumPow.MineableTopologies` at `at`.
    ///
    /// The pallet keeps at most the default and one other topology mineable,
    /// so one page holds the whole map; a full page means that invariant does
    /// not hold here and is reported rather than read past.
    pub(crate) async fn mineable_topologies(&self, at: &str) -> Result<Vec<[u8; 32]>, ChainError> {
        const PAGE: u32 = 64;
        let prefix = mineable_topologies_prefix();
        let page = self
            .rpc_call(
                "state_getKeysPaged",
                Value::Array(vec![
                    Value::String(hex_encode(&prefix)),
                    Value::Number(PAGE.into()),
                    Value::Null,
                    Value::String(at.to_string()),
                ]),
            )
            .await?;
        let keys = page
            .as_array()
            .ok_or_else(|| ChainError::Decode("state_getKeysPaged not an array".into()))?;
        if keys.len() >= PAGE as usize {
            return Err(ChainError::Decode(format!(
                "QuantumPow.MineableTopologies holds {} or more keys",
                keys.len()
            )));
        }
        keys.iter()
            .map(|k| {
                let bytes = k
                    .as_str()
                    .ok_or_else(|| ChainError::Decode("storage key not a string".into()))
                    .and_then(|hex| hex_decode(hex).map_err(ChainError::Decode))?;
                // prefix ++ blake2_128(hash) ++ hash
                bytes
                    .get(prefix.len() + 16..)
                    .and_then(|h| <[u8; 32]>::try_from(h).ok())
                    .ok_or_else(|| ChainError::Decode("mineable topology key length".into()))
            })
            .collect()
    }

    /// Is `hash` in `QuantumPow.RegisteredTopologies` at `at`?
    pub(crate) async fn topology_registered(
        &self,
        hash: &[u8; 32],
        at: &str,
    ) -> Result<bool, ChainError> {
        self.storage_value_present(&registered_topologies_storage_key(hash), at)
            .await
    }

    /// The Foundation member accounts at `at`.
    pub(crate) async fn foundation_members(&self, at: &str) -> Result<Vec<[u8; 32]>, ChainError> {
        Ok(self
            .read_storage::<Vec<[u8; 32]>>(&foundation_members_storage_key(), at)
            .await?
            .unwrap_or_default())
    }

    /// The open Foundation vote on `proposal_hash` at `at`, if any.
    pub(crate) async fn foundation_votes(
        &self,
        proposal_hash: &[u8; 32],
        at: &str,
    ) -> Result<Option<FoundationVotesScale>, ChainError> {
        self.read_storage::<FoundationVotesScale>(&foundation_voting_storage_key(proposal_hash), at)
            .await
    }

    /// The dispatch weight of `call`, from `TransactionPaymentCallApi`.
    ///
    /// `Foundation.close` needs at least the proposal's weight as its bound.
    pub(crate) async fn call_weight(&self, call: &[u8]) -> Result<WeightScale, ChainError> {
        let mut args = call.to_vec();
        u32::try_from(call.len())
            .map_err(|_| ChainError::Decode("call longer than u32".into()))?
            .encode_to(&mut args);
        let result = self
            .rpc_call(
                "state_call",
                Value::Array(vec![
                    Value::String("TransactionPaymentCallApi_query_call_info".into()),
                    Value::String(hex_encode(&args)),
                ]),
            )
            .await?;
        let hex = result
            .as_str()
            .ok_or_else(|| ChainError::Decode("query_call_info not a string".into()))?;
        let bytes = hex_decode(hex).map_err(ChainError::Decode)?;
        // RuntimeDispatchInfo { weight, class, partial_fee }: the weight leads.
        WeightScale::decode(&mut &bytes[..]).map_err(|e| ChainError::Decode(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two thirds of the members, rounded up; one member executes at once.
    #[test]
    fn the_threshold_is_two_thirds_rounded_up() {
        let got: Vec<u32> = (1..=7).map(foundation_threshold).collect();
        assert_eq!(got, [1, 2, 2, 3, 4, 4, 5]);
    }

    /// The motion is `propose(Compact(threshold), dispatch_as_root(batch_all(
    /// calls)), Compact(len))`, and the length bound is the inner call length.
    #[test]
    fn the_foundation_motion_wraps_the_batch_in_root_dispatch() {
        let call = |bytes: Vec<u8>| RootCall {
            label: String::new(),
            bytes,
        };
        let calls = vec![call(vec![10, 3, 0xaa]), call(vec![10, 6, 0xbb, 0xcc])];
        let root = root_dispatch_call(&calls);
        assert_eq!(
            root,
            [
                VALIDATOR_ADMISSION_PALLET_INDEX,
                DISPATCH_AS_ROOT_CALL_INDEX,
                UTILITY_PALLET_INDEX,
                BATCH_ALL_CALL_INDEX,
                0x08,
                10,
                3,
                0xaa,
                10,
                6,
                0xbb,
                0xcc,
            ]
        );
        let mut expected = vec![FOUNDATION_PALLET_INDEX, PROPOSE_CALL_INDEX, 0x0c];
        expected.extend_from_slice(&root);
        Compact(u32::try_from(root.len()).unwrap()).encode_to(&mut expected);
        assert_eq!(foundation_propose_call(3, &root), expected);
    }

    /// `vote(hash, Compact(index), approve)` and `close(hash, Compact(index),
    /// Weight { Compact, Compact }, Compact(len))`.
    #[test]
    fn vote_and_close_encode_the_collective_arguments() {
        let hash = [7u8; 32];
        let mut vote = vec![FOUNDATION_PALLET_INDEX, VOTE_CALL_INDEX];
        vote.extend_from_slice(&hash);
        vote.extend_from_slice(&[0x08, 0x01]);
        assert_eq!(foundation_vote_call(hash, 2, true), vote);

        let weight = WeightScale {
            ref_time: 1,
            proof_size: 2,
        };
        let mut close = vec![FOUNDATION_PALLET_INDEX, CLOSE_CALL_INDEX];
        close.extend_from_slice(&hash);
        close.extend_from_slice(&[0x08, 0x04, 0x08, 0x28]);
        assert_eq!(foundation_close_call(hash, 2, weight, 10), close);
    }
}
