//! The round root: which block hash the next `PoW` nonce derives from.

/// The block hash the next `PoW` nonce derives from.
///
/// The pallet writes `LastProofBlockHash` in `on_initialize` of the block
/// after a win, because a block cannot store its own hash. At the winning
/// block itself `LastProofBlock` already equals the head number, and the head
/// hash is the value the pallet stores next block. Returning it here starts
/// the next round one block early. A proof derived from it is valid from the
/// next block on, which is the first block that can include it.
#[must_use]
pub fn round_root(
    head_hash: [u8; 32],
    block_number: u64,
    last_proof_block: u64,
    stored_hash: [u8; 32],
) -> [u8; 32] {
    if last_proof_block == block_number {
        head_hash
    } else {
        stored_hash
    }
}

use crate::chain::client::storage::{
    parse_block_number, parse_spec_version, quantum_pow_api_version,
};
use crate::chain::client::{CachedTopology, RealChainClient};
use crate::chain::codec::extrinsic::{
    default_topology_storage_key, difficulties_storage_key, last_proof_block_hash_storage_key,
    last_proof_block_storage_key, miners_storage_key, target_proof_blocks_override_storage_key,
    topology_curve_c_storage_key, topology_energy_band_storage_key,
};
use crate::chain::codec::quantum_pow::{
    require_set_values, CurveCScale, DifficultyConfig, EnergyBandScale, MinerInfoScale,
    TopologyMetaScale,
};
use crate::chain::ChainError;
use crate::types::hex::{hex_decode, hex_encode};
use crate::types::miner::MinerInfo;
use crate::types::snapshot::{
    DecayParams, MiningSnapshot, DEFAULT_BASE_MAX_ENERGY_MILLI, DEFAULT_C_EASY_MILLI,
    DEFAULT_C_HARD_MILLI, DEFAULT_C_KNEE_MILLI, EPOCH_LENGTH_BLOCKS,
};
use parity_scale_codec::Decode;
use quantum_validation::AllowedValueSpec;
use serde_json::Value;

impl RealChainClient {
    /// The topology the snapshot mines on: the caller's choice, else the
    /// chain's `DefaultTopology` at `at_hex`. `Ok(None)` when neither names one.
    pub(in crate::chain) async fn snapshot_topology_hash(
        &self,
        chosen: Option<[u8; 32]>,
        at_hex: &str,
    ) -> Result<Option<[u8; 32]>, ChainError> {
        match chosen {
            Some(hash) => Ok(Some(hash)),
            None => {
                self.read_storage::<[u8; 32]>(&default_topology_storage_key(), at_hex)
                    .await
            }
        }
    }

    /// The registered topology under `hash`: the cached one when the last
    /// snapshot used the same hash, else `QuantumPowApi_topology_meta`, whose
    /// answer replaces the cache. `Ok(None)` when the hash is not registered.
    /// `api_version` is the runtime's `QuantumPowApi` version, which fixes the
    /// record layout.
    pub(in crate::chain) async fn topology_meta(
        &self,
        hash: [u8; 32],
        at_hex: &str,
        api_version: u32,
    ) -> Result<Option<TopologyMetaScale>, ChainError> {
        let cached = self.topology.lock().ok().and_then(|g| {
            g.as_ref()
                .filter(|c| c.hash == hash)
                .map(|c| c.meta.clone())
        });
        if cached.is_some() {
            return Ok(cached);
        }
        let meta = match self
            .quantum_pow_bytes_by_hash("QuantumPowApi_topology_meta", &hash, at_hex)
            .await?
        {
            None => None,
            Some(bytes) => decode_topology_meta(&bytes, api_version)?,
        };
        if let Some(meta) = &meta {
            tracing::info!(
                topology = %hex_encode(&hash),
                nodes = meta.nodes.len(),
                edges = meta.edges.len(),
                "topology downloaded"
            );
            if let Ok(mut g) = self.topology.lock() {
                *g = Some(CachedTopology {
                    hash,
                    meta: meta.clone(),
                });
            }
        }
        Ok(meta)
    }

