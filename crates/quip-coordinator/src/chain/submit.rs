//! Extrinsic submit + hybrid sign (CONFIRM) and groundable receipt classification.

use crate::types::proof::SubmitAction;

/// Outcome of a `MinerRegistry.set_descriptor` submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptorOutcome {
    /// The descriptor landed (create or replace).
    Filed,
    /// The pallet rejected the payload. Do not retry.
    Rejected,
}

const DESCRIPTOR_REJECT: [&str; 20] = [
    "EmptyNodeId",
    "EmptyNodeName",
    "EmptyPublicHost",
    "EmptyRpcEndpoint",
    "EmptyMinerLabel",
    "EmptyMinerBackend",
    "EmptyMinerDeviceId",
    "EmptyOsSystem",
    "EmptyCpuBrand",
    "EmptyCpuArch",
    "EmptyGpuVendor",
    "EmptyGpuName",
    "InvalidGpuUtilization",
    "EmptyPythonVersion",
    "EmptyQuipVersion",
    "EmptyDockerImage",
    "NoMiners",
    "InvalidPort",
    "InsufficientBalance",
    "LiquidityRestrictions",
];

/// Classify a `set_descriptor` pallet error. `None` is a successful dispatch.
/// Unknown strings stay `None` so the caller can treat them as transient.
#[must_use]
pub fn classify_descriptor(error: Option<&str>) -> Option<DescriptorOutcome> {
    let Some(e) = error else {
        return Some(DescriptorOutcome::Filed);
    };
    if DESCRIPTOR_REJECT.iter().any(|s| e.contains(s)) {
        return Some(DescriptorOutcome::Rejected);
    }
    None
}

/// Outcome of a `MinerRegistry.participate` submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticipationOutcome {
    /// The declaration landed.
    Declared,
    /// This account already declared this qblock. Treat as success.
    AlreadyDeclared,
    /// The candidate qblock moved under the call.
    StaleQBlock,
    /// No descriptor is stored for this account.
    DescriptorMissing,
}

/// Classify a `participate` pallet error. `None` is a successful dispatch.
/// Unknown strings stay `None` so the caller can treat them as transient.
#[must_use]
pub fn classify_participation(error: Option<&str>) -> Option<ParticipationOutcome> {
    let Some(e) = error else {
        return Some(ParticipationOutcome::Declared);
    };
    if e.contains("DuplicateParticipation") {
        return Some(ParticipationOutcome::AlreadyDeclared);
    }
    if e.contains("InvalidQBlockId") {
        return Some(ParticipationOutcome::StaleQBlock);
    }
    if e.contains("DescriptorRequired") {
        return Some(ParticipationOutcome::DescriptorMissing);
    }
    None
}

/// Outcome of a `QuantumPow.register_miner` or
/// `QuantumComputeMempool.register_solver` submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationOutcome {
    /// The registration landed (for a miner, the deposit is reserved).
    Registered,
    /// This account was already registered. Treat as success.
    AlreadyRegistered,
}

/// Classify a `register_miner` or `register_solver` pallet error. `None` is a
/// successful dispatch.
///
/// Only the already-registered race is benign. Everything else — a deposit the
/// account cannot cover above all — stays `None` so the caller reports it and
/// retries rather than marking the miner ready to submit proofs it cannot land.
#[must_use]
pub fn classify_registration(error: Option<&str>) -> Option<RegistrationOutcome> {
    let Some(e) = error else {
        return Some(RegistrationOutcome::Registered);
    };
    if e.contains("MinerAlreadyRegistered") || e.contains("SolverAlreadyRegistered") {
        return Some(RegistrationOutcome::AlreadyRegistered);
    }
    None
}

/// Classify a pallet/dispatch error string into a fire-loop action.
///
/// Mirrors `substrate/submitter.py:_classify_receipt`. Unknown → fail loud.
#[must_use]
pub fn classify_receipt(error: Option<&str>) -> SubmitAction {
    const RETRY: [&str; 2] = ["InsufficientEnergy", "ProofLimitReached"];
    const STALE: [&str; 3] = ["InvalidNonce", "TopologyNotRegistered", "InvalidTopology"];
    const FATAL: [&str; 3] = [
        "InsufficientSolutions",
        "InsufficientDiversity",
        "MinerNotRegistered",
    ];
    let Some(e) = error else {
        return SubmitAction::Success;
    };
    if RETRY.iter().any(|s| e.contains(s)) {
        return SubmitAction::Retry;
    }
    if STALE.iter().any(|s| e.contains(s)) {
        return SubmitAction::StopRoundStale;
    }
    if FATAL.iter().any(|s| e.contains(s)) {
        return SubmitAction::StopFatal;
    }
    SubmitAction::StopFatal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_pallet_errors() {
        assert!(matches!(classify_receipt(None), SubmitAction::Success));
        assert!(matches!(
            classify_receipt(Some("InsufficientEnergy")),
            SubmitAction::Retry
        ));
        assert!(matches!(
            classify_receipt(Some("ProofLimitReached")),
            SubmitAction::Retry
        ));
        assert!(matches!(
            classify_receipt(Some("InvalidNonce")),
            SubmitAction::StopRoundStale
        ));
        assert!(matches!(
            classify_receipt(Some("TopologyNotRegistered")),
            SubmitAction::StopRoundStale
        ));
        assert!(matches!(
            classify_receipt(Some("InsufficientSolutions")),
            SubmitAction::StopFatal
        ));
        assert!(matches!(
            classify_receipt(Some("SomethingUnknown")),
            SubmitAction::StopFatal
        ));
    }

    #[test]
    fn classifies_descriptor_errors() {
        assert_eq!(classify_descriptor(None), Some(DescriptorOutcome::Filed));
        assert_eq!(
            classify_descriptor(Some("MinerRegistry: EmptyNodeName")),
            Some(DescriptorOutcome::Rejected)
        );
        assert_eq!(
            classify_descriptor(Some("NoMiners")),
            Some(DescriptorOutcome::Rejected)
        );
        assert_eq!(
            classify_descriptor(Some("InsufficientBalance")),
            Some(DescriptorOutcome::Rejected)
        );
        assert_eq!(classify_descriptor(Some("SomethingUnknown")), None);
    }

    #[test]
    fn classifies_registration_errors() {
        assert_eq!(
            classify_registration(None),
            Some(RegistrationOutcome::Registered)
        );
        assert_eq!(
            classify_registration(Some("QuantumPow: MinerAlreadyRegistered")),
            Some(RegistrationOutcome::AlreadyRegistered)
        );
        // A deposit the account cannot reserve must not read as success.
        assert_eq!(classify_registration(Some("InsufficientBalance")), None);
        assert_eq!(classify_registration(Some("SomethingUnknown")), None);
    }

    #[test]
    fn classifies_participation_errors() {
        assert_eq!(
            classify_participation(None),
            Some(ParticipationOutcome::Declared)
        );
        assert_eq!(
            classify_participation(Some("MinerRegistry: DuplicateParticipation")),
            Some(ParticipationOutcome::AlreadyDeclared)
        );
        assert_eq!(
            classify_participation(Some("InvalidQBlockId")),
            Some(ParticipationOutcome::StaleQBlock)
        );
        assert_eq!(
            classify_participation(Some("DescriptorRequired")),
            Some(ParticipationOutcome::DescriptorMissing)
        );
        assert_eq!(classify_participation(Some("SomethingUnknown")), None);
    }
}
