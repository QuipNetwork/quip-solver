//! Root-origin topology activation: the calls that make a family topology the
//! mineable default, on a fresh chain or on one that already mines another.
//!
//! A chain with no `QuantumPow.DefaultTopology` accepts no proofs. `seed-chain`
//! seeds a fresh chain with the dimer1200 family topology: solver-core's live
//! recipe `x-w62-p1200` of `quip/twisted-ice-dimer` revision 1. `set-family`
//! moves a seeded chain to another parameter set of the same family.
//!
//! `register_topology` does not make a family topology the default, so the
//! activation is five Root calls, each checking the one before it: register,
//! set the difficulty, set the energy band, whitelist, and set the default.
//! On a seeded chain the batch also takes every other topology off the
//! mineable whitelist (see [`family_root_calls`]).
//!
//! Runtime 120 has no `Sudo` pallet. The calls go in one `Utility::batch_all`
//! that `ValidatorAdmission::dispatch_as_root` runs with the Root origin. That
//! call needs two thirds of the Foundation, so it is proposed as a Foundation
//! motion with threshold ⌈2N/3⌉ of N members. A threshold below 2, which is a
//! one-member Foundation such as the `dev` preset (`//Alice`), executes at
//! once. A larger Foundation votes: each member runs the same command, which
//! finds the open motion by its hash, votes aye, and closes it once the ayes
//! reach the threshold. Runtimes before 120 are refused.

use super::extrinsic::hex_encode;
use super::real::{Confirmation, RealChainClient, SignedCallOutcome};
use super::scale_types::{
    DifficultyConfig, EnergyBandScale, FamilyDescriptorScale, MinerKind, WeightScale,
    ADD_MINEABLE_TOPOLOGY_CALL_INDEX, BATCH_ALL_CALL_INDEX, CLOSE_CALL_INDEX,
    DISPATCH_AS_ROOT_CALL_INDEX, FOUNDATION_PALLET_INDEX, PROPOSE_CALL_INDEX,
    QUANTUM_POW_PALLET_INDEX, REGISTER_TOPOLOGY_CALL_INDEX, REMOVE_MINEABLE_TOPOLOGY_CALL_INDEX,
    SET_DEFAULT_TOPOLOGY_CALL_INDEX, SET_DIFFICULTY_CALL_INDEX,
    SET_TOPOLOGY_ENERGY_BAND_CALL_INDEX, UTILITY_PALLET_INDEX, VALIDATOR_ADMISSION_PALLET_INDEX,
    VOTE_CALL_INDEX,
};
use super::ChainError;
use crate::drive::TopologySpec;
use crate::topology::{family_topology_hash, topology_hash_sets, Topology};
use parity_scale_codec::{Compact, Encode};
use quantum_validation::families::{AdvertisedLimits, FamilyChallenge, CONTRACTS};
use quantum_validation::AllowedValueSpec;
use sp_core::crypto::{AccountId32, Ss58Codec};
use sp_crypto_hashing::blake2_256;

/// First runtime `spec_version` with the Foundation root path. quip-validator
/// v0.4.1 and v0.5.0 both report 120, and only v0.5.0 has family topologies;
/// on v0.4.1 the batch fails inside the motion and the default never moves.
pub const MIN_SEED_SPEC_VERSION: u32 = 120;

/// Node cap the runtime admits a family under (`QuantumPowMaxNodes`).
pub const RUNTIME_MAX_NODES: u64 = 5_000;
/// Edge cap the runtime admits a family under (`QuantumPowMaxEdges`).
pub const RUNTIME_MAX_EDGES: u64 = 50_000;

/// Family id of the twisted-ice dimer topologies.
pub const TWISTED_DIMER_FAMILY_ID: &str = "quip/twisted-ice-dimer";
/// Family revision of the twisted-ice dimer topologies.
pub const TWISTED_DIMER_REVISION: u32 = 1;
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

