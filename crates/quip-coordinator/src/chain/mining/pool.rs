//! Pending `QuantumPow.submit_proof` extrinsics from the transaction pool.
//!
//! `author_pendingExtrinsics` returns every pool entry as opaque bytes. The
//! coordinator wants only signed `submit_proof` calls, so the decoder here
//! walks the signed v4 envelope the coordinator itself emits
//! (`extrinsic::build_hybrid_signed_extrinsic`) and returns nothing for
//! every other shape. That is not an error: the pool holds every kind of
//! call.
//!
//! [`judge_pending_proof`] replays the pallet's `submit_proof` checks
//! (`quip-validator/pallets/quantum-pow/src/lib.rs`, `validate_proof`) with
//! the same `quantum_validation` functions, against the snapshot the feeder is
//! mining. Difficulty eases every block until a win, so a proof that clears
//! now also clears at its inclusion block.

use crate::chain::codec::extrinsic::account_identity_bytes;
use crate::chain::codec::extrinsic::{
    encode_additional, extrinsic_hash, signing_payload, RuntimeTxContext,
};
use crate::chain::codec::proof_encode::decode_family_witnesses;
use crate::chain::codec::quantum_pow::{QUANTUM_POW_PALLET_INDEX, SUBMIT_PROOF_CALL_INDEX};
use crate::types::family::ChallengeFamily;
use crate::types::proof::QuantumProof;
use crate::types::snapshot::{MiningSnapshot, PendingProof, Verdict};
use quantum_validation::{
    calculate_diversity, derive_nonce, energy_of_solution_indexed, generate_ising_model_indexed,
    select_diverse, unpack_solution, AllowedValueSpec, TopologyIndex,
};

use parity_scale_codec::{Compact, Decode, Input};
use quip_transaction_crypto::{HybridPair, HybridTxSignature};
use sp_core::Pair as _;

/// Version byte of a signed v4 extrinsic, the only envelope the coordinator emits.
const SIGNED_V4: u8 = 0x84;
/// `MultiAddress::Id` discriminant.
const MULTI_ADDRESS_ID: u8 = 0x00;
/// `Era::Immortal` is one zero byte. A mortal era is two bytes.
const ERA_IMMORTAL: u8 = 0x00;
/// `CheckMetadataHash` mode: no metadata hash in `additional_signed`.
const METADATA_HASH_DISABLED: u8 = 0x00;
/// `CheckMetadataHash` mode: the runtime metadata hash in `additional_signed`.
const METADATA_HASH_ENABLED: u8 = 0x01;

/// Decode one pool entry and check its signature against `ctx`.
///
/// `None` for anything that is not a signed v4 `submit_proof` whose signature
/// holds: unsigned extrinsics, other calls, other envelope versions, a compact
/// length that differs from the bytes after it, or bytes left over after the
/// call. Also `None` for a mortal era. The coordinator signs immortal
/// extrinsics, so it does not look up era birth blocks.
///
/// A signer that used `CheckMetadataHash` mode Enabled committed to the
/// runtime metadata hash, which the coordinator does not compute. For that
/// mode only the account binding is checked: the embedded public key must
/// derive the claimed account. The pool-watch judge still replays the qblock
/// gates on the proof, and the proof nonce derives from that account.
#[must_use]
pub fn decode_pending_proof(bytes: &[u8], ctx: &RuntimeTxContext) -> Option<PendingProof> {
    let input = &mut &bytes[..];
    let declared = Compact::<u32>::decode(input).ok()?.0;
    if usize::try_from(declared).ok()? != input.len() {
        return None;
    }
    if input.read_byte().ok()? != SIGNED_V4 {
        return None;
    }
    if input.read_byte().ok()? != MULTI_ADDRESS_ID {
        return None;
    }
    let account = <[u8; 32]>::decode(input).ok()?;
    let signature = HybridTxSignature::decode(input).ok()?;
    let extra_start = offset(bytes, input)?;
    let metadata_hash_mode = read_signed_extensions(input)?;
    let call_start = offset(bytes, input)?;
    if input.read_byte().ok()? != QUANTUM_POW_PALLET_INDEX {
        return None;
    }
    if input.read_byte().ok()? != SUBMIT_PROOF_CALL_INDEX {
        return None;
    }
    let proof = QuantumProof::decode(input).ok()?;
    if !input.is_empty() {
        return None;
    }
    let extra = bytes.get(extra_start..call_start)?;
    let call = bytes.get(call_start..)?;
    if !signature_holds(&signature, &account, call, extra, metadata_hash_mode, ctx) {
        return None;
    }
    Some(PendingProof {
        extrinsic_hash: extrinsic_hash(bytes),
        account,
        proof,
    })
}

