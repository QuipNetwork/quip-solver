//! `ValidatorAdmission` pallet and call indices.

/// `ValidatorAdmission` pallet index on runtime 120.
pub const VALIDATOR_ADMISSION_PALLET_INDEX: u8 = 22;

/// `ValidatorAdmission::dispatch_as_root` call index. Needs 2/3 of the
/// Foundation and dispatches its call with the Root origin.
pub const DISPATCH_AS_ROOT_CALL_INDEX: u8 = 3;
