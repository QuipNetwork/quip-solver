//! Miner identity: the backend kinds the pallets know, the advertised miner
//! specs, and the on-chain miner record.

use parity_scale_codec::{Decode, Encode};

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

/// `QuantumPow.Miners[account]`, widened for the dashboard wire shape.
///
/// The pallet stores `registered_at` and both proof counts as `u32`. They widen
/// to `u64` here so the dashboard serializer has one integer type to guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinerInfo {
    /// Block the account registered at.
    pub registered_at: u64,
    /// Reserved miner deposit, in plancks.
    pub deposit: u128,
    /// Lifetime accepted proofs. Also the `pow_sequence` a submission reports.
    pub proofs_submitted: u64,
    /// Lifetime winning proofs.
    pub proofs_won: u64,
    /// Lifetime rewards, in plancks.
    pub rewards_earned: u128,
}

impl MinerKind {
    /// Map a backend section name to the pallet `MinerKind`.
    #[must_use]
    pub fn from_backend(backend: &str) -> Self {
        match backend {
            "cuda" => Self::Gpu,
            "metal" => Self::Metal,
            "dwave" => Self::QpuDwave,
            _ => Self::Cpu,
        }
    }
}
