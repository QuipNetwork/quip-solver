//! Seeding a fresh chain: the topology and difficulty that `seed-chain`
//! registers, and the command itself.
//!
//! A chain with no `QuantumPow.DefaultTopology` accepts no proofs. `seed-chain`
//! seeds a fresh chain with the dimer1200 family topology: solver-core's live
//! recipe `x-w62-p1200` of `quip/twisted-ice-dimer` revision 1. The activation
//! calls are built in [`super::set_family`], and the Foundation motion that
//! carries them is in [`super::foundation`]. Runtimes before 120 are refused.

use crate::chain::client::RealChainClient;
use crate::chain::codec::quantum_pow::{
    DifficultyConfig, EnergyBandScale, FamilyDescriptorScale, QUANTUM_POW_PALLET_INDEX,
    REGISTER_TOPOLOGY_CALL_INDEX,
};
use crate::chain::governance::set_family::{apply_family, ExistingDefault, FamilyOutcome};
use crate::chain::ChainError;
use crate::types::miner::MinerKind;
use crate::types::topology_spec::TopologySpec;

use crate::types::topology::{family_topology_hash, topology_hash_sets, Topology};
use parity_scale_codec::Encode;
use quantum_validation::families::{AdvertisedLimits, FamilyChallenge, CONTRACTS};
use quantum_validation::AllowedValueSpec;
use sp_core::crypto::{AccountId32, Ss58Codec};

/// First runtime `spec_version` with the Foundation root path. quip-validator
/// v0.4.1 and v0.5.0 both report 120, and only v0.5.0 has family topologies;
/// on v0.4.1 the batch fails inside the motion and the default never moves.
pub const MIN_SEED_SPEC_VERSION: u32 = 120;

/// Node cap the runtime admits a family under (`QuantumPowMaxNodes`).
pub const RUNTIME_MAX_NODES: u64 = 5_000;

/// Edge cap the runtime admits a family under (`QuantumPowMaxEdges`).
pub const RUNTIME_MAX_EDGES: u64 = 50_000;

/// The energy band a fresh chain starts on. `quip-cpu-otsuka` reaches
/// -4,917,712 to -4,946,962 milli on dimer1200 in about two seconds per
/// four-salt lease, on both the live graph and the older Advantage2 graph. The
/// easy end sits above that so the first proof lands, and the hard end below
/// it so the controller has room to tighten.
pub const DEFAULT_ENERGY_BAND: EnergyBandScale = EnergyBandScale {
    hard_milli: -5_100_000,
    easy_milli: -4_800_000,
};

/// The difficulty a fresh chain starts at: a ceiling inside the band about
/// 80,000 milli above the measured solves, and one solution with no diversity
/// gate so a single CPU miner lands the first qblock. Raise the gates with
/// `set_difficulty` once more miners join.
pub const DEFAULT_SEED_DIFFICULTY: DifficultyConfig = DifficultyConfig {
    min_solutions: 1,
    max_energy_milli: -4_850_000,
    min_diversity_milli: 0,
};

// The band is ordered and the first ceiling sits inside it.
const _: () = {
    assert!(DEFAULT_ENERGY_BAND.hard_milli < DEFAULT_ENERGY_BAND.easy_milli);
    assert!(DEFAULT_SEED_DIFFICULTY.max_energy_milli > DEFAULT_ENERGY_BAND.hard_milli);
    assert!(DEFAULT_SEED_DIFFICULTY.max_energy_milli < DEFAULT_ENERGY_BAND.easy_milli);
};

/// The graph and allowed-value sets a `register_topology` call carries.
///
/// Every allowed-value argument uses the `Set` variant. The other variants
/// (`IntegerRange`, `ContinuousRange`) describe sampling ranges that no
/// hardware topology has needed, so the seed path does not offer them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedTopology {
    /// Graph node ids.
    pub nodes: Vec<u32>,
    /// Graph edges as endpoint pairs.
    pub edges: Vec<(u32, u32)>,
    /// Allowed per-node h field values, in milli units.
    pub allowed_h_milli: Vec<i32>,
    /// Allowed per-edge j coupling values, in milli units.
    pub allowed_j_milli: Vec<i32>,
    /// Allowed solution spin values, in milli units.
    pub allowed_spin_milli: Vec<i32>,
}

impl SeedTopology {
    /// Build a seed topology from a parsed topology spec.
    #[must_use]
    pub fn from_spec(spec: &TopologySpec) -> Self {
        Self {
            nodes: spec.topology.nodes.clone(),
            edges: spec.topology.edge_pairs(),
            allowed_h_milli: spec.allowed_h_milli.clone(),
            allowed_j_milli: spec.allowed_j_milli.clone(),
            allowed_spin_milli: spec.allowed_spin_milli.clone(),
        }
    }

    /// The legacy topology hash of this graph and its value sets. A family
    /// topology hash commits to it.
    #[must_use]
    pub fn legacy_hash(&self) -> [u8; 32] {
        topology_hash_sets(
            &self.nodes,
            &self.edges,
            &self.allowed_h_milli,
            &self.allowed_j_milli,
            &self.allowed_spin_milli,
        )
    }