    /// Fetch the chain's default topology at one best-block hash.
    ///
    /// Returns `None` when `QuantumPow.DefaultTopology` is unset. This reads
    /// topology metadata only and never loads signing material.
    ///
    /// # Errors
    /// Returns transport or decode errors. A stored default whose metadata
    /// is absent returns a decode error.
    pub async fn fetch_default_topology(
        &self,
    ) -> Result<Option<([u8; 32], TopologyMetaScale)>, ChainError> {
        let head = self
            .rpc_call("chain_getBlockHash", Value::Array(vec![]))
            .await?;
        let at = head
            .as_str()
            .ok_or_else(|| ChainError::Decode("chain_getBlockHash not a string".into()))?;
        let Some(hash) = self.snapshot_topology_hash(None, at).await? else {
            return Ok(None);
        };
        let runtime_version = self
            .rpc_call(
                "state_getRuntimeVersion",
                Value::Array(vec![Value::String(at.to_owned())]),
            )
            .await?;
        let api_version = quantum_pow_api_version(&runtime_version)?;
        let metadata = self
            .topology_meta(hash, at, api_version)
            .await?
            .ok_or_else(|| {
                ChainError::Decode(format!(
                    "default topology {} is not registered",
                    hex_encode(&hash)
                ))
            })?;
        Ok(Some((hash, metadata)))
    }

    /// Read `QuantumPow.DefaultTopology` at the current head.
    ///
    /// `Ok(None)` means no topology is registered, which is the state a fresh
    /// chain starts in and the only state the seed path accepts.
    ///
    /// # Errors
    /// Returns a transport error when the validator cannot be reached, or a
    /// decode error when the stored value is not a 32-byte hash.
    pub(crate) async fn default_topology(&self) -> Result<Option<[u8; 32]>, ChainError> {
        let at = self.head_hex().await?;
        self.read_storage::<[u8; 32]>(&default_topology_storage_key(), &at)
            .await
    }

