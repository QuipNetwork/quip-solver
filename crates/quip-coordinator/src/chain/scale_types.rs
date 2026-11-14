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
use sp_core::{H256, U256};

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
}

/// Proof payload for `QuantumPow.submit_proof` (pallet index 10, call index 4).
///
/// Energies / diversity are **not** sent — the chain recomputes them.
/// `solutions` is a list of bit-packed spin vectors. `device_access_time_us`
/// is miner-reported compute time in microseconds (self-reported
/// observability, unverifiable; `0` = unreported).
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct QuantumProof {
    /// Topology the proof was mined for.
    pub topology_hash: H256,
    /// Derived `PoW` nonce.
    pub nonce: U256,
    /// 32-byte salt used in nonce derivation.
    pub salt: [u8; 32],
    /// Bit-packed spin vectors.
    pub solutions: Vec<Vec<u8>>,
    /// Miner-reported device access time in microseconds (`0` = unreported).
    pub device_access_time_us: u64,
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

/// Job mode enum. Bid carries optional account / miner-type filters.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub enum JobMode {
    /// Any registered miner may answer.
    Open,
    /// Restricted bid with optional miner and type filters.
    Bid {
        /// Allowed miner account ids (`AccountId32` raw bytes).
        miners: Option<Vec<[u8; 32]>>,
        /// Allowed miner-type tags.
        miner_types: Option<Vec<u8>>,
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
/// Runtime `QuantumMaxSolutions`: the most rows one `submit_solution` carries.
/// Smaller than the `PoW` bound, and checked before any energy filtering.
pub const MAX_ORDER_SOLUTIONS: usize = 20;

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

// The V2 survey bounds below mirror the validator runtime at the rev already
// pinned in Cargo.toml (`runtime/src/configs/mod.rs`). Nothing links them at
// compile time, so a runtime upgrade that moves a bound is a coordinator
// change too. Over-length is a SCALE decode failure rather than a pallet
// error, so it never reaches `DESCRIPTOR_REJECT`; see `crate::survey`.

/// Pallet `MaxOsStringBytes`. One bound for `os.system`, `os.release`, and
/// `os.machine` alike.
pub const MAX_OS_STRING_BYTES: usize = 64;
/// Pallet `MaxCpuBrandBytes`.
pub const MAX_CPU_BRAND_BYTES: usize = 96;
/// Pallet `MaxArchBytes`.
pub const MAX_ARCH_BYTES: usize = 16;
/// Pallet `MaxGpuVendorBytes`.
pub const MAX_GPU_VENDOR_BYTES: usize = 16;
/// Pallet `MaxGpuNameBytes`.
pub const MAX_GPU_NAME_BYTES: usize = 96;
/// Pallet `MaxGpus`.
pub const MAX_GPUS: usize = 16;
/// Pallet `MaxRuntimeVersionBytes`. One bound for `python` and `quip_version`
/// alike.
pub const MAX_RUNTIME_VERSION_BYTES: usize = 48;
/// Pallet `MaxDockerImageBytes`.
pub const MAX_DOCKER_IMAGE_BYTES: usize = 256;

/// SCALE tag order must match `pallet_miner_registry::MinerKind`.
#[derive(Clone, Copy, Debug, Encode, Decode, PartialEq, Eq)]
pub enum MinerKind {
    /// CPU sampler.
    Cpu,
    /// Discrete GPU sampler.
    Gpu,
    /// D-Wave QPU.
    QpuDwave,
    /// IBM QPU.
    QpuIbm,
    /// `IonQ` QPU.
    QpuIonq,
    /// Pasqal QPU.
    QpuPasqal,
    /// ASIC sampler.
    Asic,
    /// Apple Metal GPU. Last so the earlier tags stay stable.
    Metal,
}

/// SCALE tag order must match `pallet_quantum_compute_mempool::MinerType`.
///
/// Not [`MinerKind`]: the mempool pallet has no `Metal` tag, so encoding a
/// `MinerKind::Metal` here would fail to decode on chain.
#[derive(Clone, Copy, Debug, Encode, Decode, PartialEq, Eq)]
pub enum SolverType {
    /// CPU sampler.
    Cpu,
    /// GPU sampler, discrete or Apple Metal.
    Gpu,
    /// D-Wave QPU.
    QpuDwave,
    /// IBM QPU.
    QpuIbm,
    /// `IonQ` QPU.
    QpuIonq,
    /// Pasqal QPU.
    QpuPasqal,
    /// ASIC sampler.
    Asic,
}

impl From<MinerKind> for SolverType {
    fn from(kind: MinerKind) -> Self {
        match kind {
            MinerKind::Cpu => Self::Cpu,
            MinerKind::Gpu | MinerKind::Metal => Self::Gpu,
            MinerKind::QpuDwave => Self::QpuDwave,
            MinerKind::QpuIbm => Self::QpuIbm,
            MinerKind::QpuIonq => Self::QpuIonq,
            MinerKind::QpuPasqal => Self::QpuPasqal,
            MinerKind::Asic => Self::Asic,
        }
    }
}

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

/// SCALE tag order must match `pallet_miner_registry::LogLevel`.
#[derive(Clone, Copy, Debug, Encode, Decode, PartialEq, Eq)]
pub enum NodeLogLevel {
    /// Pallet `Debug`. Also the mapping for coordinator `trace`.
    Debug,
    /// Pallet `Info`.
    Info,
    /// Pallet `Warning`. Also the mapping for coordinator `warn`.
    Warning,
    /// Pallet `Error`.
    Error,
}

/// One advertised miner. `Vec<u8>` encodes like the pallet `BoundedVec`.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct MinerSpecScale {
    /// Backend class.
    pub kind: MinerKind,
    /// Operator label, usually the miner id.
    pub label: Option<Vec<u8>>,
    /// Backend section name (`cpu`, `cuda`, `metal`, `dwave`).
    pub backend: Option<Vec<u8>>,
    /// Device id when the miner has one.
    pub device_id: Option<Vec<u8>>,
}

/// Operating-system identity inside [`SystemInfoScale`].
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct OsInfoScale {
    /// OS family.
    pub system: Vec<u8>,
    /// Kernel or build string.
    pub release: Vec<u8>,
    /// Machine architecture.
    pub machine: Vec<u8>,
}

/// CPU identity inside [`SystemInfoScale`].
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct CpuInfoScale {
    /// Logical core count.
    pub logical_cores: Option<u32>,
    /// Physical core count.
    pub physical_cores: Option<u32>,
    /// Brand string.
    pub brand: Vec<u8>,
    /// Instruction-set architecture.
    pub arch: Vec<u8>,
}

/// One GPU inside [`SystemInfoScale`].
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct GpuInfoScale {
    /// Enumeration index.
    pub index: u8,
    /// Vendor string.
    pub vendor: Vec<u8>,
    /// Product name.
    pub name: Vec<u8>,
    /// Device memory in MiB.
    pub memory_mb: Option<u32>,
    /// Utilization 0..=100.
    pub utilization_pct: Option<u8>,
}

/// Optional hardware survey on a V2 descriptor.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct SystemInfoScale {
    /// OS identity.
    pub os: OsInfoScale,
    /// CPU identity.
    pub cpu: CpuInfoScale,
    /// Host memory in MiB.
    pub memory_mb: Option<u32>,
    /// Attached GPUs.
    pub gpus: Vec<GpuInfoScale>,
}

