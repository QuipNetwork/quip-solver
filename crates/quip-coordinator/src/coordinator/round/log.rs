//! Per-round logging context: the outcome lines for one root.

use crate::coordinator::round::pool_watch::Clearing;
use crate::types::proof::{SubmitAction, SubmitReceipt};

/// Logging context shared by the feeder and session submissions for one root.
/// A receipt can arrive after the feeder has already opened the next round.
#[derive(Debug)]
pub(crate) struct RoundLog {
    qblock_id: Option<u64>,
    our_account: [u8; 32],
    started: std::time::Instant,
    our_best_milli: Option<i64>,
    pending: Option<(Clearing, i64)>,
    submissions: usize,
    closed: bool,
    reported: bool,
}

impl RoundLog {
    pub(crate) fn new(latest_qblock_id: Option<u64>, our_account: [u8; 32]) -> Self {
        Self {
            // Like participation, outcomes name the candidate, not the latest
            // completed qblock used as the attempt-log key in CoordinatorState.
            qblock_id: latest_qblock_id.map(|id| id.saturating_add(1)),
            our_account,
            started: std::time::Instant::now(),
            our_best_milli: None,
            pending: None,
            submissions: 0,
            closed: false,
            reported: false,
        }
    }

    pub(crate) fn observe_pending(&mut self, proof: Clearing, target_milli: i64) {
        self.pending = Some((proof, target_milli));
    }

    pub(crate) fn log_pause(
        &self,
        generation: u64,
        block: u64,
        cancelled_jobs: usize,
        miners_told: usize,
    ) {
        let Some((proof, target_milli)) = self.pending else {
            return;
        };
        tracing::info!(
            generation,
            qblock_id = %crate::observe::logging::display_option(self.qblock_id),
            block,
            proof_owner = %(if proof.account == self.our_account { "ours" } else { "another_miner" }),
            signer = %crate::types::hex::hex_encode(&proof.account),
            proof_energy = crate::observe::logging::energy_units(proof.best_energy_milli),
            target_energy = crate::observe::logging::energy_units(target_milli),
            our_best_energy_this_round = %crate::observe::logging::display_energy(self.our_best_milli),
            cancelled_jobs,
            miners_told,
            "pausing coordinator: a pending proof clears this round; awaiting the qblock"
        );
    }

    pub(crate) fn resume(&mut self) {
        self.pending = None;
    }

    pub(crate) fn close(&mut self) {
        self.closed = true;
        self.report_other_win();
    }

    /// Fold one validated miner result into our best for the round, so the
    /// pause, heartbeat and loss lines report what we found even when we
    /// never submitted. `i64::MAX` marks a result with no rows.
    pub(crate) fn observe_result(&mut self, energy_milli: i64) {
        if energy_milli == i64::MAX {
            return;
        }
        self.our_best_milli = Some(
            self.our_best_milli
                .map_or(energy_milli, |best| best.min(energy_milli)),
        );
    }

    pub(crate) fn submission_started(&mut self) {
        self.submissions += 1;
    }

    pub(crate) fn submission_finished(
        &mut self,
        receipt: Option<&SubmitReceipt>,
        proof_energy_milli: i64,
        target_milli: i64,
    ) {
        self.submissions = self.submissions.saturating_sub(1);
        if let Some(receipt) = receipt.filter(|r| r.action == SubmitAction::Success) {
            // A late receipt can write current_best_milli after a reseed. Only
            // its captured round owns that energy, so never import shared bests.
            self.our_best_milli = Some(
                self.our_best_milli
                    .map_or(proof_energy_milli, |old| old.min(proof_energy_milli)),
            );
            if !self.reported {
                self.reported = true;
                tracing::info!(
                    qblock_id = %crate::observe::logging::display_option(self.qblock_id),
                    inclusion_block = %crate::observe::logging::display_option(receipt.block_number),
                    proof_energy = crate::observe::logging::energy_units(proof_energy_milli),
                    target_energy = crate::observe::logging::energy_units(target_milli),
                    round_elapsed_s = self.started.elapsed().as_secs(),
                    "qblock won by us"
                );
            }
        }
        self.report_other_win();
    }

