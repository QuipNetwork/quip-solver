//! A validated proof, its solution rows, the pallet form it is submitted in,
//! and what submitting it produced.

use parity_scale_codec::{Decode, Encode};
use sp_core::{H256, U256};

/// Upper bound on solutions packed into a submitted proof. Must not exceed the
/// chain pallet's `QuantumPowMaxSolutions` (`BoundedVec` bound, currently 32):
/// a proof carrying more rows fails SCALE bounded-vector decode before the
/// extrinsic dispatches. The coordinator therefore submits only the
/// diverse-selected subset, and the pallet re-selects within it.
pub const MAX_PROOF_SOLUTIONS: usize = 32;

/// [`MAX_PROOF_SOLUTIONS`] as the wire `SetTarget.max_proof_solutions` value.
pub const MAX_PROOF_SOLUTIONS_WIRE: u32 = 32;
const _: () = assert!(MAX_PROOF_SOLUTIONS == MAX_PROOF_SOLUTIONS_WIRE as usize);

/// One decoded solution: spins in node order and the energy this coordinator
/// scored for them. The internal form of a proof row. Wire solutions are
/// decoded once, when a result is validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpinRow {
    /// `+1`/`-1` spins in topology node order.
    pub spins: Vec<i8>,
    /// Energy in milli-units.
    pub energy_milli: i64,
}

/// A validated proof ready for chain submission.
#[derive(Debug, Clone)]
pub struct Proof {
    /// Wire job id this proof answers.
    pub job_id: Vec<u8>,
    /// Best solution energy in milli units.
    pub best_energy_milli: i64,
    /// Diversity of the solution set in milli units.
    pub diversity_milli: u32,
    /// Count of solutions that passed local gates.
    pub n_valid: u32,
    /// Valid solutions (spins + reported energies).
    pub solutions: Vec<SpinRow>,
    /// `true` when this is a `PoW` proof (not a mempool order).
    pub is_pow: bool,
    /// Mempool order id bytes; empty for pure `PoW`.
    pub order_id: Vec<u8>,
    /// Producer generation this proof belongs to.
    pub generation: u64,
    /// `PoW` salt (32 bytes). Required for live `submit_proof`; empty for
    /// `FakeChain` paths that only record the proof.
    pub salt: Vec<u8>,
    /// Miner-reported compute time in microseconds, from the result's
    /// `SamplerMeta.device_access_time_us`. Plumbed into the on-chain
    /// `QuantumProof`; `0` = unreported.
    pub device_access_time_us: u64,
}

/// Action the fire-loop should take after a submit attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitAction {
    /// Extrinsic accepted (or no error string to classify).
    Success,
    /// Transient / retriable pallet rejection (energy, proof limit).
    Retry,
    /// Round-stale rejection; abandon the current generation.
    StopRoundStale,
    /// Fatal rejection or unknown error; stop submitting this proof.
    StopFatal,
}

/// What a submit attempt produced: the action the fire-loop takes, plus the
/// chain detail the dashboard reports on the submission row.
///
/// `extrinsic_hash` is known as soon as the extrinsic is signed, so it is set on
/// every path that reached the node. `block_hash` and `block_number` are set only
/// on [`SubmitAction::Success`], which the pallet confirmation defines as a win:
/// `QuantumPow.QBlocks[block_number].miner` equals the signing account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitReceipt {
    /// Action the fire-loop should take.
    pub action: SubmitAction,
    /// Hash of the signed extrinsic, once it is signed.
    pub extrinsic_hash: Option<[u8; 32]>,
    /// Hash of the winning block. `None` when the submission did not win.
    pub block_hash: Option<[u8; 32]>,
    /// Height of the winning block. `None` when the submission did not win.
    pub block_number: Option<u64>,
}

impl SubmitReceipt {
    /// A receipt that carries an action and no chain detail.
    #[must_use]
    pub const fn action_only(action: SubmitAction) -> Self {
        Self {
            action,
            extrinsic_hash: None,
            block_hash: None,
            block_number: None,
        }
    }
}

/// Outcome of coordinator-side result revalidation for miner gating.
#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    /// Minimum recomputed energy after the energy gate, Z2 deduplication, and
    /// diversity selection. `i64::MAX` when no row clears the energy gate.
    /// A finite value does not imply `accepted`.
    pub best_energy_milli: i64,
    /// Diversity (milli) of the diverse-selected subset.
    pub diversity_milli: u32,
    /// Count of unique energy-valid solutions after Z2 dedup.
    pub n_valid: u32,
    /// Whether `n_valid` and diversity clear the advertised gates.
    pub accepted: bool,
    /// The diverse-selected *gate-passing* solutions to submit now
    /// (≤ [`MAX_PROOF_SOLUTIONS`]). Empty when no rows cleared the energy gate.
    /// Packed into the immediate-submit proof instead of the raw result rows so
    /// submissions stay within the pallet's bound.
    pub selected_solutions: Vec<SpinRow>,
    /// Minimum recomputed energy over all shape-valid returned rows, before the
    /// energy gate, Z2 deduplication, and diversity selection. `i64::MAX` when
    /// no shape-valid row exists. The decay-ratchet stash uses this minimum to
    /// project when an easing gate can admit a returned solution.
    pub raw_best_energy_milli: i64,
    /// The deduped rows (≤ [`MAX_PROOF_SOLUTIONS`]) the stash retains for a
    /// future, eased-ceiling submission, chosen regardless of the current gate.
    /// Ordered by ascending energy and pruned so that every prefix of them
    /// still clears the chain's diversity gate.
    pub stash_solutions: Vec<SpinRow>,
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
