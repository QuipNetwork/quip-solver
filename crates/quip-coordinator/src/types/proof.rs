//! A validated proof, its solution rows, and what submitting it produced.

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
