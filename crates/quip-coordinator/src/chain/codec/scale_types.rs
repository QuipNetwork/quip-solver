//! Minimal SCALE-compatible types mirrored from pallet `types.rs`.
//!
//! Prefer these over path-depending the FRAME pallets (which drag
//! polkadot-sdk git + full FRAME). Layout must stay byte-identical to
//! `pallet_quantum_pow` / `pallet_quantum_compute_mempool`.

// SCALE `Encode` for multi-field enum variants emits `index as u8` in a
// derived impl; the lint span points at the variant but attributes on the
// enum do not cover the sibling impl, so suppress at module scope.
#![expect(
    clippy::cast_possible_truncation,
    reason = "SCALE Encode writes enum discriminants as u8; pallet enums are tiny"
)]

use parity_scale_codec::{Decode, Encode};
use quantum_validation::AllowedValueSpec;
use sp_core::H256;

use crate::types::hex::hex_encode;
use crate::types::miner::SolverType;
use crate::types::order::JobMode;
use crate::types::proof::QuantumProof;

use crate::types::host::{RuntimeInfoScale, SystemInfoScale};
use crate::types::miner::{MinerKind, MinerSpecScale, NodeLogLevel};

use crate::types::topology_spec::TopologySpec;

/// On-wire difficulty config (matches pallet `DifficultyConfig`).
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct DifficultyConfig {
    /// Minimum valid solutions required.
    pub min_solutions: u32,
    /// Energy ceiling in milli units (solutions must be strictly below).
    pub max_energy_milli: i64,
    /// Minimum diversity of the solution set in milli units.
    pub min_diversity_milli: u32,
}

/// Per-topology curve override (matches pallet `CurveC`): per-mille c-triple
/// stored under `TopologyCurveC[topology_hash]`. Field order is the pallet's.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct CurveCScale {
    /// Easy-regime curve c (per-mille).
    pub easy_milli: u32,
    /// Knee-regime curve c (per-mille).
    pub knee_milli: u32,
    /// Hard-regime curve c (per-mille).
    pub hard_milli: u32,
}

/// Root-set energy band of a family topology (matches pallet `EnergyBand`),
/// stored under `TopologyEnergyBand[topology_hash]`. Field order is the
/// pallet's.
#[derive(Clone, Copy, Debug, Encode, Decode, PartialEq, Eq)]
pub struct EnergyBandScale {
    /// Hardest (most negative) end of the band, in milli.
    pub hard_milli: i64,
    /// Easiest (least negative) end of the band, in milli.
    pub easy_milli: i64,
}

/// A family revision and its canonical parameter bytes (matches pallet
/// `FamilyDescriptor`). The pallet's bounded vectors encode as plain vectors.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct FamilyDescriptorScale {
    /// The family id, as ASCII bytes.
    pub family_id: Vec<u8>,
    /// The exact revision of the family.
    pub revision: u32,
    /// The canonical parameter encoding.
    pub params: Vec<u8>,
}

/// `QuantumPowApi` version whose `MiningSnapshot` and `TopologyMeta` carry the
/// `family` field. Version 2 encodes both without it.
pub const FAMILY_API_VERSION: u32 = 3;

/// Runtime-API mining snapshot (matches pallet `MiningSnapshot` at
/// `QuantumPowApi` version 3).
///
/// Note: no `block_number` field — callers fetch the header separately.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct MiningSnapshotScale {
    /// Hash of the block that contained the last winning proof.
    pub last_proof_block_hash: H256,
    /// Current (possibly decayed) difficulty gates.
    pub difficulty: DifficultyConfig,
    /// Topology identity hash.
    pub topology_hash: H256,
    /// Topology node ids.
    pub nodes: Vec<u32>,
    /// Topology undirected edges as `(u, v)` node-id pairs.
    pub edges: Vec<(u32, u32)>,
    /// Allowed linear-field values.
    pub allowed_h_values: AllowedValueSpec<Vec<i32>>,
    /// Allowed coupling values.
    pub allowed_j_values: AllowedValueSpec<Vec<i32>>,
    /// Allowed spin values.
    pub allowed_spin_values: AllowedValueSpec<Vec<i32>>,
    /// The family descriptor, when the chain draws `h` and `j` through the
    /// canonical generator. `None` keeps the legacy draw.
    pub family: Option<FamilyDescriptorScale>,
}

/// Runtime-API registered topology (matches pallet `TopologyMeta` at
/// `QuantumPowApi` version 3).
///
/// Returned by `QuantumPowApi_topology_meta(hash)`. The hash is the content
/// hash of every field but `registered_at`, so one hash names one topology
/// for the life of the chain and the client caches it by hash. Decode it with
/// [`TopologyMetaScale::decode_for_api`], because version 2 has no `family`.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct TopologyMetaScale {
    /// Topology node ids.
    pub nodes: Vec<u32>,
    /// Topology undirected edges as `(u, v)` node-id pairs.
    pub edges: Vec<(u32, u32)>,
    /// Allowed linear-field values.
    pub allowed_h_values: AllowedValueSpec<Vec<i32>>,
    /// Allowed coupling values.
    pub allowed_j_values: AllowedValueSpec<Vec<i32>>,
    /// Allowed spin values.
    pub allowed_spin_values: AllowedValueSpec<Vec<i32>>,
    /// The family descriptor, when the chain draws `h` and `j` through the
    /// canonical generator. `None` keeps the legacy draw.
    pub family: Option<FamilyDescriptorScale>,
    /// Block number the topology was registered at.
    pub registered_at: u32,
}

