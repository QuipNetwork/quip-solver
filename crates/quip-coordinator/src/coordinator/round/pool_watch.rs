//! Decide whether a proof pending in the pool ends the current round, and
//! hold the miners until the block that includes it.
//!
//! The judge that replays the pallet's checks is the caller's: the chain
//! supplies it (`chain::pool::judge_pending_proof`). This module caches its
//! verdicts per root and runs the wait for the qblock.

use std::collections::{HashMap, HashSet};

use crate::types::snapshot::{MiningSnapshot, PendingProof, Verdict};

/// Consecutive polls with no clearing proof before the round resumes. One
/// poll can race the block import that prunes the proof from the pool.
pub const AWAIT_CLEAR_POLLS: u32 = 2;

/// Blocks to hold the miners for a pending proof before the round resumes. A
/// proof that stays pending this long is not landing.
pub const AWAIT_QBLOCK_MAX_BLOCKS: u64 = 2;

/// The clearing proof a scan found, for the log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clearing {
    /// Signing account of the proof.
    pub account: [u8; 32],
    /// The proof's `PoW` nonce. With the account it names the proof across resubmissions.
    pub nonce: sp_core::U256,
    /// Its best energy, in milli-units.
    pub best_energy_milli: i64,
}

/// What the latest pool read observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolObservation {
    /// A clearing proof was seen.
    Clearing,
    /// The pool was read and holds no clearing proof.
    Clear,
    /// The pool could not be read.
    Unknown,
}

/// Why the feeder leaves `AwaitingQBlock` without a new root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeReason {
    /// No clearing proof for [`AWAIT_CLEAR_POLLS`] polls.
    PoolClear,
    /// [`AWAIT_QBLOCK_MAX_BLOCKS`] blocks passed since the stop.
    BlocksElapsed,
}

impl std::fmt::Display for ResumeReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PoolClear => "no clearing proof is pending",
            Self::BlocksElapsed => "no qblock followed the pending proof",
        })
    }
}

/// Per-round memory of the pool scan and of the wait for the qblock.
#[derive(Debug, Default)]
pub struct PoolWatch {
    /// Round the cached verdicts belong to.
    root: Option<[u8; 32]>,
    /// Extrinsic hash to the clearing verdict, if it clears.
    verdicts: HashMap<[u8; 32], Option<Clearing>>,
    /// Proofs that stopped the miners and then left the pool without a qblock,
    /// or stayed pending past the block ceiling, by signer and `PoW` nonce.
    /// A resubmission under a new extrinsic hash is ignored too. Cleared with the root.
    ignored: HashSet<([u8; 32], sp_core::U256)>,
    /// Block the miners were stopped at.
    stopped_at: Option<u64>,
    /// Consecutive polls since the stop with no clearing proof.
    clear_polls: u32,
}

impl PoolWatch {
    /// An empty watch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Judge every pending proof against `snap` with `judge`. Returns the
    /// clearing proof with the lowest energy, if any clears. Verdicts are
    /// cached by extrinsic hash for the life of the root, except `BelowGates`.
    pub fn scan(
        &mut self,
        pending: &[PendingProof],
        snap: &MiningSnapshot,
        judge: impl Fn(&PendingProof, &MiningSnapshot) -> Verdict,
    ) -> Option<Clearing> {
        if self.root != Some(snap.last_proof_block_hash) {
            self.root = Some(snap.last_proof_block_hash);
            self.verdicts.clear();
            self.ignored.clear();
        }
        let mut best: Option<Clearing> = None;
        for p in pending {
            if self.ignored.contains(&(p.account, p.proof.nonce)) {
                continue;
            }
            let verdict = if let Some(verdict) = self.verdicts.get(&p.extrinsic_hash) {
                *verdict
            } else {
                let verdict = match judge(p, snap) {
                    Verdict::Clears { best_energy_milli } => Some(Clearing {
                        account: p.account,
                        nonce: p.proof.nonce,
                        best_energy_milli,
                    }),
                    Verdict::OtherRound => None,
                    Verdict::BelowGates => continue,
                    Verdict::Malformed(reason) => {
                        tracing::debug!(
                            extrinsic = %crate::types::hex::hex_encode(&p.extrinsic_hash),
                            reason,
                            "pool: pending proof is malformed"
                        );
                        None
                    }
                };
                let _ = self.verdicts.insert(p.extrinsic_hash, verdict);
                verdict
            };
            if let Some(c) = verdict {
                if best.is_none_or(|b| c.best_energy_milli < b.best_energy_milli) {
                    best = Some(c);
                }
            }
        }
        best
    }

