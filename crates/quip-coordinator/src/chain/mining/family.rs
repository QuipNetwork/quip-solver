//! Binding a mineable topology to its challenge family, and refusing one
//! this client cannot prove.

use crate::chain::client::RealChainClient;
use crate::chain::codec::proof_encode::WitnessFormat;
use crate::chain::codec::scale_types::{FamilyDescriptorScale, TopologyMetaScale};
use crate::chain::ChainError;
use crate::types::hex::hex_encode;
use crate::types::snapshot::MiningSnapshot;
use quantum_validation::AllowedValueSpec;

impl RealChainClient {
    /// Bind the chain's family descriptor for `topology_hash`.
    ///
    /// The descriptor must pass admission on this build's contract table, and
    /// the family topology hash rebuilt from it must equal `topology_hash`. The
    /// second check ties the instance this coordinator regenerates to the one
    /// the pallet regenerates: a descriptor read from another topology, or
    /// parsed differently, cannot match.
    ///
    /// # Errors
    /// Returns why this coordinator cannot mine the topology.
    pub(in crate::chain) fn bind_family(
        topology_hash: [u8; 32],
        meta: &TopologyMetaScale,
        descriptor: &FamilyDescriptorScale,
    ) -> Result<crate::types::family::ChallengeFamily, String> {
        let id = String::from_utf8_lossy(&descriptor.family_id);
        let label = format!("{id} revision {}", descriptor.revision);
        let family_id = std::str::from_utf8(&descriptor.family_id)
            .map_err(|_| format!("family id of {label} is not UTF-8"))?;
        let topology = crate::types::topology::Topology::from_nodes_edges(
            meta.nodes.clone(),
            meta.edges.clone(),
            &[],
            &[],
            &[],
        );
        let family = crate::types::family::ChallengeFamily::admit(
            crate::types::family::FAMILY_CONTRACTS,
            family_id,
            descriptor.revision,
            descriptor.params.clone(),
            &topology,
        )
        .map_err(|code| format!("this build does not admit family {label}: {code:?}"))?;
        let legacy_hash = crate::types::topology::topology_hash(
            &meta.nodes,
            &meta.edges,
            &meta.allowed_h_values.as_slice(),
            &meta.allowed_j_values.as_slice(),
            &meta.allowed_spin_values.as_slice(),
        );
        let rebuilt = crate::types::topology::family_topology_hash(
            legacy_hash,
            *family.descriptor_digest(),
            *family.topology_order_digest(),
        );
        if rebuilt != topology_hash {
            return Err(format!(
                "family {label} hashes to {} here, not to the chain's {}",
                hex_encode(&rebuilt),
                hex_encode(&topology_hash)
            ));
        }
        Ok(family)
    }

    /// The registered topology under `topology_hash` with its bound family.
    /// `Ok(None)` when the hash is not registered, or when this client does not
    /// mine its family.
    pub(in crate::chain) async fn mineable_topology(
        &self,
        topology_hash: [u8; 32],
        at_hex: &str,
        api_version: u32,
    ) -> Result<
        Option<(
            TopologyMetaScale,
            Option<crate::types::family::ChallengeFamily>,
        )>,
        ChainError,
    > {
        let Some(meta) = self
            .topology_meta(topology_hash, at_hex, api_version)
            .await?
        else {
            return Ok(None);
        };
        Ok(self
            .chain_family(topology_hash, &meta)
            .ok()
            .map(|family| (meta, family)))
    }

    /// The family bound to `topology_hash`: `Ok(None)` for a legacy topology,
    /// and `Err` after logging why a family topology is not mined.
    pub(in crate::chain) fn chain_family(
        &self,
        topology_hash: [u8; 32],
        meta: &TopologyMetaScale,
    ) -> Result<Option<crate::types::family::ChallengeFamily>, FamilyRefused> {
        let Some(descriptor) = &meta.family else {
            return Ok(None);
        };
        Self::bind_family(topology_hash, meta, descriptor)
            .map(Some)
            .map_err(|reason| {
                self.refuse_family(topology_hash, &reason);
                FamilyRefused
            })
    }

