//! Find one extrinsic's dispatch error in a block's `System.Events`.
//!
//! A full event decode needs runtime metadata, which this client does not
//! carry: the events of other pallets ahead of ours have layouts it cannot
//! skip. The record it needs has a fixed prefix, though. `EventRecord` starts
//! with its phase, and the outcome event of extrinsic `i` is
//!
//! ```text
//! 00 <i: u32 LE>   Phase::ApplyExtrinsic(i)
//! 00               RuntimeEvent::System
//! 00 | 01          ExtrinsicSuccess | ExtrinsicFailed
//! <DispatchError>  only for ExtrinsicFailed
//! ```
//!
//! [`extrinsic_outcome`] searches the raw storage value for those seven
//! bytes. The phase pins the extrinsic, so a match elsewhere would need the
//! same seven bytes inside an unrelated event body. A blob that holds more
//! than one candidate, or both outcomes, is reported as
//! [`ExtrinsicOutcome::Unknown`] rather than guessed.
//!
//! The pallet error names below are mirrored from quip-validator. The SCALE
//! index of an error is its declaration order, and that order changed between
//! runtime 119 and runtime 120, so the name table is chosen by
//! `spec_version`.

use super::extrinsic::twox128;
use super::scale_types::{MINER_REGISTRY_PALLET_INDEX, QUANTUM_POW_PALLET_INDEX};

/// Runtime index of `frame_system`.
const SYSTEM_PALLET_INDEX: u8 = 0;
/// `frame_system::Event::ExtrinsicSuccess`.
const EXTRINSIC_SUCCESS: u8 = 0;
/// `frame_system::Event::ExtrinsicFailed`.
const EXTRINSIC_FAILED: u8 = 1;
/// `Phase::ApplyExtrinsic`.
const PHASE_APPLY_EXTRINSIC: u8 = 0;
/// SCALE variant index of `DispatchError::Module`.
const DISPATCH_ERROR_MODULE: u8 = 3;

/// Storage key of `System.Events`.
#[must_use]
pub fn system_events_storage_key() -> Vec<u8> {
    let mut key = Vec::with_capacity(32);
    key.extend_from_slice(&twox128(b"System"));
    key.extend_from_slice(&twox128(b"Events"));
    key
}

/// A dispatch error localised to one pallet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleError {
    /// Runtime index of the pallet that raised the error.
    pub pallet: u8,
    /// Pallet-local error index, little-endian in a four-byte field.
    pub error: [u8; 4],
}

/// The `sp_runtime::DispatchError` of a failed extrinsic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchError {
    /// A pallet error.
    Module(ModuleError),
    /// Any other variant, by name.
    Other(&'static str),
}

/// What `System.Events` says about one extrinsic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtrinsicOutcome {
    /// `ExtrinsicSuccess` was recorded.
    Succeeded,
    /// `ExtrinsicFailed` was recorded with this error.
    Failed(DispatchError),
    /// No single unambiguous outcome record was found.
    Unknown,
}

/// `sp_runtime::DispatchError` variant names, in SCALE index order.
const DISPATCH_ERROR_VARIANTS: [&str; 15] = [
    "Other",
    "CannotLookup",
    "BadOrigin",
    "Module",
    "ConsumerRemaining",
    "NoProviders",
    "TooManyConsumers",
    "Token",
    "Arithmetic",
    "Transactional",
    "Exhausted",
    "Corruption",
    "Unavailable",
    "RootNotAllowed",
    "Trie",
];

/// Read the outcome of extrinsic `index` from a raw `System.Events` value.
#[must_use]
pub fn extrinsic_outcome(events: &[u8], index: u32) -> ExtrinsicOutcome {
    let mut prefix = [0u8; 7];
    prefix[0] = PHASE_APPLY_EXTRINSIC;
    prefix[1..5].copy_from_slice(&index.to_le_bytes());
    prefix[5] = SYSTEM_PALLET_INDEX;

    prefix[6] = EXTRINSIC_SUCCESS;
    let successes = positions(events, &prefix);
    prefix[6] = EXTRINSIC_FAILED;
    let failures = positions(events, &prefix);

    match (successes.as_slice(), failures.as_slice()) {
        ([_], []) => ExtrinsicOutcome::Succeeded,
        ([], [at]) => events
            .get(at + prefix.len()..)
            .and_then(decode_dispatch_error)
            .map_or(ExtrinsicOutcome::Unknown, ExtrinsicOutcome::Failed),
        _ => ExtrinsicOutcome::Unknown,
    }
}

/// Every offset at which `needle` starts in `haystack`.
fn positions(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    haystack
        .windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle)
        .map(|(i, _)| i)
        .collect()
}

