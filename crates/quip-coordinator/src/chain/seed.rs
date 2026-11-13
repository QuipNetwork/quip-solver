//! Root-origin chain seeding: the calls that make a fresh chain mineable.
//!
//! A chain with no `QuantumPow.DefaultTopology` accepts no proofs. A fresh
//! chain is seeded with the dimer1200 family topology: solver-core's live
//! recipe `x-w62-p1200` of `quip/twisted-ice-dimer` revision 1, registered on
//! the topology the caller passes (by default the `zephyr-z12t4-live` preset
//! that recipe is frozen on).
//!
//! `register_topology` does not make a family topology the default, so the
//! sequence is five Root calls, each checking the one before it: register,
//! set the difficulty, set the energy band, whitelist, and set the default.
//!
//! Runtime 120 has no `Sudo` pallet. The calls go in one `Utility::batch_all`
//! that `ValidatorAdmission::dispatch_as_root` runs with the Root origin,
//! proposed as a Foundation motion with threshold 1. A threshold below 2
//! executes at once, so this seeds a chain whose Foundation has one member,
//! which is the `dev` preset (`//Alice`). A larger Foundation needs a vote.
//! Runtimes before 120 have no family topologies and are refused.

use super::real::{Confirmation, RealChainClient, SignedCallOutcome};
use super::scale_types::{
    DifficultyConfig, EnergyBandScale, FamilyDescriptorScale, MinerKind,
    ADD_MINEABLE_TOPOLOGY_CALL_INDEX, BATCH_ALL_CALL_INDEX, DISPATCH_AS_ROOT_CALL_INDEX,
    FOUNDATION_PALLET_INDEX, PROPOSE_CALL_INDEX, QUANTUM_POW_PALLET_INDEX,
    REGISTER_TOPOLOGY_CALL_INDEX, SET_DEFAULT_TOPOLOGY_CALL_INDEX, SET_DIFFICULTY_CALL_INDEX,
    SET_TOPOLOGY_ENERGY_BAND_CALL_INDEX, UTILITY_PALLET_INDEX, VALIDATOR_ADMISSION_PALLET_INDEX,
};
use super::ChainError;
use crate::drive::TopologySpec;
use crate::topology::{family_topology_hash, topology_hash_sets, Topology};
use parity_scale_codec::{Compact, Encode};
use quantum_validation::AllowedValueSpec;

/// First runtime `spec_version` with family topologies and the Foundation
/// root path (quip-validator v0.4.1). It removed `pallet-sudo`.
pub const MIN_SEED_SPEC_VERSION: u32 = 120;

/// Family id of the seeded topology.
pub const DIMER_1200_FAMILY_ID: &str = "quip/twisted-ice-dimer";
/// Family revision of the seeded topology.
pub const DIMER_1200_REVISION: u32 = 1;
/// Canonical parameters of solver-core's live recipe `x-w62-p1200`, cell
/// `ice-twisted-w62-dimer1200`: width 62 (`0x3e`) and 1200 pairs (`0x04b0`).
pub const DIMER_1200_PARAMS: [u8; 34] = [
    0x01, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x0c, 0x00, 0x02, 0x00, 0x01, 0x00, 0x00, 0x00, 0x04,
    0x03, 0x00, 0x02, 0x00, 0x00, 0x00, 0x3e, 0x00, 0x04, 0x00, 0x04, 0x00, 0x00, 0x00, 0xb0, 0x04,
    0x00, 0x00,
];

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

