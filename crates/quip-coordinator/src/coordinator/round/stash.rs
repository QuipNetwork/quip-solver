//! Win-time candidate stash (agm.2.4).
//!
//! Holds the most-viable solutions per generation and projects, from the decay
//! model, the block at which each becomes viable as difficulty eases — so the
//! coordinator can submit at the right block without polling the chain every
//! block. Already-viable solutions submit immediately on the session path; this
//! stash captures solutions that don't clear the *current* (already-decayed)
//! threshold yet but will after enough decay, so they aren't discarded.

use crate::coordinator::round::decay::{DecayAlgorithm, DecayModel};
use crate::coordinator::scoring::validate::{check_proof_gates, ProofGates, QualityGates};
use crate::observe::attempt::{CandidateSummary, StashSummary};
use crate::types::proof::SpinRow;

use std::fmt::Write as _;

/// How far ahead of the last proof the stash projects: 256 epochs of 100
/// blocks. A candidate that does not clear inside this window is not held.
pub const DECAY_HORIZON_BLOCKS: u64 = 25_600;

/// Blocks between the chain head a win-time proof is sent at and the first
/// block that can include it. The pallet checks a proof against the threshold
/// of the including block, so the win-time path gates a held candidate on the
/// projected threshold of `head + WIN_TIME_LEAD_BLOCKS` and sends it one block
/// before its viability block. Zero restores the old behaviour: gate on the
/// head's own threshold, which lands the proof one block after viability.
pub const WIN_TIME_LEAD_BLOCKS: u64 = 1;

/// The win-time decision for one feeder poll: the candidate that is due and the
/// replay of the chain's gates for the block a proof sent now would land in.
#[derive(Clone, Debug)]
pub struct WinTimeCheck {
    /// The best due, improving, unsubmitted candidate that clears `gates`,
    /// or the best due one with its failed check when none does.
    pub candidate: Candidate,
    /// Gates of the including block: the live gates with the projected ceiling.
    pub gates: QualityGates,
    /// The candidate's solutions replayed against `gates`.
    pub check: ProofGates,
}

/// A stashed candidate: a validated solution set awaiting its viability block.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// Job id (nonce) bytes.
    pub job_id: Vec<u8>,
    /// `PoW` salt (needed to build the live proof); `None` for mempool jobs.
    pub salt: Option<[u8; 32]>,
    /// Generation this candidate was mined for.
    pub generation: u64,
    /// Best energy of the stashed solutions, in milli-units.
    pub best_energy_milli: i64,
    /// Pairwise diversity of the stashed set, in milli-units.
    pub diversity_milli: u32,
    /// Count of gate-passing solutions retained.
    pub n_valid: u32,
    /// Solutions to resubmit when the candidate becomes viable.
    pub solutions: Vec<SpinRow>,
    /// Whether the job was a `PoW` job.
    pub is_pow: bool,
    /// Mempool order id (empty for `PoW`).
    pub order_id: Vec<u8>,
    /// Device access time reported by the miner, in microseconds.
    pub device_access_time_us: u64,
    /// Topology hash the job was mined on; empty for a mempool order.
    pub topology_hash: Vec<u8>,
    /// Whether this candidate has already been submitted.
    pub submitted: bool,
}

/// Per-generation stash of the top-K most-viable candidates plus the
/// projection inputs (decay model and last-proof block).
pub struct WinStash {
    generation: u64,
    /// Threshold model for the round. `None` until [`reset`](Self::reset)
    /// arms one, and while the chain reads that build it fail.
    model: Option<DecayModel>,
    last_proof_block: u64,
    k: usize,
    /// Kept sorted best-first (lowest energy), length ≤ `k`.
    candidates: Vec<Candidate>,
}

impl WinStash {
    /// Empty stash retaining the top-`k` candidates (k ≥ 1). No projection
    /// until [`reset`](Self::reset).
    #[must_use]
    pub fn new(k: usize) -> Self {
        Self {
            generation: 0,
            model: None,
            last_proof_block: 0,
            k: k.max(1),
            candidates: Vec::new(),
        }
    }