/// Canonical `quip/twisted-ice-dimer` revision 1 parameters on Zephyr
/// z(12, 4): `weak_milli` and `pairs` are the two values a recipe varies.
///
/// The bytes are the format byte `0x01` and four fields in tag order, each a
/// little-endian `u16` tag, a `u32` length and the value: `zephyr_m` (u16),
/// `zephyr_t` (u8), `weak_milli` (u16) and `pairs` (u32). Admission checks the
/// bounds; this only lays the bytes out.
#[must_use]
pub fn twisted_dimer_params(weak_milli: u16, pairs: u32) -> Vec<u8> {
    fn field(out: &mut Vec<u8>, tag: u16, value: &[u8]) {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&u32::try_from(value.len()).unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(value);
    }
    let mut out = vec![0x01];
    field(&mut out, 1, &12u16.to_le_bytes());
    field(&mut out, 2, &[4u8]);
    field(&mut out, 3, &weak_milli.to_le_bytes());
    field(&mut out, 4, &pairs.to_le_bytes());
    out
}

/// A `quip/twisted-ice-dimer` revision 1 descriptor with `params`.
#[must_use]
pub fn twisted_dimer(params: Vec<u8>) -> FamilyDescriptorScale {
    FamilyDescriptorScale {
        family_id: TWISTED_DIMER_FAMILY_ID.as_bytes().to_vec(),
        revision: TWISTED_DIMER_REVISION,
        params,
    }
}

/// The dimer1200 family descriptor.
#[must_use]
pub fn dimer_1200() -> FamilyDescriptorScale {
    twisted_dimer(DIMER_1200_PARAMS.to_vec())
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
        let admitted = crate::lease::ChallengeFamily::admit(
            crate::lease::FAMILY_CONTRACTS,
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
fn quantum_pow_hash_call(call_index: u8, topology_hash: [u8; 32], args: &impl Encode) -> Vec<u8> {
    let mut call = vec![QUANTUM_POW_PALLET_INDEX, call_index];
    call.extend_from_slice(&topology_hash);
    args.encode_to(&mut call);
    call
}

/// The topology state of the chain that decides the batch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChainTopologies {
    /// `QuantumPow.DefaultTopology`.
    pub default: Option<[u8; 32]>,
    /// Every key of `QuantumPow.MineableTopologies`.
    pub mineable: Vec<[u8; 32]>,
    /// Whether the new family topology is already registered.
    pub registered: bool,
}

/// One Root call of the batch, with a label for the dry run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootCall {
    /// What the call does, as `Pallet.call(args)`.
    pub label: String,
    /// The SCALE-encoded `RuntimeCall`.
    pub bytes: Vec<u8>,
}

/// The Root calls that make the family topology `hash` the only mineable
/// topology and the default, in the order each call checks the one before it.
///
/// - Every mineable topology other than the current default and `hash` comes
///   off the whitelist first. The pallet whitelists at most one topology
///   besides the default, so a leftover one would fail the whitelist call.
/// - `register_topology`, skipped when `hash` is already registered, which
///   the pallet would refuse.
/// - `set_difficulty`, `set_topology_energy_band`, `add_mineable_topology`
///   and `set_default_topology` on `hash`.
/// - The old default comes off the whitelist last. The pallet accepts a proof
///   on any mineable topology, so leaving it would let a coordinator that
///   missed the switch keep winning on the old graph, and would hold the one
///   spare whitelist slot the next switch needs.
#[must_use]
pub fn family_root_calls(
    params: &SeedParams,
    hash: [u8; 32],
    chain: &ChainTopologies,
) -> Vec<RootCall> {
    let remove = |old: [u8; 32]| RootCall {
        label: format!("QuantumPow.remove_mineable_topology({})", hex_encode(&old)),
        bytes: quantum_pow_hash_call(REMOVE_MINEABLE_TOPOLOGY_CALL_INDEX, old, &()),
    };
    let new = hex_encode(&hash);
    let mut calls: Vec<RootCall> = chain
        .mineable
        .iter()
        .filter(|h| Some(**h) != chain.default && **h != hash)
        .map(|h| remove(*h))
        .collect();
    if !chain.registered {
        calls.push(RootCall {
            label: format!(
                "QuantumPow.register_topology({} nodes, {} edges, family {} r{})",
                params.topology.nodes.len(),
                params.topology.edges.len(),
                String::from_utf8_lossy(&params.descriptor.family_id),
                params.descriptor.revision
            ),
            bytes: register_topology_call(&params.topology, &params.descriptor),
        });
    }
    calls.extend([
        RootCall {
            label: format!(
                "QuantumPow.set_difficulty({new}, min_solutions {}, max_energy_milli {}, \
                 min_diversity_milli {})",
                params.difficulty.min_solutions,
                params.difficulty.max_energy_milli,
                params.difficulty.min_diversity_milli
            ),
            bytes: quantum_pow_hash_call(SET_DIFFICULTY_CALL_INDEX, hash, &params.difficulty),
        },
        RootCall {
            label: format!(
                "QuantumPow.set_topology_energy_band({new}, hard_milli {}, easy_milli {})",
                params.band.hard_milli, params.band.easy_milli
            ),
            bytes: quantum_pow_hash_call(SET_TOPOLOGY_ENERGY_BAND_CALL_INDEX, hash, &params.band),
        },
        RootCall {
            label: format!("QuantumPow.add_mineable_topology({new})"),
            bytes: quantum_pow_hash_call(ADD_MINEABLE_TOPOLOGY_CALL_INDEX, hash, &()),
        },
        RootCall {
            label: format!("QuantumPow.set_default_topology({new})"),
            bytes: quantum_pow_hash_call(SET_DEFAULT_TOPOLOGY_CALL_INDEX, hash, &()),
        },
    ]);
    if let Some(old) = chain.default.filter(|old| *old != hash) {
        calls.push(remove(old));
    }
    calls
}