    /// Called on every same-root resume: the proofs that held the miners
    /// produced no qblock, so they cannot hold them again on this root.
    pub fn expire_clearing(&mut self) {
        self.verdicts.retain(|_, verdict| {
            if let Some(c) = verdict {
                let _ = self.ignored.insert((c.account, c.nonce));
                false
            } else {
                true
            }
        });
    }

    /// The miners were stopped at `block`.
    pub fn stopped(&mut self, block: u64) {
        self.stopped_at = Some(block);
        self.clear_polls = 0;
    }

    /// After one poll in `AwaitingQBlock`: whether to resume the round on the
    /// same root, and why. `observed` records the pool read outcome.
    /// `block` is the poll's head number.
    pub fn resume_reason(&mut self, observed: PoolObservation, block: u64) -> Option<ResumeReason> {
        match observed {
            PoolObservation::Clearing => self.clear_polls = 0,
            PoolObservation::Clear => self.clear_polls = self.clear_polls.saturating_add(1),
            PoolObservation::Unknown => {}
        }
        if self.clear_polls >= AWAIT_CLEAR_POLLS {
            return Some(ResumeReason::PoolClear);
        }
        let since = self.stopped_at?;
        if block.saturating_sub(since) >= AWAIT_QBLOCK_MAX_BLOCKS {
            return Some(ResumeReason::BlocksElapsed);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::extrinsic::account_identity_bytes;
    use crate::chain::pool::judge_pending_proof;
    use crate::types::proof::QuantumProof;
    use quantum_validation::derive_nonce;
    use quantum_validation::packed::pack_solution;
    use quantum_validation::{
        energy_of_solution_indexed, generate_ising_model_indexed, AllowedValueSpec, TopologyIndex,
    };
    use sp_core::H256;

    fn ring() -> MiningSnapshot {
        MiningSnapshot {
            head_hash: [0u8; 32],
            last_proof_block_hash: [7u8; 32],
            topology_hash: vec![9u8; 32],
            nodes: vec![0, 1, 2, 3],
            edges: vec![(0, 1), (1, 2), (2, 3), (0, 3)],
            allowed_h_milli: vec![-1000, 0, 1000],
            allowed_j_milli: vec![-1000, 1000],
            allowed_spin_milli: vec![-1000, 1000],
            min_solutions: 1,
            max_energy_milli: 0,
            min_diversity_milli: 0,
            block_number: 42,
            spec_version: 117,
            target_proof_blocks: crate::types::snapshot::TARGET_PROOF_BLOCKS,
            family: None,
        }
    }

    const ACCOUNT: [u8; 32] = [5u8; 32];
    const SALT: [u8; 32] = [3u8; 32];

    fn nonce_for(snap: &MiningSnapshot, salt: [u8; 32]) -> sp_core::U256 {
        let identity = account_identity_bytes(&ACCOUNT);
        derive_nonce(&snap.last_proof_block_hash, &identity, &salt)
    }

    /// Brute-force the ground state of the drawn model over all 16 spin
    /// assignments, so the test needs no golden value.
    fn ground_state(snap: &MiningSnapshot, salt: [u8; 32]) -> (i64, Vec<i8>) {
        let index = TopologyIndex::new(&snap.nodes, &snap.edges).expect("index");
        let (h, j) = generate_ising_model_indexed(
            nonce_for(snap, salt),
            &snap.nodes,
            &snap.edges,
            &AllowedValueSpec::Set(snap.allowed_h_milli.as_slice()),
            &AllowedValueSpec::Set(snap.allowed_j_milli.as_slice()),
            &index,
        )
        .expect("model");
        let mut best: Option<(i64, Vec<i8>)> = None;
        for bits in 0u8..16 {
            let spins: Vec<i8> = (0..4)
                .map(|i| if bits & (1 << i) == 0 { -1 } else { 1 })
                .collect();
            let e =
                energy_of_solution_indexed(&spins, &h, &snap.edges, &j, &index).expect("energy");
            if best.as_ref().is_none_or(|(b, _)| e < *b) {
                best = Some((e, spins));
            }
        }
        best.expect("one assignment")
    }

    fn pending_with(snap: &MiningSnapshot, rows: &[Vec<i8>], salt: [u8; 32]) -> PendingProof {
        let spec = AllowedValueSpec::Set(snap.allowed_spin_milli.as_slice());
        let solutions = rows
            .iter()
            .map(|spins| {
                let milli: Vec<i32> = spins.iter().map(|&s| i32::from(s) * 1000).collect();
                pack_solution(&milli, &spec).expect("pack")
            })
            .collect();
        PendingProof {
            extrinsic_hash: [1u8; 32],
            account: ACCOUNT,
            proof: QuantumProof {
                topology_hash: H256::from_slice(&snap.topology_hash),
                nonce: nonce_for(snap, salt),
                salt,
                solutions,
                device_access_time_us: 0,
            },
        }
    }

    /// A polariton snapshot, the first salt whose nonce gives an instance, and
    /// one feasible witness of that instance.
    fn family_case() -> (MiningSnapshot, [u8; 32], Vec<i8>) {
        use quantum_validation::families::{AdvertisedLimits, FamilyChallenge, CONTRACTS};
        let blob =
            quip_solver_conformance::topology(quip_solver_conformance::families::ROUTING_TOPOLOGY)
                .expect("routing topology fixture");
        let topology = crate::types::topology::Topology::from_nodes_edges(
            blob.nodes.clone(),
            blob.edges.clone(),
            &[0],
            &[1000],
            &[-1000, 1000],
        );
        let family = crate::types::family::ChallengeFamily::admit(
            crate::types::family::FAMILY_CONTRACTS,
            ising_generators::polariton::POLARITON_V1.family_id,
            ising_generators::polariton::POLARITON_V1.revision,
            quip_solver_conformance::families::polariton::P_POLARITON_Z24_L2_P0.to_vec(),
            &topology,
        )
        .expect("admitted");
        let mut snap = ring();
        snap.nodes = blob.nodes;
        snap.edges = blob.edges;
        snap.max_energy_milli = i64::MAX;
        snap.family = Some(family.clone());
        let descriptor = family.descriptor();
        let limits = AdvertisedLimits {
            max_nodes: u64::MAX,
            max_edges: u64::MAX,
            max_degree: 0,
        };
        let challenge = FamilyChallenge::admit(
            CONTRACTS,
            descriptor.family_id.as_bytes(),
            descriptor.revision,
            &descriptor.params,
            &snap.nodes,
            &snap.edges,
            &limits,
        )
        .expect("the validator admits the fixture");
        let (salt, instance) = (0u8..64)
            .find_map(|i| {
                let salt = [i; 32];
                challenge
                    .regenerate(nonce_for(&snap, salt).to_big_endian())
                    .ok()
                    .map(|instance| (salt, instance))
            })
            .expect("a salt yields an instance");
        let n = instance.node_ids().len();
        assert!(n <= 20, "the witness search is exhaustive: {n} nodes");
        let spins = (0u32..1 << n)
            .map(|bits| {
                (0..n)
                    .map(|k| if bits >> k & 1 == 1 { -1 } else { 1 })
                    .collect::<Vec<i8>>()
            })
            .find(|spins| challenge.verify_witness(&instance, spins).is_ok())
            .expect("the instance has a feasible witness");
        (snap, salt, spins)
    }

    /// A pending family proof is judged on the instance its nonce regenerates,
    /// not on the legacy draw. Its rows are compact codec witnesses, and one
    /// that does not decode rejects the whole proof.
    #[test]
    fn a_family_proof_is_judged_on_its_regenerated_instance() {
        let (snap, salt, spins) = family_case();
        let family = snap.family.clone().expect("a family snapshot");
        let mut pending = pending_with(&snap, &[], salt);
        pending.proof.solutions = crate::chain::proof_encode::encode_family_witnesses(
            &family,
            &snap.nodes,
            &snap.edges,
            pending.proof.nonce.to_big_endian(),
            [spins.as_slice()],
        )
        .expect("a feasible witness encodes");
        assert!(judge_pending_proof(&pending, &snap).clears());

        for witness in &mut pending.proof.solutions {
            witness.truncate(witness.len().saturating_sub(1));
        }
        match judge_pending_proof(&pending, &snap) {
            Verdict::Malformed(reason) => assert!(reason.contains("family witness"), "{reason}"),
            other => panic!("a truncated witness is rejected: {other:?}"),
        }
    }

    #[test]
    fn judge_pending_proof_verdicts() {
        type Mutate = fn(&mut MiningSnapshot, i64);
        type Expect = fn(i64) -> Verdict;
        // The pallet gate is strict: energy must be below the ceiling.
        let cases: [(&str, Mutate, Expect); 5] = [
            (
                "ceiling one milli above the ground state",
                |snap, energy| snap.max_energy_milli = energy + 1,
                |energy| Verdict::Clears {
                    best_energy_milli: energy,
                },
            ),
            (
                "ceiling at the ground state",
                |snap, energy| snap.max_energy_milli = energy,
                |_| Verdict::BelowGates,
            ),
            (
                "nonce from another root",
                |snap, energy| {
                    snap.max_energy_milli = energy + 1;
                    snap.last_proof_block_hash = [8u8; 32];
                },
                |_| Verdict::OtherRound,
            ),
            (
                "another topology",
                |snap, energy| {
                    snap.max_energy_milli = energy + 1;
                    snap.topology_hash = vec![10u8; 32];
                },
                |_| Verdict::OtherRound,
            ),
            (
                "too few valid rows",
                |snap, energy| {
                    snap.max_energy_milli = energy + 1;
                    snap.min_solutions = 2;
                },
                |_| Verdict::BelowGates,
            ),
        ];
        for (label, mutate, expected) in cases {
            let mut snap = ring();
            let (energy, spins) = ground_state(&snap, SALT);
            let pending = pending_with(&snap, &[spins], SALT);
            mutate(&mut snap, energy);
            assert_eq!(
                judge_pending_proof(&pending, &snap),
                expected(energy),
                "{label}"
            );
        }
    }

    #[test]
    fn a_bad_packed_row_is_malformed() {
        let snap = ring();
        let mut pending = pending_with(&snap, &[], SALT);
        pending.proof.solutions = vec![Vec::new()];
        assert!(matches!(
            judge_pending_proof(&pending, &snap),
            Verdict::Malformed(_)
        ));
    }

    #[test]
    fn scan_reports_the_best_clearing_proof_and_caches_by_hash() {
        let mut snap = ring();
        let (energy, spins) = ground_state(&snap, SALT);
        snap.max_energy_milli = energy + 1;
        let pending = pending_with(&snap, &[spins], SALT);
        let mut watch = PoolWatch::new();
        let first = watch
            .scan(std::slice::from_ref(&pending), &snap, judge_pending_proof)
            .expect("clears");
        assert_eq!(first.account, ACCOUNT);
        assert_eq!(first.best_energy_milli, energy);
        // The same hash is not judged again: a corrupted copy under the same
        // hash still reports the cached verdict.
        let mut same_hash = pending.clone();
        same_hash.proof.solutions = vec![Vec::new()];
        assert!(watch
            .scan(&[same_hash], &snap, judge_pending_proof)
            .is_some());
        // Verdicts are forgotten when the root changes.
        snap.last_proof_block_hash = [8u8; 32];
        assert!(watch.scan(&[pending], &snap, judge_pending_proof).is_none());
    }

    #[test]
    fn a_below_gates_proof_is_judged_again_when_the_ceiling_eases() {
        let mut snap = ring();
        let (energy, spins) = ground_state(&snap, SALT);
        snap.max_energy_milli = energy;
        let pending = pending_with(&snap, &[spins], SALT);
        let mut watch = PoolWatch::new();
        assert!(watch
            .scan(std::slice::from_ref(&pending), &snap, judge_pending_proof)
            .is_none());
        snap.max_energy_milli = energy + 1;
        assert!(watch.scan(&[pending], &snap, judge_pending_proof).is_some());
    }

    #[test]
    fn a_timed_out_proof_cannot_stop_the_same_root_again() {
        let mut snap = ring();
        let (energy, spins) = ground_state(&snap, SALT);
        snap.max_energy_milli = energy + 1;
        let pending = pending_with(&snap, &[spins], SALT);
        let mut watch = PoolWatch::new();
        assert!(watch
            .scan(std::slice::from_ref(&pending), &snap, judge_pending_proof)
            .is_some());
        watch.stopped(100);
        assert_eq!(
            watch.resume_reason(PoolObservation::Clearing, 102),
            Some(ResumeReason::BlocksElapsed)
        );
        watch.expire_clearing();
        assert!(watch
            .scan(std::slice::from_ref(&pending), &snap, judge_pending_proof)
            .is_none());
        let mut resubmitted = pending;
        resubmitted.extrinsic_hash = [2u8; 32];
        assert!(watch
            .scan(&[resubmitted], &snap, judge_pending_proof)
            .is_none());

        snap.last_proof_block_hash = [8u8; 32];
        let (energy, spins) = ground_state(&snap, SALT);
        snap.max_energy_milli = energy + 1;
        let pending = pending_with(&snap, &[spins], SALT);
        assert!(watch.scan(&[pending], &snap, judge_pending_proof).is_some());
    }

    #[test]
    fn a_proof_that_left_the_pool_without_a_qblock_cannot_stop_the_same_root_again() {
        let mut snap = ring();
        let (energy, spins) = ground_state(&snap, SALT);
        snap.max_energy_milli = energy + 1;
        let mut pending = pending_with(&snap, &[spins], SALT);
        let mut watch = PoolWatch::new();
        assert!(watch
            .scan(std::slice::from_ref(&pending), &snap, judge_pending_proof)
            .is_some());
        watch.stopped(100);
        assert_eq!(watch.resume_reason(PoolObservation::Clear, 100), None);
        assert_eq!(
            watch.resume_reason(PoolObservation::Clear, 100),
            Some(ResumeReason::PoolClear)
        );
        watch.expire_clearing();
        pending.extrinsic_hash = [2u8; 32];
        assert!(watch.scan(&[pending], &snap, judge_pending_proof).is_none());

        let salt = [4u8; 32];
        let (energy, spins) = ground_state(&snap, salt);
        snap.max_energy_milli = energy + 1;
        let pending = pending_with(&snap, &[spins], salt);
        assert!(watch.scan(&[pending], &snap, judge_pending_proof).is_some());
    }

    #[test]
    fn scan_picks_the_lowest_energy_among_clearing_proofs() {
        let mut snap = ring();
        let (first_energy, first_spins) = ground_state(&snap, SALT);
        let salt = [4u8; 32];
        let (second_energy, second_spins) = ground_state(&snap, salt);
        assert_ne!(
            first_energy, second_energy,
            "fixtures need different energies"
        );
        snap.max_energy_milli = first_energy.max(second_energy) + 1;
        let first = pending_with(&snap, &[first_spins], SALT);
        let mut second = pending_with(&snap, &[second_spins], salt);
        second.extrinsic_hash = [2u8; 32];
        let mut watch = PoolWatch::new();
        let mut pending = [first, second];
        let best = watch
            .scan(&pending, &snap, judge_pending_proof)
            .expect("both proofs clear");
        assert_eq!(best.best_energy_milli, first_energy.min(second_energy));
        pending.reverse();
        let best = watch
            .scan(&pending, &snap, judge_pending_proof)
            .expect("both proofs still clear");
        assert_eq!(best.best_energy_milli, first_energy.min(second_energy));
    }

    #[test]
    fn an_unreadable_pool_does_not_count_as_clear() {
        let mut watch = PoolWatch::new();
        watch.stopped(100);
        assert_eq!(watch.resume_reason(PoolObservation::Unknown, 100), None);
        assert_eq!(watch.resume_reason(PoolObservation::Unknown, 100), None);
        assert_eq!(watch.resume_reason(PoolObservation::Clear, 100), None);
        assert_eq!(
            watch.resume_reason(PoolObservation::Clear, 100),
            Some(ResumeReason::PoolClear)
        );

        // An unreadable poll preserves a clear observation and the block ceiling.
        watch.stopped(100);
        assert_eq!(watch.resume_reason(PoolObservation::Clear, 100), None);
        assert_eq!(watch.resume_reason(PoolObservation::Unknown, 101), None);
        assert_eq!(
            watch.resume_reason(PoolObservation::Clear, 101),
            Some(ResumeReason::PoolClear)
        );
        watch.stopped(100);
        assert_eq!(
            watch.resume_reason(PoolObservation::Unknown, 102),
            Some(ResumeReason::BlocksElapsed)
        );
        watch.stopped(100);
        assert_eq!(
            watch.resume_reason(PoolObservation::Clear, 102),
            Some(ResumeReason::BlocksElapsed)
        );
    }

    #[test]
    fn resume_reason_follows_clear_polls_and_the_block_ceiling() {
        let mut watch = PoolWatch::new();

        // Two clear polls resume; a clearing poll does not count.
        watch.stopped(100);
        assert_eq!(watch.resume_reason(PoolObservation::Clearing, 100), None);
        assert_eq!(watch.resume_reason(PoolObservation::Clear, 100), None);
        assert_eq!(
            watch.resume_reason(PoolObservation::Clear, 100),
            Some(ResumeReason::PoolClear)
        );

        // A clearing poll resets the clear count.
        watch.stopped(100);
        assert_eq!(watch.resume_reason(PoolObservation::Clear, 100), None);
        assert_eq!(watch.resume_reason(PoolObservation::Clearing, 100), None);
        assert_eq!(watch.resume_reason(PoolObservation::Clear, 100), None);
        assert_eq!(
            watch.resume_reason(PoolObservation::Clear, 100),
            Some(ResumeReason::PoolClear)
        );

        // Two blocks resume even while the proof is pending.
        watch.stopped(100);
        assert_eq!(watch.resume_reason(PoolObservation::Clearing, 101), None);
        assert_eq!(
            watch.resume_reason(PoolObservation::Clearing, 102),
            Some(ResumeReason::BlocksElapsed)
        );

        // stopped() resets the wait.
        watch.stopped(100);
        assert_eq!(watch.resume_reason(PoolObservation::Clear, 100), None);
        watch.stopped(105);
        assert_eq!(watch.resume_reason(PoolObservation::Clear, 105), None);
        assert_eq!(watch.resume_reason(PoolObservation::Clearing, 106), None);
        assert_eq!(
            watch.resume_reason(PoolObservation::Clearing, 107),
            Some(ResumeReason::BlocksElapsed)
        );
    }
}
