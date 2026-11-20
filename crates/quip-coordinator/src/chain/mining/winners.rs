//! Reference winners: recent winning qblocks read from the chain, for
//! calibration. The walk goes down from the latest qblock id.

use crate::chain::client::RealChainClient;
use crate::chain::codec::extrinsic::participants_by_qblock_storage_key;
use crate::chain::codec::scale_types::{ParticipationRecordScale, QBlockWithNonceScale};
use crate::chain::{ChainClient, ChainError};
use crate::types::calibration::{HardwareFilter, ReferenceWinner};
use crate::types::hex::{hex_decode, hex_encode};
use crate::types::miner::MinerKind;
use parity_scale_codec::{Decode, Encode};
use serde_json::Value;

/// Decode the answer of `QuantumPowApi_qblock_by_id` into a winner with no
/// kind. The nonce is big-endian, like the proof encoder's.
pub(in crate::chain) fn decode_qblock_with_nonce(
    qblock_id: u64,
    bytes: &[u8],
) -> Result<Option<ReferenceWinner>, ChainError> {
    let decoded = Option::<QBlockWithNonceScale>::decode(&mut &bytes[..])
        .map_err(|e| ChainError::Decode(e.to_string()))?;
    Ok(decoded.map(|w| ReferenceWinner {
        qblock_id,
        miner: w.solution.miner,
        nonce: w.nonce.to_big_endian(),
        energy_milli: w.solution.energy_milli,
        max_energy_milli: w.solution.difficulty.max_energy_milli,
        topology_hash: w.solution.topology_hash.0,
        kind: None,
    }))
}

impl RealChainClient {
    /// Read one winning qblock; see `ChainClient::fetch_qblock_by_id`.
    pub(in crate::chain) async fn read_qblock_by_id(
        &self,
        qblock_id: u64,
    ) -> Result<Option<ReferenceWinner>, ChainError> {
        let at = self.head_hex().await?;
        let result = self
            .rpc_call(
                "state_call",
                Value::Array(vec![
                    Value::String("QuantumPowApi_qblock_by_id".into()),
                    Value::String(hex_encode(&qblock_id.encode())),
                    Value::String(at),
                ]),
            )
            .await?;
        let Some(hex) = result.as_str() else {
            return Ok(None);
        };
        let bytes = hex_decode(hex).map_err(ChainError::Decode)?;
        decode_qblock_with_nonce(qblock_id, &bytes)
    }

    /// Read a participant's declared kind; see
    /// `ChainClient::fetch_participation_kind`.
    pub(in crate::chain) async fn read_participation_kind(
        &self,
        qblock_id: u64,
        account: [u8; 32],
    ) -> Result<Option<MinerKind>, ChainError> {
        let at = self.head_hex().await?;
        let record: Option<ParticipationRecordScale> = self
            .read_storage(
                &participants_by_qblock_storage_key(qblock_id, &account),
                &at,
            )
            .await?;
        Ok(record.map(|r| r.kind))
    }
}

