//! The production chain client over Substrate JSON-RPC, and how it talks to
//! the node: the RPC transport, storage reads, extrinsic watch, and dry runs.
//! The client's operations live in the sibling modules by concern.
//!
//! Live end-to-end verification requires a running node. Offline unit tests
//! cover SCALE encode/decode, hybrid extrinsic assembly, and proof packing.
//! Methods return transport errors when no validator is reachable.

pub mod dryrun;
pub mod jsonrpsee;
pub mod storage;
pub mod transport;
pub mod watch;

use crate::chain::client::jsonrpsee::JsonrpseeTransport;
use crate::chain::client::transport::RpcTransport;
use crate::chain::codec::extrinsic::RuntimeTxContext;
use crate::chain::codec::miner_registry::NodeDescriptorV2Input;
use crate::chain::codec::quantum_pow::TopologyMetaScale;
use crate::chain::sign::submit::{DescriptorOutcome, ParticipationOutcome, RegistrationOutcome};
use crate::chain::{ChainClient, ChainError};
use crate::types::miner::{MinerInfo, MinerKind, SolverType};
use crate::types::order::JobOrder;
use crate::types::proof::{Proof, SubmitReceipt};
use crate::types::snapshot::{DecayParams, MiningSnapshot, PendingProof};
use async_trait::async_trait;
use quantum_validation::AllowedValueSpec;
use quip_transaction_crypto::HybridPair;
use std::sync::{Arc, Mutex};

/// A registered topology and the hash it was fetched under.
pub(in crate::chain) struct CachedTopology {
    pub(in crate::chain) hash: [u8; 32],
    pub(in crate::chain) meta: TopologyMetaScale,
}

/// Production chain client (RPC + hybrid-signed submit).
pub struct RealChainClient {
    /// Validator WebSocket / HTTP RPC URLs (primary first).
    pub validators: Vec<String>,
    /// Hybrid keystore path, any substrate secret URI, or 32-byte hex seed.
    pub signer_key: String,
    /// Cached hybrid pair (loaded lazily from `signer_key`).
    pub(in crate::chain) pair: Mutex<Option<HybridPair>>,
    /// Last fetched mining snapshot — used by `submit_proof` to pack spins
    /// and derive the nonce. Updated by `fetch_mining_snapshot`.
    pub(in crate::chain) last_snapshot: Mutex<Option<MiningSnapshot>>,
    /// Last `allowed_spin` spec (full `AllowedValueSpec`, not just Set values).
    pub(in crate::chain) last_spin_spec: Mutex<Option<AllowedValueSpec<Vec<i32>>>>,
    /// The registered topology the last snapshot was built from, with its
    /// hash. A topology never changes under its hash, so a poll on the same
    /// hash reuses it and only the difficulty and root are read again. One
    /// slot: a poll on another hash downloads that topology and replaces it.
    pub(in crate::chain) topology: Mutex<Option<CachedTopology>>,
    /// Genesis hash and runtime versions that pool signatures commit to.
    /// Reused while it matches the last snapshot's `spec_version`.
    pub(in crate::chain) tx_context: Mutex<Option<RuntimeTxContext>>,
    /// How this client reaches the validator. Boxed so a WebAssembly build can
    /// supply a browser transport in place of the native one.
    pub(in crate::chain) transport: Arc<dyn RpcTransport>,
    /// Whether the last RPC round-trip reached the validator. `None` before the
    /// first call. Every RPC builds a fresh client (see `rpc_http` / `rpc_ws`),
    /// so a per-call "connected" line would run to thousands an hour; this
    /// tracks reachability so only the *transitions* are reported.
    pub(in crate::chain) reachable: Mutex<Option<bool>>,
    /// Kind declared on `participate`. Derived from the miners this process starts.
    pub(in crate::chain) participate_kind: MinerKind,
    /// The last family topology this client refused, so the refusal is logged
    /// once per topology rather than on every poll.
    pub(in crate::chain) refused_family: Mutex<Option<[u8; 32]>>,
    /// Registered mempool type, scoped to the account that was read.
    pub(in crate::chain) registered_solver: Mutex<Option<([u8; 32], SolverType)>>,
}

impl RealChainClient {
    /// Construct a client over the given validators and signer material.
    #[must_use]
    pub fn new(validators: Vec<String>, signer_key: String, participate_kind: MinerKind) -> Self {
        Self {
            validators,
            signer_key,
            pair: Mutex::new(None),
            last_snapshot: Mutex::new(None),
            last_spin_spec: Mutex::new(None),
            topology: Mutex::new(None),
            tx_context: Mutex::new(None),
            transport: Arc::new(JsonrpseeTransport::default()),
            reachable: Mutex::new(None),
            participate_kind,
            registered_solver: Mutex::new(None),
            refused_family: Mutex::new(None),
        }
    }

    /// Construct a client over a caller-supplied transport.
    #[must_use]
    pub fn with_transport(
        validators: Vec<String>,
        signer_key: String,
        participate_kind: MinerKind,
        transport: Arc<dyn RpcTransport>,
    ) -> Self {
        let mut c = Self::new(validators, signer_key, participate_kind);
        c.transport = transport;
        c
    }
}

#[async_trait]
impl ChainClient for RealChainClient {
    async fn ensure_miner_registered(&self) -> Result<RegistrationOutcome, ChainError> {
        self.register_miner().await
    }

    async fn ensure_solver_registered(&self) -> Result<RegistrationOutcome, ChainError> {
        self.register_solver().await
    }

    async fn file_descriptor(
        &self,
        descriptor: &NodeDescriptorV2Input,
    ) -> Result<DescriptorOutcome, ChainError> {
        self.send_descriptor(descriptor).await
    }

    async fn fetch_mining_snapshot(
        &self,
        at: Option<[u8; 32]>,
        _miner_account: [u8; 32],
        topology_hash: Option<[u8; 32]>,
    ) -> Result<Option<MiningSnapshot>, ChainError> {
        self.read_mining_snapshot(at, topology_hash).await
    }

    async fn fetch_latest_qblock_id(&self) -> Result<Option<u64>, ChainError> {
        self.read_latest_qblock_id().await
    }

    async fn fetch_miner_info(&self, account: [u8; 32]) -> Result<Option<MinerInfo>, ChainError> {
        self.read_miner_info(account).await
    }

    async fn fetch_decay_params(
        &self,
        topology_hash: [u8; 32],
    ) -> Result<Option<DecayParams>, ChainError> {
        self.read_decay_params(topology_hash).await
    }

    async fn declare_participation(
        &self,
        qblock_id: u64,
    ) -> Result<ParticipationOutcome, ChainError> {
        self.send_participation(qblock_id).await
    }

    async fn submit_proof(&self, proof: &Proof) -> Result<SubmitReceipt, ChainError> {
        self.send_proof(proof).await
    }

    async fn submit_solution(&self, proof: &Proof) -> Result<SubmitReceipt, ChainError> {
        self.send_solution(proof).await
    }

    async fn fetch_pending_proofs(&self) -> Result<Vec<PendingProof>, ChainError> {
        self.read_pending_proofs().await
    }

    async fn fetch_mempool_orders(
        &self,
        miner_account: [u8; 32],
    ) -> Result<Vec<JobOrder>, ChainError> {
        self.read_mempool_orders(miner_account).await
    }
}