/// Ayes a Foundation motion needs for `dispatch_as_root`: two thirds of the
/// members, rounded up.
#[must_use]
pub fn foundation_threshold(members: usize) -> u32 {
    let members = u32::try_from(members).unwrap_or(u32::MAX);
    members.saturating_mul(2).saturating_add(2) / 3
}

/// `ValidatorAdmission.dispatch_as_root(Utility.batch_all(calls))`: the
/// proposal a Foundation motion carries.
///
/// A `Box<RuntimeCall>` encodes as the call itself, and `Vec<RuntimeCall>` as
/// a compact count followed by the calls.
#[must_use]
pub fn root_dispatch_call(calls: &[RootCall]) -> Vec<u8> {
    let mut root = vec![
        VALIDATOR_ADMISSION_PALLET_INDEX,
        DISPATCH_AS_ROOT_CALL_INDEX,
        UTILITY_PALLET_INDEX,
        BATCH_ALL_CALL_INDEX,
    ];
    let count = u32::try_from(calls.len()).unwrap_or(u32::MAX);
    Compact(count).encode_to(&mut root);
    for call in calls {
        root.extend_from_slice(&call.bytes);
    }
    root
}

/// The hash `Foundation.Voting` keys a proposal by: BLAKE2-256 of the call.
#[must_use]
pub fn proposal_hash(proposal: &[u8]) -> [u8; 32] {
    blake2_256(proposal)
}

/// `Foundation.propose(threshold, proposal, length_bound)`.
#[must_use]
pub fn foundation_propose_call(threshold: u32, proposal: &[u8]) -> Vec<u8> {
    let mut motion = vec![FOUNDATION_PALLET_INDEX, PROPOSE_CALL_INDEX];
    Compact(threshold).encode_to(&mut motion);
    motion.extend_from_slice(proposal);
    let length_bound = u32::try_from(proposal.len()).unwrap_or(u32::MAX);
    Compact(length_bound).encode_to(&mut motion);
    motion
}

/// `Foundation.vote(proposal, index, approve)`.
#[must_use]
pub fn foundation_vote_call(proposal: [u8; 32], index: u32, approve: bool) -> Vec<u8> {
    let mut call = vec![FOUNDATION_PALLET_INDEX, VOTE_CALL_INDEX];
    call.extend_from_slice(&proposal);
    Compact(index).encode_to(&mut call);
    approve.encode_to(&mut call);
    call
}

/// `Foundation.close(proposal, index, proposal_weight_bound, length_bound)`.
#[must_use]
pub fn foundation_close_call(
    proposal: [u8; 32],
    index: u32,
    weight_bound: WeightScale,
    length_bound: u32,
) -> Vec<u8> {
    let mut call = vec![FOUNDATION_PALLET_INDEX, CLOSE_CALL_INDEX];
    call.extend_from_slice(&proposal);
    Compact(index).encode_to(&mut call);
    weight_bound.encode_to(&mut call);
    Compact(length_bound).encode_to(&mut call);
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

/// What to do when the chain already has a default topology.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExistingDefault {
    /// Refuse: `seed-chain`, which only seeds a fresh chain.
    Refuse,
    /// Replace it with the new family topology: `set-family`.
    Replace,
}