/// `TopologyMeta` as `QuantumPowApi` version 2 encodes it: no `family`.
#[derive(Decode)]
struct TopologyMetaV2Scale {
    nodes: Vec<u32>,
    edges: Vec<(u32, u32)>,
    allowed_h_values: AllowedValueSpec<Vec<i32>>,
    allowed_j_values: AllowedValueSpec<Vec<i32>>,
    allowed_spin_values: AllowedValueSpec<Vec<i32>>,
    registered_at: u32,
}

impl TopologyMetaScale {
    /// Decode one `TopologyMeta` in the layout of `QuantumPowApi` version
    /// `api_version`. Version 3 inserts `family` before `registered_at`, so a
    /// version-2 decoder would read a wrong `registered_at` from it.
    ///
    /// # Errors
    /// Returns the SCALE error when `input` does not hold that layout.
    pub fn decode_for_api(
        input: &mut &[u8],
        api_version: u32,
    ) -> Result<Self, parity_scale_codec::Error> {
        if api_version >= FAMILY_API_VERSION {
            return Self::decode(input);
        }
        let v2 = TopologyMetaV2Scale::decode(input)?;
        Ok(Self {
            nodes: v2.nodes,
            edges: v2.edges,
            allowed_h_values: v2.allowed_h_values,
            allowed_j_values: v2.allowed_j_values,
            allowed_spin_values: v2.allowed_spin_values,
            family: None,
            registered_at: v2.registered_at,
        })
    }

    /// Convert registered chain metadata into a topology spec.
    ///
    /// Keeps node, edge, and value-set order. Difficulty gates use the same
    /// defaults as a JSON spec without gates. Registration height is excluded
    /// from topology identity.
    ///
    /// # Errors
    /// Returns an error for unsupported value specs, invalid graph data,
    /// a computed topology hash that differs from `hash`, or a topology bound
    /// to a problem family.
    pub fn into_topology_spec(self, hash: [u8; 32]) -> Result<TopologySpec, String> {
        if let Some(family) = &self.family {
            return Err(format!(
                "chain topology {} is bound to family {} revision {}; pass the \
                 descriptor with --family-id, --family-revision, and --family-params",
                hex_encode(&hash),
                String::from_utf8_lossy(&family.family_id),
                family.revision
            ));
        }
        let spec = TopologySpec::from_sets(
            self.nodes,
            self.edges,
            require_set_values(&self.allowed_h_values)
                .map_err(|error| format!("chain topology allowed_h_milli: {error}"))?,
            require_set_values(&self.allowed_j_values)
                .map_err(|error| format!("chain topology allowed_j_milli: {error}"))?,
            require_set_values(&self.allowed_spin_values)
                .map_err(|error| format!("chain topology allowed_spin_milli: {error}"))?,
        )
        .map_err(|error| error.to_string())?;
        if spec.topology.hash.as_slice() != hash.as_slice() {
            return Err(format!(
                "topology hash mismatch: chain reported {}, computed {}",
                hex_encode(&hash),
                hex_encode(&spec.topology.hash)
            ));
        }
        Ok(spec)
    }
}

/// Ising params nested in a mempool `JobOrder`.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct IsingParams {
    /// Topology node ids.
    pub nodes: Vec<u32>,
    /// Topology undirected edges as `(u, v)` node-id pairs.
    pub edges: Vec<(u32, u32)>,
    /// Per-node linear fields (aligned with `nodes`).
    pub h_values: Vec<i32>,
    /// Per-edge couplings (aligned with `edges`).
    pub j_values: Vec<i32>,
    /// Optional energy gate.
    pub min_energy_milli: Option<i64>,
    /// Optional diversity gate.
    pub min_diversity_milli: Option<u32>,
    /// Optional minimum solution count.
    pub min_solutions: Option<u32>,
}

/// Order timing (`BlockNumber` = `u32` on the default runtime).
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct OrderTiming {
    /// Blocks until the order expires.
    pub deadline_blocks: u32,
    /// Blocks to wait after first solution before closing.
    pub block_wait: u32,
}

/// Minimal status enum (SCALE tag order must match the pallet).
#[derive(Clone, Copy, Debug, Encode, Decode, PartialEq, Eq)]
pub enum OrderStatus {
    /// Order is open for solutions.
    Opened,
    /// Order expired without closing.
    Expired,
    /// Order closed (rewarded / settled).
    Closed,
}

/// Reward resolution enum (SCALE tag order must match the pallet).
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub enum RewardResolution {
    /// Winner-take-all on best energy.
    SingleBest,
    /// Weighted split across top-`n` solutions.
    TopNWeighted {
        /// Number of top solutions that share the reward.
        n: u32,
    },
    /// Equal split across top-`n` solutions.
    TopNEqual {
        /// Number of top solutions that share the reward.
        n: u32,
    },
}

/// Result delivery enum.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub enum ResultDelivery {
    /// Results stay on-chain only.
    OnChainOnly,
    /// Push results to a callback endpoint.
    Callback {
        /// Callback endpoint bytes.
        endpoint: Vec<u8>,
    },
    /// Callback plus poll-for-result path.
    CallbackWithPoll {
        /// Callback endpoint bytes.
        endpoint: Vec<u8>,
    },
}