/// Collect up to `count` recent winners on `topology_hash` whose hardware
/// `filter` admits, newest first.
///
/// The walk starts at the latest qblock id and stops at `count` matches, at
/// id 1, or after `4 * count` ids. With an empty filter it skips the
/// participation read, and `kind` stays `None`.
///
/// # Errors
/// Returns the first chain error from a read.
pub async fn reference_winners(
    client: &(impl ChainClient + ?Sized),
    topology_hash: &[u8; 32],
    filter: &HardwareFilter,
    count: usize,
) -> Result<Vec<ReferenceWinner>, ChainError> {
    let mut found = Vec::new();
    if count == 0 {
        return Ok(found);
    }
    let Some(latest) = client.fetch_latest_qblock_id().await? else {
        return Ok(found);
    };
    let budget = u64::try_from(count.saturating_mul(4)).unwrap_or(u64::MAX);
    let mut id = latest;
    let mut visited = 0u64;
    while id >= 1 && visited < budget && found.len() < count {
        visited += 1;
        if let Some(mut winner) = client.fetch_qblock_by_id(id).await? {
            if winner.topology_hash == *topology_hash {
                if !filter.is_empty() {
                    winner.kind = client.fetch_participation_kind(id, winner.miner).await?;
                }
                if filter.admits(winner.kind) {
                    found.push(winner);
                }
            }
        }
        id -= 1;
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::codec::scale_types::{DifficultyConfig, QBlockScale};
    use crate::testing::fake_chain::FakeChain;
    use crate::types::snapshot::MiningSnapshot;
    use sp_core::{H256, U256};

    const A: [u8; 32] = [0xA; 32];
    const B: [u8; 32] = [0xB; 32];

    fn snap() -> MiningSnapshot {
        MiningSnapshot {
            head_hash: [0u8; 32],
            last_proof_block_hash: [1u8; 32],
            topology_hash: vec![0u8; 32],
            nodes: vec![0, 1],
            edges: vec![(0, 1)],
            allowed_h_milli: vec![0],
            allowed_j_milli: vec![0],
            allowed_spin_milli: vec![-1000, 1000],
            min_solutions: 1,
            max_energy_milli: 0,
            min_diversity_milli: 0,
            block_number: 1,
            spec_version: 117,
            target_proof_blocks: crate::types::snapshot::TARGET_PROOF_BLOCKS,
            family: None,
        }
    }

    fn winner(id: u64, topology: [u8; 32], kind: Option<MinerKind>) -> ReferenceWinner {
        ReferenceWinner {
            qblock_id: id,
            miner: [u8::try_from(id).expect("small id"); 32],
            nonce: [0; 32],
            energy_milli: -5000,
            max_energy_milli: -4000,
            topology_hash: topology,
            kind,
        }
    }

    fn ids(winners: &[ReferenceWinner]) -> Vec<u64> {
        winners.iter().map(|w| w.qblock_id).collect()
    }

    #[test]
    fn decodes_qblock_with_nonce() {
        let wire = Some(QBlockWithNonceScale {
            solution: QBlockScale {
                miner: [7; 32],
                salt: [8; 32],
                energy_milli: -4_900_000,
                reward: 10,
                submitted_at: 99,
                difficulty: DifficultyConfig {
                    min_solutions: 1,
                    max_energy_milli: -4_850_000,
                    min_diversity_milli: 0,
                },
                last_proof_block_hash: H256([1; 32]),
                topology_hash: H256([2; 32]),
                device_access_time_us: 5,
            },
            nonce: U256::one(),
        });
        let mut nonce = [0u8; 32];
        nonce[31] = 1;
        let decoded = decode_qblock_with_nonce(42, &wire.encode()).expect("ok");
        assert_eq!(
            decoded,
            Some(ReferenceWinner {
                qblock_id: 42,
                miner: [7; 32],
                nonce,
                energy_milli: -4_900_000,
                max_energy_milli: -4_850_000,
                topology_hash: [2; 32],
                kind: None,
            })
        );
        assert_eq!(
            decode_qblock_with_nonce(42, &Option::<QBlockWithNonceScale>::None.encode())
                .expect("ok"),
            None
        );
    }

    #[tokio::test]
    async fn skips_other_topologies_and_stops_at_count() {
        let chain = FakeChain::new(snap(), None);
        chain.set_qblock_id(Some(10));
        for id in [10, 8, 6] {
            chain.script_qblock(id, winner(id, A, None));
        }
        for id in [9, 7] {
            chain.script_qblock(id, winner(id, B, None));
        }
        let got = reference_winners(&chain, &A, &HardwareFilter::default(), 2)
            .await
            .expect("ok");
        assert_eq!(ids(&got), vec![10, 8]);
    }

    #[tokio::test]
    async fn bounded_walk() {
        let chain = FakeChain::new(snap(), None);
        chain.set_qblock_id(Some(100));
        for id in 1..=100 {
            chain.script_qblock(id, winner(id, B, None));
        }
        let got = reference_winners(&chain, &A, &HardwareFilter::default(), 5)
            .await
            .expect("ok");
        assert!(got.is_empty());
        assert_eq!(chain.qblock_reads(), 20);
    }

    #[tokio::test]
    async fn stops_at_id_one() {
        let chain = FakeChain::new(snap(), None);
        chain.set_qblock_id(Some(2));
        for id in 1..=2 {
            chain.script_qblock(id, winner(id, A, None));
        }
        let got = reference_winners(&chain, &A, &HardwareFilter::default(), 5)
            .await
            .expect("ok");
        assert_eq!(ids(&got), vec![2, 1]);
        assert_eq!(chain.qblock_reads(), 2);
    }

    #[tokio::test]
    async fn no_qblock_yet() {
        let chain = FakeChain::new(snap(), None);
        chain.set_qblock_id(None);
        let got = reference_winners(&chain, &A, &HardwareFilter::default(), 3)
            .await
            .expect("ok");
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn filters_by_kind() {
        let chain = FakeChain::new(snap(), None);
        chain.set_qblock_id(Some(5));
        chain.script_qblock(5, winner(5, A, Some(MinerKind::Gpu)));
        chain.script_qblock(4, winner(4, A, Some(MinerKind::Cpu)));
        chain.script_qblock(3, winner(3, A, Some(MinerKind::Metal)));
        chain.script_qblock(2, winner(2, A, None));

        let gpu = HardwareFilter::parse(&["gpu"]).expect("ok");
        let got = reference_winners(&chain, &A, &gpu, 3).await.expect("ok");
        assert_eq!(ids(&got), vec![5, 3]);
        assert_eq!(got.first().and_then(|w| w.kind), Some(MinerKind::Gpu));

        let before = chain.participation_reads();
        let got = reference_winners(&chain, &A, &HardwareFilter::default(), 3)
            .await
            .expect("ok");
        assert_eq!(ids(&got), vec![5, 4, 3]);
        assert_eq!(chain.participation_reads(), before);
    }

    #[test]
    fn decodes_miner_kind() {
        let expected = [
            MinerKind::Cpu,
            MinerKind::Gpu,
            MinerKind::QpuDwave,
            MinerKind::QpuIbm,
            MinerKind::QpuIonq,
            MinerKind::QpuPasqal,
            MinerKind::Asic,
            MinerKind::Metal,
        ];
        for (tag, kind) in (0u8..).zip(expected) {
            let record = ParticipationRecordScale {
                qblock_id: 1,
                kind,
                budget_seconds: None,
                updated_at: 0,
            };
            let bytes = record.encode();
            assert_eq!(bytes.get(8), Some(&tag));
            let decoded = ParticipationRecordScale::decode(&mut &bytes[..]).expect("ok");
            assert_eq!(decoded.kind, kind);
        }
        assert!(HardwareFilter::parse(&["tpu"]).is_err());
    }

    #[test]
    fn hardware_filter_matches_classes() {
        let qpu = HardwareFilter::parse(&["qpu"]).expect("ok");
        assert!(qpu.admits(Some(MinerKind::QpuIonq)));
        assert!(!qpu.admits(Some(MinerKind::Asic)));
        assert!(!qpu.admits(None));
        let cpu = HardwareFilter::parse(&["cpu"]).expect("ok");
        assert!(cpu.admits(Some(MinerKind::Cpu)));
        assert!(!cpu.admits(Some(MinerKind::Gpu)));
        assert!(HardwareFilter::default().admits(None));
    }
}