/// The activation worked out against the chain, before anything is signed.
#[derive(Clone, Debug)]
pub struct FamilyPlan {
    /// The family topology hash computed locally.
    pub topology_hash: [u8; 32],
    /// The legacy hash of the graph, which the family hash commits to.
    pub legacy_hash: [u8; 32],
    /// The chain's topology state the calls were built from.
    pub chain: ChainTopologies,
    /// Foundation member accounts.
    pub members: Vec<[u8; 32]>,
    /// Ayes the motion needs.
    pub threshold: u32,
    /// The Root calls, empty when the topology is already the default.
    pub calls: Vec<RootCall>,
    /// `dispatch_as_root(batch_all(calls))`.
    pub proposal: Vec<u8>,
    /// The hash the motion is voted on under.
    pub proposal_hash: [u8; 32],
    /// `Foundation.propose(threshold, proposal, len)`, the call a member signs.
    pub motion: Vec<u8>,
}

/// What an activation did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FamilyOutcome {
    /// `DefaultTopology` now equals the family topology hash, read back.
    Applied {
        /// The family topology hash.
        topology_hash: [u8; 32],
        /// Node count in the registered graph.
        nodes: usize,
        /// Edge count in the registered graph.
        edges: usize,
        /// Block that executed the motion.
        block: String,
    },
    /// The family topology was already the default; nothing was sent.
    AlreadyDefault {
        /// The family topology hash.
        topology_hash: [u8; 32],
    },
    /// The motion is open and needs more ayes.
    Pending {
        /// The family topology hash the motion will make the default.
        topology_hash: [u8; 32],
        /// The proposal hash members vote on.
        proposal_hash: [u8; 32],
        /// The proposal index members vote on.
        index: u32,
        /// Ayes so far.
        ayes: u32,
        /// Ayes the motion needs.
        threshold: u32,
    },
}

/// Refuse a band that is not ordered or a ceiling outside it.
///
/// # Errors
/// Returns the reason when `hard < ceiling < easy` does not hold.
pub fn check_energies(difficulty: &DifficultyConfig, band: &EnergyBandScale) -> Result<(), String> {
    if band.hard_milli >= band.easy_milli {
        return Err(format!(
            "band hard end {} must be below the easy end {}",
            band.hard_milli, band.easy_milli
        ));
    }
    if !(band.hard_milli < difficulty.max_energy_milli
        && difficulty.max_energy_milli < band.easy_milli)
    {
        return Err(format!(
            "max_energy_milli {} must sit inside the band ({}, {})",
            difficulty.max_energy_milli, band.hard_milli, band.easy_milli
        ));
    }
    Ok(())
}

/// SS58 text of an account, for messages.
#[must_use]
pub fn ss58(account: &[u8; 32]) -> String {
    AccountId32::from(*account).to_ss58check()
}

fn client_for(params: &SeedParams) -> RealChainClient {
    RealChainClient::new(
        vec![params.validator.clone()],
        params.sudo_key.clone(),
        MinerKind::Cpu,
    )
}

/// Work out the activation against the chain without signing anything.
///
/// # Errors
/// Returns an error when the validator is unreachable, when the runtime is
/// older than [`MIN_SEED_SPEC_VERSION`], when the energies are out of order,
/// when either admission refuses the descriptor on the graph, when
/// `existing` is [`ExistingDefault::Refuse`] and a default is set, or when
/// the Foundation has no members.
pub async fn plan_family(
    params: &SeedParams,
    existing: ExistingDefault,
) -> Result<FamilyPlan, ChainError> {
    plan_with(&client_for(params), params, existing).await
}

