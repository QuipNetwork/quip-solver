//! SCALE mirrors of `pallet_quantum_pow`: difficulty, topology metadata, the
//! mining snapshot, miner info, and the `QuantumPow` call encoders.

use parity_scale_codec::{Decode, Encode};
use quantum_validation::AllowedValueSpec;
use sp_core::H256;

use crate::types::hex::hex_encode;
use crate::types::proof::QuantumProof;
use crate::types::topology_spec::TopologySpec;

/// On-wire difficulty config (matches pallet `DifficultyConfig`).
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct DifficultyConfig {
    /// Minimum valid solutions required.
    pub min_solutions: u32,
    /// Energy ceiling in milli units (solutions must be strictly below).
    pub max_energy_milli: i64,
    /// Minimum diversity of the solution set in milli units.
    pub min_diversity_milli: u32,
}

/// Per-topology curve override (matches pallet `CurveC`): per-mille c-triple
/// stored under `TopologyCurveC[topology_hash]`. Field order is the pallet's.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct CurveCScale {
    /// Easy-regime curve c (per-mille).
    pub easy_milli: u32,
    /// Knee-regime curve c (per-mille).
    pub knee_milli: u32,
    /// Hard-regime curve c (per-mille).
    pub hard_milli: u32,
}

/// Root-set energy band of a family topology (matches pallet `EnergyBand`),
/// stored under `TopologyEnergyBand[topology_hash]`. Field order is the
/// pallet's.
#[derive(Clone, Copy, Debug, Encode, Decode, PartialEq, Eq)]
pub struct EnergyBandScale {
    /// Hardest (most negative) end of the band, in milli.
    pub hard_milli: i64,
    /// Easiest (least negative) end of the band, in milli.
    pub easy_milli: i64,
}

/// A family revision and its canonical parameter bytes (matches pallet
/// `FamilyDescriptor`). The pallet's bounded vectors encode as plain vectors.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct FamilyDescriptorScale {
    /// The family id, as ASCII bytes.
    pub family_id: Vec<u8>,
    /// The exact revision of the family.
    pub revision: u32,
    /// The canonical parameter encoding.
    pub params: Vec<u8>,
}

/// `QuantumPowApi` version whose `MiningSnapshot` and `TopologyMeta` carry the
/// `family` field. Version 2 encodes both without it.
pub const FAMILY_API_VERSION: u32 = 3;

/// Runtime-API mining snapshot (matches pallet `MiningSnapshot` at
/// `QuantumPowApi` version 3).
///
/// Note: no `block_number` field — callers fetch the header separately.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct MiningSnapshotScale {
    /// Hash of the block that contained the last winning proof.
    pub last_proof_block_hash: H256,
    /// Current (possibly decayed) difficulty gates.
    pub difficulty: DifficultyConfig,
    /// Topology identity hash.
    pub topology_hash: H256,
    /// Topology node ids.
    pub nodes: Vec<u32>,
    /// Topology undirected edges as `(u, v)` node-id pairs.
    pub edges: Vec<(u32, u32)>,
    /// Allowed linear-field values.
    pub allowed_h_values: AllowedValueSpec<Vec<i32>>,
    /// Allowed coupling values.
    pub allowed_j_values: AllowedValueSpec<Vec<i32>>,
    /// Allowed spin values.
    pub allowed_spin_values: AllowedValueSpec<Vec<i32>>,
    /// The family descriptor, when the chain draws `h` and `j` through the
    /// canonical generator. `None` keeps the legacy draw.
    pub family: Option<FamilyDescriptorScale>,
}

/// Runtime-API registered topology (matches pallet `TopologyMeta` at
/// `QuantumPowApi` version 3).
///
/// Returned by `QuantumPowApi_topology_meta(hash)`. The hash is the content
/// hash of every field but `registered_at`, so one hash names one topology
/// for the life of the chain and the client caches it by hash. Decode it with
/// [`TopologyMetaScale::decode_for_api`], because version 2 has no `family`.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct TopologyMetaScale {
    /// Topology node ids.
    pub nodes: Vec<u32>,
    /// Topology undirected edges as `(u, v)` node-id pairs.
    pub edges: Vec<(u32, u32)>,
    /// Allowed linear-field values.
    pub allowed_h_values: AllowedValueSpec<Vec<i32>>,
    /// Allowed coupling values.
    pub allowed_j_values: AllowedValueSpec<Vec<i32>>,
    /// Allowed spin values.
    pub allowed_spin_values: AllowedValueSpec<Vec<i32>>,
    /// The family descriptor, when the chain draws `h` and `j` through the
    /// canonical generator. `None` keeps the legacy draw.
    pub family: Option<FamilyDescriptorScale>,
    /// Block number the topology was registered at.
    pub registered_at: u32,
}

