//! The chain state one mining round runs against, and the pallet defaults
//! that fill it when the chain leaves a value unset.

use crate::types::family::ChallengeFamily;
use crate::types::proof::QuantumProof;
use crate::types::topology::{topology_hash_sets, DEFAULT_SPIN_SET};

/// Round length the chain targets: fixed in runtimes 118 and 119, and the
/// default of runtime 120 (`QuantumPowTargetProofBlocks`) when
/// `TargetProofBlocksOverride` is unset. Past this many blocks in one round
/// the continuous rule eases faster and the gated rule starts to ease.
pub const TARGET_PROOF_BLOCKS: u64 = 100;
// The pallet exposes these as constants/defaults, not via any runtime API, so
// (per the independent-reads path) the coordinator mirrors them. They match
// quip-validator @ v0.2 `QuantumPow`; a chain retune must be echoed here.
/// Blocks per decay epoch (`QuantumPow::EpochLength`).
pub const EPOCH_LENGTH_BLOCKS: u64 = 100;
/// Default curve calibration c-triple (per-mille: 700 == 0.70), from the
/// `CurveC{Easy,Knee,Hard}Milli` constants. A topology may override these via
/// `TopologyCurveC` storage, probed separately.
pub const DEFAULT_C_EASY_MILLI: u32 = 700;
/// Default knee calibration c (per-mille: 725 == 0.725).
pub const DEFAULT_C_KNEE_MILLI: u32 = 725;
/// Default hard calibration c (per-mille: 750 == 0.75).
pub const DEFAULT_C_HARD_MILLI: u32 = 750;
/// Default base difficulty `max_energy_milli` when `Difficulties[hash]` is unset
/// (`DifficultyConfig::default()` on chain: `{5, -1_200_000, 200}`).
pub const DEFAULT_BASE_MAX_ENERGY_MILLI: i64 = -1_200_000;

/// Chain mining snapshot consumed by the `PoW` producer.
#[derive(Debug, Clone)]
pub struct MiningSnapshot {
    /// Hash of the block the snapshot was taken at. The coordinator's own view
    /// of the chain head, distinct from [`Self::last_proof_block_hash`], which
    /// changes only when a proof wins.
    pub head_hash: [u8; 32],
    /// Hash of the block that contained the last winning proof. At that block
    /// itself this is the head hash, one block before the pallet stores it.
    /// See `chain::snapshot::round_root`.
    pub last_proof_block_hash: [u8; 32],
    /// Topology identity bytes (32-byte hash when provided by the chain).
    pub topology_hash: Vec<u8>,
    /// Topology node ids.
    pub nodes: Vec<u32>,
    /// Topology undirected edges as `(u, v)` node-id pairs.
    pub edges: Vec<(u32, u32)>,
    /// Allowed linear-field values in milli units.
    pub allowed_h_milli: Vec<i32>,
    /// Allowed coupling values in milli units.
    pub allowed_j_milli: Vec<i32>,
    /// Allowed spin milli values (Set); default binary `±1000` when empty.
    pub allowed_spin_milli: Vec<i32>,
    /// Minimum number of valid solutions required.
    pub min_solutions: u32,
    /// Energy ceiling: solutions must be strictly below this (milli).
    pub max_energy_milli: i64,
    /// Minimum diversity of the solution set (milli).
    pub min_diversity_milli: u32,
    /// Chain block number the snapshot was taken at.
    pub block_number: u64,
    /// Runtime `specVersion` at the snapshot block. Selects the decay rule
    /// the coordinator mirrors (`DecayAlgorithm::for_spec_version`).
    pub spec_version: u32,
    /// Round-length target that gates difficulty decay:
    /// `QuantumPow::TargetProofBlocksOverride` at the snapshot block, or the
    /// runtime default [`TARGET_PROOF_BLOCKS`] when unset.
    /// Runtime 119 has no override, so the read always falls back there.
    pub target_proof_blocks: u64,
    /// Problem family bound to this challenge. `None` keeps the legacy draw.
    /// Set once when the snapshot is built. A change is a new challenge.
    pub family: Option<ChallengeFamily>,
}

/// Difficulty-decay parameters read alongside the snapshot (independent
/// storage/constant reads, pinned at the snapshot's block) so the coordinator
/// can project the decay schedule the chain's already-decayed
/// `max_energy_milli` hides. The current block comes from
/// [`MiningSnapshot::block_number`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecayParams {
    /// Base (un-decayed) `max_energy_milli` from `Difficulties[topology_hash]`.
    pub base_max_energy_milli: i64,
    /// Block number of the last winning proof (0 = genesis, no decay yet).
    pub last_proof_block: u64,
    /// Blocks per decay epoch (`EpochLength`).
    pub epoch_length: u64,
    /// Resolved curve c-triple (per-mille: 700 == 0.70), after any per-topology
    /// `TopologyCurveC` override.
    pub c_easy_milli: u32,
    /// Knee c of the difficulty curve (per-mille).
    pub c_knee_milli: u32,
    /// Hard c of the difficulty curve (per-mille).
    pub c_hard_milli: u32,
    /// `TopologyEnergyBand[topology_hash]` as `(hard_milli, easy_milli)`. A
    /// family topology has one, and the chain runs its curve on the band in
    /// place of the c-triple.
    pub energy_band_milli: Option<(i64, i64)>,
}