/// Full on-chain `JobOrder` (`AccountId32` / `Balance=u128` / `BlockNumber=u32`).
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct JobOrderScale {
    /// Spec / topology identity.
    pub spec_id: H256,
    /// Proposer account (`AccountId32` raw bytes).
    pub proposer: [u8; 32],
    /// Nested Ising problem statement.
    pub ising_params: IsingParams,
    /// Reward amount in plancks.
    pub reward: u128,
    /// Open vs bid mode.
    pub mode: JobMode,
    /// How the reward is split among winners.
    pub resolution: RewardResolution,
    /// Deadline and wait timing.
    pub timing: OrderTiming,
    /// How results are delivered.
    pub delivery: ResultDelivery,
    /// Current order status.
    pub status: OrderStatus,
    /// Block number when the order was created.
    pub created_at: u32,
    /// Block of the first accepted solution, if any.
    pub first_solution_at: Option<u32>,
    /// Number of solutions accepted so far.
    pub solution_count: u32,
}

/// SCALE mirror of `pallet_quantum_pow::MinerInfo<Balance, BlockNumber>` with
/// the runtime's `Balance = u128` and `BlockNumber = u32`.
///
/// Field order is the pallet's and must not be reordered: SCALE carries no field
/// names, so a swap decodes into a wrong number with no error.
#[derive(Clone, Copy, Debug, Encode, Decode, PartialEq, Eq)]
pub struct MinerInfoScale {
    /// Block the account registered at.
    pub registered_at: u32,
    /// Reserved miner deposit, in plancks.
    pub deposit: u128,
    /// Lifetime accepted proofs.
    pub proofs_submitted: u32,
    /// Lifetime winning proofs.
    pub proofs_won: u32,
    /// Lifetime rewards, in plancks.
    pub rewards_earned: u128,
}

/// Pallet index of `QuantumPow` in the runtime construct.
pub const QUANTUM_POW_PALLET_INDEX: u8 = 10;
/// Call index of `submit_proof` within `QuantumPow`.
pub const SUBMIT_PROOF_CALL_INDEX: u8 = 4;
/// `QuantumPow::register_miner` call index. Takes no arguments; the origin is
/// the miner. `QuantumPow::submit_proof` rejects any account that has not made
/// this call with `MinerNotRegistered`.
pub const REGISTER_MINER_CALL_INDEX: u8 = 0;
/// `QuantumPow::register_topology` call index.
pub const REGISTER_TOPOLOGY_CALL_INDEX: u8 = 2;
/// `QuantumPow::set_difficulty` call index.
pub const SET_DIFFICULTY_CALL_INDEX: u8 = 3;
/// `QuantumPow::set_default_topology` call index.
pub const SET_DEFAULT_TOPOLOGY_CALL_INDEX: u8 = 5;
/// `QuantumPow::add_mineable_topology` call index.
pub const ADD_MINEABLE_TOPOLOGY_CALL_INDEX: u8 = 6;
/// `QuantumPow::set_topology_energy_band` call index.
pub const SET_TOPOLOGY_ENERGY_BAND_CALL_INDEX: u8 = 11;
/// `QuantumPow::remove_mineable_topology` call index. Refuses the current
/// default.
pub const REMOVE_MINEABLE_TOPOLOGY_CALL_INDEX: u8 = 7;

/// `Utility` pallet index in the runtime.
pub const UTILITY_PALLET_INDEX: u8 = 17;
/// `pallet_utility::Call::batch_all` call index. All calls or none.
pub const BATCH_ALL_CALL_INDEX: u8 = 2;
/// `Foundation` (`pallet_collective` instance 1) pallet index on runtime 120.
pub const FOUNDATION_PALLET_INDEX: u8 = 20;
/// `pallet_collective::Call::propose` call index. A threshold below 2
/// executes the proposal at once with the proposer's member origin.
pub const PROPOSE_CALL_INDEX: u8 = 2;
/// `pallet_collective::Call::vote` call index. A vote never executes the
/// proposal; `close` does.
pub const VOTE_CALL_INDEX: u8 = 3;
/// `pallet_collective::Call::close` call index. Executes the proposal once
/// its ayes reach the threshold.
pub const CLOSE_CALL_INDEX: u8 = 6;

/// `pallet_collective::Votes` as stored under `Foundation.Voting[hash]`.
/// Field order is the pallet's; accounts are `AccountId32`.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct FoundationVotesScale {
    /// The proposal index `vote` and `close` name.
    pub index: u32,
    /// Ayes the motion needs.
    pub threshold: u32,
    /// Members that voted aye.
    pub ayes: Vec<[u8; 32]>,
    /// Members that voted nay.
    pub nays: Vec<[u8; 32]>,
    /// Last block of the voting period.
    pub end: u32,
}

/// `sp_weights::Weight`: both fields are compact on the wire.
#[derive(Clone, Copy, Debug, Encode, Decode, PartialEq, Eq)]
pub struct WeightScale {
    /// Reference execution time.
    #[codec(compact)]
    pub ref_time: u64,
    /// Proof size in bytes.
    #[codec(compact)]
    pub proof_size: u64,
}
/// `ValidatorAdmission` pallet index on runtime 120.
pub const VALIDATOR_ADMISSION_PALLET_INDEX: u8 = 22;
/// `ValidatorAdmission::dispatch_as_root` call index. Needs 2/3 of the
/// Foundation and dispatches its call with the Root origin.
pub const DISPATCH_AS_ROOT_CALL_INDEX: u8 = 3;

/// Pallet index of `MinerRegistry` in the runtime construct.
pub const MINER_REGISTRY_PALLET_INDEX: u8 = 13;
/// Call index of `set_descriptor` within `MinerRegistry`.
pub const SET_DESCRIPTOR_CALL_INDEX: u8 = 0;
/// Call index of `participate` within `MinerRegistry`.
pub const PARTICIPATE_CALL_INDEX: u8 = 2;

