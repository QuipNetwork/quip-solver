//! Registering the signing account as a miner and as a mempool solver.

use crate::chain::client::RealChainClient;
use crate::chain::codec::extrinsic::{
    miners_storage_key, signer_account_bytes, solvers_storage_key,
};
use crate::chain::codec::quantum_compute_mempool::{
    encode_deregister_solver_call, encode_register_solver_call, SolverInfoScale,
};
use crate::chain::codec::quantum_pow::encode_register_miner_call;
use crate::chain::sign::submit::{
    classify_registration, Confirmation, RegistrationOutcome, SignedCallOutcome,
};
use crate::chain::ChainError;
use crate::types::miner::SolverType;
use serde_json::Value;

impl RealChainClient {
    /// Read the account's mempool solver type, reusing a successful registration read.
    pub(in crate::chain) async fn registered_solver_type(
        &self,
        account: [u8; 32],
        at_hex: &str,
    ) -> Result<Option<SolverType>, ChainError> {
        let cached = *self
            .registered_solver
            .lock()
            .map_err(|e| ChainError::Unavailable(e.to_string()))?;
        let cached_type = cached
            .filter(|(cached_account, _)| *cached_account == account)
            .map(|(_, solver_type)| solver_type);
        if cached_type.is_some() {
            return Ok(cached_type);
        }
        let stored = self
            .read_storage::<SolverInfoScale>(&solvers_storage_key(&account), at_hex)
            .await?;
        let solver_type = stored.map(|info| info.solver_type);
        *self
            .registered_solver
            .lock()
            .map_err(|e| ChainError::Unavailable(e.to_string()))? =
            solver_type.map(|kind| (account, kind));
        Ok(solver_type)
    }

    /// Is `account` already in `QuantumPow.Miners` at the current head?
    pub(in crate::chain) async fn miner_is_registered(
        &self,
        account: &[u8; 32],
    ) -> Result<bool, ChainError> {
        let head = self
            .rpc_call("chain_getBlockHash", Value::Array(vec![]))
            .await?;
        let at = head
            .as_str()
            .ok_or_else(|| ChainError::Decode("chain_getBlockHash not a string".into()))?;
        self.storage_value_present(&miners_storage_key(account), at)
            .await
    }

    /// Register the signing account as a miner; see `ChainClient::ensure_miner_registered`.
    pub(in crate::chain) async fn register_miner(&self) -> Result<RegistrationOutcome, ChainError> {
        let account = signer_account_bytes(&self.pair()?);
        if self.miner_is_registered(&account).await? {
            return Ok(RegistrationOutcome::AlreadyRegistered);
        }
        let call = encode_register_miner_call();
        let (outcome, _) = self
            .submit_signed_call(&call, Confirmation::MinerRegistered { account })
            .await?;
        match outcome {
            SignedCallOutcome::Success { .. } => Ok(RegistrationOutcome::Registered),
            SignedCallOutcome::DispatchFailed { error, .. }
            | SignedCallOutcome::Invalid { message: error } => {
                classify_registration(Some(&error)).ok_or(ChainError::Submit(error))
            }
            SignedCallOutcome::Dropped { message } => Err(ChainError::Submit(message)),
        }
    }

    /// Register the signing account as a mempool solver; see `ChainClient::ensure_solver_registered`.
    pub(in crate::chain) async fn register_solver(
        &self,
    ) -> Result<RegistrationOutcome, ChainError> {
        *self
            .registered_solver
            .lock()
            .map_err(|e| ChainError::Unavailable(e.to_string()))? = None;
        let account = signer_account_bytes(&self.pair()?);
        let want = SolverType::from(self.participate_kind);
        let head = self
            .rpc_call("chain_getBlockHash", Value::Array(vec![]))
            .await?;
        let at = head
            .as_str()
            .ok_or_else(|| ChainError::Decode("chain_getBlockHash not a string".into()))?;
        match self.registered_solver_type(account, at).await? {
            Some(registered) if registered == want => {
                return Ok(RegistrationOutcome::AlreadyRegistered);
            }
            Some(registered) => {
                tracing::info!(
                    registered = ?registered,
                    configured = ?want,
                    "mempool solver type changed; deregistering before re-registering"
                );
                *self
                    .registered_solver
                    .lock()
                    .map_err(|e| ChainError::Unavailable(e.to_string()))? = None;
                let (outcome, _) = self
                    .submit_signed_call(
                        &encode_deregister_solver_call(),
                        Confirmation::SolverDeregistered { account },
                    )
                    .await?;
                match outcome {
                    SignedCallOutcome::Success { .. } => {}
                    SignedCallOutcome::DispatchFailed { error, .. }
                    | SignedCallOutcome::Invalid { message: error }
                    | SignedCallOutcome::Dropped { message: error } => {
                        return Err(ChainError::Submit(format!("deregister_solver: {error}")));
                    }
                }
            }
            None => {}
        }
        let (outcome, _) = self
            .submit_signed_call(
                &encode_register_solver_call(want),
                Confirmation::SolverRegistered { account },
            )
            .await?;
        match outcome {
            SignedCallOutcome::Success { .. } => Ok(RegistrationOutcome::Registered),
            SignedCallOutcome::DispatchFailed { error, .. }
            | SignedCallOutcome::Invalid { message: error } => {
                classify_registration(Some(&error)).ok_or(ChainError::Submit(error))
            }
            SignedCallOutcome::Dropped { message } => Err(ChainError::Submit(message)),
        }
    }
}
