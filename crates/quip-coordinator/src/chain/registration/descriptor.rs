//! Filing the node descriptor with `MinerRegistry.set_descriptor`.

use crate::chain::client::RealChainClient;
use crate::chain::codec::extrinsic::signer_account_bytes;
use crate::chain::codec::miner_registry::{encode_set_descriptor_call, NodeDescriptorV2Input};
use crate::chain::sign::submit::{
    classify_descriptor, Confirmation, DescriptorOutcome, SignedCallOutcome,
};
use crate::chain::ChainError;

impl RealChainClient {
    /// File the node descriptor; see `ChainClient::file_descriptor`.
    pub(in crate::chain) async fn send_descriptor(
        &self,
        descriptor: &NodeDescriptorV2Input,
    ) -> Result<DescriptorOutcome, ChainError> {
        let call = encode_set_descriptor_call(descriptor);
        let account = signer_account_bytes(&self.pair()?);
        let (outcome, _) = self
            .submit_signed_call(&call, Confirmation::Descriptor { account })
            .await?;
        match outcome {
            SignedCallOutcome::Success { .. } => Ok(DescriptorOutcome::Filed),
            SignedCallOutcome::DispatchFailed { error, .. }
            | SignedCallOutcome::Invalid { message: error } => {
                classify_descriptor(Some(&error)).ok_or(ChainError::Submit(error))
            }
            SignedCallOutcome::Dropped { message } => Err(ChainError::Submit(message)),
        }
    }
}
