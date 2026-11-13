//! Root-origin chain seeding: the two `Sudo`-wrapped calls that make a fresh
//! chain mineable.
//!
//! A chain with no `QuantumPow.DefaultTopology` accepts no proofs, so a fresh
//! network needs one root call to register a topology and a second to set its
//! difficulty. `register_topology` writes `DefaultTopology` and the
//! `MineableTopologies` whitelist entry itself when no default exists yet, so
//! those two calls are the whole sequence.
//!
//! This module encodes the calls and drives the seed sequence.
//!
//! Runtime 119 and earlier wrap each call in `Sudo`. Runtime 120 removed
//! `pallet-sudo`, so there the calls go in one `Utility::batch_all` that
//! `ValidatorAdmission::dispatch_as_root` runs with the Root origin, proposed
//! as a Foundation motion with threshold 1. A threshold below 2 executes at
//! once, so this path seeds a chain whose Foundation has one member, which
//! is the `dev` preset (`//Alice`). A larger Foundation needs a real vote.
//!
//! A family topology is not made the default by `register_topology`, so its
//! sequence adds the energy band, the whitelist entry, and the default.

use super::real::{Confirmation, RealChainClient, SignedCallOutcome};
use super::scale_types::{
    DifficultyConfig, EnergyBandScale, FamilyDescriptorScale, MinerKind,
    ADD_MINEABLE_TOPOLOGY_CALL_INDEX, BATCH_ALL_CALL_INDEX, DISPATCH_AS_ROOT_CALL_INDEX,
    FOUNDATION_PALLET_INDEX, PROPOSE_CALL_INDEX, QUANTUM_POW_PALLET_INDEX,
    REGISTER_TOPOLOGY_CALL_INDEX, SET_DEFAULT_TOPOLOGY_CALL_INDEX, SET_DIFFICULTY_CALL_INDEX,
    SET_TOPOLOGY_ENERGY_BAND_CALL_INDEX, SUDO_CALL_INDEX, SUDO_PALLET_INDEX,
    SUDO_REMOVED_SPEC_VERSION, UTILITY_PALLET_INDEX, VALIDATOR_ADMISSION_PALLET_INDEX,
};
use super::ChainError;
use crate::drive::TopologySpec;
use crate::topology::{family_topology_hash, topology_hash_sets, Topology};
use parity_scale_codec::{Compact, Encode};
use quantum_validation::AllowedValueSpec;

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
    /// The chain-canonical topology hash this graph registers under.
    ///
    /// Computed locally so the caller can confirm what the chain stored
    /// instead of trusting the submit to have encoded correctly.
    #[must_use]
    pub fn topology_hash(&self) -> [u8; 32] {
        topology_hash_sets(
            &self.nodes,
            &self.edges,
            &self.allowed_h_milli,
            &self.allowed_j_milli,
            &self.allowed_spin_milli,
        )
    }
}

/// Encode `Sudo.sudo(QuantumPow.register_topology(..))`.
///
/// `Box<RuntimeCall>` encodes as the `RuntimeCall` itself, so the wrapped call
/// is the sudo prefix followed by the inner call bytes with no length prefix
/// between them.
#[must_use]
pub fn encode_register_topology(topology: &SeedTopology) -> Vec<u8> {
    let mut call = vec![
        SUDO_PALLET_INDEX,
        SUDO_CALL_INDEX,
        QUANTUM_POW_PALLET_INDEX,
        REGISTER_TOPOLOGY_CALL_INDEX,
    ];
    (
        topology.nodes.clone(),
        topology.edges.clone(),
        AllowedValueSpec::Set(topology.allowed_h_milli.clone()),
        AllowedValueSpec::Set(topology.allowed_j_milli.clone()),
        AllowedValueSpec::Set(topology.allowed_spin_milli.clone()),
    )
        .encode_to(&mut call);
    call
}

/// Encode `Sudo.sudo(QuantumPow.set_difficulty(topology_hash, difficulty))`.
#[must_use]
pub fn encode_set_difficulty(topology_hash: [u8; 32], difficulty: &DifficultyConfig) -> Vec<u8> {
    let mut call = vec![
        SUDO_PALLET_INDEX,
        SUDO_CALL_INDEX,
        QUANTUM_POW_PALLET_INDEX,
        SET_DIFFICULTY_CALL_INDEX,
    ];
    call.extend_from_slice(&topology_hash);
    difficulty.encode_to(&mut call);
    call
}