/// Decode the `DispatchError` at the start of `bytes`.
pub(crate) fn decode_dispatch_error(bytes: &[u8]) -> Option<DispatchError> {
    let variant = bytes.first().copied()?;
    if variant == DISPATCH_ERROR_MODULE {
        let pallet = bytes.get(1).copied()?;
        let mut error = [0u8; 4];
        error.copy_from_slice(bytes.get(2..6)?);
        return Some(DispatchError::Module(ModuleError { pallet, error }));
    }
    DISPATCH_ERROR_VARIANTS
        .get(usize::from(variant))
        .map(|name| DispatchError::Other(name))
}

/// `QuantumPow` error names through runtime 119 (quip-validator v0.4.0).
const QUANTUM_POW_ERRORS_119: [&str; 25] = [
    "MinerAlreadyRegistered",
    "MinerNotRegistered",
    "TopologyAlreadyRegistered",
    "TopologyNotRegistered",
    "InvalidCurve",
    "GraphTooSmall",
    "InvalidTopology",
    "ProofLimitReached",
    "InvalidNonce",
    "NoSolutionsSubmitted",
    "InvalidSpinValues",
    "SolutionLengthMismatch",
    "InsufficientEnergy",
    "InsufficientDiversity",
    "InsufficientSolutions",
    "ArithmeticOverflow",
    "EmptyAllowedValues",
    "EncodingTooWide",
    "PackedSolutionLengthMismatch",
    "InvalidEncodedSpin",
    "PackedSolutionTooLarge",
    "TopologyNotMineable",
    "TopologyIsDefault",
    "MineableTopologyConflict",
    "InvalidDiversityConfig",
];

/// `QuantumPow` error names from runtime 120 (quip-validator v0.4.1 through
/// v0.5.0-rc2, which only appended to the list).
const QUANTUM_POW_ERRORS_120: [&str; 34] = [
    "MinerAlreadyRegistered",
    "BlockRewardOutOfBounds",
    "TargetProofBlocksOutOfBounds",
    "MinerNotRegistered",
    "TopologyAlreadyRegistered",
    "TopologyNotRegistered",
    "InvalidCurve",
    "GraphTooSmall",
    "InvalidTopology",
    "ProofLimitReached",
    "InvalidNonce",
    "NoSolutionsSubmitted",
    "InvalidSpinValues",
    "SolutionLengthMismatch",
    "InsufficientEnergy",
    "InsufficientDiversity",
    "InsufficientSolutions",
    "ArithmeticOverflow",
    "EmptyAllowedValues",
    "EncodingTooWide",
    "PackedSolutionLengthMismatch",
    "InvalidEncodedSpin",
    "PackedSolutionTooLarge",
    "TopologyNotMineable",
    "TopologyIsDefault",
    "MineableTopologyConflict",
    "InvalidDiversityConfig",
    "FamilyRejected",
    "FamilySpinSetRequired",
    "InvalidEnergyBand",
    "EnergyBandMissing",
    "FamilyNonceSkipped",
    "FamilyWitnessInvalid",
    "FamilyCodecInvalid",
];

/// `MinerRegistry` error names (quip-validator v0.5.0-rc2).
const MINER_REGISTRY_ERRORS: [&str; 22] = [
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
    "DescriptorNotFound",
    "DescriptorRequired",
    "InvalidQBlockId",
    "DuplicateParticipation",
];

/// Render a module error raised under runtime `spec_version`.
///
/// A known error renders as `Pallet::Name`, the form
/// [`classify_receipt`](super::submit::classify_receipt) matches. An unknown
/// pallet or index renders with its numbers, which match no name.
#[must_use]
pub fn describe_module_error(e: &ModuleError, spec_version: u32) -> String {
    let idx = u32::from_le_bytes(e.error);
    let (pallet_name, table): (&str, &[&str]) = match e.pallet {
        QUANTUM_POW_PALLET_INDEX if spec_version >= 120 => ("QuantumPow", &QUANTUM_POW_ERRORS_120),
        QUANTUM_POW_PALLET_INDEX => ("QuantumPow", &QUANTUM_POW_ERRORS_119),
        MINER_REGISTRY_PALLET_INDEX => ("MinerRegistry", &MINER_REGISTRY_ERRORS),
        other => return format!("pallet {other} error {idx} (no name table for this pallet)"),
    };
    match usize::try_from(idx).ok().and_then(|i| table.get(i)) {
        Some(name) => format!("{pallet_name}::{name}"),
        None => format!("{pallet_name} error {idx} (index beyond the known table)"),
    }
}

