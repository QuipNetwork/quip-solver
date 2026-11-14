//! Declaring participation in a quantum block.

use crate::chain::client::RealChainClient;
use crate::chain::codec::extrinsic::signer_account_bytes;
use crate::chain::codec::miner_registry::encode_participate_call;
use crate::chain::sign::submit::{
    classify_participation, Confirmation, ParticipationOutcome, SignedCallOutcome,
};
use crate::chain::ChainError;

impl RealChainClient {
    /// Declare participation; see `ChainClient::declare_participation`.
    pub(in crate::chain) async fn send_participation(
        &self,
        qblock_id: u64,
    ) -> Result<ParticipationOutcome, ChainError> {
        let call = encode_participate_call(qblock_id, self.participate_kind, None);
        let account = signer_account_bytes(&self.pair()?);
        let (outcome, _) = self
            .submit_signed_call(&call, Confirmation::Participation { qblock_id, account })
            .await?;
        match outcome {
            SignedCallOutcome::Success { .. } => Ok(ParticipationOutcome::Declared),
            SignedCallOutcome::DispatchFailed { error, .. }
            | SignedCallOutcome::Invalid { message: error } => {
                classify_participation(Some(&error)).ok_or(ChainError::Submit(error))
            }
            SignedCallOutcome::Dropped { message } => Err(ChainError::Submit(message)),
        }
    }
}