/// Position of `rest` inside `bytes`, where `rest` is a suffix of `bytes`.
fn offset(bytes: &[u8], rest: &[u8]) -> Option<usize> {
    bytes.len().checked_sub(rest.len())
}

/// Read the signed extensions in metadata order, mirroring
/// `extrinsic::encode_extra`: era, nonce, tip, metadata-hash mode.
///
/// Returns the metadata-hash mode. `None` for a mortal era or an unknown mode.
fn read_signed_extensions(input: &mut &[u8]) -> Option<u8> {
    if input.read_byte().ok()? != ERA_IMMORTAL {
        return None;
    }
    let _nonce = Compact::<u32>::decode(input).ok()?;
    let _tip = Compact::<u128>::decode(input).ok()?;
    match input.read_byte().ok()? {
        mode @ (METADATA_HASH_DISABLED | METADATA_HASH_ENABLED) => Some(mode),
        _ => None,
    }
}

/// Whether `signature` binds `account` and, unless the signer committed to
/// a metadata hash, signs `call || extra || additional` under `ctx`.
fn signature_holds(
    signature: &HybridTxSignature,
    account: &[u8; 32],
    call: &[u8],
    extra: &[u8],
    metadata_hash_mode: u8,
    ctx: &RuntimeTxContext,
) -> bool {
    let derived = signature.derived_account_id();
    if AsRef::<[u8; 32]>::as_ref(&derived) != account {
        return false;
    }
    if metadata_hash_mode == METADATA_HASH_ENABLED {
        return true;
    }
    let additional =
        encode_additional(ctx.spec_version, ctx.transaction_version, &ctx.genesis_hash);
    let payload = signing_payload(call, extra, &additional);
    HybridPair::verify(&signature.signature, &payload, &signature.public)
}

/// Replay the pallet's `submit_proof` checks against `snap`.
///
/// Registration of the signer is not checked. The pool already ran the signed
/// extensions. A clearing proof from an unregistered account is included, fails,
/// and leaves the pool without a qblock. The feeder then resumes and ignores
/// that proof, by signer and nonce, for the rest of the root, so a resubmission
/// cannot stop the miners again.
#[must_use]
pub fn judge_pending_proof(pending: &PendingProof, snap: &MiningSnapshot) -> Verdict {
    if pending.proof.topology_hash.as_bytes() != snap.topology_hash.as_slice() {
        return Verdict::OtherRound;
    }
    let identity = account_identity_bytes(&pending.account);
    let expected = derive_nonce(&snap.last_proof_block_hash, &identity, &pending.proof.salt);
    if pending.proof.nonce != expected {
        return Verdict::OtherRound;
    }
    match score(&pending.proof, snap) {
        Ok(verdict) => verdict,
        Err(reason) => Verdict::Malformed(reason),
    }
}

/// Score every row, and apply the gates. `Err` is the pallet's pre-scoring
/// rejection.
fn score(proof: &QuantumProof, snap: &MiningSnapshot) -> Result<Verdict, String> {
    let scored = match &snap.family {
        None => legacy_rows(proof, snap)?,
        Some(family) => family_rows(proof, snap, family)?,
    };
    let valid: Vec<(i64, Vec<i8>)> = scored
        .into_iter()
        .filter(|(energy, _)| *energy < snap.max_energy_milli)
        .collect();

    let Some(best_energy_milli) = valid.iter().map(|(e, _)| *e).min() else {
        return Ok(Verdict::BelowGates);
    };
    let rows: Vec<&[i8]> = valid.iter().map(|(_, s)| s.as_slice()).collect();
    let target = rows
        .len()
        .min(usize::try_from(snap.min_solutions.max(1)).unwrap_or(usize::MAX));
    let selected_idx = select_diverse(&rows, target).map_err(|e| format!("select: {e:?}"))?;
    let selected: Vec<&[i8]> = selected_idx
        .iter()
        .filter_map(|&i| rows.get(i).copied())
        .collect();
    let diversity = calculate_diversity(&selected).map_err(|e| format!("diversity: {e:?}"))?;
    let n_valid = u32::try_from(rows.len()).unwrap_or(u32::MAX);

    if n_valid >= snap.min_solutions && diversity >= snap.min_diversity_milli {
        Ok(Verdict::Clears { best_energy_milli })
    } else {
        Ok(Verdict::BelowGates)
    }
}