/// A family descriptor to register with the topology, and the energy band
/// the difficulty controller runs on for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedFamily {
    /// Family id, revision, and canonical parameter bytes.
    pub descriptor: FamilyDescriptorScale,
    /// Root-set band, `hard_milli < easy_milli`.
    pub band: EnergyBandScale,
}

impl SeedFamily {
    /// The family topology hash the pallet stores for `topology`.
    ///
    /// # Errors
    /// Returns the admission code when this build refuses the descriptor on
    /// the topology. The pallet would refuse it too.
    pub fn topology_hash(&self, topology: &SeedTopology) -> Result<[u8; 32], String> {
        let family_id = std::str::from_utf8(&self.descriptor.family_id)
            .map_err(|_| "family id is not UTF-8".to_string())?;
        let ordered = Topology::from_nodes_edges(
            topology.nodes.clone(),
            topology.edges.clone(),
            &topology.allowed_h_milli,
            &topology.allowed_j_milli,
            &topology.allowed_spin_milli,
        );
        let admitted = crate::lease::ChallengeFamily::admit(
            crate::lease::FAMILY_CONTRACTS,
            family_id,
            self.descriptor.revision,
            self.descriptor.params.clone(),
            &ordered,
        )
        .map_err(|code| format!("family {family_id} r{}: {code:?}", self.descriptor.revision))?;
        Ok(family_topology_hash(
            topology.topology_hash(),
            *admitted.descriptor_digest(),
            *admitted.topology_order_digest(),
        ))
    }
}

