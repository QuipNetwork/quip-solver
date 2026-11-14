//! A validated proof and its solution rows.

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