    /// Re-arm for a new generation with fresh projection inputs, dropping all
    /// held candidates (the prior round's problem is stale after a reseed).
    pub fn reset(&mut self, generation: u64, model: Option<DecayModel>, last_proof_block: u64) {
        self.generation = generation;
        self.model = model;
        self.last_proof_block = last_proof_block;
        self.candidates.clear();
    }

    /// Swap the decay rule and round-length target in place, keeping every
    /// candidate. Returns whether either changed. A runtime upgrade or a
    /// council `set_target_proof_blocks` takes effect at one block, so the
    /// feeder calls this on every poll with the head's rule and target.
    pub fn set_rule(&mut self, algorithm: DecayAlgorithm, target_proof_blocks: u64) -> bool {
        match self.model.as_mut() {
            Some(m) if m.algorithm != algorithm || m.target_proof_blocks != target_proof_blocks => {
                m.algorithm = algorithm;
                m.target_proof_blocks = target_proof_blocks;
                true
            }
            _ => false,
        }
    }

    /// The rule the projection runs under, or `None` without a model.
    #[must_use]
    pub fn algorithm(&self) -> Option<DecayAlgorithm> {
        self.model.as_ref().map(|m| m.algorithm)
    }

    /// Current generation this stash is armed for.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether no candidates are currently held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// Block at which `energy_milli` becomes viable: the last proof block
    /// plus the first elapsed count whose threshold strictly exceeds it.
    /// `None` without a model, or if it never clears within
    /// [`DECAY_HORIZON_BLOCKS`].
    #[must_use]
    pub fn viability_block(&self, energy_milli: i64) -> Option<u64> {
        let model = self.model.as_ref()?;
        model
            .first_clearing_elapsed(energy_milli, DECAY_HORIZON_BLOCKS)
            .map(|elapsed| self.last_proof_block.saturating_add(elapsed))
    }

    /// Insert a candidate if it becomes viable within the horizon and ranks in
    /// the top-K by energy (lowest wins). Returns whether it was kept.
    pub fn insert(&mut self, cand: Candidate) -> bool {
        // A result verified just before a reseed belongs to the old challenge
        // and family. Mempool work has generation 0 and no round to match.
        if cand.is_pow && cand.generation != self.generation {
            return false;
        }
        if self.viability_block(cand.best_energy_milli).is_none() {
            return false; // never clears within the horizon → not worth holding
        }
        let job_id = cand.job_id.clone();
        self.candidates.push(cand);
        self.candidates.sort_by_key(|c| c.best_energy_milli);
        self.candidates.truncate(self.k);
        self.candidates.iter().any(|c| c.job_id == job_id)
    }

    /// The `max_energy_milli` of block `head_block + lead`, from the model,
    /// when the model reproduces `live_ceiling` (the chain's value at
    /// `head_block`) exactly. A model that disagrees with the chain at the head
    /// (a council difficulty change mid-round, a rule the coordinator does not
    /// know) is not trusted one block ahead: the live ceiling is returned, as
    /// it is without a model or with `lead` zero. An early proof would cost a
    /// fee and one of the submit attempts, and a wrong model could repeat that
    /// until the attempts run out.
    #[must_use]
    pub fn projected_ceiling(&self, head_block: u64, live_ceiling: i64, lead: u64) -> i64 {
        let Some(model) = self.model.as_ref() else {
            return live_ceiling;
        };
        if lead == 0 {
            return live_ceiling;
        }
        let at = |block: u64| model.threshold_at(block.saturating_sub(self.last_proof_block));
        if at(head_block) != live_ceiling {
            return live_ceiling;
        }
        at(head_block.saturating_add(lead)).max(live_ceiling)
    }

