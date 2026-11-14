//! Named state machine for one mining round.
//!
//! The feeder and startup drive this machine and perform the I/O. Transitions
//! are a pure function of the current [`RoundState`] and a [`RoundEvent`].

pub mod decay;
pub mod log;
pub mod pool_watch;
pub mod stash;
pub mod timing;

use std::fmt;

/// Where the coordinator is in the round lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoundState {
    /// Stop every miner. Broadcast `Cancel{max_generation}`.
    StopMining,
    /// Wait until the validator has caught up.
    ValidatorSynced,
    /// Confirm the miner account can pay submit fees.
    AccountFunded,
    /// Confirm the signing account is in `QuantumPow.Miners`.
    MinerRegistered,
    /// Download topology, target, minimum solutions, and diversity.
    RequirementsDownloaded,
    /// File a node descriptor. Submits only on the first walk after process start.
    ///
    /// Participation is not a walk step. The feeder declares it for the
    /// candidate qblock once a miner has returned a Result for the round.
    DescriptorFiled,
    /// Broadcast the requirements and stage jobs.
    StartMining,
    /// Miners are stopped. A proof that clears this round is pending in the
    /// pool. Wait for the block that includes it, then mine from that block.
    AwaitingQBlock,
}

/// What happened in the current state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoundEvent {
    /// The current step finished. Advance.
    Succeeded,
    /// The current step failed. Stay and retry.
    Failed,
    /// A new qblock head arrived. Return to [`RoundState::StopMining`].
    NewHead,
    /// The process is stopping. The machine ends.
    Shutdown,
    /// A proof that clears the round is pending in the transaction pool.
    /// Stop the miners and wait for the block that includes it.
    WinPending,
    /// The pending proof did not land. Restart the round on the same root.
    Resume,
}

impl RoundState {
    /// First state of every round.
    #[must_use]
    pub(crate) const fn start() -> Self {
        Self::StopMining
    }

    /// Next state, or `None` when the machine should stop.
    #[must_use]
    pub(crate) fn transition(self, event: RoundEvent) -> Option<Self> {
        match event {
            RoundEvent::Shutdown => None,
            RoundEvent::NewHead => Some(Self::StopMining),
            RoundEvent::Failed => Some(self),
            RoundEvent::WinPending => Some(match self {
                Self::StartMining => Self::AwaitingQBlock,
                other => other,
            }),
            RoundEvent::Resume => Some(match self {
                Self::AwaitingQBlock => Self::StopMining,
                other => other,
            }),
            RoundEvent::Succeeded => match self {
                Self::StopMining => Some(Self::ValidatorSynced),
                Self::ValidatorSynced => Some(Self::AccountFunded),
                Self::AccountFunded => Some(Self::MinerRegistered),
                Self::MinerRegistered => Some(Self::RequirementsDownloaded),
                Self::RequirementsDownloaded => Some(Self::DescriptorFiled),
                Self::DescriptorFiled => Some(Self::StartMining),
                Self::StartMining | Self::AwaitingQBlock => Some(self),
            },
        }
    }

    /// Snake-case name for the log.
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::StopMining => "stop_mining",
            Self::ValidatorSynced => "validator_synced",
            Self::AccountFunded => "account_funded",
            Self::MinerRegistered => "miner_registered",
            Self::RequirementsDownloaded => "requirements_downloaded",
            Self::DescriptorFiled => "descriptor_filed",
            Self::StartMining => "start_mining",
            Self::AwaitingQBlock => "awaiting_qblock",
        }
    }

    /// Why the coordinator is in this state. Logged on entry.
    #[must_use]
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::StopMining => "stopping miners for a new round",
            Self::ValidatorSynced => "waiting until the validator is synced",
            Self::AccountFunded => "confirming the miner account can pay submit fees",
            Self::MinerRegistered => "registering the signing account as a miner on chain",
            Self::RequirementsDownloaded => "downloading the next qblock requirements",
            Self::DescriptorFiled => "filing the node descriptor",
            Self::StartMining => "starting mining",
            Self::AwaitingQBlock => {
                "miners stopped; a pending proof clears this round, waiting for the block that includes it"
            }
        }
    }

    /// Log this state once, when the machine enters it.
    ///
    /// A healthy walk leaves each state without a retry in well under a
    /// second. That is `trace`. A retry, or a state held more than 10
    /// seconds, uses [`Self::log_unhealthy`].
    pub(crate) fn log_entry(self, generation: u64) {
        tracing::trace!(state = %self, generation, "{}", self.reason());
    }

    /// Warn once for a state that is retrying or has been held too long.
    pub(crate) fn log_unhealthy(self, generation: u64) {
        tracing::warn!(state = %self, generation, "{}", self.reason());
    }
}