/// Pallet index of `QuantumComputeMempool` in the runtime construct.
pub const QUANTUM_COMPUTE_MEMPOOL_PALLET_INDEX: u8 = 9;
/// `QuantumComputeMempool::register_solver(solver_type)` call index. Not
/// idempotent: a second call fails with `SolverAlreadyRegistered`.
pub const REGISTER_SOLVER_CALL_INDEX: u8 = 0;
/// `QuantumComputeMempool::deregister_solver()` call index.
pub const DEREGISTER_SOLVER_CALL_INDEX: u8 = 1;
/// `QuantumComputeMempool::submit_solution(order_id, solutions)` call index.
pub const SUBMIT_SOLUTION_CALL_INDEX: u8 = 4;

/// Pallet `MaxNodeIdBytes`.
pub const MAX_NODE_ID_BYTES: usize = 64;
/// Pallet `MaxNodeNameBytes`.
pub const MAX_NODE_NAME_BYTES: usize = 64;
/// Pallet `MaxPublicHostBytes`.
pub const MAX_PUBLIC_HOST_BYTES: usize = 253;
/// Pallet `MaxRpcEndpointBytes`.
pub const MAX_RPC_ENDPOINT_BYTES: usize = 256;
/// Pallet `MaxRpcEndpoints`.
pub const MAX_RPC_ENDPOINTS: usize = 8;
/// Pallet `MaxMinerSpecs`.
pub const MAX_MINER_SPECS: usize = 16;
/// Pallet `MaxMinerLabelBytes`.
pub const MAX_MINER_LABEL_BYTES: usize = 64;
/// Pallet `MaxMinerBackendBytes`.
pub const MAX_MINER_BACKEND_BYTES: usize = 32;
/// Pallet `MaxMinerDeviceIdBytes`.
pub const MAX_MINER_DEVICE_ID_BYTES: usize = 128;

/// SCALE mirror of `QuantumComputeMempool.Solvers[account]`
/// (`SolverInfo<AccountId32, u128, u32>`). Field order is the pallet's.
#[derive(Clone, Copy, Debug, Encode, Decode, PartialEq, Eq)]
pub struct SolverInfoScale {
    /// The registered account.
    pub account: [u8; 32],
    /// Type the account registered as. Bid orders filter on it.
    pub solver_type: SolverType,
    /// Block the account registered at.
    pub registered_at: u32,
    /// Lifetime accepted solutions.
    pub solutions_submitted: u64,
    /// Lifetime rewards, in plancks.
    pub rewards_earned: u128,
}

/// SCALE mirror of `QuantumComputeMempool.OrderSolutions[order_id][account]`
/// (`JobSolution<AccountId32, u32, BoundedVec<BoundedVec<i8>>>`).
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct JobSolutionScale {
    /// The solving account.
    pub solver: [u8; 32],
    /// The solver's registered type.
    pub solver_type: SolverType,
    /// The diverse subset the pallet kept, as `±1` spins.
    pub solutions: Vec<Vec<i8>>,
    /// Best kept energy in milli units.
    pub best_energy_milli: i64,
    /// Diversity of the kept subset in milli units.
    pub diversity_milli: u32,
    /// Number of kept rows.
    pub num_valid: u32,
    /// Block that accepted this submission. A resubmission overwrites it.
    pub submitted_at: u32,
}

/// Caller-supplied V2 descriptor. This is the payload the coordinator files.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct NodeDescriptorV2Input {
    /// Node identity bytes.
    pub node_id: Vec<u8>,
    /// Display name.
    pub node_name: Vec<u8>,
    /// Optional public host.
    pub public_host: Option<Vec<u8>>,
    /// Optional public port.
    pub public_port: Option<u16>,
    /// Validator RPC endpoints.
    pub rpc_endpoints: Vec<Vec<u8>>,
    /// Whether the node mines without a manual start.
    pub auto_mine: bool,
    /// Advertised log verbosity.
    pub log_level: NodeLogLevel,
    /// Miners on this node.
    pub miners: Vec<MinerSpecScale>,
    /// Optional hardware survey, from [`crate::host::survey::collect`]. `None` when
    /// the probe missed its budget or left a pallet bound violated.
    pub system_info: Option<SystemInfoScale>,
    /// Optional node-software block, from [`crate::host::survey::collect`]. `None`
    /// when the probe missed its budget or left a pallet bound violated.
    pub runtime: Option<RuntimeInfoScale>,
}

/// SCALE-encode the `QuantumPow.submit_proof(proof)` call body.
#[must_use]
pub fn encode_submit_proof_call(proof: &QuantumProof) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(QUANTUM_POW_PALLET_INDEX);
    out.push(SUBMIT_PROOF_CALL_INDEX);
    // Call args: single composite field `proof`.
    out.extend(proof.encode());
    out
}

/// SCALE-encode the `QuantumPow.register_miner()` call body.
///
/// The call takes no arguments, so the body is the two dispatch indices. The
/// deposit is reserved from the signing account.
#[must_use]
pub fn encode_register_miner_call() -> Vec<u8> {
    vec![QUANTUM_POW_PALLET_INDEX, REGISTER_MINER_CALL_INDEX]
}

/// SCALE-encode `QuantumComputeMempool.register_solver(solver_type)`.
#[must_use]
pub fn encode_register_solver_call(solver_type: SolverType) -> Vec<u8> {
    let mut out = vec![
        QUANTUM_COMPUTE_MEMPOOL_PALLET_INDEX,
        REGISTER_SOLVER_CALL_INDEX,
    ];
    out.extend(solver_type.encode());
    out
}

