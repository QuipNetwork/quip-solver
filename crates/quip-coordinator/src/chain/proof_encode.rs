//! Build a chain `QuantumProof` from a coordinator `Proof` + mining context.

use super::scale_types::QuantumProof;
use super::submit::Proof;
use crate::lease::ChallengeFamily;
use parity_scale_codec::Encode;
use quantum_validation::families::{AdvertisedLimits, FamilyChallenge, CONTRACTS};
use quantum_validation::packed::pack_solution;
use quantum_validation::{derive_nonce, AllowedValueSpec, MilliValue};
use sp_core::{H256, U256};

/// Inputs needed to encode and nonce a proof for submission.
#[derive(Clone, Debug)]
pub struct ProofBuildContext {
    /// Topology hash the proof is mined against.
    pub topology_hash: [u8; 32],
    /// Last winning proof's block hash (nonce derivation input).
    pub last_proof_block_hash: [u8; 32],
    /// Miner identity bytes (nonce derivation input).
    pub miner_identity: [u8; 32],
    /// 32-byte `PoW` salt.
    pub salt: [u8; 32],
    /// How each solution row becomes chain bytes.
    pub witness: WitnessFormat,
}

/// The wire format of the solutions in a `submit_proof` call. The pallet
/// picks it from the topology: a family topology accepts only its compact
/// codec, and any other topology accepts only the spin-spec packing.
#[derive(Clone, Debug)]
pub enum WitnessFormat {
    /// Bit-pack every row under the topology's spin spec. Each row covers
    /// every topology node.
    Packed {
        /// Expected spin vector length (topology node count).
        num_nodes: usize,
        /// Allowed spin values used when bit-packing solutions.
        allowed_spin: AllowedValueSpec<Vec<i32>>,
    },
    /// Encode every row with the compact witness codec of the family the
    /// topology is bound to. The codec belongs to the instance that the proof
    /// nonce regenerates, so each row covers that instance's nodes.
    Family {
        /// The family the chain bound to the topology.
        family: ChallengeFamily,
        /// Topology node ids, in chain order.
        nodes: Vec<u32>,
        /// Topology edges, in chain order.
        edges: Vec<(u32, u32)>,
    },
}

/// Encode a coordinator proof into the on-chain `QuantumProof` SCALE struct.
///
/// Spins are encoded in `ctx.witness`. Energies are discarded — the chain
/// recomputes them. Nonce is `derive_nonce(last_proof, miner, salt)`.
///
/// # Errors
/// Returns an error when a row has the wrong length or its spins cannot be
/// encoded in the witness format.
pub fn build_quantum_proof(proof: &Proof, ctx: &ProofBuildContext) -> Result<QuantumProof, String> {
    let nonce: U256 = derive_nonce(&ctx.last_proof_block_hash, &ctx.miner_identity, &ctx.salt);
    let rows = proof.solutions.iter().map(|row| row.spins.as_slice());
    let solutions = match &ctx.witness {
        WitnessFormat::Packed {
            num_nodes,
            allowed_spin,
        } => pack_rows(rows, *num_nodes, allowed_spin)?,
        WitnessFormat::Family {
            family,
            nodes,
            edges,
        } => encode_family_witnesses(family, nodes, edges, nonce.to_big_endian(), rows)?,
    };

    Ok(QuantumProof {
        topology_hash: H256::from(ctx.topology_hash),
        nonce,
        salt: ctx.salt,
        solutions,
        device_access_time_us: proof.device_access_time_us,
    })
}

/// Bit-pack each row of `num_nodes` signs under `allowed_spin`.
fn pack_rows<'a>(
    rows: impl Iterator<Item = &'a [i8]>,
    num_nodes: usize,
    allowed_spin: &AllowedValueSpec<Vec<i32>>,
) -> Result<Vec<Vec<u8>>, String> {
    let spin_spec = allowed_spin.as_slice();
    let mut packed = Vec::new();
    for spins in rows {
        if spins.len() != num_nodes {
            return Err(format!(
                "solution length {} != topology nodes {num_nodes}",
                spins.len()
            ));
        }
        let milli: Vec<MilliValue> = spins.iter().map(|&s| i32::from(s) * 1000).collect();
        packed
            .push(pack_solution(&milli, &spin_spec).map_err(|e| format!("pack_solution: {e:?}"))?);
    }
    Ok(packed)
}