    fn report_other_win(&mut self) {
        if !self.closed || self.reported || self.submissions != 0 {
            return;
        }
        let Some((proof, target_milli)) = self.pending else {
            return;
        };
        if proof.account == self.our_account {
            return;
        }
        // Pool proofs can vanish without inclusion, so wait for a new root.
        // The chain seam exposes no foreign winning receipt: attribute this
        // closure to the last observed clearing proof and state that source.
        // Outstanding own receipts settle first, preventing two outcomes.
        self.reported = true;
        tracing::info!(
            qblock_id = %crate::observe::logging::display_option(self.qblock_id),
            winner = %crate::types::hex::hex_encode(&proof.account),
            proof_energy = crate::observe::logging::energy_units(proof.best_energy_milli),
            target_energy = crate::observe::logging::energy_units(target_milli),
            our_best_energy_this_round = %crate::observe::logging::display_energy(self.our_best_milli),
            winner_source = %"pending_pool",
            "qblock won by another miner"
        );
    }

    pub(crate) fn log_heartbeat(
        &self,
        block: u64,
        generation: u64,
        results_validated: u64,
        paused: bool,
    ) {
        tracing::info!(
            block,
            generation,
            qblock_id = %crate::observe::logging::display_option(self.qblock_id),
            results_validated,
            our_best_energy_this_round = %crate::observe::logging::display_energy(self.our_best_milli),
            round_state = %(if paused { "paused_awaiting_qblock" } else { "mining" }),
            "coordinator alive"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::logging::test_support::capture_logs;

    fn pending(account: [u8; 32]) -> Clearing {
        Clearing {
            account,
            nonce: 1.into(),
            best_energy_milli: -5_038_000,
        }
    }

    fn won() -> SubmitReceipt {
        SubmitReceipt {
            action: SubmitAction::Success,
            extrinsic_hash: Some([3; 32]),
            block_hash: Some([4; 32]),
            block_number: Some(693),
        }
    }

    #[test]
    fn round_log_pause_names_the_owner_target_and_our_best() {
        for (account, best, owner, best_text) in [
            ([1; 32], Some(-4_999_000), "ours", "-4999"),
            ([2; 32], None, "another_miner", "none"),
        ] {
            let mut log = RoundLog::new(Some(64), [1; 32]);
            log.our_best_milli = best;
            log.observe_pending(pending(account), -5_030_000);
            let output = capture_logs(|| log.log_pause(23, 692, 2, 1));
            for field in [
                "qblock_id=65".to_owned(),
                format!("proof_owner={owner}"),
                "proof_energy=-5038".to_owned(),
                "target_energy=-5030".to_owned(),
                format!("our_best_energy_this_round={best_text}"),
            ] {
                assert!(output.contains(&field), "{output}");
            }
            assert!(output.contains("pausing coordinator"), "{output}");
            assert!(!output.contains(" best_energy="), "{output}");
            assert!(!output.contains("qblock won"), "{output}");
        }
    }

    #[test]
    fn round_log_reports_our_best_result_without_a_submission() {
        let mut log = RoundLog::new(Some(64), [1; 32]);
        log.observe_result(-5_020_000);
        log.observe_result(i64::MAX); // a result with no rows changes nothing
        log.observe_result(-5_038_000);
        log.observe_result(-5_001_000);
        log.observe_pending(pending([2; 32]), -5_030_000);
        let output = capture_logs(|| {
            log.log_pause(23, 692, 2, 1);
            log.close();
        });
        assert_eq!(
            output.matches("our_best_energy_this_round=-5038").count(),
            2,
            "{output}"
        );
        assert!(output.contains("qblock won by another miner"), "{output}");
    }

    #[test]
    fn round_log_reports_only_foreign_pending_proofs_after_closure() {
        for (account, best, expected) in [
            ([2; 32], Some(-4_999_000), Some("-4999")),
            ([2; 32], None, Some("none")),
            ([1; 32], None, None),
        ] {
            let mut log = RoundLog::new(Some(64), [1; 32]);
            let output = capture_logs(|| {
                log.our_best_milli = best;
                log.observe_pending(pending(account), -5_030_000);
            });
            assert!(output.is_empty(), "pending is not included: {output}");
            let output = capture_logs(|| {
                log.close();
                log.close();
            });
            if let Some(best) = expected {
                assert_eq!(
                    output.matches("qblock won by another miner").count(),
                    1,
                    "{output}"
                );
                for field in [
                    "qblock_id=65".to_owned(),
                    format!("winner=0x{}", "02".repeat(32)),
                    "proof_energy=-5038".to_owned(),
                    "target_energy=-5030".to_owned(),
                    format!("our_best_energy_this_round={best}"),
                    "winner_source=pending_pool".to_owned(),
                ] {
                    assert!(output.contains(&field), "{output}");
                }
                assert!(!output.contains("qblock won by us"), "{output}");
            } else {
                assert!(
                    output.is_empty(),
                    "our pending proof needs a receipt: {output}"
                );
            }
        }
    }

    #[test]
    fn round_log_resume_does_not_report_an_unincluded_proof() {
        let mut log = RoundLog::new(Some(64), [1; 32]);
        let output = capture_logs(|| {
            log.our_best_milli = Some(-4_999_000);
            log.observe_pending(pending([2; 32]), -5_030_000);
            log.resume();
            log.close();
        });
        assert!(output.is_empty(), "{output}");
    }

    #[test]
    fn round_log_late_own_receipt_prevents_a_foreign_report() {
        let mut log = RoundLog::new(Some(64), [1; 32]);
        log.submission_started();
        log.observe_pending(pending([2; 32]), -5_030_000);
        let output = capture_logs(|| log.close());
        assert!(
            output.is_empty(),
            "wait for our outstanding receipt: {output}"
        );
        let output = capture_logs(|| {
            log.submission_finished(Some(&won()), -5_038_000, -5_030_000);
            log.close();
        });
        assert_eq!(output.matches("qblock won by us").count(), 1, "{output}");
        assert!(!output.contains("another miner"), "{output}");
    }

    #[test]
    fn round_log_failed_submit_releases_the_foreign_report() {
        for receipt in [
            None,
            Some(SubmitReceipt::action_only(SubmitAction::Retry)),
            Some(SubmitReceipt::action_only(SubmitAction::StopRoundStale)),
            Some(SubmitReceipt::action_only(SubmitAction::StopFatal)),
        ] {
            let mut log = RoundLog::new(Some(64), [1; 32]);
            log.submission_started();
            log.observe_pending(pending([2; 32]), -5_030_000);
            let output = capture_logs(|| log.close());
            assert!(output.is_empty(), "{output}");
            let output = capture_logs(|| {
                log.submission_finished(receipt.as_ref(), -5_038_000, -5_030_000);
                log.close();
            });
            assert_eq!(
                output.matches("qblock won by another miner").count(),
                1,
                "{output}"
            );
            assert!(!output.contains("qblock won by us"), "{output}");
        }
    }

    #[test]
    fn round_log_does_not_invent_an_unobserved_winner() {
        let mut log = RoundLog::new(None, [1; 32]);
        let output = capture_logs(|| log.close());
        assert!(output.is_empty(), "{output}");
    }

    #[test]
    fn round_log_heartbeat_keeps_our_best_while_paused() {
        let mut log = RoundLog::new(Some(64), [1; 32]);
        let output = capture_logs(|| {
            log.our_best_milli = Some(-4_999_000);
            log.log_heartbeat(691, 22, 2, false);
            log.observe_pending(pending([2; 32]), -5_030_000);
            log.log_heartbeat(692, 23, 2, true);
        });
        assert!(output.contains("round_state=mining"), "{output}");
        assert!(
            output.contains("round_state=paused_awaiting_qblock"),
            "{output}"
        );
        assert_eq!(
            output.matches("our_best_energy_this_round=-4999").count(),
            2,
            "{output}"
        );
        assert!(!output.contains(" best_energy="), "{output}");
    }
}