/// `QuantumPow.register_topology` with the runtime 120 seventh argument, the
/// optional family descriptor. No `Sudo` prefix.
#[must_use]
pub fn register_topology_call(topology: &SeedTopology, family: Option<&SeedFamily>) -> Vec<u8> {
    let mut call = vec![QUANTUM_POW_PALLET_INDEX, REGISTER_TOPOLOGY_CALL_INDEX];
    (
        topology.nodes.clone(),
        topology.edges.clone(),
        AllowedValueSpec::Set(topology.allowed_h_milli.clone()),
        AllowedValueSpec::Set(topology.allowed_j_milli.clone()),
        AllowedValueSpec::Set(topology.allowed_spin_milli.clone()),
        family.map(|f| f.descriptor.clone()),
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

/// The Root calls that make `topology` the mineable default on runtime 120.
///
/// A legacy topology becomes the default when it is registered on a chain
/// with none. A family topology does not, so it also needs its band, its
/// whitelist entry, and the default pointer, in the order each call checks.
#[must_use]
pub fn seed_root_calls(
    topology: &SeedTopology,
    family: Option<&SeedFamily>,
    topology_hash: [u8; 32],
    difficulty: &DifficultyConfig,
) -> Vec<Vec<u8>> {
    let mut calls = vec![
        register_topology_call(topology, family),
        quantum_pow_hash_call(SET_DIFFICULTY_CALL_INDEX, topology_hash, difficulty),
    ];
    if let Some(family) = family {
        calls.push(quantum_pow_hash_call(
            SET_TOPOLOGY_ENERGY_BAND_CALL_INDEX,
            topology_hash,
            &family.band,
        ));
        calls.push(quantum_pow_hash_call(
            ADD_MINEABLE_TOPOLOGY_CALL_INDEX,
            topology_hash,
            &(),
        ));
        calls.push(quantum_pow_hash_call(
            SET_DEFAULT_TOPOLOGY_CALL_INDEX,
            topology_hash,
            &(),
        ));
    }
    calls
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

/// The difficulty a fresh chain starts at. These are the values the public
/// testnet was seeded with. They are deliberately loose: a chain with no
/// proof history has no difficulty curve to sit on, and a CPU miner has to be
/// able to land the first proof.
pub const DEFAULT_SEED_DIFFICULTY: DifficultyConfig = DifficultyConfig {
    min_solutions: 5,
    max_energy_milli: -2_500_000,
    min_diversity_milli: 200,
};

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
}

/// Everything `seed_chain` needs.
#[derive(Clone, Debug)]
pub struct SeedParams {
    /// Validator RPC endpoint.
    pub validator: String,
    /// Sudo signer material, in any form `load_hybrid_pair` accepts.
    pub sudo_key: String,
    /// The topology to register as the chain default.
    pub topology: SeedTopology,
    /// The difficulty to set for that topology.
    pub difficulty: DifficultyConfig,
    /// A family to register the topology under. Runtime 120 and later only.
    pub family: Option<SeedFamily>,
}

/// What `seed_chain` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedReport {
    /// The registered topology hash, confirmed by reading it back.
    pub topology_hash: [u8; 32],
    /// Node count in the registered graph.
    pub nodes: usize,
    /// Edge count in the registered graph.
    pub edges: usize,
    /// Whether the topology carries a family descriptor.
    pub family: bool,
    /// Each submitted extrinsic and the block that included it.
    pub included: Vec<(&'static str, String)>,
}

/// Register `params.topology` as the chain default and set its difficulty.
///
/// Runtime 119 and earlier take two `Sudo` calls. Runtime 120 and later take
/// one Foundation motion (see the module docs), and only there can the
/// topology carry a family. Refuses to run when `DefaultTopology` is already set. The pallet writes that
/// value only when it is empty (`register_topology`), and a second registration
/// would land a topology that no miner selects, so a chain that is already
/// seeded has to be wiped rather than re-seeded.
///
/// # Errors
/// Returns an error when the validator is unreachable, when a family is given
/// for a runtime before 120, when a topology is already registered, when a
/// call fails to dispatch, or when the hash the chain stored does not match
/// the hash computed locally.
pub async fn seed_chain(params: SeedParams) -> Result<SeedReport, ChainError> {
    let client = RealChainClient::new(
        vec![params.validator.clone()],
        params.sudo_key.clone(),
        MinerKind::Cpu,
    );

    let spec_version = super::real::parse_spec_version(&client.runtime_version_raw().await?)?;

    if let Some(existing) = client.default_topology().await? {
        return Err(ChainError::Submit(format!(
            "DefaultTopology is already set to 0x{}; a chain can only be seeded \
             once, so wipe the chain data and restart the validator first",
            hex_lower(&existing)
        )));
    }

    if spec_version >= SUDO_REMOVED_SPEC_VERSION {
        return seed_through_foundation(&client, &params).await;
    }
    if params.family.is_some() {
        return Err(ChainError::Submit(format!(
            "runtime {spec_version} has no family topologies; they start at \
             {SUDO_REMOVED_SPEC_VERSION}"
        )));
    }
    seed_through_sudo(&client, &params).await
}

/// The runtime 120 path: one Foundation motion that runs every Root call.
async fn seed_through_foundation(
    client: &RealChainClient,
    params: &SeedParams,
) -> Result<SeedReport, ChainError> {
    let family = params.family.as_ref();
    let expected = match family {
        Some(family) => family
            .topology_hash(&params.topology)
            .map_err(ChainError::Submit)?,
        None => params.topology.topology_hash(),
    };
    let calls = seed_root_calls(&params.topology, family, expected, &params.difficulty);
    let motion = encode_foundation_root_motion(&calls);
    let block = dispatch(
        client,
        &motion,
        "Foundation.propose(dispatch_as_root(batch_all))",
        Confirmation::DefaultTopology,
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
        family: family.is_some(),
        included: vec![("Foundation root motion", block)],
    })
}

/// The runtime 119 path: two `Sudo` calls.
async fn seed_through_sudo(
    client: &RealChainClient,
    params: &SeedParams,
) -> Result<SeedReport, ChainError> {
    let expected = params.topology.topology_hash();
    let register_block = dispatch(
        client,
        &encode_register_topology(&params.topology),
        "register_topology",
        Confirmation::DefaultTopology,
    )
    .await?;

    // Confirm the chain stored the hash computed locally. This checks the whole
    // encoding chain in one comparison: argument order, bounded-vector
    // mirroring, allowed-value variant bytes, and canonical ordering.
    let stored = client.default_topology().await?.ok_or_else(|| {
        ChainError::Submit(
            "register_topology was included but DefaultTopology is still unset; \
             the call failed inner validation"
                .into(),
        )
    })?;
    if stored != expected {
        return Err(ChainError::Submit(format!(
            "chain registered topology 0x{} but this build computed 0x{}; \
             the call encoding and the hash function disagree",
            hex_lower(&stored),
            hex_lower(&expected)
        )));
    }

    let difficulty_block = dispatch(
        client,
        &encode_set_difficulty(expected, &params.difficulty),
        "set_difficulty",
        Confirmation::Difficulty {
            topology_hash: expected,
        },
    )
    .await?;

    Ok(SeedReport {
        topology_hash: expected,
        nodes: params.topology.nodes.len(),
        edges: params.topology.edges.len(),
        family: false,
        included: vec![
            ("register_topology", register_block),
            ("set_difficulty", difficulty_block),
        ],
    })
}

/// Submit one call and return the block that included it.
async fn dispatch(
    client: &RealChainClient,
    call: &[u8],
    what: &str,
    confirmation: Confirmation,
) -> Result<String, ChainError> {
    tracing::info!(call = what, "submitting root call");
    let (outcome, _) = client.submit_signed_call(call, confirmation).await?;
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

    fn tiny() -> SeedTopology {
        SeedTopology {
            nodes: vec![0, 1, 2],
            edges: vec![(0, 1), (1, 2)],
            allowed_h_milli: vec![-1000, 0, 1000],
            allowed_j_milli: vec![-1000, 1000],
            allowed_spin_milli: vec![-1000, 1000],
        }
    }

    /// The call is the sudo and inner prefixes, then the arguments as the
    /// pallet tuple, with each allowed-value list as the `Set` variant.
    #[test]
    fn register_topology_call_is_the_prefixes_then_the_pallet_tuple() {
        let t = tiny();
        let call = encode_register_topology(&t);
        let expected = (
            t.nodes.clone(),
            t.edges.clone(),
            AllowedValueSpec::Set(t.allowed_h_milli.clone()),
            AllowedValueSpec::Set(t.allowed_j_milli.clone()),
            AllowedValueSpec::Set(t.allowed_spin_milli),
        )
            .encode();
        // nodes: Compact(3) = 0x0c, then 3 * u32 = 12 bytes -> 13 bytes.
        // edges: Compact(2) = 0x08, then 2 * (u32,u32) = 16 bytes -> 17 bytes.
        let h_start = 4 + 13 + 17;
        #[expect(
            clippy::indexing_slicing,
            reason = "a well-formed call is the four prefix bytes then the argument block"
        )]
        {
            assert_eq!(call[0], SUDO_PALLET_INDEX);
            assert_eq!(call[1], SUDO_CALL_INDEX);
            assert_eq!(call[2], QUANTUM_POW_PALLET_INDEX);
            assert_eq!(call[3], REGISTER_TOPOLOGY_CALL_INDEX);
            assert_eq!(&call[4..], &expected[..]);
            assert_eq!(call[h_start], 0, "AllowedValueSpec::Set is variant 0");
            assert_eq!(call[h_start + 1], 0x0c, "three h values, Compact(3)");
        }
    }

    #[test]
    fn set_difficulty_call_carries_the_hash_then_the_config() {
        let hash = [7u8; 32];
        let difficulty = DifficultyConfig {
            min_solutions: 5,
            max_energy_milli: -2_500_000,
            min_diversity_milli: 200,
        };
        let call = encode_set_difficulty(hash, &difficulty);
        #[expect(
            clippy::indexing_slicing,
            reason = "a well-formed call is prefix plus 32-byte hash plus config"
        )]
        {
            assert_eq!(call[0], SUDO_PALLET_INDEX);
            assert_eq!(call[1], SUDO_CALL_INDEX);
            assert_eq!(call[2], QUANTUM_POW_PALLET_INDEX);
            assert_eq!(call[3], SET_DIFFICULTY_CALL_INDEX);
            assert_eq!(&call[4..36], &hash[..], "H256 encodes as 32 raw bytes");
            assert_eq!(&call[36..], &difficulty.encode()[..]);
        }
    }

    #[test]
    fn a_seed_topology_hashes_to_the_canonical_chain_hash() {
        let t = tiny();
        assert_eq!(
            t.topology_hash(),
            t.topology_hash(),
            "hash is deterministic"
        );
        let mut reordered = tiny();
        reordered.nodes = vec![2, 0, 1];
        reordered.edges = vec![(1, 2), (0, 1)];
        assert_eq!(
            t.topology_hash(),
            reordered.topology_hash(),
            "the chain hash canonicalizes node and edge order"
        );
    }

    /// `parse_topology_spec` builds `Topology::hash` through the same canonical
    /// hash function. If these ever disagree, the confirmation read in
    /// `seed_chain` would compare against the wrong value.
    #[test]
    fn a_topology_spec_converts_to_a_seed_topology_with_the_same_hash() {
        for preset in ["smoke", "advantage2-system1"] {
            let text = crate::presets::preset_spec(preset).unwrap();
            let spec = crate::drive::parse_topology_spec(text).unwrap();
            let seed = SeedTopology::from_spec(&spec);
            assert_eq!(seed.nodes, spec.topology.nodes, "{preset}");
            assert_eq!(seed.edges, spec.topology.edge_pairs(), "{preset}");
            assert_eq!(seed.allowed_h_milli, spec.allowed_h_milli, "{preset}");
            assert_eq!(seed.allowed_spin_milli, spec.allowed_spin_milli, "{preset}");
            assert_eq!(
                seed.topology_hash().to_vec(),
                spec.topology.hash,
                "{preset}"
            );
        }
    }

    /// The solver-core live recipe `x-w62-p1200` of `quip/twisted-ice-dimer`
    /// revision 1, frozen for the z(12, 4) live graph.
    const TWISTED_W62_P1200: &str =
        "010100020000000c00020001000000040300020000003e00040004000000b0040000";

    fn twisted_dimer() -> SeedFamily {
        SeedFamily {
            descriptor: FamilyDescriptorScale {
                family_id: b"quip/twisted-ice-dimer".to_vec(),
                revision: 1,
                params: crate::chain::extrinsic::hex_decode(TWISTED_W62_P1200).unwrap(),
            },
            band: EnergyBandScale {
                hard_milli: -9_000_000,
                easy_milli: -1_000_000,
            },
        }
    }

    fn advantage2() -> SeedTopology {
        let text = crate::presets::preset_spec("advantage2-system1").unwrap();
        SeedTopology::from_spec(&crate::drive::parse_topology_spec(text).unwrap())
    }

    /// The twisted dimer recipe admits on the Advantage2 preset, and its family
    /// hash differs from the legacy hash of the same graph.
    #[test]
    fn the_twisted_dimer_recipe_admits_on_advantage2() {
        let topology = advantage2();
        let hash = twisted_dimer().topology_hash(&topology).unwrap();
        assert_ne!(hash, topology.topology_hash());
    }

    /// A family seed is five Root calls in the order each one checks the one
    /// before it. A legacy seed is two, because registration makes it the
    /// default.
    #[test]
    #[expect(
        clippy::indexing_slicing,
        reason = "the call count and each fixed-width prefix are asserted first"
    )]
    fn a_family_seed_sends_the_activation_calls_in_order() {
        let topology = tiny();
        let difficulty = DEFAULT_SEED_DIFFICULTY;
        let hash = [9u8; 32];
        let legacy = seed_root_calls(&topology, None, hash, &difficulty);
        let family = twisted_dimer();
        let calls = seed_root_calls(&topology, Some(&family), hash, &difficulty);
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
        assert_eq!(legacy.len(), 2);
        // The legacy registration ends with `None`; the family one with
        // `Some(descriptor)`.
        assert_eq!(legacy[0].last(), Some(&0));
        let mut tail = vec![1u8];
        family.descriptor.encode_to(&mut tail);
        assert!(calls[0].ends_with(&tail));
        assert_eq!(&calls[2][2..34], &hash[..]);
        assert_eq!(&calls[2][34..], &family.band.encode()[..]);
        assert_eq!(
            calls[3].len(),
            34,
            "add_mineable_topology takes only the hash"
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
}