impl fmt::Display for RoundState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::{RoundEvent, RoundState};

    fn step(state: RoundState, event: RoundEvent) -> RoundState {
        let next = state.transition(event);
        assert!(
            next.is_some(),
            "expected {state:?} + {event:?} to continue the round"
        );
        match next {
            Some(s) => s,
            None => state,
        }
    }

    /// The pre-mining walk, in order. `AwaitingQBlock` is off the walk: the
    /// feeder enters it from `StartMining` and leaves it through
    /// `StopMining`.
    const WALK: [RoundState; 7] = [
        RoundState::StopMining,
        RoundState::ValidatorSynced,
        RoundState::AccountFunded,
        RoundState::MinerRegistered,
        RoundState::RequirementsDownloaded,
        RoundState::DescriptorFiled,
        RoundState::StartMining,
    ];

    const ALL: [RoundState; 8] = [
        RoundState::StopMining,
        RoundState::ValidatorSynced,
        RoundState::AccountFunded,
        RoundState::MinerRegistered,
        RoundState::RequirementsDownloaded,
        RoundState::DescriptorFiled,
        RoundState::StartMining,
        RoundState::AwaitingQBlock,
    ];

    /// Each `Succeeded` advances one step. `DescriptorFiled` leads straight
    /// to `StartMining`: participation is declared by the feeder once a miner
    /// returns a Result for the round, not by the pre-mining walk.
    #[test]
    fn full_round_visits_each_state_in_order() {
        let mut state = RoundState::start();
        let mut seen = vec![state];
        for _ in 0..6 {
            state = step(state, RoundEvent::Succeeded);
            seen.push(state);
        }
        assert_eq!(seen, WALK);
    }

    /// Every event against every state. A failed step stays and retries; a new
    /// head (a win during mining included) returns to `StopMining`; shutdown
    /// stops the machine; `WinPending` moves only `StartMining` to
    /// `AwaitingQBlock`; `Resume` moves only `AwaitingQBlock` back to
    /// `StopMining`; and `Succeeded` holds in the two mining states.
    #[test]
    fn every_event_against_every_state() {
        for state in ALL {
            for (event, expected) in [
                (RoundEvent::Failed, Some(state)),
                (RoundEvent::NewHead, Some(RoundState::StopMining)),
                (RoundEvent::Shutdown, None),
                (
                    RoundEvent::WinPending,
                    Some(if state == RoundState::StartMining {
                        RoundState::AwaitingQBlock
                    } else {
                        state
                    }),
                ),
                (
                    RoundEvent::Resume,
                    Some(if state == RoundState::AwaitingQBlock {
                        RoundState::StopMining
                    } else {
                        state
                    }),
                ),
            ] {
                assert_eq!(state.transition(event), expected, "{state:?} on {event:?}");
            }
        }
        // `StartMining` keeps mining until a new head, and `AwaitingQBlock`
        // holds until `Resume`.
        for state in [RoundState::StartMining, RoundState::AwaitingQBlock] {
            assert_eq!(
                state.transition(RoundEvent::Succeeded),
                Some(state),
                "{state:?}"
            );
        }
    }
}