/// The validator's challenge for `family` on the chain topology, admitted as
/// the pallet admits it. Registration already bounded every instance by the
/// chain's limits, so admission only needs to accept what the chain accepted.
fn family_challenge<'a>(
    family: &'a ChallengeFamily,
    nodes: &[u32],
    edges: &[(u32, u32)],
) -> Result<FamilyChallenge<'a>, String> {
    let descriptor = family.descriptor();
    let limits = AdvertisedLimits {
        max_nodes: u64::MAX,
        max_edges: u64::MAX,
        max_degree: 0,
    };
    FamilyChallenge::admit(
        CONTRACTS,
        descriptor.family_id.as_bytes(),
        descriptor.revision,
        &descriptor.params,
        nodes,
        edges,
        &limits,
    )
    .map_err(|e| format!("family admission: {e:?}"))
}

/// Encode each row with the compact codec of the instance `nonce`
/// regenerates. The codec rejects a row of the wrong length or one that
/// breaks a hard constraint.
///
/// # Errors
/// Returns the admission, regeneration, or codec error.
pub fn encode_family_witnesses<'a>(
    family: &ChallengeFamily,
    nodes: &[u32],
    edges: &[(u32, u32)],
    nonce: [u8; 32],
    rows: impl IntoIterator<Item = &'a [i8]>,
) -> Result<Vec<Vec<u8>>, String> {
    let challenge = family_challenge(family, nodes, edges)?;
    let instance = challenge
        .regenerate(nonce)
        .map_err(|e| format!("family instance: {e:?}"))?;
    let codec = challenge
        .prepare_witness_codec(&instance)
        .map_err(|e| format!("family codec: {e:?}"))?;
    rows.into_iter()
        .map(|spins| {
            codec
                .encode(spins)
                .map_err(|e| format!("family witness: {e}"))
        })
        .collect()
}

/// Decode and score compact family witnesses with the validator's own
/// `FamilyChallenge::decode_witnesses`, the call the pallet makes. Returns
/// each row's energy and spins. One bad witness rejects them all.
///
/// # Errors
/// Returns the admission, regeneration, codec, or witness error.
pub fn decode_family_witnesses(
    family: &ChallengeFamily,
    nodes: &[u32],
    edges: &[(u32, u32)],
    nonce: [u8; 32],
    witnesses: &[Vec<u8>],
) -> Result<Vec<(i64, Vec<i8>)>, String> {
    let challenge = family_challenge(family, nodes, edges)?;
    let (rows, energies) = challenge
        .decode_witnesses(nonce, witnesses.iter().map(Vec::as_slice))
        .map_err(|e| format!("family witness: {e:?}"))?;
    Ok(energies.into_iter().zip(rows).collect())
}

/// SCALE-encode a quantum proof (for tests / debugging).
#[must_use]
pub fn encode_quantum_proof(proof: &QuantumProof) -> Vec<u8> {
    proof.encode()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::submit::Proof;
    use parity_scale_codec::Decode;

    fn binary_ctx() -> ProofBuildContext {
        ProofBuildContext {
            topology_hash: [0xab; 32],
            last_proof_block_hash: [0x11; 32],
            miner_identity: [0x22; 32],
            salt: [0x33; 32],
            witness: WitnessFormat::Packed {
                num_nodes: 4,
                allowed_spin: AllowedValueSpec::Set(vec![-1000, 1000]),
            },
        }
    }

    #[test]
    fn packs_binary_spins_and_derives_nonce() {
        let ctx = binary_ctx();
        let proof = Proof {
            job_id: vec![1],
            best_energy_milli: -100,
            diversity_milli: 200,
            n_valid: 1,
            solutions: vec![crate::validate::SpinRow {
                spins: vec![1, -1, 1, -1],
                energy_milli: -100,
            }],
            is_pow: true,
            order_id: vec![],
            generation: 1,
            salt: ctx.salt.to_vec(),
            device_access_time_us: 123_456,
        };
        let qp = build_quantum_proof(&proof, &ctx).expect("build");
        assert_eq!(qp.topology_hash, H256::from(ctx.topology_hash));
        assert_eq!(qp.salt, ctx.salt);
        assert_eq!(qp.device_access_time_us, 123_456);
        assert_eq!(qp.solutions.len(), 1);
        // 4 spins * 1 bit = 1 byte
        #[expect(
            clippy::indexing_slicing,
            reason = "single packed solution asserted above"
        )]
        {
            assert_eq!(qp.solutions[0].len(), 1);
        }
        let expected = derive_nonce(&ctx.last_proof_block_hash, &ctx.miner_identity, &ctx.salt);
        assert_eq!(qp.nonce, expected);

        // SCALE roundtrip of the built proof.
        let enc = encode_quantum_proof(&qp);
        let dec = QuantumProof::decode(&mut &enc[..]).expect("decode");
        assert_eq!(dec, qp);
    }
}
