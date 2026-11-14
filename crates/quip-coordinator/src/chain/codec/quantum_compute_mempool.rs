//! SCALE mirrors of `pallet_quantum_compute_mempool`: job orders, solver
//! info, solutions, and the `QuantumComputeMempool` call encoders.

// SCALE `Encode` for multi-field enum variants emits `index as u8` in a
// derived impl; the lint span points at the variant but attributes on the
// enum do not cover the sibling impl, so suppress at module scope.
#![expect(
    clippy::cast_possible_truncation,
    reason = "SCALE Encode writes enum discriminants as u8; pallet enums are tiny"
)]

use parity_scale_codec::{Decode, Encode};
use sp_core::H256;

use crate::types::miner::SolverType;
use crate::types::order::JobMode;

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

/// Pallet index of `QuantumComputeMempool` in the runtime construct.
pub const QUANTUM_COMPUTE_MEMPOOL_PALLET_INDEX: u8 = 9;

/// `QuantumComputeMempool::register_solver(solver_type)` call index. Not
/// idempotent: a second call fails with `SolverAlreadyRegistered`.
pub const REGISTER_SOLVER_CALL_INDEX: u8 = 0;

/// `QuantumComputeMempool::deregister_solver()` call index.
pub const DEREGISTER_SOLVER_CALL_INDEX: u8 = 1;

/// `QuantumComputeMempool::submit_solution(order_id, solutions)` call index.
pub const SUBMIT_SOLUTION_CALL_INDEX: u8 = 4;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::codec::quantum_pow::MinerInfoScale;
    use crate::types::miner::MinerKind;

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
}