    /// Read the mining snapshot; see `ChainClient::fetch_mining_snapshot`.
    #[expect(
        clippy::too_many_lines,
        reason = "one snapshot read: head, topology, family binding, difficulty, and root"
    )]
    pub(in crate::chain) async fn read_mining_snapshot(
        &self,
        at: Option<[u8; 32]>,
        topology_hash: Option<[u8; 32]>,
    ) -> Result<Option<MiningSnapshot>, ChainError> {
        let block_hash = if let Some(h) = at {
            h
        } else {
            let head = self
                .rpc_call("chain_getBlockHash", Value::Array(vec![]))
                .await?;
            let hex = head
                .as_str()
                .ok_or_else(|| ChainError::Decode("chain_getBlockHash not a string".into()))?;
            let bytes = hex_decode(hex).map_err(ChainError::Decode)?;
            if bytes.len() != 32 {
                return Err(ChainError::Decode(format!(
                    "block hash len {}",
                    bytes.len()
                )));
            }
            let mut h = [0u8; 32];
            h.copy_from_slice(&bytes);
            h
        };
        let at_hex = hex_encode(&block_hash);

        // Every read is pinned at `block_hash`, so the six round trips that
        // only depend on it run together. The topology itself is not among
        // them: it is fetched once per hash and cached (see `topology_meta`).
        // `LastProofBlock` at the head names the winning block one block
        // before `LastProofBlockHash` catches up. Both are ValueQuery: 0 and
        // the zero hash when unset.
        let last_proof_block_key = last_proof_block_storage_key();
        let stored_root_key = last_proof_block_hash_storage_key();
        // Runtime 120 lets the council move the decay gate. The override is
        // OptionQuery, and runtime 119 has no such item, so unset means the
        // runtime default on both.
        let target_key = target_proof_blocks_override_storage_key();
        let (
            header,
            topology_hash,
            last_proof_block,
            stored_root,
            target_override,
            runtime_version,
        ) = futures::try_join!(
            self.rpc_call(
                "chain_getHeader",
                Value::Array(vec![Value::String(at_hex.clone())]),
            ),
            self.snapshot_topology_hash(topology_hash, &at_hex),
            self.read_storage::<u32>(&last_proof_block_key, &at_hex),
            self.read_storage::<[u8; 32]>(&stored_root_key, &at_hex),
            self.read_storage::<u32>(&target_key, &at_hex),
            self.rpc_call(
                "state_getRuntimeVersion",
                Value::Array(vec![Value::String(at_hex.clone())]),
            ),
        )?;
        let Some(topology_hash) = topology_hash else {
            return Ok(None);
        };
        let block_number = parse_block_number(&header)?;
        let spec_version = parse_spec_version(&runtime_version)?;
        let api_version = quantum_pow_api_version(&runtime_version)?;

        // Decayed difficulty for this block. `None` means the hash is not
        // registered, which is the same "nothing to mine" as no topology.
        let difficulty = self
            .quantum_pow_call_by_hash::<DifficultyConfig>(
                "QuantumPowApi_difficulty_for",
                &topology_hash,
                &at_hex,
            )
            .await?;
        let Some(difficulty) = difficulty else {
            return Ok(None);
        };
        let Some((meta, family)) = self
            .mineable_topology(topology_hash, &at_hex, api_version)
            .await?
        else {
            return Ok(None);
        };
        let bound = family.is_some();

        let snap = MiningSnapshot {
            head_hash: block_hash,
            last_proof_block_hash: round_root(
                block_hash,
                block_number,
                u64::from(last_proof_block.unwrap_or(0)),
                stored_root.unwrap_or([0u8; 32]),
            ),
            topology_hash: topology_hash.to_vec(),
            nodes: meta.nodes,
            edges: meta.edges,
            allowed_h_milli: model_values(bound, &meta.allowed_h_values)?,
            allowed_j_milli: model_values(bound, &meta.allowed_j_values)?,
            allowed_spin_milli: require_set_values(&meta.allowed_spin_values)
                .map_err(ChainError::Decode)?,
            min_solutions: difficulty.min_solutions,
            max_energy_milli: difficulty.max_energy_milli,
            min_diversity_milli: difficulty.min_diversity_milli,
            block_number,
            spec_version,
            target_proof_blocks: target_override
                .map_or(crate::types::snapshot::TARGET_PROOF_BLOCKS, u64::from),
            family,
        };

        if let Ok(mut g) = self.last_snapshot.lock() {
            *g = Some(snap.clone());
        }
        if let Ok(mut g) = self.last_spin_spec.lock() {
            *g = Some(meta.allowed_spin_values);
        }
        Ok(Some(snap))
    }

    /// Read the latest qblock id; see `ChainClient::fetch_latest_qblock_id`.
    pub(in crate::chain) async fn read_latest_qblock_id(&self) -> Result<Option<u64>, ChainError> {
        let head = self
            .rpc_call("chain_getBlockHash", Value::Array(vec![]))
            .await?;
        let block_hash = head
            .as_str()
            .ok_or_else(|| ChainError::Decode("chain_getBlockHash not a string".into()))
            .and_then(|hex| hex_decode(hex).map_err(ChainError::Decode))?;

        // `QuantumPowApi_latest_qblock_id()` takes no args → empty SCALE params.
        let result = self
            .rpc_call(
                "state_call",
                Value::Array(vec![
                    Value::String("QuantumPowApi_latest_qblock_id".into()),
                    Value::String(hex_encode(&[])),
                    Value::String(hex_encode(&block_hash)),
                ]),
            )
            .await?;

        let Some(hex) = result.as_str() else {
            return Ok(None);
        };
        let bytes = hex_decode(hex).map_err(ChainError::Decode)?;
        Decode::decode(&mut &bytes[..]).map_err(|e| ChainError::Decode(e.to_string()))
    }

    /// Read a miner record; see `ChainClient::fetch_miner_info`.
    pub(in crate::chain) async fn read_miner_info(
        &self,
        account: [u8; 32],
    ) -> Result<Option<MinerInfo>, ChainError> {
        let head = self
            .rpc_call("chain_getBlockHash", Value::Array(vec![]))
            .await?;
        let at = head
            .as_str()
            .ok_or_else(|| ChainError::Decode("chain_getBlockHash not a string".into()))?;
        let scale: Option<MinerInfoScale> =
            self.read_storage(&miners_storage_key(&account), at).await?;
        Ok(scale.map(|s| MinerInfo {
            registered_at: u64::from(s.registered_at),
            deposit: s.deposit,
            proofs_submitted: u64::from(s.proofs_submitted),
            proofs_won: u64::from(s.proofs_won),
            rewards_earned: s.rewards_earned,
        }))
    }

    /// Read the decay inputs; see `ChainClient::fetch_decay_params`.
    pub(in crate::chain) async fn read_decay_params(
        &self,
        topology_hash: [u8; 32],
    ) -> Result<Option<DecayParams>, ChainError> {
        let head = self
            .rpc_call("chain_getBlockHash", Value::Array(vec![]))
            .await?;
        let at = head
            .as_str()
            .ok_or_else(|| ChainError::Decode("chain_getBlockHash not a string".into()))?
            .to_string();

        // Base (un-decayed) difficulty; unset topology → chain genesis default.
        let base: Option<DifficultyConfig> = self
            .read_storage(&difficulties_storage_key(&topology_hash), &at)
            .await?;
        // LastProofBlock is a u32 BlockNumber (ValueQuery → 0 when unset).
        let last_proof_block: u32 = self
            .read_storage(&last_proof_block_storage_key(), &at)
            .await?
            .unwrap_or(0);
        // Per-topology curve override; unset → the runtime CurveC* constants.
        let curve: Option<CurveCScale> = self
            .read_storage(&topology_curve_c_storage_key(&topology_hash), &at)
            .await?;
        let (c_easy_milli, c_knee_milli, c_hard_milli) = curve.map_or(
            (
                DEFAULT_C_EASY_MILLI,
                DEFAULT_C_KNEE_MILLI,
                DEFAULT_C_HARD_MILLI,
            ),
            |c| (c.easy_milli, c.knee_milli, c.hard_milli),
        );
        // Only a family topology has a band.
        let band: Option<EnergyBandScale> = self
            .read_storage(&topology_energy_band_storage_key(&topology_hash), &at)
            .await?;

        Ok(Some(DecayParams {
            base_max_energy_milli: base
                .map_or(DEFAULT_BASE_MAX_ENERGY_MILLI, |d| d.max_energy_milli),
            last_proof_block: u64::from(last_proof_block),
            epoch_length: EPOCH_LENGTH_BLOCKS,
            c_easy_milli,
            c_knee_milli,
            c_hard_milli,
            energy_band_milli: band.map(|b| (b.hard_milli, b.easy_milli)),
        }))
    }
}