async fn plan_with(
    client: &RealChainClient,
    params: &SeedParams,
    existing: ExistingDefault,
) -> Result<FamilyPlan, ChainError> {
    check_energies(&params.difficulty, &params.band).map_err(ChainError::Submit)?;
    let topology_hash = params
        .topology
        .family_hash(&params.descriptor)
        .map_err(ChainError::Submit)?;

    let spec_version = super::real::parse_spec_version(&client.runtime_version_raw().await?)?;
    if spec_version < MIN_SEED_SPEC_VERSION {
        return Err(ChainError::Submit(format!(
            "runtime {spec_version} has no family topologies; this needs runtime \
             {MIN_SEED_SPEC_VERSION} or later"
        )));
    }

    let at = client.head_hex().await?;
    let default = client.default_topology().await?;
    if let (ExistingDefault::Refuse, Some(existing)) = (existing, default) {
        return Err(ChainError::Submit(format!(
            "DefaultTopology is already set to {}; a chain can only be seeded once, so \
             wipe the chain data and restart the validator, or use set-family to move \
             the chain to another family topology",
            hex_encode(&existing)
        )));
    }
    let chain = ChainTopologies {
        default,
        mineable: client.mineable_topologies(&at).await?,
        registered: client.topology_registered(&topology_hash, &at).await?,
    };
    let members = client.foundation_members(&at).await?;
    if members.is_empty() {
        return Err(ChainError::Submit(
            "Foundation.Members is empty; no member can propose".into(),
        ));
    }
    let threshold = foundation_threshold(members.len());
    let calls = if default == Some(topology_hash) {
        Vec::new()
    } else {
        family_root_calls(params, topology_hash, &chain)
    };
    let proposal = root_dispatch_call(&calls);
    Ok(FamilyPlan {
        topology_hash,
        legacy_hash: params.topology.legacy_hash(),
        chain,
        members,
        threshold,
        proposal_hash: proposal_hash(&proposal),
        motion: foundation_propose_call(threshold, &proposal),
        calls,
        proposal,
    })
}

/// Make the family topology the default: propose the motion, or vote on and
/// close the one another member proposed.
///
/// Success is reported only once `DefaultTopology` equals the family topology
/// hash computed locally. Until enough members have voted, the outcome is
/// [`FamilyOutcome::Pending`] with the proposal hash and index.
///
/// # Errors
/// Returns the errors of [`plan_family`], and an error when the signer is not
/// a Foundation member, when a call is rejected or fails, or when the default
/// the chain stored is not the family topology hash.
pub async fn apply_family(
    params: &SeedParams,
    existing: ExistingDefault,
) -> Result<FamilyOutcome, ChainError> {
    let client = client_for(params);
    let plan = plan_with(&client, params, existing).await?;
    let topology_hash = plan.topology_hash;
    if plan.calls.is_empty() {
        return Ok(FamilyOutcome::AlreadyDefault { topology_hash });
    }
    let me = client.signer_account()?;
    if !plan.members.contains(&me) {
        return Err(ChainError::Submit(format!(
            "the signer {} is not a Foundation member; members are {}",
            ss58(&me),
            plan.members.iter().map(ss58).collect::<Vec<_>>().join(", ")
        )));
    }
    let proposal = plan.proposal_hash;
    let executed = Confirmation::DefaultTopologyIs {
        hash: topology_hash,
    };

    let open = client
        .foundation_votes(&proposal, &client.head_hex().await?)
        .await?;
    if open.is_none() {
        if plan.threshold < 2 {
            let block = dispatch(&client, &plan.motion, "Foundation.propose", executed).await?;
            return applied(&client, params, topology_hash, block).await;
        }
        // `dispatch` logs the including block; the motion's state is read
        // back below.
        let _proposed_in = dispatch(
            &client,
            &plan.motion,
            "Foundation.propose",
            Confirmation::MotionOpen { proposal },
        )
        .await?;
    }

    // `propose` does not count the proposer's vote, so every member votes,
    // the proposer included.
    let votes = open_votes(&client, &proposal).await?;
    if !votes.ayes.contains(&me) {
        let _voted_in = dispatch(
            &client,
            &foundation_vote_call(proposal, votes.index, true),
            "Foundation.vote",
            Confirmation::Voted {
                proposal,
                account: me,
            },
        )
        .await?;
    }
    let votes = open_votes(&client, &proposal).await?;
    let ayes = u32::try_from(votes.ayes.len()).unwrap_or(u32::MAX);
    if ayes < votes.threshold {
        return Ok(FamilyOutcome::Pending {
            topology_hash,
            proposal_hash: proposal,
            index: votes.index,
            ayes,
            threshold: votes.threshold,
        });
    }

    let weight = client.call_weight(&plan.proposal).await?;
    let length = u32::try_from(plan.proposal.len()).unwrap_or(u32::MAX);
    let block = dispatch(
        &client,
        &foundation_close_call(proposal, votes.index, weight, length),
        "Foundation.close",
        executed,
    )
    .await?;
    applied(&client, params, topology_hash, block).await
}