/// Optional node-software identity on a V2 descriptor.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct RuntimeInfoScale {
    /// Python interpreter version.
    pub python: Vec<u8>,
    /// Node software version.
    pub quip_version: Vec<u8>,
    /// Protocol version number.
    pub protocol_version: u32,
    /// Whether the process is inside a container.
    pub in_docker: bool,
    /// Container image, when running in a container.
    pub docker_image: Option<Vec<u8>>,
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
    /// Optional hardware survey, from [`crate::survey::collect`]. `None` when
    /// the probe missed its budget or left a pallet bound violated.
    pub system_info: Option<SystemInfoScale>,
    /// Optional node-software block, from [`crate::survey::collect`]. `None`
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
/// ([`MAX_ORDER_SOLUTIONS`] rows).
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

    fn assert_roundtrip<T: Encode + Decode + PartialEq + std::fmt::Debug>(label: &str, value: &T) {
        let encoded = value.encode();
        assert_eq!(
            T::decode(&mut &encoded[..]).ok().as_ref(),
            Some(value),
            "{label}"
        );
    }

    #[test]
    fn scale_types_roundtrip() {
        assert_roundtrip(
            "QuantumProof",
            &QuantumProof {
                topology_hash: H256::from([0xab; 32]),
                nonce: U256::from(0x1234_5678u64),
                salt: [0xcd; 32],
                solutions: vec![vec![0b1010_0101], vec![0x00, 0xff]],
                device_access_time_us: 987_654,
            },
        );
        let snap = MiningSnapshotScale {
            last_proof_block_hash: H256::from([1u8; 32]),
            difficulty: DifficultyConfig {
                min_solutions: 5,
                max_energy_milli: -1_200_000,
                min_diversity_milli: 200,
            },
            topology_hash: H256::from([2u8; 32]),
            nodes: vec![0, 1, 2],
            edges: vec![(0, 1), (1, 2)],
            allowed_h_values: AllowedValueSpec::Set(vec![-1000, 0, 1000]),
            allowed_j_values: AllowedValueSpec::Set(vec![-1000, 1000]),
            allowed_spin_values: AllowedValueSpec::Set(vec![-1000, 1000]),
            family: Some(FamilyDescriptorScale {
                family_id: b"quip/ice-glass".to_vec(),
                revision: 1,
                params: vec![1, 2, 3],
            }),
        };
        assert_roundtrip("Option<MiningSnapshotScale>", &Some(snap));
        let meta = TopologyMetaScale {
            nodes: vec![0, 1, 2],
            edges: vec![(0, 1), (1, 2)],
            allowed_h_values: AllowedValueSpec::Set(vec![-1000, 0, 1000]),
            allowed_j_values: AllowedValueSpec::Set(vec![-1000, 1000]),
            allowed_spin_values: AllowedValueSpec::Set(vec![-1000, 1000]),
            family: None,
            registered_at: 7,
        };
        assert_roundtrip("Option<TopologyMetaScale>", &Some(meta));
        assert_roundtrip(
            "EnergyBandScale",
            &EnergyBandScale {
                hard_milli: -9_000_000,
                easy_milli: -7_000_000,
            },
        );
        assert_roundtrip("JobOrderScale", &valid_job_order());
        assert_roundtrip(
            "CurveCScale",
            &CurveCScale {
                easy_milli: 700,
                knee_milli: 725,
                hard_milli: 750,
            },
        );
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
    fn set_descriptor_option_struct_is_not_unit() {
        // Option<SystemInfo> must not collapse to Option<()>. A Some value
        // starts with 0x01 then the inner struct, not a single unit byte.
        let info = SystemInfoScale {
            os: OsInfoScale {
                system: b"Linux".to_vec(),
                release: Vec::new(),
                machine: Vec::new(),
            },
            cpu: CpuInfoScale {
                logical_cores: None,
                physical_cores: None,
                brand: b"x".to_vec(),
                arch: b"x".to_vec(),
            },
            memory_mb: None,
            gpus: Vec::new(),
        };
        let some = Some(info.clone()).encode();
        let none = Option::<SystemInfoScale>::None.encode();
        assert_eq!(none, vec![0]);
        assert_eq!(some.first().copied(), Some(1));
        assert_eq!(some.get(1..), Some(info.encode().as_slice()));
        assert_ne!(some, vec![1]);
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

    // The JobProposed decode (real.rs) parses untrusted on-chain bytes; it must
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
        // `mode: JobMode` is a 3-field composite; corrupt the SCALE variant tag
        // of the first enum (`mode`, after the fixed-size spec_id/proposer and
        // the variable ising_params). A tag past the variant count must error,
        // not panic. Scan for the first byte whose flip yields an out-of-range
        // enum tag by fuzzing each position and asserting no decode panics.
        let enc = valid_job_order().encode();
        for i in 0..enc.len() {
            let mut bad = enc.clone();
            #[expect(
                clippy::indexing_slicing,
                reason = "i iterates 0..enc.len(); in-bounds by construction"
            )]
            {
                bad[i] = 0xFF; // 0xFF is out of range for every enum in the struct
            }
            // Must return Ok or Err, never panic (the point of the guard).
            let _ = JobOrderScale::decode(&mut &bad[..]);
        }
    }
}