    /// The family topology hash the pallet stores for this graph under
    /// `descriptor`.
    ///
    /// Computed locally so the caller can confirm what the chain stored
    /// instead of trusting the submit to have encoded correctly. The
    /// descriptor must pass two admissions on the graph: the runtime's
    /// (`quantum_validation`'s allowlist under the runtime's size caps, which
    /// is what `register_topology` runs) and the coordinator's lease library,
    /// which is what lets the coordinator mine it. Both must derive the same
    /// digests, or the coordinator would mine a topology the chain does not
    /// hold.
    ///
    /// # Errors
    /// Returns the refusal when either admission refuses the descriptor on the
    /// graph, or when the two disagree on a digest.
    pub fn family_hash(&self, descriptor: &FamilyDescriptorScale) -> Result<[u8; 32], String> {
        let family_id = std::str::from_utf8(&descriptor.family_id)
            .map_err(|_| "family id is not UTF-8".to_string())?;
        let what = format!("family {family_id} r{}", descriptor.revision);
        let runtime = FamilyChallenge::admit(
            CONTRACTS,
            &descriptor.family_id,
            descriptor.revision,
            &descriptor.params,
            &self.nodes,
            &self.edges,
            &AdvertisedLimits {
                max_nodes: RUNTIME_MAX_NODES,
                max_edges: RUNTIME_MAX_EDGES,
                max_degree: 0,
            },
        )
        .map_err(|e| format!("the runtime refuses {what} on this graph: {e:?}"))?;
        let ordered = Topology::from_nodes_edges(
            self.nodes.clone(),
            self.edges.clone(),
            &self.allowed_h_milli,
            &self.allowed_j_milli,
            &self.allowed_spin_milli,
        );
        let admitted = crate::types::family::ChallengeFamily::admit(
            crate::types::family::FAMILY_CONTRACTS,
            family_id,
            descriptor.revision,
            descriptor.params.clone(),
            &ordered,
        )
        .map_err(|code| format!("the coordinator refuses {what} on this graph: {code:?}"))?;
        if runtime.descriptor_digest() != *admitted.descriptor_digest()
            || runtime.topology_order_digest() != *admitted.topology_order_digest()
        {
            return Err(format!(
                "the runtime and the coordinator derive different digests for {what}"
            ));
        }
        Ok(family_topology_hash(
            self.legacy_hash(),
            *admitted.descriptor_digest(),
            *admitted.topology_order_digest(),
        ))
    }
}

/// `QuantumPow.register_topology(nodes, edges, h, j, spin, Some(descriptor))`.
#[must_use]
pub fn register_topology_call(
    topology: &SeedTopology,
    descriptor: &FamilyDescriptorScale,
) -> Vec<u8> {
    let mut call = vec![QUANTUM_POW_PALLET_INDEX, REGISTER_TOPOLOGY_CALL_INDEX];
    (
        topology.nodes.clone(),
        topology.edges.clone(),
        AllowedValueSpec::Set(topology.allowed_h_milli.clone()),
        AllowedValueSpec::Set(topology.allowed_j_milli.clone()),
        AllowedValueSpec::Set(topology.allowed_spin_milli.clone()),
        Some(descriptor.clone()),
    )
        .encode_to(&mut call);
    call
}

/// A `QuantumPow` call that takes a topology hash and then `args`.
pub(super) fn quantum_pow_hash_call(
    call_index: u8,
    topology_hash: [u8; 32],
    args: &impl Encode,
) -> Vec<u8> {
    let mut call = vec![QUANTUM_POW_PALLET_INDEX, call_index];
    call.extend_from_slice(&topology_hash);
    args.encode_to(&mut call);
    call
}

/// Everything an activation needs.
#[derive(Clone, Debug)]
pub struct SeedParams {
    /// Validator RPC endpoint.
    pub validator: String,
    /// Foundation member signer material, in any form `load_hybrid_pair`
    /// accepts. Unused, and may be empty, for a plan.
    pub sudo_key: String,
    /// The graph to register the family topology on.
    pub topology: SeedTopology,
    /// The family descriptor.
    pub descriptor: FamilyDescriptorScale,
    /// The difficulty to set for that topology.
    pub difficulty: DifficultyConfig,
    /// The energy band, `hard_milli < easy_milli`.
    pub band: EnergyBandScale,
}

/// SS58 text of an account, for messages.
#[must_use]
pub fn ss58(account: &[u8; 32]) -> String {
    AccountId32::from(*account).to_ss58check()
}

pub(super) fn client_for(params: &SeedParams) -> RealChainClient {
    RealChainClient::new(
        vec![params.validator.clone()],
        params.sudo_key.clone(),
        MinerKind::Cpu,
    )
}

/// `seed-chain`: seed a fresh chain, refusing one that already has a default.
///
/// # Errors
/// As [`apply_family`] with [`ExistingDefault::Refuse`].
pub async fn seed_chain(params: SeedParams) -> Result<FamilyOutcome, ChainError> {
    apply_family(&params, ExistingDefault::Refuse).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parse_topology_spec` builds `Topology::hash` through the same canonical
    /// hash function as `legacy_hash`, and the hash ignores node and edge order.
    #[test]
    fn a_topology_spec_converts_to_a_seed_topology_with_the_same_hash() {
        for preset in crate::types::topology_spec::PRESET_NAMES {
            let text = crate::types::topology_spec::preset_spec(preset).unwrap();
            let spec = crate::types::topology_spec::parse_topology_spec(text).unwrap();
            let seed = SeedTopology::from_spec(&spec);
            assert_eq!(seed.nodes, spec.topology.nodes, "{preset}");
            assert_eq!(seed.edges, spec.topology.edge_pairs(), "{preset}");
            assert_eq!(seed.legacy_hash().to_vec(), spec.topology.hash, "{preset}");
        }
        let tiny = SeedTopology {
            nodes: vec![0, 1, 2],
            edges: vec![(0, 1), (1, 2)],
            allowed_h_milli: vec![0],
            allowed_j_milli: vec![-1000, 1000],
            allowed_spin_milli: vec![-1000, 1000],
        };
        let mut reordered = tiny.clone();
        reordered.nodes = vec![2, 0, 1];
        reordered.edges = vec![(1, 2), (0, 1)];
        assert_eq!(tiny.legacy_hash(), reordered.legacy_hash());
    }
}
