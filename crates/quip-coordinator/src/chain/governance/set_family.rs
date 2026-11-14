//! Root-origin topology activation: the calls that make a family topology the
//! mineable default, on a fresh chain or on one that already mines another.
//! `set-family` moves a seeded chain to another parameter set of the same
//! family.
//!
//! `register_topology` does not make a family topology the default, so the
//! activation is five Root calls, each checking the one before it: register,
//! set the difficulty, set the energy band, whitelist, and set the default.
//! On a seeded chain the batch also takes every other topology off the
//! mineable whitelist (see [`family_root_calls`]).

use crate::chain::client::RealChainClient;
use crate::chain::codec::quantum_pow::{
    DifficultyConfig, EnergyBandScale, FamilyDescriptorScale, ADD_MINEABLE_TOPOLOGY_CALL_INDEX,
    REMOVE_MINEABLE_TOPOLOGY_CALL_INDEX, SET_DEFAULT_TOPOLOGY_CALL_INDEX,
    SET_DIFFICULTY_CALL_INDEX, SET_TOPOLOGY_ENERGY_BAND_CALL_INDEX,
};
use crate::chain::governance::foundation::{
    applied, dispatch, foundation_close_call, foundation_propose_call, foundation_threshold,
    foundation_vote_call, open_votes, proposal_hash, root_dispatch_call, RootCall,
};
use crate::chain::governance::seed::{
    client_for, quantum_pow_hash_call, register_topology_call, ss58, SeedParams,
    MIN_SEED_SPEC_VERSION,
};
use crate::chain::sign::submit::Confirmation;
use crate::chain::ChainError;
use crate::types::hex::hex_encode;

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

    let spec_version =
        crate::chain::client::storage::parse_spec_version(&client.runtime_version_raw().await?)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::codec::quantum_pow::{
        ADD_MINEABLE_TOPOLOGY_CALL_INDEX, QUANTUM_POW_PALLET_INDEX, REGISTER_TOPOLOGY_CALL_INDEX,
        REMOVE_MINEABLE_TOPOLOGY_CALL_INDEX, SET_DEFAULT_TOPOLOGY_CALL_INDEX,
        SET_DIFFICULTY_CALL_INDEX, SET_TOPOLOGY_ENERGY_BAND_CALL_INDEX,
    };
    use crate::chain::governance::seed::{
        SeedTopology, DEFAULT_ENERGY_BAND, DEFAULT_SEED_DIFFICULTY,
    };
    use parity_scale_codec::Encode;

    fn spec_topology(preset: &str) -> SeedTopology {
        let text = crate::types::topology_spec::preset_spec(preset).unwrap();
        SeedTopology::from_spec(&crate::types::topology_spec::parse_topology_spec(text).unwrap())
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

    /// The encoder lays out solver-core's frozen recipe bytes, for the built-in
    /// recipe (62, 1200) and for weak 45 with 1350 pairs.
    #[test]
    fn twisted_dimer_params_match_the_canonical_bytes() {
        for (label, params, expected) in [
            (
                "built-in dimer1200 constant",
                DIMER_1200_PARAMS.to_vec(),
                "0x010100020000000c00020001000000040300020000003e00040004000000b0040000",
            ),
            (
                "encoder at (62, 1200)",
                twisted_dimer_params(62, 1200),
                "0x010100020000000c00020001000000040300020000003e00040004000000b0040000",
            ),
            (
                "encoder at (45, 1350)",
                twisted_dimer_params(45, 1350),
                "0x010100020000000c00020001000000040300020000002d0004000400000046050000",
            ),
        ] {
            assert_eq!(hex_encode(&params), expected, "{label}");
        }
    }

    /// Both families admit on the live preset. Each family hash differs from
    /// the legacy hash of the same graph, and the two differ from each other.
    #[test]
    fn dimer_families_admit_on_the_live_preset() {
        let topology = live();
        let dimer = topology.family_hash(&dimer_1200()).unwrap();
        let weak = topology
            .family_hash(&twisted_dimer(twisted_dimer_params(45, 1350)))
            .unwrap();
        assert_ne!(dimer, topology.legacy_hash());
        assert_ne!(weak, topology.legacy_hash());
        assert_ne!(dimer, weak);
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
}