/// SCALE-encode `QuantumComputeMempool.deregister_solver()`.
#[must_use]
pub fn encode_deregister_solver_call() -> Vec<u8> {
    vec![
        QUANTUM_COMPUTE_MEMPOOL_PALLET_INDEX,
        DEREGISTER_SOLVER_CALL_INDEX,
    ]
}

/// SCALE-encode `QuantumComputeMempool.submit_solution(order_id, solutions)`.
///
/// Each row is one `±1` spin per node, in node order. A `BoundedVec` encodes
/// like a `Vec`, so the bounds are the caller's to respect
/// ([`crate::types::order::MAX_ORDER_SOLUTIONS`] rows).
#[must_use]
pub fn encode_submit_solution_call(order_id: u64, solutions: &[Vec<i8>]) -> Vec<u8> {
    let mut out = vec![
        QUANTUM_COMPUTE_MEMPOOL_PALLET_INDEX,
        SUBMIT_SOLUTION_CALL_INDEX,
    ];
    out.extend(order_id.encode());
    out.extend(solutions.encode());
    out
}

/// SCALE-encode `MinerRegistry.participate(qblock_id, kind, budget_seconds)`.
#[must_use]
pub fn encode_participate_call(
    qblock_id: u64,
    kind: MinerKind,
    budget_seconds: Option<u32>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(MINER_REGISTRY_PALLET_INDEX);
    out.push(PARTICIPATE_CALL_INDEX);
    out.extend(qblock_id.encode());
    out.extend(kind.encode());
    out.extend(budget_seconds.encode());
    out
}

/// SCALE-encode `MinerRegistry.set_descriptor(V2(descriptor))`.
///
/// The version tag is `1` so a V1 signed extrinsic (tag `0`) still decodes.
#[must_use]
pub fn encode_set_descriptor_call(descriptor: &NodeDescriptorV2Input) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(MINER_REGISTRY_PALLET_INDEX);
    out.push(SET_DESCRIPTOR_CALL_INDEX);
    out.push(1);
    out.extend(descriptor.encode());
    out
}