/// The `h` or `J` values of a snapshot. A family draws `h` and `J` through its
/// generator, so a family topology's value spec need not be a set: only the
/// spin set encodes the witnesses.
pub(in crate::chain) fn model_values(
    family_bound: bool,
    spec: &AllowedValueSpec<Vec<i32>>,
) -> Result<Vec<i32>, ChainError> {
    match require_set_values(spec) {
        Ok(values) => Ok(values),
        Err(_) if family_bound => Ok(Vec::new()),
        Err(e) => Err(ChainError::Decode(e)),
    }
}

/// One `Option<TopologyMeta>` answer in the layout of `api_version`.
pub(in crate::chain) fn decode_topology_meta(
    bytes: &[u8],
    api_version: u32,
) -> Result<Option<TopologyMetaScale>, ChainError> {
    let decode_error = |e: parity_scale_codec::Error| ChainError::Decode(e.to_string());
    match bytes.split_first() {
        Some((0, _)) => Ok(None),
        Some((1, mut rest)) => TopologyMetaScale::decode_for_api(&mut rest, api_version)
            .map(Some)
            .map_err(decode_error),
        _ => Err(ChainError::Decode("topology_meta: not an Option".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantum_pow_storage_keys_have_expected_shape() {
        use crate::chain::codec::extrinsic::{
            difficulties_storage_key, last_proof_block_hash_storage_key,
            last_proof_block_storage_key, topology_curve_c_storage_key,
        };
        let hash = [9u8; 32];
        let diff = difficulties_storage_key(&hash);
        // twox128(pallet)=16 + twox128(item)=16 + blake2_128(hash)=16 + hash=32
        assert_eq!(diff.len(), 80);
        assert!(diff.ends_with(&hash));
        let curve = topology_curve_c_storage_key(&hash);
        assert_eq!(curve.len(), 80);
        // Same pallet prefix, different storage item.
        #[expect(
            clippy::indexing_slicing,
            reason = "storage keys are fixed 80/32 bytes; prefixes checked by len asserts"
        )]
        {
            assert_eq!(diff[..16], curve[..16]);
            assert_ne!(diff[16..32], curve[16..32]);
            // Plain StorageValue: pallet + item, no key suffix.
            assert_eq!(last_proof_block_storage_key().len(), 32);
            assert_eq!(last_proof_block_storage_key()[..16], diff[..16]);
            assert_eq!(last_proof_block_hash_storage_key().len(), 32);
            assert_eq!(last_proof_block_hash_storage_key()[..16], diff[..16]);
            assert_ne!(
                last_proof_block_hash_storage_key()[16..],
                last_proof_block_storage_key()[16..]
            );
        }
    }

    /// The pallet writes `LastProofBlockHash` one block after the win. At the
    /// winning block, `LastProofBlock` already names the head, and the head
    /// hash is the value the pallet will store next block.
    #[test]
    fn root_is_the_head_hash_only_at_the_winning_block() {
        let head = [1u8; 32];
        let stored = [2u8; 32];
        for (label, head_number, last_proof_block, expected) in [
            ("at the winning block", 100, 100, head),
            ("after the winning block", 101, 100, stored),
            ("before any win", 5, 0, stored),
        ] {
            assert_eq!(
                round_root(head, head_number, last_proof_block, stored),
                expected,
                "{label}"
            );
        }
    }
}

#[cfg(test)]
mod default_topology_tests {
    use super::RealChainClient;
    use crate::chain::codec::extrinsic::default_topology_storage_key;
    use crate::chain::codec::quantum_pow::TopologyMetaScale;
    use crate::chain::{BoxStream, ChainError, RpcTransport};
    use crate::types::hex::hex_encode;
    use crate::types::miner::MinerKind;

    use parity_scale_codec::Encode;
    use quantum_validation::AllowedValueSpec;
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    const HEAD: [u8; 32] = [0xaa; 32];
    const URL: &str = "ws://topology-fixture";

    struct ExpectedCall {
        method: &'static str,
        params: Value,
        response: Result<Value, ChainError>,
    }

    struct TopologyTransport {
        steps: Mutex<VecDeque<ExpectedCall>>,
    }

    #[async_trait::async_trait]
    impl RpcTransport for TopologyTransport {
        async fn request(
            &self,
            url: &str,
            method: &str,
            params: Value,
        ) -> Result<Value, ChainError> {
            let step = self
                .steps
                .lock()
                .map_err(|_| ChainError::Unavailable("RPC script lock poisoned".into()))?
                .pop_front()
                .ok_or_else(|| ChainError::Unavailable("unexpected extra RPC".into()))?;
            assert_eq!(url, URL);
            assert_eq!(method, step.method);
            assert_eq!(params, step.params);
            step.response
        }

        async fn subscribe(
            &self,
            _url: &str,
            _sub: &str,
            _params: Value,
            _unsub: &str,
        ) -> Result<BoxStream<'static, Result<Value, ChainError>>, ChainError> {
            Err(ChainError::Unavailable(
                "topology reads do not subscribe".into(),
            ))
        }
    }

    fn meta() -> TopologyMetaScale {
        TopologyMetaScale {
            nodes: vec![10, 30],
            edges: vec![(10, 30)],
            allowed_h_values: AllowedValueSpec::Set(vec![0]),
            allowed_j_values: AllowedValueSpec::Set(vec![-1000, 1000]),
            allowed_spin_values: AllowedValueSpec::Set(vec![-1000, 1000]),
            family: None,
            registered_at: 7,
        }
    }

    fn topology_steps(
        hash: Option<[u8; 32]>,
        metadata: Result<Value, ChainError>,
    ) -> Vec<ExpectedCall> {
        let mut steps = vec![
            ExpectedCall {
                method: "chain_getBlockHash",
                params: serde_json::json!([]),
                response: Ok(Value::String(hex_encode(&HEAD))),
            },
            ExpectedCall {
                method: "state_getStorage",
                params: serde_json::json!([
                    hex_encode(&default_topology_storage_key()),
                    hex_encode(&HEAD),
                ]),
                response: Ok(hash.map_or(Value::Null, |hash| Value::String(hex_encode(&hash)))),
            },
        ];
        if let Some(hash) = hash {
            steps.push(ExpectedCall {
                method: "state_getRuntimeVersion",
                params: serde_json::json!([hex_encode(&HEAD)]),
                response: Ok(crate::chain::startup::preflight::runtime_version_json(
                    120, 3,
                )),
            });
            steps.push(ExpectedCall {
                method: "state_call",
                params: serde_json::json!([
                    "QuantumPowApi_topology_meta",
                    hex_encode(&hash),
                    hex_encode(&HEAD),
                ]),
                response: metadata,
            });
        }
        steps
    }

    fn client(steps: Vec<ExpectedCall>) -> (RealChainClient, Arc<TopologyTransport>) {
        let transport = Arc::new(TopologyTransport {
            steps: Mutex::new(steps.into()),
        });
        let client = RealChainClient::with_transport(
            vec![URL.to_string()],
            String::new(),
            MinerKind::Cpu,
            transport.clone(),
        );
        (client, transport)
    }

    fn assert_drained(transport: &TopologyTransport) {
        assert!(transport
            .steps
            .lock()
            .expect("RPC script lock poisoned")
            .is_empty());
    }

    #[tokio::test]
    async fn default_topology_uses_one_head_and_needs_no_signer() {
        let hash = [0x11; 32];
        let metadata = meta();
        let response = Value::String(hex_encode(&Some(metadata.clone()).encode()));
        let (client, transport) = client(topology_steps(Some(hash), Ok(response)));
        let fetched = client
            .fetch_default_topology()
            .await
            .expect("fetch default topology")
            .expect("missing default topology");
        assert_eq!(fetched, (hash, metadata));
        assert!(client.pair.lock().expect("signer lock poisoned").is_none());
        assert_drained(&transport);
    }

    #[tokio::test]
    async fn unset_default_topology_skips_metadata() {
        let (client, transport) = client(topology_steps(None, Ok(Value::Null)));
        assert!(client
            .fetch_default_topology()
            .await
            .expect("fetch unset default topology")
            .is_none());
        assert_drained(&transport);
    }

    /// Transport and decode errors keep their variant. A stored default with
    /// no metadata is an error, and a non-string head fails before any
    /// storage read.
    #[tokio::test]
    async fn topology_read_errors_keep_their_variant_and_message() {
        let unregistered = Value::String(hex_encode(&Option::<TopologyMetaScale>::None.encode()));
        let stored = Some([0x11; 32]);
        for (label, steps, expected_variant, expected) in [
            (
                "stored default without metadata",
                topology_steps(stored, Ok(unregistered)),
                ChainError::Decode(String::new()),
                "is not registered",
            ),
            (
                "transport error",
                topology_steps(
                    stored,
                    Err(ChainError::Unavailable("fixture unavailable".into())),
                ),
                ChainError::Unavailable(String::new()),
                "chain unavailable: fixture unavailable",
            ),
            (
                "undecodable metadata",
                topology_steps(stored, Ok(Value::String("0x03".into()))),
                ChainError::Decode(String::new()),
                "decode error:",
            ),
            (
                "non-string head",
                vec![ExpectedCall {
                    method: "chain_getBlockHash",
                    params: serde_json::json!([]),
                    response: Ok(Value::Null),
                }],
                ChainError::Decode(String::new()),
                "chain_getBlockHash not a string",
            ),
        ] {
            let (client, transport) = client(steps);
            let error = client.fetch_default_topology().await.expect_err(label);
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&expected_variant),
                "{label}: unexpected chain error variant: {error}"
            );
            assert!(error.to_string().contains(expected), "{label}: {error}");
            assert_drained(&transport);
        }
    }
}