/// The dimer1200 family descriptor.
#[must_use]
pub fn dimer_1200() -> FamilyDescriptorScale {
    FamilyDescriptorScale {
        family_id: DIMER_1200_FAMILY_ID.as_bytes().to_vec(),
        revision: DIMER_1200_REVISION,
        params: DIMER_1200_PARAMS.to_vec(),
    }
}

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
    /// instead of trusting the submit to have encoded correctly.
    ///
    /// # Errors
    /// Returns the admission code when this build refuses the descriptor on
    /// the graph. The pallet would refuse it too.
    pub fn family_hash(&self, descriptor: &FamilyDescriptorScale) -> Result<[u8; 32], String> {
        let family_id = std::str::from_utf8(&descriptor.family_id)
            .map_err(|_| "family id is not UTF-8".to_string())?;
        let ordered = Topology::from_nodes_edges(
            self.nodes.clone(),
            self.edges.clone(),
            &self.allowed_h_milli,
            &self.allowed_j_milli,
            &self.allowed_spin_milli,
        );
        let admitted = crate::lease::ChallengeFamily::admit(
            crate::lease::FAMILY_CONTRACTS,
            family_id,
            descriptor.revision,
            descriptor.params.clone(),
            &ordered,
        )
        .map_err(|code| format!("family {family_id} r{}: {code:?}", descriptor.revision))?;
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
fn quantum_pow_hash_call(call_index: u8, topology_hash: [u8; 32], args: &impl Encode) -> Vec<u8> {
    let mut call = vec![QUANTUM_POW_PALLET_INDEX, call_index];
    call.extend_from_slice(&topology_hash);
    args.encode_to(&mut call);
    call
}

/// The five Root calls that make the family topology the mineable default,
/// in the order each call checks the one before it.
#[must_use]
pub fn seed_root_calls(params: &SeedParams, topology_hash: [u8; 32]) -> Vec<Vec<u8>> {
    vec![
        register_topology_call(&params.topology, &params.descriptor),
        quantum_pow_hash_call(SET_DIFFICULTY_CALL_INDEX, topology_hash, &params.difficulty),
        quantum_pow_hash_call(
            SET_TOPOLOGY_ENERGY_BAND_CALL_INDEX,
            topology_hash,
            &params.band,
        ),
        quantum_pow_hash_call(ADD_MINEABLE_TOPOLOGY_CALL_INDEX, topology_hash, &()),
        quantum_pow_hash_call(SET_DEFAULT_TOPOLOGY_CALL_INDEX, topology_hash, &()),
    ]
}

/// `Foundation.propose(1, ValidatorAdmission.dispatch_as_root(Utility.batch_all(calls)))`.
///
/// Each `Box<RuntimeCall>` encodes as the call itself, and `Vec<RuntimeCall>`
/// as a compact count followed by the calls.
#[must_use]
pub fn encode_foundation_root_motion(calls: &[Vec<u8>]) -> Vec<u8> {
    let mut root = vec![
        VALIDATOR_ADMISSION_PALLET_INDEX,
        DISPATCH_AS_ROOT_CALL_INDEX,
        UTILITY_PALLET_INDEX,
        BATCH_ALL_CALL_INDEX,
    ];
    let count = u32::try_from(calls.len()).unwrap_or(u32::MAX);
    Compact(count).encode_to(&mut root);
    for call in calls {
        root.extend_from_slice(call);
    }
    let mut motion = vec![FOUNDATION_PALLET_INDEX, PROPOSE_CALL_INDEX];
    Compact(1u32).encode_to(&mut motion);
    motion.extend_from_slice(&root);
    let length_bound = u32::try_from(root.len()).unwrap_or(u32::MAX);
    Compact(length_bound).encode_to(&mut motion);
    motion
}

/// Everything `seed_chain` needs.
#[derive(Clone, Debug)]
pub struct SeedParams {
    /// Validator RPC endpoint.
    pub validator: String,
    /// Foundation member signer material, in any form `load_hybrid_pair` accepts.
    pub sudo_key: String,
    /// The graph to register the family topology on.
    pub topology: SeedTopology,
    /// The family descriptor; [`dimer_1200`] from the CLI.
    pub descriptor: FamilyDescriptorScale,
    /// The difficulty to set for that topology.
    pub difficulty: DifficultyConfig,
    /// The energy band, `hard_milli < easy_milli`.
    pub band: EnergyBandScale,
}

/// What `seed_chain` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedReport {
    /// The registered family topology hash, confirmed by reading it back.
    pub topology_hash: [u8; 32],
    /// Node count in the registered graph.
    pub nodes: usize,
    /// Edge count in the registered graph.
    pub edges: usize,
    /// Block that included the Foundation motion.
    pub block: String,
}