/// Each row's spins and energy on the legacy model the nonce draws, as the
/// pallet's `validate_proof` computes them.
fn legacy_rows(proof: &QuantumProof, snap: &MiningSnapshot) -> Result<Vec<(i64, Vec<i8>)>, String> {
    let index =
        TopologyIndex::new(&snap.nodes, &snap.edges).map_err(|e| format!("topology: {e:?}"))?;
    let (h, j) = generate_ising_model_indexed(
        proof.nonce,
        &snap.nodes,
        &snap.edges,
        &AllowedValueSpec::Set(snap.allowed_h_milli.as_slice()),
        &AllowedValueSpec::Set(snap.allowed_j_milli.as_slice()),
        &index,
    )
    .map_err(|e| format!("model: {e:?}"))?;
    let mut rows = Vec::with_capacity(proof.solutions.len());
    for spins in unpack_rows(proof, snap)? {
        let energy = energy_of_solution_indexed(&spins, &h, &snap.edges, &j, &index)
            .map_err(|e| format!("energy: {e:?}"))?;
        rows.push((energy, spins));
    }
    Ok(rows)
}

/// Each row's spins and energy on the family instance the nonce regenerates,
/// as the pallet's `validate_family_proof` computes them: the validator's own
/// `FamilyChallenge::decode_witnesses` over the compact codec bytes. A skipped
/// nonce or one bad witness rejects the whole proof, as it does on chain.
fn family_rows(
    proof: &QuantumProof,
    snap: &MiningSnapshot,
    family: &ChallengeFamily,
) -> Result<Vec<(i64, Vec<i8>)>, String> {
    decode_family_witnesses(
        family,
        &snap.nodes,
        &snap.edges,
        proof.nonce.to_big_endian(),
        &proof.solutions,
    )
}

/// Unpack every row to one sign per topology node under the snapshot's spin set.
fn unpack_rows(proof: &QuantumProof, snap: &MiningSnapshot) -> Result<Vec<Vec<i8>>, String> {
    let num_spins = snap.nodes.len();
    let spin_spec = AllowedValueSpec::Set(snap.allowed_spin_milli.as_slice());
    let mut rows = Vec::with_capacity(proof.solutions.len());
    for packed in &proof.solutions {
        let milli =
            unpack_solution(packed, num_spins, &spin_spec).map_err(|e| format!("unpack: {e:?}"))?;
        let mut spins = Vec::with_capacity(milli.len());
        for value in milli {
            spins.push(match value.signum() {
                1 => 1i8,
                -1 => -1i8,
                _ => return Err("zero spin".into()),
            });
        }
        rows.push(spins);
    }
    Ok(rows)
}

use crate::chain::client::RealChainClient;
use crate::chain::ChainError;
use crate::types::hex::hex_decode;
use serde_json::Value;