/// `TopologyMeta` as `QuantumPowApi` version 2 encodes it: no `family`.
#[derive(Decode)]
struct TopologyMetaV2Scale {
    nodes: Vec<u32>,
    edges: Vec<(u32, u32)>,
    allowed_h_values: AllowedValueSpec<Vec<i32>>,
    allowed_j_values: AllowedValueSpec<Vec<i32>>,
    allowed_spin_values: AllowedValueSpec<Vec<i32>>,
    registered_at: u32,
}

impl TopologyMetaScale {
    /// Decode one `TopologyMeta` in the layout of `QuantumPowApi` version
    /// `api_version`. Version 3 inserts `family` before `registered_at`, so a
    /// version-2 decoder would read a wrong `registered_at` from it.
    ///
    /// # Errors
    /// Returns the SCALE error when `input` does not hold that layout.
    pub fn decode_for_api(
        input: &mut &[u8],
        api_version: u32,
    ) -> Result<Self, parity_scale_codec::Error> {
        if api_version >= FAMILY_API_VERSION {
            return Self::decode(input);
        }
        let v2 = TopologyMetaV2Scale::decode(input)?;
        Ok(Self {
            nodes: v2.nodes,
            edges: v2.edges,
            allowed_h_values: v2.allowed_h_values,
            allowed_j_values: v2.allowed_j_values,
            allowed_spin_values: v2.allowed_spin_values,
            family: None,
            registered_at: v2.registered_at,
        })
    }

    /// Convert registered chain metadata into a topology spec.
    ///
    /// Keeps node, edge, and value-set order. Difficulty gates use the same
    /// defaults as a JSON spec without gates. Registration height is excluded
    /// from topology identity.
    ///
    /// # Errors
    /// Returns an error for unsupported value specs, invalid graph data,
    /// a computed topology hash that differs from `hash`, or a topology bound
    /// to a problem family.
    pub fn into_topology_spec(self, hash: [u8; 32]) -> Result<TopologySpec, String> {
        if let Some(family) = &self.family {
            return Err(format!(
                "chain topology {} is bound to family {} revision {}; pass the \
                 descriptor with --family-id, --family-revision, and --family-params",
                hex_encode(&hash),
                String::from_utf8_lossy(&family.family_id),
                family.revision
            ));
        }
        let spec = TopologySpec::from_sets(
            self.nodes,
            self.edges,
            require_set_values(&self.allowed_h_values)
                .map_err(|error| format!("chain topology allowed_h_milli: {error}"))?,
            require_set_values(&self.allowed_j_values)
                .map_err(|error| format!("chain topology allowed_j_milli: {error}"))?,
            require_set_values(&self.allowed_spin_values)
                .map_err(|error| format!("chain topology allowed_spin_milli: {error}"))?,
        )
        .map_err(|error| error.to_string())?;
        if spec.topology.hash.as_slice() != hash.as_slice() {
            return Err(format!(
                "topology hash mismatch: chain reported {}, computed {}",
                hex_encode(&hash),
                hex_encode(&spec.topology.hash)
            ));
        }
        Ok(spec)
    }
}

/// SCALE mirror of `pallet_quantum_pow::MinerInfo<Balance, BlockNumber>` with
/// the runtime's `Balance = u128` and `BlockNumber = u32`.
///
/// Field order is the pallet's and must not be reordered: SCALE carries no field
/// names, so a swap decodes into a wrong number with no error.
#[derive(Clone, Copy, Debug, Encode, Decode, PartialEq, Eq)]
pub struct MinerInfoScale {
    /// Block the account registered at.
    pub registered_at: u32,
    /// Reserved miner deposit, in plancks.
    pub deposit: u128,
    /// Lifetime accepted proofs.
    pub proofs_submitted: u32,
    /// Lifetime winning proofs.
    pub proofs_won: u32,
    /// Lifetime rewards, in plancks.
    pub rewards_earned: u128,
}

/// Pallet index of `QuantumPow` in the runtime construct.
pub const QUANTUM_POW_PALLET_INDEX: u8 = 10;

/// Call index of `submit_proof` within `QuantumPow`.
pub const SUBMIT_PROOF_CALL_INDEX: u8 = 4;