/// `seed-chain`: seed a fresh chain, refusing one that already has a default.
///
/// # Errors
/// As [`apply_family`] with [`ExistingDefault::Refuse`].
pub async fn seed_chain(params: SeedParams) -> Result<FamilyOutcome, ChainError> {
    apply_family(&params, ExistingDefault::Refuse).await
}

/// The open vote on `proposal`, which must exist.
async fn open_votes(
    client: &RealChainClient,
    proposal: &[u8; 32],
) -> Result<super::scale_types::FoundationVotesScale, ChainError> {
    client
        .foundation_votes(proposal, &client.head_hex().await?)
        .await?
        .ok_or_else(|| {
            ChainError::Submit(format!(
                "the motion {} closed before this member could vote; run the command again \
                 to see where the chain stands",
                hex_encode(proposal)
            ))
        })
}

/// Read `DefaultTopology` back at the head and report the switch.
///
/// The motion and the Root dispatch both succeed as extrinsics even when the
/// batch inside fails; only the events carry that result. The batch is
/// all-or-nothing and sets the default, so the stored default equal to the
/// local hash shows that every call landed.
async fn applied(
    client: &RealChainClient,
    params: &SeedParams,
    topology_hash: [u8; 32],
    block: String,
) -> Result<FamilyOutcome, ChainError> {
    let stored = client.default_topology().await?;
    if stored != Some(topology_hash) {
        return Err(ChainError::Submit(format!(
            "the motion executed in {block}, but DefaultTopology is {} instead of {}; read \
             the Foundation.Executed and ValidatorAdmission.RootDispatched events of that \
             block for the inner error",
            stored.map_or_else(|| "unset".to_string(), |h| hex_encode(&h)),
            hex_encode(&topology_hash)
        )));
    }
    Ok(FamilyOutcome::Applied {
        topology_hash,
        nodes: params.topology.nodes.len(),
        edges: params.topology.edges.len(),
        block,
    })
}