/// Render a dispatch error raised under runtime `spec_version`.
#[must_use]
pub fn describe_dispatch_error(e: &DispatchError, spec_version: u32) -> String {
    match e {
        DispatchError::Module(m) => describe_module_error(m, spec_version),
        DispatchError::Other(name) => format!("DispatchError::{name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `System.Events` value of block 22 on a runtime-120 dev chain. It
    /// holds the inherent's events at index 0 and a `submit_proof` at index 1
    /// that failed with `QuantumPow` error 10.
    const BLOCK_22_EVENTS: &str = concat!(
        "2c020407a3f7b863eb4b7fa3934405ecd0929b8046ddbc96891b8ae1762513093b441d524b7094789802000000000000",
        "00000000000204114b70947898020000000000000000000000021300000000004b709478980200000000000000000000",
        "00020407737b254848a98fc63acbcbfa7e5026293ff9119ee2c2d45fdd7d9df7c23b2cba4b7094789802000000000000",
        "00000000000204114b70947898020000000000000000000000021300010000004b709478980200000000000000000000",
        "0000000000000000c2e4f96f551702000000010000000408d724cd9a10029903f3978cadbf348587d086f8109d38da83",
        "5723ce0e090aedbd8080640e260000000000000000000000000001000000040d8080640e260000000000000000000000",
        "0000010000000500d724cd9a10029903f3978cadbf348587d086f8109d38da835723ce0e090aedbd8080640e26000000",
        "0000000000000000000000000000000000000000000000000000010000000001030a0a000000073377640e2682d94100",
        "000000",
    );

    fn record(index: u32, outcome: &[u8]) -> Vec<u8> {
        let mut out = vec![PHASE_APPLY_EXTRINSIC];
        out.extend(index.to_le_bytes());
        out.push(SYSTEM_PALLET_INDEX);
        out.extend(outcome);
        out
    }

    #[test]
    fn a_failed_proof_names_invalid_nonce_on_runtime_120() {
        let events = crate::chain::extrinsic::hex_decode(BLOCK_22_EVENTS).unwrap();
        let outcome = extrinsic_outcome(&events, 1);
        let ExtrinsicOutcome::Failed(error) = outcome else {
            panic!("expected a failure, got {outcome:?}");
        };
        assert_eq!(
            describe_dispatch_error(&error, 120),
            "QuantumPow::InvalidNonce"
        );
        assert_eq!(
            crate::chain::submit::classify_receipt(Some(&describe_dispatch_error(&error, 120))),
            crate::chain::SubmitAction::StopRoundStale
        );
    }

    #[test]
    fn the_events_key_is_the_well_known_one() {
        assert_eq!(
            crate::chain::extrinsic::hex_encode(&system_events_storage_key()),
            "0x26aa394eea5630e07c48ae0c9558cef780d41e5e16056765bc8461851072c9d7"
        );
    }

    #[test]
    fn outcome_is_read_for_the_named_extrinsic_only() {
        let mut events = record(0, &[EXTRINSIC_SUCCESS, 0xAA]);
        events.extend(record(2, &[EXTRINSIC_FAILED, 0x02]));
        assert_eq!(extrinsic_outcome(&events, 0), ExtrinsicOutcome::Succeeded);
        assert_eq!(
            extrinsic_outcome(&events, 2),
            ExtrinsicOutcome::Failed(DispatchError::Other("BadOrigin"))
        );
        assert_eq!(extrinsic_outcome(&events, 1), ExtrinsicOutcome::Unknown);
    }

    #[test]
    fn an_ambiguous_or_truncated_record_is_unknown() {
        let mut both = record(3, &[EXTRINSIC_SUCCESS]);
        both.extend(record(3, &[EXTRINSIC_FAILED, 0x02]));
        assert_eq!(extrinsic_outcome(&both, 3), ExtrinsicOutcome::Unknown);

        let truncated = record(3, &[EXTRINSIC_FAILED, DISPATCH_ERROR_MODULE, 10, 1]);
        assert_eq!(extrinsic_outcome(&truncated, 3), ExtrinsicOutcome::Unknown);
        assert_eq!(extrinsic_outcome(&[], 3), ExtrinsicOutcome::Unknown);
    }

    /// The same index names different errors before and after runtime 120,
    /// which inserted two variants near the top of the enum.
    #[test]
    fn quantum_pow_names_follow_the_runtime_version() {
        let invalid_nonce_120 = ModuleError {
            pallet: QUANTUM_POW_PALLET_INDEX,
            error: [10, 0, 0, 0],
        };
        assert_eq!(
            describe_module_error(&invalid_nonce_120, 120),
            "QuantumPow::InvalidNonce"
        );
        assert_eq!(
            describe_module_error(&invalid_nonce_120, 119),
            "QuantumPow::InvalidSpinValues"
        );
        let invalid_nonce_119 = ModuleError {
            pallet: QUANTUM_POW_PALLET_INDEX,
            error: [8, 0, 0, 0],
        };
        assert_eq!(
            describe_module_error(&invalid_nonce_119, 119),
            "QuantumPow::InvalidNonce"
        );
    }

    #[test]
    fn unknown_errors_are_still_readable() {
        let unknown = describe_module_error(
            &ModuleError {
                pallet: 99,
                error: [200, 0, 0, 0],
            },
            120,
        );
        assert!(unknown.contains("99"), "{unknown}");
        assert!(unknown.contains("200"), "{unknown}");
        let beyond = describe_module_error(
            &ModuleError {
                pallet: QUANTUM_POW_PALLET_INDEX,
                error: [200, 0, 0, 0],
            },
            120,
        );
        assert!(beyond.contains("200"), "{beyond}");
    }
}