/// `QuantumPow::register_miner` call index. Takes no arguments; the origin is
/// the miner. `QuantumPow::submit_proof` rejects any account that has not made
/// this call with `MinerNotRegistered`.
pub const REGISTER_MINER_CALL_INDEX: u8 = 0;

/// `QuantumPow::register_topology` call index.
pub const REGISTER_TOPOLOGY_CALL_INDEX: u8 = 2;

/// `QuantumPow::set_difficulty` call index.
pub const SET_DIFFICULTY_CALL_INDEX: u8 = 3;

/// `QuantumPow::set_default_topology` call index.
pub const SET_DEFAULT_TOPOLOGY_CALL_INDEX: u8 = 5;

/// `QuantumPow::add_mineable_topology` call index.
pub const ADD_MINEABLE_TOPOLOGY_CALL_INDEX: u8 = 6;

/// `QuantumPow::set_topology_energy_band` call index.
pub const SET_TOPOLOGY_ENERGY_BAND_CALL_INDEX: u8 = 11;

/// `QuantumPow::remove_mineable_topology` call index. Refuses the current
/// default.
pub const REMOVE_MINEABLE_TOPOLOGY_CALL_INDEX: u8 = 7;

/// SCALE-encode the `QuantumPow.submit_proof(proof)` call body.
#[must_use]
pub fn encode_submit_proof_call(proof: &QuantumProof) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(QUANTUM_POW_PALLET_INDEX);
    out.push(SUBMIT_PROOF_CALL_INDEX);
    // Call args: single composite field `proof`.
    out.extend(proof.encode());
    out
}

/// SCALE-encode the `QuantumPow.register_miner()` call body.
///
/// The call takes no arguments, so the body is the two dispatch indices. The
/// deposit is reserved from the signing account.
#[must_use]
pub fn encode_register_miner_call() -> Vec<u8> {
    vec![QUANTUM_POW_PALLET_INDEX, REGISTER_MINER_CALL_INDEX]
}