    /// The win-time decision at chain head `head_block`, whose live gates are
    /// `live` (`QuantumPowApi_difficulty_for` at the head). A proof sent now is
    /// included at `head_block + lead` at the earliest, where the threshold is
    /// at least the projected one, so the gates are the live gates with the
    /// projected ceiling. `min_solutions` and `min_diversity_milli` do not
    /// decay; `n_valid` and diversity are recomputed over the rows the
    /// projected ceiling admits.
    ///
    /// The due candidates are the unsubmitted ones whose viability block has
    /// arrived by that landing block or by `estimated_block`, whichever is
    /// later, and that strictly improve on `current_best` (or when there is
    /// none), mirroring
    /// [`crate::coordinator::scoring::validate::beats_current`] so the
    /// win-time path never regresses what the session path sent. They are
    /// replayed best-first and the first that clears the gates is returned
    /// with an accepted check. When none clears them, the best due candidate
    /// is returned with its failed check, so the caller can report why it
    /// stays stashed. `None` when no candidate is due.
    #[must_use]
    pub fn win_time_check(
        &self,
        head_block: u64,
        estimated_block: u64,
        current_best: Option<i64>,
        live: &QualityGates,
        lead: u64,
    ) -> Option<WinTimeCheck> {
        let due_by = estimated_block.max(head_block.saturating_add(lead));
        let gates = QualityGates {
            min_energy_milli: self.projected_ceiling(head_block, live.min_energy_milli, lead),
            ..*live
        };
        let mut best_failed: Option<WinTimeCheck> = None;
        for candidate in self.candidates.iter().filter(|c| {
            !c.submitted
                && self
                    .viability_block(c.best_energy_milli)
                    .is_some_and(|b| b <= due_by)
                && current_best.is_none_or(|b| c.best_energy_milli < b)
        }) {
            let check = check_proof_gates(&candidate.solutions, &gates);
            let decision = WinTimeCheck {
                candidate: candidate.clone(),
                gates,
                check,
            };
            if decision.check.accepted {
                return Some(decision);
            }
            if best_failed.is_none() {
                best_failed = Some(decision);
            }
        }
        best_failed
    }

    /// Mark a candidate submitted so the driver won't re-submit it.
    pub fn mark_submitted(&mut self, job_id: &[u8]) {
        if let Some(c) = self.candidates.iter_mut().find(|c| c.job_id == job_id) {
            c.submitted = true;
        }
    }