    /// Log once per topology that a family topology is not mined.
    pub(in crate::chain) fn refuse_family(&self, topology_hash: [u8; 32], reason: &str) {
        let Ok(mut last) = self.refused_family.lock() else {
            return;
        };
        if *last != Some(topology_hash) {
            tracing::warn!(
                topology = %hex_encode(&topology_hash),
                reason,
                "not mining the family topology"
            );
            *last = Some(topology_hash);
        }
    }
}

/// A family topology this client does not mine. The reason is already logged.
pub(in crate::chain) struct FamilyRefused;

/// The solution format the pallet accepts on `snap`'s topology: the family's
/// compact codec on a family topology, otherwise the spin-spec packing over
/// every topology node.
pub(in crate::chain) fn witness_format(
    snap: &MiningSnapshot,
    allowed_spin: AllowedValueSpec<Vec<i32>>,
) -> WitnessFormat {
    match &snap.family {
        Some(family) => WitnessFormat::Family {
            family: family.clone(),
            nodes: snap.nodes.clone(),
            edges: snap.edges.clone(),
        },
        None => WitnessFormat::Packed {
            num_nodes: snap.nodes.len(),
            allowed_spin,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::client::transport::RpcTransport;
    use crate::chain::{ChainError, MiningSnapshot};
    use crate::types::miner::MinerKind;

    use serde_json::Value as JsonValue;

    /// A validator scripted for the snapshot path: a fixed head at block 16,
    /// one registered topology per hash, a `DefaultTopology` the test can
    /// move, and a `LastProofBlock` the test can set. `None` answers null for
    /// both `LastProofBlock` and `LastProofBlockHash`, which is genesis.
    ///
    /// `api_version` fixes the `TopologyMeta` layout it answers in. `meta`
    /// replaces the generated topology for every hash, and `band` answers
    /// `TopologyEnergyBand`.
    struct SnapshotValidator {
        default_topology: std::sync::Mutex<[u8; 32]>,
        last_proof_block: std::sync::Mutex<Option<u32>>,
        target_override: std::sync::Mutex<Option<u32>>,
        topology_meta_calls: std::sync::Mutex<Vec<[u8; 32]>>,
        api_version: u32,
        meta: Option<TopologyMetaScale>,
        band: Option<crate::chain::codec::scale_types::EnergyBandScale>,
    }

    impl SnapshotValidator {
        fn new(default_topology: [u8; 32], api_version: u32) -> Self {
            Self {
                default_topology: std::sync::Mutex::new(default_topology),
                last_proof_block: std::sync::Mutex::new(Some(10)),
                target_override: std::sync::Mutex::new(None),
                topology_meta_calls: std::sync::Mutex::new(Vec::new()),
                api_version,
                meta: None,
                band: None,
            }
        }

        /// `meta` as `Some(meta)` in the layout of `self.api_version`.
        fn encode_meta(&self, meta: &TopologyMetaScale) -> Vec<u8> {
            use parity_scale_codec::Encode as _;
            let mut out = vec![1];
            if self.api_version >= crate::chain::codec::scale_types::FAMILY_API_VERSION {
                out.extend(meta.encode());
            } else {
                out.extend(
                    (
                        &meta.nodes,
                        &meta.edges,
                        &meta.allowed_h_values,
                        &meta.allowed_j_values,
                        &meta.allowed_spin_values,
                        meta.registered_at,
                    )
                        .encode(),
                );
            }
            out
        }
    }

    const HEAD: [u8; 32] = [0xAA; 32];
    const STORED_ROOT: [u8; 32] = [0xBB; 32];

    fn topology_for(hash: [u8; 32]) -> TopologyMetaScale {
        use quantum_validation::AllowedValueSpec;
        let n = u32::from(hash[0]);
        TopologyMetaScale {
            nodes: vec![0, 1, n],
            edges: vec![(0, 1), (1, n)],
            allowed_h_values: AllowedValueSpec::Set(vec![-1000, 1000]),
            allowed_j_values: AllowedValueSpec::Set(vec![-1000, 1000]),
            allowed_spin_values: AllowedValueSpec::Set(vec![-1000, 1000]),
            family: None,
            registered_at: 3,
        }
    }

    #[async_trait::async_trait]
    impl RpcTransport for SnapshotValidator {
        async fn request(
            &self,
            _url: &str,
            method: &str,
            params: JsonValue,
        ) -> Result<JsonValue, ChainError> {
            use crate::chain::codec::extrinsic::{
                default_topology_storage_key, difficulties_storage_key,
                last_proof_block_hash_storage_key, last_proof_block_storage_key,
                target_proof_blocks_override_storage_key, topology_curve_c_storage_key,
                topology_energy_band_storage_key,
            };
            use crate::types::hex::hex_decode;
            use parity_scale_codec::Encode as _;

            let arg = |i: usize| {
                params
                    .get(i)
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let hex = |bytes: &[u8]| JsonValue::String(hex_encode(bytes));
            Ok(match method {
                "chain_getBlockHash" => hex(&HEAD),
                "chain_getHeader" => {
                    assert_eq!(arg(0), hex_encode(&HEAD), "header read pinned at the head");
                    serde_json::json!({ "number": "0x10" })
                }
                "state_getRuntimeVersion" => {
                    assert_eq!(
                        arg(0),
                        hex_encode(&HEAD),
                        "runtime version read pinned at the head"
                    );
                    let spec = if self.api_version >= 3 { 120 } else { 118 };
                    crate::chain::startup::preflight::runtime_version_json(spec, self.api_version)
                }
                "state_getStorage" => {
                    assert_eq!(arg(1), hex_encode(&HEAD), "storage read pinned at the head");
                    let key = hex_decode(&arg(0)).expect("storage key hex");
                    if key == default_topology_storage_key() {
                        hex(&*self.default_topology.lock().unwrap())
                    } else if key == last_proof_block_storage_key() {
                        match *self.last_proof_block.lock().unwrap() {
                            Some(n) => hex(&n.encode()),
                            None => JsonValue::Null,
                        }
                    } else if key == last_proof_block_hash_storage_key() {
                        match *self.last_proof_block.lock().unwrap() {
                            Some(_) => hex(&STORED_ROOT),
                            None => JsonValue::Null,
                        }
                    } else if key == target_proof_blocks_override_storage_key() {
                        match *self.target_override.lock().unwrap() {
                            Some(n) => hex(&n.encode()),
                            None => JsonValue::Null,
                        }
                    } else if key
                        == topology_energy_band_storage_key(&self.default_topology.lock().unwrap())
                    {
                        self.band
                            .map_or(JsonValue::Null, |band| hex(&band.encode()))
                    } else if key
                        == difficulties_storage_key(&self.default_topology.lock().unwrap())
                        || key
                            == topology_curve_c_storage_key(&self.default_topology.lock().unwrap())
                    {
                        JsonValue::Null
                    } else {
                        panic!("unexpected storage key {}", arg(0))
                    }
                }
                "state_call" => {
                    assert_eq!(arg(2), hex_encode(&HEAD), "runtime call pinned at the head");
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(&hex_decode(&arg(1)).expect("hash hex"));
                    match arg(0).as_str() {
                        "QuantumPowApi_difficulty_for" => {
                            hex(&Some(crate::chain::codec::scale_types::DifficultyConfig {
                                min_solutions: 1,
                                max_energy_milli: -5_000,
                                min_diversity_milli: 0,
                            })
                            .encode())
                        }
                        "QuantumPowApi_topology_meta" => {
                            self.topology_meta_calls.lock().unwrap().push(hash);
                            let meta = self.meta.clone().unwrap_or_else(|| topology_for(hash));
                            hex(&self.encode_meta(&meta))
                        }
                        other => panic!("unexpected runtime call {other}"),
                    }
                }
                other => panic!("unexpected rpc method {other}"),
            })
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

    /// A polariton topology as runtime API version 3 records it, bound to
    /// `revision` of the family.
    fn family_meta(revision: u32) -> TopologyMetaScale {
        use quantum_validation::AllowedValueSpec;
        let blob =
            quip_solver_conformance::topology(quip_solver_conformance::families::ROUTING_TOPOLOGY)
                .expect("routing topology fixture");
        TopologyMetaScale {
            nodes: blob.nodes,
            edges: blob.edges,
            allowed_h_values: AllowedValueSpec::Set(vec![0]),
            allowed_j_values: AllowedValueSpec::Set(vec![1000]),
            allowed_spin_values: AllowedValueSpec::Set(vec![-1000, 1000]),
            family: Some(FamilyDescriptorScale {
                family_id: ising_generators::polariton::POLARITON_V1
                    .family_id
                    .as_bytes()
                    .to_vec(),
                revision,
                params: quip_solver_conformance::families::polariton::P_POLARITON_Z24_L2_P0
                    .to_vec(),
            }),
            registered_at: 4,
        }
    }

    /// The hash the pallet registers `meta` under, from the validator's own
    /// admission code: `register_topology` hashes the legacy fields, then mixes
    /// in the descriptor digest and topology order digest of the challenge.
    fn chain_family_hash(meta: &TopologyMetaScale) -> [u8; 32] {
        use quantum_validation::families::{AdvertisedLimits, FamilyChallenge, CONTRACTS};
        let descriptor = meta.family.as_ref().expect("a family topology");
        let challenge = FamilyChallenge::admit(
            CONTRACTS,
            &descriptor.family_id,
            descriptor.revision,
            &descriptor.params,
            &meta.nodes,
            &meta.edges,
            &AdvertisedLimits {
                max_nodes: 5_000,
                max_edges: 50_000,
                max_degree: 0,
            },
        )
        .expect("the validator admits the fixture");
        let legacy = crate::types::topology::topology_hash(
            &meta.nodes,
            &meta.edges,
            &meta.allowed_h_values.as_slice(),
            &meta.allowed_j_values.as_slice(),
            &meta.allowed_spin_values.as_slice(),
        );
        crate::types::topology::family_topology_hash(
            legacy,
            challenge.descriptor_digest(),
            challenge.topology_order_digest(),
        )
    }

    fn family_client(validator: SnapshotValidator) -> RealChainClient {
        RealChainClient::with_transport(
            vec!["ws://scripted".to_string()],
            "//Alice".to_string(),
            MinerKind::Cpu,
            std::sync::Arc::new(validator),
        )
    }

    /// A family topology on runtime API version 3 binds the chain's own
    /// descriptor, and its decay curve comes from the root-set band.
    #[tokio::test]
    async fn a_family_topology_binds_the_chain_descriptor_and_band() {
        use crate::chain::ChainClient as _;
        let meta = family_meta(1);
        let hash = chain_family_hash(&meta);
        let mut validator = SnapshotValidator::new(hash, 3);
        validator.meta = Some(meta.clone());
        validator.band = Some(crate::chain::codec::scale_types::EnergyBandScale {
            hard_milli: -900_000,
            easy_milli: -700_000,
        });
        let client = family_client(validator);

        let snap = client
            .fetch_mining_snapshot(None, [0u8; 32], None)
            .await
            .expect("snapshot")
            .expect("the family topology is mineable here");
        assert_eq!(snap.topology_hash, hash.to_vec());
        let family = snap.family.expect("the chain binds a family");
        let chain = meta.family.expect("a family topology");
        assert_eq!(family.descriptor().family_id.as_bytes(), chain.family_id);
        assert_eq!(family.descriptor().revision, chain.revision);
        assert_eq!(family.descriptor().params, chain.params);

        let decay = client
            .fetch_decay_params(hash)
            .await
            .expect("decay read")
            .expect("decay params");
        assert_eq!(decay.energy_band_milli, Some((-900_000, -700_000)));
    }

    /// Nothing is mined on a family topology that this build cannot admit, or
    /// whose descriptor does not hash to the topology the chain names. Each is
    /// a round the chain would reject.
    #[tokio::test]
    async fn a_family_topology_is_not_mined_when_it_cannot_be_proven() {
        use crate::chain::ChainClient as _;
        let meta = family_meta(1);
        let hash = chain_family_hash(&meta);
        let legacy = crate::types::topology::topology_hash(
            &meta.nodes,
            &meta.edges,
            &meta.allowed_h_values.as_slice(),
            &meta.allowed_j_values.as_slice(),
            &meta.allowed_spin_values.as_slice(),
        );
        for (label, revision, default) in [
            ("unknown revision", 99, hash),
            ("descriptor under the legacy hash", 1, legacy),
        ] {
            let mut validator = SnapshotValidator::new(default, 3);
            validator.meta = Some(family_meta(revision));
            let snap = family_client(validator)
                .fetch_mining_snapshot(None, [0u8; 32], None)
                .await
                .expect("snapshot read");
            assert!(snap.is_none(), "{label}");
        }
    }

    /// A family topology takes the family's compact codec. Any other topology
    /// takes the spin-spec packing over every topology node.
    #[test]
    fn a_family_topology_takes_the_codec_and_a_legacy_one_the_packing() {
        use crate::chain::codec::proof_encode::WitnessFormat;
        use quantum_validation::AllowedValueSpec;
        let meta = family_meta(1);
        let topology = crate::types::topology::Topology::from_nodes_edges(
            meta.nodes.clone(),
            meta.edges.clone(),
            &[],
            &[],
            &[],
        );
        let mut snap = MiningSnapshot {
            head_hash: [0; 32],
            last_proof_block_hash: [0; 32],
            topology_hash: vec![0; 32],
            nodes: meta.nodes.clone(),
            edges: meta.edges.clone(),
            allowed_h_milli: vec![0],
            allowed_j_milli: vec![1000],
            allowed_spin_milli: vec![-1000, 1000],
            min_solutions: 1,
            max_energy_milli: 0,
            min_diversity_milli: 0,
            block_number: 1,
            spec_version: 120,
            target_proof_blocks: crate::types::snapshot::TARGET_PROOF_BLOCKS,
            family: None,
        };
        let spin = AllowedValueSpec::Set(vec![-1000, 1000]);
        assert!(matches!(
            witness_format(&snap, spin.clone()),
            WitnessFormat::Packed { num_nodes, .. } if num_nodes == meta.nodes.len()
        ));
        let descriptor = meta.family.expect("a family topology");
        let family = crate::types::family::ChallengeFamily::admit(
            crate::types::family::FAMILY_CONTRACTS,
            std::str::from_utf8(&descriptor.family_id).expect("ascii id"),
            descriptor.revision,
            descriptor.params,
            &topology,
        )
        .expect("admitted");
        snap.family = Some(family.clone());
        match witness_format(&snap, spin) {
            WitnessFormat::Family {
                family: chosen,
                nodes,
                edges,
            } => {
                assert_eq!(chosen, family);
                assert_eq!((nodes, edges), (meta.nodes, meta.edges));
            }
            WitnessFormat::Packed { .. } => panic!("a family topology must take its codec"),
        }
    }

    /// A legacy topology on runtime API version 3 records `family: None` and
    /// mines as before.
    #[tokio::test]
    async fn a_legacy_topology_on_api_3_stays_legacy() {
        use crate::chain::ChainClient as _;
        let snap = family_client(SnapshotValidator::new([0x11; 32], 3))
            .fetch_mining_snapshot(None, [0u8; 32], None)
            .await
            .expect("snapshot")
            .expect("a legacy topology");
        assert_eq!(snap.spec_version, 120);
        assert!(snap.family.is_none());
        assert_eq!(snap.nodes, vec![0, 1, 0x11]);
    }

    /// The topology is the one payload that never changes under its hash, so
    /// polling the same hash must not download it again. A new default
    /// topology is a new hash and is downloaded when it first appears. The
    /// root and the difficulty are read on every poll.
    #[tokio::test]
    async fn the_topology_is_downloaded_only_when_the_hash_changes() {
        use crate::chain::ChainClient as _;
        let first = [0x11; 32];
        let second = [0x22; 32];
        let validator = std::sync::Arc::new(SnapshotValidator::new(first, 2));
        let client = RealChainClient::with_transport(
            vec!["ws://scripted".to_string()],
            "//Alice".to_string(),
            MinerKind::Cpu,
            validator.clone(),
        );

        let snap = client
            .fetch_mining_snapshot(None, [0u8; 32], None)
            .await
            .expect("first snapshot")
            .expect("a topology is registered");
        assert_eq!(snap.head_hash, HEAD);
        assert_eq!(
            snap.last_proof_block_hash, STORED_ROOT,
            "the win was at block 10, so the stored hash is the root"
        );
        assert_eq!(snap.block_number, 16);
        assert_eq!(snap.spec_version, 118);
        assert_eq!(snap.topology_hash, first.to_vec());
        assert_eq!(snap.nodes, vec![0, 1, 0x11]);
        assert_eq!(snap.edges, vec![(0, 1), (1, 0x11)]);
        assert_eq!(snap.allowed_spin_milli, vec![-1000, 1000]);
        assert_eq!(snap.min_solutions, 1);
        assert_eq!(snap.max_energy_milli, -5_000);
        assert_eq!(
            snap.target_proof_blocks, 100,
            "an unset override, as on runtime 119, reads as the default"
        );

        *validator.target_override.lock().unwrap() = Some(200);
        let council = client
            .fetch_mining_snapshot(None, [0u8; 32], None)
            .await
            .expect("snapshot after a council target change")
            .expect("still registered");
        assert_eq!(council.target_proof_blocks, 200, "runtime 120 override");
        *validator.target_override.lock().unwrap() = None;

        *validator.last_proof_block.lock().unwrap() = Some(16);
        let won = client
            .fetch_mining_snapshot(None, [0u8; 32], None)
            .await
            .expect("snapshot at the winning block")
            .expect("still registered");
        assert_eq!(
            won.last_proof_block_hash, HEAD,
            "LastProofBlock equals the head number, so the head is the root"
        );

        *validator.last_proof_block.lock().unwrap() = None;
        let genesis = client
            .fetch_mining_snapshot(None, [0u8; 32], None)
            .await
            .expect("snapshot before any win")
            .expect("still registered");
        assert_eq!(
            genesis.last_proof_block_hash, [0u8; 32],
            "unset storage reads as the ValueQuery defaults: block 0, zero hash"
        );

        let again = client
            .fetch_mining_snapshot(None, [0u8; 32], None)
            .await
            .expect("second snapshot")
            .expect("still registered");
        assert_eq!(again.nodes, snap.nodes);
        assert_eq!(
            validator.topology_meta_calls.lock().unwrap().as_slice(),
            &[first],
            "the second poll on the same hash reuses the cached topology"
        );

        *validator.default_topology.lock().unwrap() = second;
        let moved = client
            .fetch_mining_snapshot(None, [0u8; 32], None)
            .await
            .expect("third snapshot")
            .expect("the new default is registered");
        assert_eq!(moved.topology_hash, second.to_vec());
        assert_eq!(moved.nodes, vec![0, 1, 0x22]);
        assert_eq!(
            validator.topology_meta_calls.lock().unwrap().as_slice(),
            &[first, second],
            "a new hash is downloaded when it first appears"
        );

        // The caller's own choice bypasses `DefaultTopology` and hits the cache.
        let chosen = client
            .fetch_mining_snapshot(None, [0u8; 32], Some(second))
            .await
            .expect("chosen snapshot")
            .expect("registered");
        assert_eq!(chosen.nodes, moved.nodes);
        assert_eq!(validator.topology_meta_calls.lock().unwrap().len(), 2);
    }
}