impl DecayParams {
    /// Genesis / no-proof defaults: base difficulty is active (no decay).
    #[must_use]
    pub fn genesis() -> Self {
        Self {
            base_max_energy_milli: DEFAULT_BASE_MAX_ENERGY_MILLI,
            last_proof_block: 0,
            epoch_length: EPOCH_LENGTH_BLOCKS,
            c_easy_milli: DEFAULT_C_EASY_MILLI,
            c_knee_milli: DEFAULT_C_KNEE_MILLI,
            c_hard_milli: DEFAULT_C_HARD_MILLI,
            energy_band_milli: None,
        }
    }
}

/// Change-detection key over the round-identifying snapshot fields.
///
/// Mirrors `substrate/event_manager.py:_state_key`: a new key means a new
/// `PoW` generation. Pure `blake3` over identifying fields (groundable).
#[must_use]
pub fn head_state_key(snap: &MiningSnapshot) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    let _ = h.update(&snap.last_proof_block_hash);
    let _ = h.update(&snap.topology_hash);
    let _ = h.update(&snap.min_solutions.to_le_bytes());
    let _ = h.update(&snap.max_energy_milli.to_le_bytes());
    let _ = h.update(&snap.min_diversity_milli.to_le_bytes());
    let _ = h.update(&snap.block_number.to_le_bytes());
    if let Some(family) = &snap.family {
        let _ = h.update(&family.commitment());
    }
    *h.finalize().as_bytes()
}

/// Build topology hash for a snapshot's graph when not provided by the chain.
#[must_use]
pub fn snapshot_topology_hash(snap: &MiningSnapshot) -> Vec<u8> {
    if snap.topology_hash.len() == 32 {
        snap.topology_hash.clone()
    } else {
        let spin = if snap.allowed_spin_milli.is_empty() {
            DEFAULT_SPIN_SET.as_slice()
        } else {
            snap.allowed_spin_milli.as_slice()
        };
        topology_hash_sets(
            &snap.nodes,
            &snap.edges,
            &snap.allowed_h_milli,
            &snap.allowed_j_milli,
            spin,
        )
        .to_vec()
    }
}

/// A `QuantumPow.submit_proof` waiting in the pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingProof {
    /// `blake2_256` of the extrinsic bytes. Keys the per-round verdict cache.
    pub extrinsic_hash: [u8; 32],
    /// The signing `AccountId32`.
    pub account: [u8; 32],
    /// The proof as submitted.
    pub proof: QuantumProof,
}

/// Why a pending proof does or does not end the round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The pallet will mint a qblock from this proof at its inclusion block.
    Clears {
        /// Lowest energy among the rows under the ceiling, in milli-units.
        best_energy_milli: i64,
    },
    /// The nonce derives from another root, or the topology differs.
    OtherRound,
    /// Fails the energy, count, or diversity gate at the current difficulty.
    BelowGates,
    /// Bytes the pallet rejects before scoring: bad packing or a zero spin.
    Malformed(String),
}

impl Verdict {
    /// Whether the proof ends the round.
    #[must_use]
    pub const fn clears(&self) -> bool {
        matches!(self, Self::Clears { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MiningSnapshot {
        MiningSnapshot {
            head_hash: [0u8; 32],
            last_proof_block_hash: [7u8; 32],
            topology_hash: vec![9u8; 32],
            nodes: vec![0, 1],
            edges: vec![(0, 1)],
            allowed_h_milli: vec![-1000, 0, 1000],
            allowed_j_milli: vec![-1000, 1000],
            allowed_spin_milli: vec![-1000, 1000],
            min_solutions: 5,
            max_energy_milli: -14_000_000,
            min_diversity_milli: 200,
            block_number: 42,
            spec_version: 117,
            target_proof_blocks: TARGET_PROOF_BLOCKS,
            family: None,
        }
    }

    #[test]
    fn head_key_changes_when_last_proof_changes() {
        let mut s = sample();
        let k1 = head_state_key(&s);
        s.last_proof_block_hash = [8u8; 32];
        let k2 = head_state_key(&s);
        assert_ne!(k1, k2);
    }
}