/// Register the family topology as the chain default, with its difficulty
/// and band.
///
/// Refuses to run when `DefaultTopology` is already set: a chain can only be
/// seeded once, so a seeded chain has to be wiped rather than re-seeded.
///
/// # Errors
/// Returns an error when the validator is unreachable, when the runtime is
/// older than [`MIN_SEED_SPEC_VERSION`], when a topology is already the
/// default, when this build refuses the descriptor on the graph, when the
/// motion fails, or when the default the chain stored is not the family
/// topology hash computed locally.
pub async fn seed_chain(params: SeedParams) -> Result<SeedReport, ChainError> {
    let client = RealChainClient::new(
        vec![params.validator.clone()],
        params.sudo_key.clone(),
        MinerKind::Cpu,
    );

    let spec_version = super::real::parse_spec_version(&client.runtime_version_raw().await?)?;
    if spec_version < MIN_SEED_SPEC_VERSION {
        return Err(ChainError::Submit(format!(
            "runtime {spec_version} has no family topologies; seed-chain needs \
             runtime {MIN_SEED_SPEC_VERSION} or later"
        )));
    }

    if let Some(existing) = client.default_topology().await? {
        return Err(ChainError::Submit(format!(
            "DefaultTopology is already set to 0x{}; a chain can only be seeded \
             once, so wipe the chain data and restart the validator first",
            hex_lower(&existing)
        )));
    }

    let expected = params
        .topology
        .family_hash(&params.descriptor)
        .map_err(ChainError::Submit)?;
    let motion = encode_foundation_root_motion(&seed_root_calls(&params, expected));
    let block = dispatch(
        &client,
        &motion,
        "Foundation.propose(dispatch_as_root(batch_all))",
    )
    .await?;

    // The motion and the Root dispatch both succeed as extrinsics even when
    // the batch inside fails; only the events carry that result. The batch is
    // all-or-nothing and its last call sets the default, so the stored
    // default equal to the local hash shows that every call landed.
    let stored = client.default_topology().await?;
    if stored != Some(expected) {
        return Err(ChainError::Submit(format!(
            "the motion was included in {block}, but DefaultTopology is {} \
             instead of 0x{}; read the Foundation.Executed and \
             ValidatorAdmission.RootDispatched events of that block for the \
             inner error",
            stored.map_or_else(|| "unset".to_string(), |h| format!("0x{}", hex_lower(&h))),
            hex_lower(&expected)
        )));
    }
    Ok(SeedReport {
        topology_hash: expected,
        nodes: params.topology.nodes.len(),
        edges: params.topology.edges.len(),
        block,
    })
}

/// Submit the motion and return the block that included it.
async fn dispatch(client: &RealChainClient, call: &[u8], what: &str) -> Result<String, ChainError> {
    tracing::info!(call = what, "submitting root call");
    let (outcome, _) = client
        .submit_signed_call(call, Confirmation::DefaultTopology)
        .await?;
    match outcome {
        SignedCallOutcome::Success { block, .. } => {
            tracing::info!(call = what, block = %block, "root call included");
            Ok(block)
        }
        SignedCallOutcome::DispatchFailed { error, .. } => Err(ChainError::Submit(format!(
            "{what} was included but the dispatch failed: {error}"
        ))),
        SignedCallOutcome::Invalid { message } => Err(ChainError::Submit(format!(
            "{what} was rejected by the transaction pool: {message}"
        ))),
        SignedCallOutcome::Dropped { message } => Err(ChainError::Submit(format!(
            "{what} was dropped before inclusion: {message}"
        ))),
    }
}