/// Require an `AllowedValueSpec::Set` and return its (non-empty) values.
///
/// The coordinator draws `PoW` models with [`draw_ising_milli`] over a discrete
/// allowed set. The chain's `generate_ising_model` samples `IntegerRange` /
/// `ContinuousRange` specs with a *different* RNG consumption (see
/// `quantum_validation::AllowedValueSpec::sample`), so a coordinator-side
/// expansion of a range into a `Set` would neither reproduce the chain's model
/// nor match its topology hash. Rather than mine unverifiable jobs, reject
/// non-`Set` specs (and empty sets) here so the failure surfaces at snapshot
/// decode with a clear message.
///
/// [`draw_ising_milli`]: quip_protocol::chacha8::draw_ising_milli
///
/// # Errors
/// Returns an error for `IntegerRange` / `ContinuousRange` specs and for an
/// empty `Set`.
pub fn require_set_values(spec: &AllowedValueSpec<Vec<i32>>) -> Result<Vec<i32>, String> {
    match spec {
        AllowedValueSpec::Set(v) if !v.is_empty() => Ok(v.clone()),
        AllowedValueSpec::Set(_) => {
            Err("allowed-value Set is empty; a PoW model cannot be drawn".to_string())
        }
        AllowedValueSpec::IntegerRange { .. } | AllowedValueSpec::ContinuousRange { .. } => Err(
            "coordinator requires AllowedValueSpec::Set; range specs (IntegerRange/\
             ContinuousRange) sample differently on-chain and are not supported"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::topology_spec::{parse_topology_spec, preset_spec};
    use sp_core::U256;

    #[test]
    fn require_set_values_accepts_only_a_nonempty_set() {
        assert_eq!(
            require_set_values(&AllowedValueSpec::Set(vec![-1000, 0, 1000])).ok(),
            Some(vec![-1000, 0, 1000])
        );
        // Range specs sample differently on-chain; the coordinator must not
        // silently expand them into a Set (would diverge from consensus).
        for (label, spec) in [
            ("empty set", AllowedValueSpec::Set(Vec::new())),
            (
                "integer range",
                AllowedValueSpec::IntegerRange { min: -2, max: 2 },
            ),
            (
                "continuous range",
                AllowedValueSpec::ContinuousRange {
                    min: -1000,
                    max: 1000,
                },
            ),
        ] {
            assert!(require_set_values(&spec).is_err(), "{label}");
        }
    }

    /// The pallet's `TopologyMeta` byte layout under each API version, built
    /// field by field. Version 3 puts `family` before `registered_at`.
    #[test]
    fn topology_meta_decodes_in_the_layout_of_each_api_version() {
        let spec = AllowedValueSpec::Set(vec![-1000, 1000]);
        let mut common = Vec::new();
        common.extend(vec![0u32, 1].encode());
        common.extend(vec![(0u32, 1u32)].encode());
        common.extend(spec.encode());
        common.extend(spec.encode());
        common.extend(spec.encode());
        let registered_at = 0x0102_0304u32.encode();

        let mut v2 = common.clone();
        v2.extend(&registered_at);
        let legacy = TopologyMetaScale::decode_for_api(&mut v2.as_slice(), 2).unwrap();
        assert_eq!((legacy.family, legacy.registered_at), (None, 0x0102_0304));

        let descriptor = FamilyDescriptorScale {
            family_id: b"quip/ice-glass".to_vec(),
            revision: 1,
            params: vec![9; 5],
        };
        let mut v3 = common.clone();
        v3.extend(Some(descriptor.clone()).encode());
        v3.extend(&registered_at);
        let family = TopologyMetaScale::decode_for_api(&mut v3.as_slice(), 3).unwrap();
        assert_eq!(family.family, Some(descriptor));
        assert_eq!(family.registered_at, 0x0102_0304);

        let mut v3_legacy = common;
        v3_legacy.push(0);
        v3_legacy.extend(&registered_at);
        let unbound = TopologyMetaScale::decode_for_api(&mut v3_legacy.as_slice(), 3).unwrap();
        assert_eq!((unbound.family, unbound.registered_at), (None, 0x0102_0304));
    }

    #[test]
    fn register_miner_call_is_the_two_dispatch_indices() {
        let call = encode_register_miner_call();
        // The call takes no arguments, so anything longer is an encoding bug.
        assert_eq!(call, [QUANTUM_POW_PALLET_INDEX, REGISTER_MINER_CALL_INDEX]);
        assert_ne!(REGISTER_MINER_CALL_INDEX, SUBMIT_PROOF_CALL_INDEX);
    }

    #[test]
    fn submit_proof_call_starts_with_pallet_and_call_index() {
        let proof = QuantumProof {
            topology_hash: H256::repeat_byte(0),
            nonce: U256::zero(),
            salt: [0u8; 32],
            solutions: vec![vec![0]],
            device_access_time_us: 0,
        };
        let call = encode_submit_proof_call(&proof);
        #[expect(
            clippy::indexing_slicing,
            reason = "call always starts with pallet+call index bytes"
        )]
        {
            assert_eq!(call[0], QUANTUM_POW_PALLET_INDEX);
            assert_eq!(call[1], SUBMIT_PROOF_CALL_INDEX);
            // Remainder is the proof encoding.
            assert_eq!(&call[2..], &proof.encode());
        }
    }

    /// SCALE field order must match `pallet_quantum_pow::MinerInfo`. A reordered
    /// field decodes into the wrong number silently, so pin the byte layout.
    ///
    /// The fixture is built from the pallet's declared layout directly,
    /// independent of `MinerInfoScale`'s own field order. An encode-then-decode
    /// round trip through the same struct cannot catch a transposition. This
    /// test can, because the blob comes from
    /// `quip-validator/pallets/quantum-pow/src/types.rs`'s
    /// `MinerInfo<Balance, BlockNumber>` field order (`registered_at, deposit,
    /// proofs_submitted, proofs_won, rewards_earned`), not from the struct.
    /// Any change to that pallet layout must be mirrored here.
    #[test]
    fn miner_info_blob_built_from_the_pallet_layout_decodes_correctly() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&1_234_u32.to_le_bytes()); // registered_at
        blob.extend_from_slice(&1_000_000_000_000_u128.to_le_bytes()); // deposit
        blob.extend_from_slice(&7_u32.to_le_bytes()); // proofs_submitted
        blob.extend_from_slice(&3_u32.to_le_bytes()); // proofs_won
        blob.extend_from_slice(&70_000_000_000_000_u128.to_le_bytes()); // rewards_earned
        assert_eq!(blob.len(), 44);

        let decoded = MinerInfoScale::decode(&mut &blob[..]).unwrap();
        assert_eq!(decoded.registered_at, 1_234);
        assert_eq!(decoded.deposit, 1_000_000_000_000);
        assert_eq!(decoded.proofs_submitted, 7);
        assert_eq!(decoded.proofs_won, 3);
        assert_eq!(decoded.rewards_earned, 70_000_000_000_000);
        // The struct encodes back to the same pallet-layout bytes.
        assert_eq!(decoded.encode(), blob);
    }

    fn chain_meta_fixture() -> Result<([u8; 32], TopologyMetaScale), String> {
        let spec =
            parse_topology_spec(preset_spec("aglais")?).map_err(|error| error.to_string())?;
        let hash = spec
            .topology
            .hash
            .as_slice()
            .try_into()
            .map_err(|_| "topology hash is not 32 bytes".to_string())?;
        let edges = spec.topology.edge_pairs();
        Ok((
            hash,
            TopologyMetaScale {
                nodes: spec.topology.nodes,
                edges,
                allowed_h_values: AllowedValueSpec::Set(spec.allowed_h_milli),
                allowed_j_values: AllowedValueSpec::Set(spec.allowed_j_milli),
                allowed_spin_values: AllowedValueSpec::Set(spec.allowed_spin_milli),
                family: None,
                registered_at: 7,
            },
        ))
    }

    #[test]
    fn from_chain_meta_preserves_aglais_and_uses_neutral_gates() {
        let (hash, meta) = chain_meta_fixture().expect("valid Aglais metadata");
        let expected_nodes = meta.nodes.clone();
        let expected_edges = meta.edges.clone();
        let mut late_registration = meta.clone();
        let spec = meta.into_topology_spec(hash).expect("valid chain topology");
        assert_eq!(spec.topology.hash, hash.to_vec());
        assert_eq!(spec.topology.nodes, expected_nodes);
        assert_eq!(spec.topology.edge_pairs(), expected_edges);
        assert_eq!(spec.allowed_h_milli, vec![0]);
        assert_eq!(spec.allowed_j_milli, vec![-1000, 1000]);
        assert_eq!(spec.allowed_spin_milli, vec![-1000, 1000]);
        assert_eq!(spec.min_solutions, 0);
        assert_eq!(spec.max_energy_milli, i64::MAX);
        assert_eq!(spec.min_diversity_milli, 0);
        let snapshot = spec.to_snapshot();
        assert_eq!(snapshot.head_hash, [0; 32]);
        assert_eq!(snapshot.last_proof_block_hash, [0; 32]);
        assert_eq!(snapshot.block_number, 0);
        assert_eq!(snapshot.spec_version, 0);
        assert_eq!(snapshot.topology_hash, hash.to_vec());

        // The registration height is not part of the topology hash.
        late_registration.registered_at = u32::MAX;
        let spec = late_registration
            .into_topology_spec(hash)
            .expect("valid chain topology");
        assert_eq!(spec.topology.hash, hash.to_vec());
    }

    #[test]
    fn from_chain_meta_rejects_unsupported_sets_bad_graphs_and_hashes() {
        let (hash, meta) = chain_meta_fixture().expect("valid Aglais metadata");
        let mut integer_h = meta.clone();
        integer_h.allowed_h_values = AllowedValueSpec::IntegerRange { min: -1, max: 1 };
        let mut continuous_j = meta.clone();
        continuous_j.allowed_j_values = AllowedValueSpec::ContinuousRange {
            min: -1000,
            max: 1000,
        };
        let mut empty_spin = meta.clone();
        empty_spin.allowed_spin_values = AllowedValueSpec::Set(vec![]);
        let mut empty_nodes = meta.clone();
        empty_nodes.nodes.clear();
        let mut bad_edge = meta.clone();
        bad_edge.edges.push((938, u32::MAX));
        let mut incorrect_hash = hash;
        *incorrect_hash.first_mut().expect("nonempty hash") ^= 1;
        let mut bound = meta.clone();
        bound.family = Some(FamilyDescriptorScale {
            family_id: b"quip/ice-glass".to_vec(),
            revision: 1,
            params: Vec::new(),
        });
        let cases = [
            (
                "family topology",
                hash,
                bound,
                "bound to family quip/ice-glass revision 1",
            ),
            (
                "integer range h",
                hash,
                integer_h,
                "requires AllowedValueSpec::Set",
            ),
            (
                "continuous range j",
                hash,
                continuous_j,
                "requires AllowedValueSpec::Set",
            ),
            ("empty spin set", hash, empty_spin, "Set is empty"),
            ("empty nodes", hash, empty_nodes, "nodes must be non-empty"),
            (
                "edge with unknown node",
                hash,
                bad_edge,
                "references node id",
            ),
            (
                "hash mismatch",
                incorrect_hash,
                meta,
                "topology hash mismatch",
            ),
        ];
        for (label, hash, meta, message) in cases {
            let error = meta
                .into_topology_spec(hash)
                .expect_err("expected a chain topology error");
            assert!(error.contains(message), "{label}: {error}");
        }
    }
}