/// Require an `AllowedValueSpec::Set` and return its (non-empty) values.
///
/// The coordinator draws `PoW` models with [`draw_ising_milli`] over a discrete
/// allowed set. The chain's `generate_ising_model` samples `IntegerRange` /
/// `ContinuousRange` specs with a *different* RNG consumption (see
/// `quantum_validation::AllowedValueSpec::sample`), so a coordinator-side
/// expansion of a range into a `Set` would neither reproduce the chain's model
/// nor match its topology hash. Rather than mine unverifiable jobs, reject
/// non-`Set` specs (and empty sets) here so the failure surfaces at snapshot
/// decode with a clear message.
///
/// [`draw_ising_milli`]: quip_protocol::chacha8::draw_ising_milli
///
/// # Errors
/// Returns an error for `IntegerRange` / `ContinuousRange` specs and for an
/// empty `Set`.
pub fn require_set_values(spec: &AllowedValueSpec<Vec<i32>>) -> Result<Vec<i32>, String> {
    match spec {
        AllowedValueSpec::Set(v) if !v.is_empty() => Ok(v.clone()),
        AllowedValueSpec::Set(_) => {
            Err("allowed-value Set is empty; a PoW model cannot be drawn".to_string())
        }
        AllowedValueSpec::IntegerRange { .. } | AllowedValueSpec::ContinuousRange { .. } => Err(
            "coordinator requires AllowedValueSpec::Set; range specs (IntegerRange/\
             ContinuousRange) sample differently on-chain and are not supported"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::topology_spec::{parse_topology_spec, preset_spec};
    use sp_core::U256;

    #[test]
    fn register_solver_call_is_the_dispatch_indices_and_the_type_tag() {
        assert_eq!(
            encode_register_solver_call(SolverType::QpuDwave),
            [
                QUANTUM_COMPUTE_MEMPOOL_PALLET_INDEX,
                REGISTER_SOLVER_CALL_INDEX,
                2
            ]
        );
        assert_eq!(
            encode_deregister_solver_call(),
            [
                QUANTUM_COMPUTE_MEMPOOL_PALLET_INDEX,
                DEREGISTER_SOLVER_CALL_INDEX
            ]
        );
    }

    /// The mempool pallet has no `Metal` tag. `MinerKind::Metal` is tag 7,
    /// which the pallet cannot decode, so it must map onto `Gpu` (tag 1).
    #[test]
    fn metal_registers_as_a_gpu_solver() {
        assert_eq!(SolverType::from(MinerKind::Metal), SolverType::Gpu);
        assert_eq!(SolverType::from(MinerKind::Metal).encode(), [1]);
        assert_eq!(SolverType::from(MinerKind::Asic).encode(), [6]);
    }

    #[test]
    fn submit_solution_call_encodes_order_id_then_nested_spin_rows() {
        let call = encode_submit_solution_call(0x0102, &[vec![1, -1], vec![-1, 1]]);
        let mut expected = vec![
            QUANTUM_COMPUTE_MEMPOOL_PALLET_INDEX,
            SUBMIT_SOLUTION_CALL_INDEX,
        ];
        expected.extend_from_slice(&0x0102u64.to_le_bytes());
        // Compact lengths: 2 rows, then 2 spins per row; -1 is 0xFF.
        expected.extend_from_slice(&[0x08, 0x08, 0x01, 0xFF, 0x08, 0xFF, 0x01]);
        assert_eq!(call, expected);
    }

    #[test]
    fn solver_info_decodes_the_pallet_field_order() {
        let mut blob = vec![0xAB; 32];
        blob.push(1); // Gpu
        blob.extend_from_slice(&7u32.to_le_bytes());
        blob.extend_from_slice(&3u64.to_le_bytes());
        blob.extend_from_slice(&9u128.to_le_bytes());
        let info = SolverInfoScale::decode(&mut blob.as_slice()).unwrap();
        assert_eq!(info.account, [0xAB; 32]);
        assert_eq!(info.solver_type, SolverType::Gpu);
        assert_eq!(info.registered_at, 7);
        assert_eq!(info.solutions_submitted, 3);
        assert_eq!(info.rewards_earned, 9);
    }

    #[test]
    fn job_solution_decodes_the_pallet_field_order() {
        let mut blob = vec![0x11; 32];
        blob.push(0); // Cpu
        blob.extend_from_slice(&[0x04, 0x08, 0x01, 0xFF]); // one row: [1, -1]
        blob.extend_from_slice(&(-5i64).to_le_bytes());
        blob.extend_from_slice(&250u32.to_le_bytes());
        blob.extend_from_slice(&1u32.to_le_bytes());
        blob.extend_from_slice(&188_000u32.to_le_bytes());
        let sol = JobSolutionScale::decode(&mut blob.as_slice()).unwrap();
        assert_eq!(sol.solutions, vec![vec![1, -1]]);
        assert_eq!(sol.best_energy_milli, -5);
        assert_eq!(sol.diversity_milli, 250);
        assert_eq!(sol.num_valid, 1);
        assert_eq!(sol.submitted_at, 188_000);
    }

    #[test]
    fn require_set_values_accepts_only_a_nonempty_set() {
        assert_eq!(
            require_set_values(&AllowedValueSpec::Set(vec![-1000, 0, 1000])).ok(),
            Some(vec![-1000, 0, 1000])
        );
        // Range specs sample differently on-chain; the coordinator must not
        // silently expand them into a Set (would diverge from consensus).
        for (label, spec) in [
            ("empty set", AllowedValueSpec::Set(Vec::new())),
            (
                "integer range",
                AllowedValueSpec::IntegerRange { min: -2, max: 2 },
            ),
            (
                "continuous range",
                AllowedValueSpec::ContinuousRange {
                    min: -1000,
                    max: 1000,
                },
            ),
        ] {
            assert!(require_set_values(&spec).is_err(), "{label}");
        }
    }

    /// The pallet's `TopologyMeta` byte layout under each API version, built
    /// field by field. Version 3 puts `family` before `registered_at`.
    #[test]
    fn topology_meta_decodes_in_the_layout_of_each_api_version() {
        let spec = AllowedValueSpec::Set(vec![-1000, 1000]);
        let mut common = Vec::new();
        common.extend(vec![0u32, 1].encode());
        common.extend(vec![(0u32, 1u32)].encode());
        common.extend(spec.encode());
        common.extend(spec.encode());
        common.extend(spec.encode());
        let registered_at = 0x0102_0304u32.encode();

        let mut v2 = common.clone();
        v2.extend(&registered_at);
        let legacy = TopologyMetaScale::decode_for_api(&mut v2.as_slice(), 2).unwrap();
        assert_eq!((legacy.family, legacy.registered_at), (None, 0x0102_0304));

        let descriptor = FamilyDescriptorScale {
            family_id: b"quip/ice-glass".to_vec(),
            revision: 1,
            params: vec![9; 5],
        };
        let mut v3 = common.clone();
        v3.extend(Some(descriptor.clone()).encode());
        v3.extend(&registered_at);
        let family = TopologyMetaScale::decode_for_api(&mut v3.as_slice(), 3).unwrap();
        assert_eq!(family.family, Some(descriptor));
        assert_eq!(family.registered_at, 0x0102_0304);

        let mut v3_legacy = common;
        v3_legacy.push(0);
        v3_legacy.extend(&registered_at);
        let unbound = TopologyMetaScale::decode_for_api(&mut v3_legacy.as_slice(), 3).unwrap();
        assert_eq!((unbound.family, unbound.registered_at), (None, 0x0102_0304));
    }

    #[test]
    fn set_descriptor_call_encodes_pallet_call_and_v2_args() {
        let v2 = NodeDescriptorV2Input {
            node_id: b"n".to_vec(),
            node_name: b"N".to_vec(),
            public_host: None,
            public_port: None,
            rpc_endpoints: Vec::new(),
            auto_mine: true,
            log_level: NodeLogLevel::Info,
            miners: vec![MinerSpecScale {
                kind: MinerKind::Cpu,
                label: None,
                backend: None,
                device_id: None,
            }],
            system_info: None,
            runtime: None,
        };
        let call = encode_set_descriptor_call(&v2);
        let mut expected = vec![MINER_REGISTRY_PALLET_INDEX, SET_DESCRIPTOR_CALL_INDEX, 1];
        expected.extend(v2.encode());
        assert_eq!(call, expected);
        // V2 variant index 1, then compact-len 1 + b'n', compact-len 1 + b'N',
        // three Nones / empty, auto_mine=true, log_level=Info, one Cpu miner
        // with three Nones, system_info=None, runtime=None.
        assert_eq!(
            call.get(2..),
            Some([1, 4, b'n', 4, b'N', 0, 0, 0, 1, 1, 4, 0, 0, 0, 0, 0, 0].as_slice()),
            "V2 variant index must be 1"
        );
    }

    #[test]
    fn register_miner_call_is_the_two_dispatch_indices() {
        let call = encode_register_miner_call();
        // The call takes no arguments, so anything longer is an encoding bug.
        assert_eq!(call, [QUANTUM_POW_PALLET_INDEX, REGISTER_MINER_CALL_INDEX]);
        assert_ne!(REGISTER_MINER_CALL_INDEX, SUBMIT_PROOF_CALL_INDEX);
    }

    #[test]
    fn participate_call_encodes_pallet_call_and_args() {
        let call = encode_participate_call(5, MinerKind::Cpu, None);
        assert_eq!(
            call,
            [
                MINER_REGISTRY_PALLET_INDEX,
                PARTICIPATE_CALL_INDEX,
                5,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ]
        );
        let with_budget = encode_participate_call(1, MinerKind::Metal, Some(30));
        assert_eq!(
            with_budget,
            [
                MINER_REGISTRY_PALLET_INDEX,
                PARTICIPATE_CALL_INDEX,
                1,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                7,
                1,
                30,
                0,
                0,
                0,
            ]
        );
    }

    #[test]
    fn submit_proof_call_starts_with_pallet_and_call_index() {
        let proof = QuantumProof {
            topology_hash: H256::repeat_byte(0),
            nonce: U256::zero(),
            salt: [0u8; 32],
            solutions: vec![vec![0]],
            device_access_time_us: 0,
        };
        let call = encode_submit_proof_call(&proof);
        #[expect(
            clippy::indexing_slicing,
            reason = "call always starts with pallet+call index bytes"
        )]
        {
            assert_eq!(call[0], QUANTUM_POW_PALLET_INDEX);
            assert_eq!(call[1], SUBMIT_PROOF_CALL_INDEX);
            // Remainder is the proof encoding.
            assert_eq!(&call[2..], &proof.encode());
        }
    }

    fn valid_job_order() -> JobOrderScale {
        JobOrderScale {
            spec_id: H256::repeat_byte(9),
            proposer: [0x11; 32],
            ising_params: IsingParams {
                nodes: vec![0, 1],
                edges: vec![(0, 1)],
                h_values: vec![0, 0],
                j_values: vec![-1000],
                min_energy_milli: Some(-500),
                min_diversity_milli: Some(100),
                min_solutions: Some(2),
            },
            reward: 1_000_000_000_000,
            mode: JobMode::Open,
            resolution: RewardResolution::SingleBest,
            timing: OrderTiming {
                deadline_blocks: 100,
                block_wait: 10,
            },
            delivery: ResultDelivery::OnChainOnly,
            status: OrderStatus::Opened,
            created_at: 42,
            first_solution_at: None,
            solution_count: 0,
        }
    }

    /// SCALE field order must match `pallet_quantum_pow::MinerInfo`. A reordered
    /// field decodes into the wrong number silently, so pin the byte layout.
    ///
    /// The fixture is built from the pallet's declared layout directly,
    /// independent of `MinerInfoScale`'s own field order. An encode-then-decode
    /// round trip through the same struct cannot catch a transposition. This
    /// test can, because the blob comes from
    /// `quip-validator/pallets/quantum-pow/src/types.rs`'s
    /// `MinerInfo<Balance, BlockNumber>` field order (`registered_at, deposit,
    /// proofs_submitted, proofs_won, rewards_earned`), not from the struct.
    /// Any change to that pallet layout must be mirrored here.
    #[test]
    fn miner_info_blob_built_from_the_pallet_layout_decodes_correctly() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&1_234_u32.to_le_bytes()); // registered_at
        blob.extend_from_slice(&1_000_000_000_000_u128.to_le_bytes()); // deposit
        blob.extend_from_slice(&7_u32.to_le_bytes()); // proofs_submitted
        blob.extend_from_slice(&3_u32.to_le_bytes()); // proofs_won
        blob.extend_from_slice(&70_000_000_000_000_u128.to_le_bytes()); // rewards_earned
        assert_eq!(blob.len(), 44);

        let decoded = MinerInfoScale::decode(&mut &blob[..]).unwrap();
        assert_eq!(decoded.registered_at, 1_234);
        assert_eq!(decoded.deposit, 1_000_000_000_000);
        assert_eq!(decoded.proofs_submitted, 7);
        assert_eq!(decoded.proofs_won, 3);
        assert_eq!(decoded.rewards_earned, 70_000_000_000_000);
        // The struct encodes back to the same pallet-layout bytes.
        assert_eq!(decoded.encode(), blob);
    }

    // The JobProposed decode (`mining::mempool`) parses untrusted on-chain bytes; it must
    // reject malformed input with an error, never panic. Only a live #[ignore]
    // devnet test exercised this before — these are the offline guards.

    #[test]
    fn truncated_blobs_fail_to_decode() {
        let order = valid_job_order().encode();
        let miner = MinerInfoScale {
            registered_at: 1,
            deposit: 2,
            proofs_submitted: 3,
            proofs_won: 4,
            rewards_earned: 5,
        }
        .encode();
        // Chopping the tail of a job order leaves the later fields
        // (timing/status/counts) undecodable.
        let order_short = order.get(..order.len() - 4).expect("in bounds");
        let miner_short = miner.get(..miner.len() - 1).expect("in bounds");
        assert!(
            JobOrderScale::decode(&mut &order_short[..]).is_err(),
            "job order missing four bytes"
        );
        assert!(
            JobOrderScale::decode(&mut &[][..]).is_err(),
            "empty job order"
        );
        assert!(
            MinerInfoScale::decode(&mut &miner_short[..]).is_err(),
            "miner info missing one byte"
        );
    }

    #[test]
    fn job_order_decode_rejects_bad_enum_tag() {
        // `mode` follows spec_id, proposer, the variable-length ising_params,
        // and reward. Its first byte is the SCALE variant tag; 0xFF is past
        // every variant, so decode must return Err at exactly that offset.
        let order = valid_job_order();
        let mode_tag_offset = order.spec_id.encode().len()
            + order.proposer.encode().len()
            + order.ising_params.encode().len()
            + order.reward.encode().len();
        let mut bytes = order.encode();
        #[expect(
            clippy::indexing_slicing,
            reason = "the offset is the encoded length of the preceding fields"
        )]
        {
            bytes[mode_tag_offset] = 0xFF;
        }
        assert!(JobOrderScale::decode(&mut &bytes[..]).is_err());
    }

    fn chain_meta_fixture() -> Result<([u8; 32], TopologyMetaScale), String> {
        let spec =
            parse_topology_spec(preset_spec("aglais")?).map_err(|error| error.to_string())?;
        let hash = spec
            .topology
            .hash
            .as_slice()
            .try_into()
            .map_err(|_| "topology hash is not 32 bytes".to_string())?;
        let edges = spec.topology.edge_pairs();
        Ok((
            hash,
            TopologyMetaScale {
                nodes: spec.topology.nodes,
                edges,
                allowed_h_values: AllowedValueSpec::Set(spec.allowed_h_milli),
                allowed_j_values: AllowedValueSpec::Set(spec.allowed_j_milli),
                allowed_spin_values: AllowedValueSpec::Set(spec.allowed_spin_milli),
                family: None,
                registered_at: 7,
            },
        ))
    }

    #[test]
    fn from_chain_meta_preserves_aglais_and_uses_neutral_gates() {
        let (hash, meta) = chain_meta_fixture().expect("valid Aglais metadata");
        let expected_nodes = meta.nodes.clone();
        let expected_edges = meta.edges.clone();
        let mut late_registration = meta.clone();
        let spec = meta.into_topology_spec(hash).expect("valid chain topology");
        assert_eq!(spec.topology.hash, hash.to_vec());
        assert_eq!(spec.topology.nodes, expected_nodes);
        assert_eq!(spec.topology.edge_pairs(), expected_edges);
        assert_eq!(spec.allowed_h_milli, vec![0]);
        assert_eq!(spec.allowed_j_milli, vec![-1000, 1000]);
        assert_eq!(spec.allowed_spin_milli, vec![-1000, 1000]);
        assert_eq!(spec.min_solutions, 0);
        assert_eq!(spec.max_energy_milli, i64::MAX);
        assert_eq!(spec.min_diversity_milli, 0);
        let snapshot = spec.to_snapshot();
        assert_eq!(snapshot.head_hash, [0; 32]);
        assert_eq!(snapshot.last_proof_block_hash, [0; 32]);
        assert_eq!(snapshot.block_number, 0);
        assert_eq!(snapshot.spec_version, 0);
        assert_eq!(snapshot.topology_hash, hash.to_vec());

        // The registration height is not part of the topology hash.
        late_registration.registered_at = u32::MAX;
        let spec = late_registration
            .into_topology_spec(hash)
            .expect("valid chain topology");
        assert_eq!(spec.topology.hash, hash.to_vec());
    }

    #[test]
    fn from_chain_meta_rejects_unsupported_sets_bad_graphs_and_hashes() {
        let (hash, meta) = chain_meta_fixture().expect("valid Aglais metadata");
        let mut integer_h = meta.clone();
        integer_h.allowed_h_values = AllowedValueSpec::IntegerRange { min: -1, max: 1 };
        let mut continuous_j = meta.clone();
        continuous_j.allowed_j_values = AllowedValueSpec::ContinuousRange {
            min: -1000,
            max: 1000,
        };
        let mut empty_spin = meta.clone();
        empty_spin.allowed_spin_values = AllowedValueSpec::Set(vec![]);
        let mut empty_nodes = meta.clone();
        empty_nodes.nodes.clear();
        let mut bad_edge = meta.clone();
        bad_edge.edges.push((938, u32::MAX));
        let mut incorrect_hash = hash;
        *incorrect_hash.first_mut().expect("nonempty hash") ^= 1;
        let mut bound = meta.clone();
        bound.family = Some(FamilyDescriptorScale {
            family_id: b"quip/ice-glass".to_vec(),
            revision: 1,
            params: Vec::new(),
        });
        let cases = [
            (
                "family topology",
                hash,
                bound,
                "bound to family quip/ice-glass revision 1",
            ),
            (
                "integer range h",
                hash,
                integer_h,
                "requires AllowedValueSpec::Set",
            ),
            (
                "continuous range j",
                hash,
                continuous_j,
                "requires AllowedValueSpec::Set",
            ),
            ("empty spin set", hash, empty_spin, "Set is empty"),
            ("empty nodes", hash, empty_nodes, "nodes must be non-empty"),
            (
                "edge with unknown node",
                hash,
                bad_edge,
                "references node id",
            ),
            (
                "hash mismatch",
                incorrect_hash,
                meta,
                "topology hash mismatch",
            ),
        ];
        for (label, hash, meta, message) in cases {
            let error = meta
                .into_topology_spec(hash)
                .expect_err("expected a chain topology error");
            assert!(error.contains(message), "{label}: {error}");
        }
    }
}