    /// Per-qblock summary for the `attempts.json` annotation file.
    #[must_use]
    pub fn summary(&self) -> StashSummary {
        StashSummary {
            generation: self.generation,
            last_proof_block: self.last_proof_block,
            epoch_length: self.model.as_ref().map_or(0, |m| m.epoch_length),
            decay_algorithm: self.model.as_ref().map_or("none", |m| m.algorithm.name()),
            target_proof_blocks: self.model.as_ref().map_or(0, |m| m.target_proof_blocks),
            candidates: self
                .candidates
                .iter()
                .map(|c| CandidateSummary {
                    job_id: hex(&c.job_id),
                    best_energy_milli: c.best_energy_milli,
                    diversity_milli: c.diversity_milli,
                    n_valid: c.n_valid,
                    viability_block: self.viability_block(c.best_energy_milli),
                    submitted: c.submitted,
                })
                .collect(),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::round::decay::{DecayAlgorithm, DecayModel, EnergyCurve};

    /// Stepwise thresholds -50000, -48775, -47581, -46416 at blocks 100, 110,
    /// 120, 130 (last proof 100, epoch 10).
    fn model() -> DecayModel {
        DecayModel {
            base_max_energy_milli: -50_000,
            curve: Some(EnergyCurve {
                min_milli: -100_000,
                knee_milli: -50_000,
                max_milli: -1_000,
            }),
            epoch_length: 10,
            algorithm: DecayAlgorithm::Stepwise,
            target_proof_blocks: crate::types::snapshot::TARGET_PROOF_BLOCKS,
        }
    }

    fn stash_with(model: Option<DecayModel>, last_proof_block: u64, k: usize) -> WinStash {
        let mut s = WinStash::new(k);
        s.reset(1, model, last_proof_block);
        s
    }

    fn all_pass() -> QualityGates {
        crate::coordinator::scoring::validate::gates_from_target(None)
    }

    fn cand(job_id: u8, energy: i64) -> Candidate {
        Candidate {
            job_id: vec![job_id],
            salt: Some([job_id; 32]),
            generation: 1,
            best_energy_milli: energy,
            diversity_milli: 200,
            n_valid: 5,
            solutions: vec![row(energy, vec![1, -1])],
            is_pow: true,
            order_id: vec![],
            device_access_time_us: 0,
            topology_hash: vec![9; 32],
            submitted: false,
        }
    }

    #[test]
    fn only_proof_of_work_candidates_are_matched_to_the_generation() {
        let mut stash = stash_with(Some(model()), 0, 3);
        let mut stale = cand(1, -60_000);
        stale.generation = 2;
        assert!(!stash.insert(stale));
        assert!(stash.is_empty());
        assert!(stash.insert(cand(2, -60_000)), "the current generation");

        // A mempool candidate has no generation to match.
        let mut stash = stash_with(Some(model()), 0, 3);
        let mut mempool = cand(1, -60_000);
        mempool.generation = 0;
        mempool.is_pow = false;
        assert!(stash.insert(mempool));
    }

    fn job_byte(c: &Candidate) -> u8 {
        *c.job_id
            .first()
            .expect("test candidates use 1-byte job ids")
    }

    #[test]
    fn viability_block_offsets_the_clearing_block_from_the_last_proof() {
        let s = stash_with(Some(model()), 100, 4);
        // -49000 clears at step 1 → block 110.
        assert_eq!(s.viability_block(-49_000), Some(110));
        // -47000 clears at step 3 (-46416 is the first threshold above it) → 130.
        assert_eq!(s.viability_block(-47_000), Some(130));
        // Already below the base threshold → viable at the last proof block.
        assert_eq!(s.viability_block(-51_000), Some(100));
        // Above the easy cap → never.
        assert_eq!(s.viability_block(-1), None);
        // No projection → never.
        assert_eq!(stash_with(None, 100, 4).viability_block(-51_000), None);
    }

    #[test]
    fn insert_keeps_top_k_and_rejects_never_viable() {
        let mut s = stash_with(Some(model()), 100, 2);
        assert!(s.insert(cand(1, -49_000)));
        assert!(s.insert(cand(2, -47_500)));
        // A third, worse candidate is dropped (k=2, keeps the two best).
        assert!(!s.insert(cand(3, -46_500)));
        // A better one displaces the worst.
        assert!(s.insert(cand(4, -49_900)));
        // Never-viable (above the easy cap) is rejected outright.
        assert!(!s.insert(cand(5, -100)));
        let ids: Vec<u8> = s.candidates.iter().map(job_byte).collect();
        assert_eq!(ids, vec![4, 1]); // best-first: -49900, -49000
    }

    /// The candidate `win_time_check` returns at `block` with lead 0, which
    /// gates on the live ceiling, and whether its check passed.
    fn due_at(
        s: &WinStash,
        block: u64,
        best: Option<i64>,
        gates: &QualityGates,
    ) -> Option<(u8, bool)> {
        s.win_time_check(block, block, best, gates, 0)
            .map(|w| (job_byte(&w.candidate), w.check.accepted))
    }

    #[test]
    fn due_candidates_need_arrival_and_improvement() {
        let mut s = stash_with(Some(model()), 100, 4);
        let _ = s.insert(cand(1, -49_000)); // viable at block 110
        let _ = s.insert(cand(2, -47_000)); // viable at block 130

        let g = all_pass();
        assert_eq!(due_at(&s, 109, None, &g), None);
        assert_eq!(due_at(&s, 110, None, &g), Some((1, true)));
        // Current best already stronger → nothing improves.
        assert_eq!(due_at(&s, 110, Some(-49_500), &g), None);
        // Current best weaker → submit.
        assert_eq!(due_at(&s, 110, Some(-48_000), &g), Some((1, true)));

        // At 135 both are due; cand 1 (-49000) is the stronger one.
        assert_eq!(due_at(&s, 135, None, &g), Some((1, true)));
        s.mark_submitted(&[1]);
        assert_eq!(due_at(&s, 135, None, &g), Some((2, true)));
    }

    #[test]
    fn win_time_check_skips_a_due_candidate_that_fails_the_gates() {
        let mut s = stash_with(Some(model()), 100, 4);
        let mut a = cand(1, -49_000);
        a.solutions = vec![
            row(-49_000, vec![1, -1]),
            row(-30_000, vec![1, -1]),
            row(-30_000, vec![1, -1]),
        ];
        let mut b = cand(2, -48_000);
        b.solutions = vec![
            row(-48_000, vec![1, -1]),
            row(-48_000, vec![1, -1]),
            row(-48_000, vec![1, -1]),
        ];
        let _ = s.insert(a);
        let _ = s.insert(b);
        let gates = QualityGates {
            min_energy_milli: -47_581,
            min_diversity_milli: 0,
            min_solutions: 3,
        };
        assert_eq!(due_at(&s, 135, None, &gates), Some((2, true)));
    }

    #[test]
    fn win_time_check_reports_the_best_due_candidate_when_none_clears_the_gates() {
        let mut s = stash_with(Some(model()), 100, 4);
        let mut a = cand(1, -49_000);
        // Two rows, min_solutions 3.
        a.solutions = vec![row(-49_000, vec![1, -1]), row(-49_000, vec![1, -1])];
        let _ = s.insert(a);
        let gates = QualityGates {
            min_energy_milli: -47_581,
            min_diversity_milli: 0,
            min_solutions: 3,
        };
        assert_eq!(due_at(&s, u64::MAX, None, &gates), Some((1, false)));
    }

    /// Gated rule, last proof 1000, epoch 10, target 100: flat through block
    /// 1100, then easing every block.
    fn gated_stash() -> WinStash {
        let mut m = model();
        m.algorithm = DecayAlgorithm::Gated;
        m.target_proof_blocks = 100;
        stash_with(Some(m), 1_000, 4)
    }

    /// The chain's live ceiling at `block`: `QuantumPowApi_difficulty_for` at
    /// that head, which the pallet evaluates at the head's own number.
    fn chain_ceiling(s: &WinStash, block: u64) -> i64 {
        s.model
            .as_ref()
            .map_or(i64::MAX, |m| m.threshold_at(block.saturating_sub(1_000)))
    }

    fn live_gates(s: &WinStash, head: u64, min_solutions: u32) -> QualityGates {
        QualityGates {
            min_energy_milli: chain_ceiling(s, head),
            min_diversity_milli: 0,
            min_solutions,
        }
    }

    fn row(energy_milli: i64, spins: Vec<i8>) -> SpinRow {
        SpinRow {
            spins,
            energy_milli,
        }
    }

    /// The feeder decision at heads V-2, V-1 and V for a held candidate whose
    /// viability block is V: with the lead it goes out at V-1 (included at V,
    /// the first block whose threshold admits it); without, at V (included at
    /// V+1, one block late).
    #[test]
    fn win_time_check_sends_one_block_before_viability() {
        let mut s = gated_stash();
        // An energy that first clears 150 blocks after the last proof.
        let energy = chain_ceiling(&s, 1_150) - 1;
        let v = s.viability_block(energy).unwrap_or(0);
        assert!(v > 1_100, "viability {v} past the gated target");
        assert!(chain_ceiling(&s, v - 1) <= energy && energy < chain_ceiling(&s, v));
        let mut c = cand(1, energy);
        c.solutions = vec![row(energy, vec![1, -1, 1, -1])];
        assert!(s.insert(c));

        let decide = |s: &WinStash, head: u64, lead: u64| {
            s.win_time_check(head, head, None, &live_gates(s, head, 1), lead)
                .filter(|w| w.check.accepted)
                .map(|w| (job_byte(&w.candidate), w.gates.min_energy_milli))
        };
        // With the lead: hold at V-2, send at V-1 against the ceiling of V.
        assert_eq!(decide(&s, v - 2, WIN_TIME_LEAD_BLOCKS), None);
        assert_eq!(
            decide(&s, v - 1, WIN_TIME_LEAD_BLOCKS),
            Some((1, chain_ceiling(&s, v)))
        );
        assert_eq!(decide(&s, v, WIN_TIME_LEAD_BLOCKS).map(|d| d.0), Some(1));
        // The live gate alone at V-1 refuses it, so moving only the due test
        // one block earlier would change nothing.
        let at_head = s
            .win_time_check(v - 1, v - 1, None, &live_gates(&s, v - 1, 1), 0)
            .map(|w| w.check.accepted);
        assert_eq!(at_head, None, "lead 0: not due at V-1");
        let live_only = check_proof_gates(
            &[row(energy, vec![1, -1, 1, -1])],
            &live_gates(&s, v - 1, 1),
        );
        assert!(!live_only.accepted);
        // Lead zero is the old behaviour: hold at V-1, send at V.
        assert_eq!(decide(&s, v - 1, 0), None);
        assert_eq!(decide(&s, v, 0), Some((1, chain_ceiling(&s, v))));

        // `min_solutions` does not decay: one admitted row never clears two.
        let two = s
            .win_time_check(
                v - 1,
                v - 1,
                None,
                &live_gates(&s, v - 1, 2),
                WIN_TIME_LEAD_BLOCKS,
            )
            .map(|w| (w.check.n_valid, w.check.accepted));
        assert_eq!(two, Some((1, false)));

        // A current best at or below the candidate's energy suppresses it.
        let beaten = s.win_time_check(
            v - 1,
            v - 1,
            Some(energy),
            &live_gates(&s, v - 1, 1),
            WIN_TIME_LEAD_BLOCKS,
        );
        assert!(beaten.is_none());
        // Submitted candidates are not offered again.
        s.mark_submitted(&[1]);
        assert!(s
            .win_time_check(
                v - 1,
                v - 1,
                None,
                &live_gates(&s, v - 1, 1),
                WIN_TIME_LEAD_BLOCKS
            )
            .is_none());
    }

    #[test]
    fn projected_ceiling_trusts_the_model_only_where_it_matches_the_chain() {
        let s = gated_stash();
        // Flat inside the gated target: no easing over the lead.
        assert_eq!(s.projected_ceiling(1_050, -50_000, 1), -50_000);
        // Past the target, with the model matching the live value: the
        // model's ceiling of the next block.
        let live = chain_ceiling(&s, 1_150);
        assert!(chain_ceiling(&s, 1_151) > live);
        assert_eq!(
            s.projected_ceiling(1_150, live, 1),
            chain_ceiling(&s, 1_151)
        );
        assert_eq!(s.projected_ceiling(1_150, live, 0), live);
        // A live value the model does not reproduce: no projection.
        assert_eq!(s.projected_ceiling(1_150, live - 1, 1), live - 1);
        assert_eq!(s.projected_ceiling(1_150, -40_000, 1), -40_000);
        // No model: the live value.
        assert_eq!(
            stash_with(None, 1_000, 4).projected_ceiling(1_150, -40_000, 1),
            -40_000
        );
    }

    #[test]
    fn set_rule_keeps_candidates_and_reprojects_them() {
        let mut s = stash_with(Some(model()), 100, 4);
        assert!(s.insert(cand(1, -49_500)));
        assert_eq!(s.viability_block(-49_500), Some(110));
        assert_eq!(s.algorithm(), Some(DecayAlgorithm::Stepwise));

        assert!(s.set_rule(DecayAlgorithm::Continuous, 100));
        assert_eq!(s.algorithm(), Some(DecayAlgorithm::Continuous));
        let block = s.viability_block(-49_500).unwrap_or(u64::MAX);
        assert!(
            100 < block && block < 110,
            "continuous viability at {block}"
        );
        assert_eq!(s.candidates.len(), 1);
        assert_eq!(s.summary().decay_algorithm, "continuous");

        // Same rule and target again is not a change.
        assert!(!s.set_rule(DecayAlgorithm::Continuous, 100));
        // No projection → nothing to switch.
        assert!(!stash_with(None, 100, 4).set_rule(DecayAlgorithm::Continuous, 100));

        // Runtime 120: under the gated rule, a council target of 150 instead
        // of 100 holds the candidate 50 blocks longer.
        assert!(s.set_rule(DecayAlgorithm::Gated, 100));
        let at_default = s.viability_block(-49_500).unwrap_or(u64::MAX);
        assert!(s.set_rule(DecayAlgorithm::Gated, 150));
        assert_eq!(s.viability_block(-49_500), Some(at_default + 50));
        assert_eq!(s.summary().target_proof_blocks, 150);
        assert_eq!(s.candidates.len(), 1);
    }

    #[test]
    fn reset_drops_candidates_and_summary_names_the_model() {
        let mut s = stash_with(Some(model()), 100, 4);
        let _ = s.insert(cand(1, -49_000));
        s.reset(2, None, 0);
        assert!(s.is_empty());
        assert_eq!(s.generation(), 2);
        let summary = s.summary();
        assert_eq!(summary.epoch_length, 0);
        assert_eq!(summary.decay_algorithm, "none");
        assert_eq!(summary.target_proof_blocks, 0);
    }
}