/// Lower-case hex without a prefix, for error text.
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> SeedTopology {
        let text = crate::presets::preset_spec("zephyr-z12t4-live").unwrap();
        SeedTopology::from_spec(&crate::drive::parse_topology_spec(text).unwrap())
    }

    fn params(topology: SeedTopology) -> SeedParams {
        SeedParams {
            validator: String::new(),
            sudo_key: String::new(),
            topology,
            descriptor: dimer_1200(),
            difficulty: DEFAULT_SEED_DIFFICULTY,
            band: DEFAULT_ENERGY_BAND,
        }
    }

    /// The built-in parameters are the recipe bytes solver-core freezes.
    #[test]
    fn the_dimer_1200_params_are_the_solver_core_recipe() {
        assert_eq!(
            crate::chain::extrinsic::hex_encode(&DIMER_1200_PARAMS),
            "0x010100020000000c00020001000000040300020000003e00040004000000b0040000"
        );
    }

    /// dimer1200 admits on the live preset, and its family hash differs from
    /// the legacy hash of the same graph.
    #[test]
    fn dimer_1200_admits_on_the_live_preset() {
        let topology = live();
        let hash = topology.family_hash(&dimer_1200()).unwrap();
        assert_ne!(hash, topology.legacy_hash());
    }

    /// The seed is five Root calls in the order each one checks the one before
    /// it, and registration carries `Some(descriptor)` last.
    #[test]
    #[expect(
        clippy::indexing_slicing,
        reason = "the call count and each fixed-width prefix are asserted first"
    )]
    fn the_seed_sends_the_activation_calls_in_order() {
        let p = params(live());
        let hash = [9u8; 32];
        let calls = seed_root_calls(&p, hash);
        let prefixes: Vec<[u8; 2]> = calls.iter().map(|c| [c[0], c[1]]).collect();
        assert_eq!(
            prefixes,
            [
                [QUANTUM_POW_PALLET_INDEX, REGISTER_TOPOLOGY_CALL_INDEX],
                [QUANTUM_POW_PALLET_INDEX, SET_DIFFICULTY_CALL_INDEX],
                [
                    QUANTUM_POW_PALLET_INDEX,
                    SET_TOPOLOGY_ENERGY_BAND_CALL_INDEX
                ],
                [QUANTUM_POW_PALLET_INDEX, ADD_MINEABLE_TOPOLOGY_CALL_INDEX],
                [QUANTUM_POW_PALLET_INDEX, SET_DEFAULT_TOPOLOGY_CALL_INDEX],
            ]
        );
        let mut tail = vec![1u8];
        dimer_1200().encode_to(&mut tail);
        assert!(calls[0].ends_with(&tail));
        assert_eq!(&calls[1][2..34], &hash[..]);
        assert_eq!(&calls[1][34..], &DEFAULT_SEED_DIFFICULTY.encode()[..]);
        assert_eq!(&calls[2][34..], &DEFAULT_ENERGY_BAND.encode()[..]);
        assert_eq!(
            calls[3].len(),
            34,
            "add_mineable_topology takes only the hash"
        );
        assert_eq!(
            calls[4].len(),
            34,
            "set_default_topology takes only the hash"
        );
    }

    /// The motion is `propose(Compact(1), dispatch_as_root(batch_all(calls)),
    /// Compact(len))`, and the length bound is the inner call length.
    #[test]
    fn the_foundation_motion_wraps_the_batch_in_root_dispatch() {
        let calls = vec![vec![10, 3, 0xaa], vec![10, 6, 0xbb, 0xcc]];
        let motion = encode_foundation_root_motion(&calls);
        let root = [
            VALIDATOR_ADMISSION_PALLET_INDEX,
            DISPATCH_AS_ROOT_CALL_INDEX,
            UTILITY_PALLET_INDEX,
            BATCH_ALL_CALL_INDEX,
            0x08,
            10,
            3,
            0xaa,
            10,
            6,
            0xbb,
            0xcc,
        ];
        let mut expected = vec![FOUNDATION_PALLET_INDEX, PROPOSE_CALL_INDEX, 0x04];
        expected.extend_from_slice(&root);
        Compact(u32::try_from(root.len()).unwrap()).encode_to(&mut expected);
        assert_eq!(motion, expected);
    }

    /// `parse_topology_spec` builds `Topology::hash` through the same canonical
    /// hash function as `legacy_hash`, and the hash ignores node and edge order.
    #[test]
    fn a_topology_spec_converts_to_a_seed_topology_with_the_same_hash() {
        for preset in crate::presets::PRESET_NAMES {
            let text = crate::presets::preset_spec(preset).unwrap();
            let spec = crate::drive::parse_topology_spec(text).unwrap();
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