/// Submit `call` and return the block that included it once `confirmation`
/// holds there.
async fn dispatch(
    client: &RealChainClient,
    call: &[u8],
    what: &str,
    confirmation: Confirmation,
) -> Result<String, ChainError> {
    tracing::info!(call = what, "submitting Foundation call");
    let (outcome, _) = client.submit_signed_call(call, confirmation).await?;
    match outcome {
        SignedCallOutcome::Success { block, .. } => {
            tracing::info!(call = what, block = %block, "Foundation call included");
            Ok(block)
        }
        SignedCallOutcome::DispatchFailed { error, .. } => Err(ChainError::Submit(format!(
            "{what} was included but did not take effect: {error}; read the \
             Foundation and ValidatorAdmission events of that block for the inner error"
        ))),
        SignedCallOutcome::Invalid { message } => Err(ChainError::Submit(format!(
            "{what} was rejected by the transaction pool: {message}"
        ))),
        SignedCallOutcome::Dropped { message } => Err(ChainError::Submit(format!(
            "{what} was dropped before inclusion: {message}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_topology(preset: &str) -> SeedTopology {
        let text = crate::presets::preset_spec(preset).unwrap();
        SeedTopology::from_spec(&crate::drive::parse_topology_spec(text).unwrap())
    }

    fn live() -> SeedTopology {
        spec_topology("zephyr-z12t4-live")
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

    fn prefixes(calls: &[RootCall]) -> Vec<[u8; 2]> {
        calls
            .iter()
            .map(|c| {
                c.bytes
                    .get(..2)
                    .and_then(|p| <[u8; 2]>::try_from(p).ok())
                    .expect("every call has a pallet and a call index")
            })
            .collect()
    }

    const REMOVE: [u8; 2] = [
        QUANTUM_POW_PALLET_INDEX,
        REMOVE_MINEABLE_TOPOLOGY_CALL_INDEX,
    ];
    const REGISTER: [u8; 2] = [QUANTUM_POW_PALLET_INDEX, REGISTER_TOPOLOGY_CALL_INDEX];
    const DIFFICULTY: [u8; 2] = [QUANTUM_POW_PALLET_INDEX, SET_DIFFICULTY_CALL_INDEX];
    const BAND: [u8; 2] = [
        QUANTUM_POW_PALLET_INDEX,
        SET_TOPOLOGY_ENERGY_BAND_CALL_INDEX,
    ];
    const ADD: [u8; 2] = [QUANTUM_POW_PALLET_INDEX, ADD_MINEABLE_TOPOLOGY_CALL_INDEX];
    const DEFAULT: [u8; 2] = [QUANTUM_POW_PALLET_INDEX, SET_DEFAULT_TOPOLOGY_CALL_INDEX];

    /// The built-in parameters are the recipe bytes solver-core freezes.
    #[test]
    fn the_dimer_1200_params_are_the_solver_core_recipe() {
        assert_eq!(
            hex_encode(&DIMER_1200_PARAMS),
            "0x010100020000000c00020001000000040300020000003e00040004000000b0040000"
        );
    }

    /// The encoder lays out the recipe bytes for both the built-in recipe and
    /// weak 45, 1350 pairs.
    #[test]
    fn twisted_dimer_params_match_the_canonical_bytes() {
        assert_eq!(twisted_dimer_params(62, 1200), DIMER_1200_PARAMS.to_vec());
        assert_eq!(
            hex_encode(&twisted_dimer_params(45, 1350)),
            "0x010100020000000c00020001000000040300020000002d0004000400000046050000"
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

    /// The runtime's admission and the coordinator's both take weak 45 with
    /// 1350 pairs on the live preset, and it is a different topology from
    /// dimer1200 on the same graph.
    #[test]
    fn weak_45_pairs_1350_admits_on_the_live_preset() {
        let topology = live();
        let hash = topology
            .family_hash(&twisted_dimer(twisted_dimer_params(45, 1350)))
            .unwrap();
        assert_ne!(hash, topology.family_hash(&dimer_1200()).unwrap());
    }

    /// A weak magnitude outside 1..=999 is refused before anything is sent.
    #[test]
    fn out_of_bounds_params_are_refused() {
        let topology = live();
        for weak in [0, 1000] {
            let err = topology
                .family_hash(&twisted_dimer(twisted_dimer_params(weak, 1350)))
                .unwrap_err();
            assert!(err.contains("refuses"), "{weak}: {err}");
        }
    }

    /// A fresh chain gets the five activation calls in order, and registration
    /// carries `Some(descriptor)` last.
    #[test]
    #[expect(
        clippy::indexing_slicing,
        reason = "the call count and each fixed-width prefix are asserted first"
    )]
    fn a_fresh_chain_gets_the_five_activation_calls_in_order() {
        let p = params(live());
        let hash = [9u8; 32];
        let calls = family_root_calls(&p, hash, &ChainTopologies::default());
        assert_eq!(prefixes(&calls), [REGISTER, DIFFICULTY, BAND, ADD, DEFAULT]);
        let mut tail = vec![1u8];
        dimer_1200().encode_to(&mut tail);
        assert!(calls[0].bytes.ends_with(&tail));
        assert_eq!(&calls[1].bytes[2..34], &hash[..]);
        assert_eq!(&calls[1].bytes[34..], &DEFAULT_SEED_DIFFICULTY.encode()[..]);
        assert_eq!(&calls[2].bytes[34..], &DEFAULT_ENERGY_BAND.encode()[..]);
        assert_eq!(
            calls[3].bytes.len(),
            34,
            "add_mineable_topology takes only the hash"
        );
        assert_eq!(
            calls[4].bytes.len(),
            34,
            "set_default_topology takes only the hash"
        );
    }

    /// On a seeded chain a leftover spare comes off the whitelist first and
    /// the old default last, so only the new topology stays mineable.
    #[test]
    #[expect(clippy::indexing_slicing, reason = "the call count is asserted first")]
    fn a_seeded_chain_retires_every_other_mineable_topology() {
        let p = params(live());
        let (new, old, spare) = ([9u8; 32], [1u8; 32], [2u8; 32]);
        let chain = ChainTopologies {
            default: Some(old),
            mineable: vec![old, spare],
            registered: false,
        };
        let calls = family_root_calls(&p, new, &chain);
        assert_eq!(
            prefixes(&calls),
            [REMOVE, REGISTER, DIFFICULTY, BAND, ADD, DEFAULT, REMOVE]
        );
        assert_eq!(&calls[0].bytes[2..], &spare[..]);
        assert_eq!(&calls[6].bytes[2..], &old[..]);
    }

    /// A topology registered by an earlier, failed attempt is not registered
    /// again, which the pallet would refuse.
    #[test]
    fn a_registered_topology_is_not_registered_again() {
        let p = params(live());
        let (new, old) = ([9u8; 32], [1u8; 32]);
        let chain = ChainTopologies {
            default: Some(old),
            mineable: vec![old, new],
            registered: true,
        };
        assert_eq!(
            prefixes(&family_root_calls(&p, new, &chain)),
            [DIFFICULTY, BAND, ADD, DEFAULT, REMOVE]
        );
    }

    /// Two thirds of the members, rounded up; one member executes at once.
    #[test]
    fn the_threshold_is_two_thirds_rounded_up() {
        let got: Vec<u32> = (1..=7).map(foundation_threshold).collect();
        assert_eq!(got, [1, 2, 2, 3, 4, 4, 5]);
    }

    /// The motion is `propose(Compact(threshold), dispatch_as_root(batch_all(
    /// calls)), Compact(len))`, and the length bound is the inner call length.
    #[test]
    fn the_foundation_motion_wraps_the_batch_in_root_dispatch() {
        let call = |bytes: Vec<u8>| RootCall {
            label: String::new(),
            bytes,
        };
        let calls = vec![call(vec![10, 3, 0xaa]), call(vec![10, 6, 0xbb, 0xcc])];
        let root = root_dispatch_call(&calls);
        assert_eq!(
            root,
            [
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
            ]
        );
        let mut expected = vec![FOUNDATION_PALLET_INDEX, PROPOSE_CALL_INDEX, 0x0c];
        expected.extend_from_slice(&root);
        Compact(u32::try_from(root.len()).unwrap()).encode_to(&mut expected);
        assert_eq!(foundation_propose_call(3, &root), expected);
    }

    /// `vote(hash, Compact(index), approve)` and `close(hash, Compact(index),
    /// Weight { Compact, Compact }, Compact(len))`.
    #[test]
    fn vote_and_close_encode_the_collective_arguments() {
        let hash = [7u8; 32];
        let mut vote = vec![FOUNDATION_PALLET_INDEX, VOTE_CALL_INDEX];
        vote.extend_from_slice(&hash);
        vote.extend_from_slice(&[0x08, 0x01]);
        assert_eq!(foundation_vote_call(hash, 2, true), vote);

        let weight = WeightScale {
            ref_time: 1,
            proof_size: 2,
        };
        let mut close = vec![FOUNDATION_PALLET_INDEX, CLOSE_CALL_INDEX];
        close.extend_from_slice(&hash);
        close.extend_from_slice(&[0x08, 0x04, 0x08, 0x28]);
        assert_eq!(foundation_close_call(hash, 2, weight, 10), close);
    }

    /// The band is ordered and the ceiling sits strictly inside it.
    #[test]
    fn energies_must_nest() {
        let band = EnergyBandScale {
            hard_milli: -5_136_000,
            easy_milli: -4_836_000,
        };
        let ceiling = |max_energy_milli| DifficultyConfig {
            max_energy_milli,
            ..DEFAULT_SEED_DIFFICULTY
        };
        assert!(check_energies(&ceiling(-4_886_000), &band).is_ok());
        assert!(check_energies(&ceiling(-4_836_000), &band).is_err());
        assert!(check_energies(&ceiling(-5_200_000), &band).is_err());
        let flipped = EnergyBandScale {
            hard_milli: band.easy_milli,
            easy_milli: band.hard_milli,
        };
        assert!(check_energies(&ceiling(-4_886_000), &flipped).is_err());
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