impl RealChainClient {
    /// Read the pending proofs; see `ChainClient::fetch_pending_proofs`.
    pub(in crate::chain) async fn read_pending_proofs(
        &self,
    ) -> Result<Vec<PendingProof>, ChainError> {
        let raw = self
            .rpc_call("author_pendingExtrinsics", Value::Array(vec![]))
            .await?;
        let Some(entries) = raw.as_array() else {
            return Err(ChainError::Decode(
                "author_pendingExtrinsics did not return an array".into(),
            ));
        };
        let ctx = self.runtime_tx_context().await?;
        let mut proofs = Vec::new();
        for entry in entries {
            let Some(hex) = entry.as_str() else { continue };
            let bytes = hex_decode(hex).map_err(ChainError::Decode)?;
            if let Some(p) = decode_pending_proof(&bytes, &ctx) {
                proofs.push(p);
            }
        }
        Ok(proofs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::codec::extrinsic::{
        build_hybrid_signed_extrinsic, signer_account_bytes, SignedExtensionContext,
    };
    use crate::chain::codec::quantum_pow::{encode_register_miner_call, encode_submit_proof_call};
    use parity_scale_codec::Encode;
    use sp_core::{H256, U256};
    use sp_crypto_hashing::blake2_256;

    fn proof() -> QuantumProof {
        QuantumProof {
            topology_hash: H256::repeat_byte(1),
            nonce: U256::from(42u64),
            salt: [7u8; 32],
            solutions: vec![vec![0b01], vec![0b10]],
            device_access_time_us: 5,
        }
    }

    fn signed(call: &[u8]) -> (HybridPair, Vec<u8>) {
        let pair = HybridPair::from_string("//Alice", None).expect("alice");
        let ctx = SignedExtensionContext {
            account_nonce: 3,
            genesis_hash: [0x11; 32],
            spec_version: 100,
            transaction_version: 1,
            tip: 0,
        };
        let ext = build_hybrid_signed_extrinsic(&pair, call, &ctx);
        (pair, ext)
    }

    use crate::chain::codec::extrinsic::{
        assemble_signed_extrinsic, encode_additional, signing_payload, RuntimeTxContext,
    };
    use quip_transaction_crypto::HybridTxSignature;

    /// Matches the `SignedExtensionContext` in `signed`.
    const CTX: RuntimeTxContext = RuntimeTxContext {
        genesis_hash: [0x11; 32],
        spec_version: 100,
        transaction_version: 1,
    };

    /// Signed extensions with a chosen era and metadata-hash mode.
    fn extra_with(era: &[u8], mode: u8) -> Vec<u8> {
        let mut extra = era.to_vec();
        extra.extend(Compact(3u32).encode());
        extra.extend(Compact(0u128).encode());
        extra.push(mode);
        extra
    }

    /// A signed extrinsic with chosen extensions and additional data.
    fn signed_with(pair: &HybridPair, call: &[u8], extra: &[u8], additional: &[u8]) -> Vec<u8> {
        let sig = HybridTxSignature::sign(pair, &signing_payload(call, extra, additional));
        assemble_signed_extrinsic(&signer_account_bytes(pair), &sig, extra, call)
    }

    /// `additional` for `CheckMetadataHash` mode Enabled with a made-up hash.
    fn additional_with_metadata_hash() -> Vec<u8> {
        let mut additional = encode_additional(100, 1, &[0x11; 32]);
        let _ = additional.pop(); // Option::None
        additional.push(0x01); // Option::Some
        additional.extend_from_slice(&[0xab; 32]);
        additional
    }

    /// Replace the account field with Bob's account.
    fn with_bob_as_signer(mut ext: Vec<u8>, alice: &HybridPair) -> Vec<u8> {
        let bob = HybridPair::from_string("//Bob", None).expect("bob");
        let at = ext
            .windows(32)
            .position(|w| w == signer_account_bytes(alice))
            .expect("account field");
        let _ = ext.splice(at..at + 32, signer_account_bytes(&bob));
        ext
    }

    /// Re-prefix the body with a compact length off by `delta`.
    fn with_length_off_by(ext: &[u8], delta: i64) -> Vec<u8> {
        let input = &mut &ext[..];
        let len = Compact::<u32>::decode(input).expect("len").0;
        let wrong = u32::try_from(i64::from(len) + delta).expect("len fits");
        let mut out = Compact(wrong).encode();
        out.extend_from_slice(input);
        out
    }

    #[test]
    fn a_metadata_hash_signer_is_accepted_on_its_account_binding() {
        let alice = HybridPair::from_string("//Alice", None).expect("alice");
        let call = encode_submit_proof_call(&proof());
        let ext = signed_with(
            &alice,
            &call,
            &extra_with(&[0x00], 0x01),
            &additional_with_metadata_hash(),
        );
        let pending = decode_pending_proof(&ext, &CTX).expect("decodes");
        assert_eq!(pending.account, signer_account_bytes(&alice));
    }

    #[test]
    fn a_signed_submit_proof_round_trips() {
        let (pair, ext) = signed(&encode_submit_proof_call(&proof()));
        let pending = decode_pending_proof(&ext, &CTX).expect("decodes");
        assert_eq!(pending.account, signer_account_bytes(&pair));
        assert_eq!(pending.proof, proof());
        // The pool hash is `blake2_256` of the whole blob, length prefix included.
        assert_eq!(pending.extrinsic_hash, blake2_256(&ext));
    }

    #[test]
    fn a_malformed_or_foreign_entry_decodes_to_nothing() {
        let call = encode_submit_proof_call(&proof());
        let (alice, ext) = signed(&call);

        // The last byte belongs to `device_access_time_us`, so the bytes still
        // decode and only the signature catches the change.
        let mut tampered = ext.clone();
        *tampered.last_mut().expect("byte") ^= 0x01;

        let metadata_hash_ext = signed_with(
            &alice,
            &call,
            &extra_with(&[0x00], 0x01),
            &additional_with_metadata_hash(),
        );

        let mut unsigned = Compact(u32::try_from(call.len() + 1).expect("len")).encode();
        unsigned.push(0x04);
        unsigned.extend_from_slice(&call);

        let mut trailing = call.clone();
        trailing.push(0xff);
        let mut truncated = call.clone();
        truncated.truncate(call.len() - 10);

        let other_chain = RuntimeTxContext {
            genesis_hash: [0x22; 32],
            ..CTX
        };
        let other_runtime = RuntimeTxContext {
            spec_version: 101,
            ..CTX
        };

        for (label, entry, ctx) in [
            (
                "compact length one short",
                with_length_off_by(&ext, -1),
                CTX,
            ),
            ("compact length one long", with_length_off_by(&ext, 1), CTX),
            ("signed for another chain", ext.clone(), other_chain),
            ("signed for another runtime", ext.clone(), other_runtime),
            ("tampered call", tampered, CTX),
            ("swapped account", with_bob_as_signer(ext, &alice), CTX),
            (
                "mortal era",
                signed_with(
                    &alice,
                    &call,
                    &extra_with(&[0x15, 0x00], 0x00),
                    &encode_additional(100, 1, &[0x11; 32]),
                ),
                CTX,
            ),
            (
                "metadata-hash entry with a foreign account",
                with_bob_as_signer(metadata_hash_ext, &alice),
                CTX,
            ),
            (
                "unknown metadata-hash mode",
                signed_with(
                    &alice,
                    &call,
                    &extra_with(&[0x00], 0x02),
                    &encode_additional(100, 1, &[0x11; 32]),
                ),
                CTX,
            ),
            ("another call", signed(&encode_register_miner_call()).1, CTX),
            ("unsigned extrinsic", unsigned, CTX),
            ("trailing call bytes", signed(&trailing).1, CTX),
            ("truncated call bytes", signed(&truncated).1, CTX),
        ] {
            assert!(decode_pending_proof(&entry, &ctx).is_none(), "{label}");
        }
    }
}

#[cfg(test)]
mod fetch_tests {
    use super::*;
    use crate::chain::client::transport::RpcTransport;
    use crate::chain::{ChainError, MiningSnapshot};
    use crate::types::hex::hex_encode;
    use crate::types::miner::MinerKind;
    use quip_transaction_crypto::HybridPair;
    use serde_json::Value as JsonValue;

    /// Answers the pool decoder's reads and counts the runtime-version calls.
    struct PoolValidator {
        pending: Vec<String>,
        runtime_version_calls: std::sync::Mutex<u32>,
        spec_version: std::sync::Mutex<u32>,
        transaction_version: std::sync::Mutex<u32>,
        fail_runtime_version: bool,
    }

    #[async_trait::async_trait]
    impl RpcTransport for PoolValidator {
        async fn request(
            &self,
            _url: &str,
            method: &str,
            _params: JsonValue,
        ) -> Result<JsonValue, ChainError> {
            match method {
                "chain_getBlockHash" => Ok(JsonValue::String(hex_encode(&[0x11; 32]))),
                "state_getRuntimeVersion" => {
                    *self.runtime_version_calls.lock().unwrap() += 1;
                    if self.fail_runtime_version {
                        return Err(ChainError::Unavailable("scripted outage".into()));
                    }
                    Ok(serde_json::json!({
                        "specVersion": *self.spec_version.lock().unwrap(),
                        "transactionVersion": *self.transaction_version.lock().unwrap(),
                    }))
                }
                "author_pendingExtrinsics" => Ok(JsonValue::Array(
                    self.pending
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                )),
                other => panic!("unexpected rpc method {other}"),
            }
        }

        async fn subscribe(
            &self,
            _url: &str,
            _sub: &str,
            _params: JsonValue,
            _unsub: &str,
        ) -> Result<
            crate::chain::client::transport::BoxStream<'static, Result<JsonValue, ChainError>>,
            ChainError,
        > {
            Err(ChainError::Unavailable(
                "no subscriptions in this test".into(),
            ))
        }
    }

    fn pool_snapshot(spec_version: u32) -> MiningSnapshot {
        MiningSnapshot {
            head_hash: [0u8; 32],
            last_proof_block_hash: [0u8; 32],
            topology_hash: vec![0u8; 32],
            nodes: vec![0, 1],
            edges: vec![(0, 1)],
            allowed_h_milli: vec![0],
            allowed_j_milli: vec![-1000, 1000],
            allowed_spin_milli: vec![-1000, 1000],
            min_solutions: 1,
            max_energy_milli: 0,
            min_diversity_milli: 0,
            block_number: 1,
            spec_version,
            target_proof_blocks: crate::types::snapshot::TARGET_PROOF_BLOCKS,
            family: None,
        }
    }

    fn pool_entry(genesis_hash: [u8; 32], spec_version: u32, transaction_version: u32) -> String {
        use crate::chain::codec::extrinsic::{
            build_hybrid_signed_extrinsic, SignedExtensionContext,
        };
        use crate::chain::codec::quantum_pow::encode_submit_proof_call;
        use crate::types::proof::QuantumProof;
        let pair = HybridPair::from_string("//Alice", None).expect("alice");
        let call = encode_submit_proof_call(&QuantumProof {
            topology_hash: sp_core::H256::repeat_byte(1),
            nonce: sp_core::U256::from(42u64),
            salt: [7u8; 32],
            solutions: vec![vec![0b01]],
            device_access_time_us: 5,
        });
        let ctx = SignedExtensionContext {
            account_nonce: 0,
            genesis_hash,
            spec_version,
            transaction_version,
            tip: 0,
        };
        hex_encode(&build_hybrid_signed_extrinsic(&pair, &call, &ctx))
    }

    #[tokio::test]
    async fn pending_proofs_are_verified_and_the_context_is_cached_per_spec_version() {
        use crate::chain::codec::extrinsic::extrinsic_hash;
        use crate::chain::ChainClient as _;
        use crate::types::hex::hex_decode;
        let upgraded_entry = pool_entry([0x11; 32], 101, 2);
        let upgraded_hash = extrinsic_hash(&hex_decode(&upgraded_entry).expect("upgraded hex"));
        let validator = std::sync::Arc::new(PoolValidator {
            pending: vec![
                pool_entry([0x11; 32], 100, 1),
                pool_entry([0x22; 32], 100, 1),
                upgraded_entry,
            ],
            runtime_version_calls: std::sync::Mutex::new(0),
            spec_version: std::sync::Mutex::new(100),
            transaction_version: std::sync::Mutex::new(1),
            fail_runtime_version: false,
        });
        let client = RealChainClient::with_transport(
            vec!["ws://scripted".to_string()],
            "//Alice".to_string(),
            MinerKind::Cpu,
            validator.clone(),
        );
        *client.last_snapshot.lock().unwrap() = Some(pool_snapshot(100));

        let proofs = client.fetch_pending_proofs().await.expect("pool read");
        assert_eq!(
            proofs.len(),
            1,
            "entries signed for another genesis or runtime are dropped"
        );
        let _ = client.fetch_pending_proofs().await.expect("pool read");
        assert_eq!(
            *validator.runtime_version_calls.lock().unwrap(),
            1,
            "the second poll at the same spec_version reuses the context"
        );

        *validator.spec_version.lock().unwrap() = 101;
        *validator.transaction_version.lock().unwrap() = 2;
        *client.last_snapshot.lock().unwrap() = Some(pool_snapshot(101));
        let after_upgrade = client.fetch_pending_proofs().await.expect("pool read");
        assert_eq!(*validator.runtime_version_calls.lock().unwrap(), 2);
        assert_eq!(
            after_upgrade.len(),
            1,
            "only the entry signed for spec 101 / transaction version 2 is accepted"
        );
        assert_eq!(
            after_upgrade
                .first()
                .expect("upgraded proof")
                .extrinsic_hash,
            upgraded_hash,
            "entries signed for spec 100 / transaction version 1 are dropped"
        );
    }

    #[tokio::test]
    async fn a_runtime_version_outage_is_an_error_not_a_panic() {
        use crate::chain::ChainClient as _;
        let validator = std::sync::Arc::new(PoolValidator {
            pending: vec![pool_entry([0x11; 32], 100, 1)],
            runtime_version_calls: std::sync::Mutex::new(0),
            spec_version: std::sync::Mutex::new(100),
            transaction_version: std::sync::Mutex::new(1),
            fail_runtime_version: true,
        });
        let client = RealChainClient::with_transport(
            vec!["ws://scripted".to_string()],
            "//Alice".to_string(),
            MinerKind::Cpu,
            validator,
        );
        assert!(client.fetch_pending_proofs().await.is_err());
    }
}
