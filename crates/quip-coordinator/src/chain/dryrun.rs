//! Optional `system_dryRun` diagnostics.
//!
//! Submit outcome comes from chain state, which says whether a proof landed but
//! not why it did not. A dry run returns `ApplyExtrinsicResult`, a small closed
//! type that decodes without runtime metadata, so it can name the failing
//! pallet error.
//!
//! `system_dryRun` is an unsafe RPC method. A node started with
//! `--rpc-methods=safe`, which is what this project deploys, denies it. This
//! module is therefore probed at runtime and contributes to log text only.
//!
//! Nothing in this module may influence a `SubmitAction` or any other
//! control-flow decision. It produces log text only.

use super::events::{decode_dispatch_error as decode_inner, DispatchError, ModuleError};

/// Decode a `system_dryRun` result into a module error, when it is one.
///
/// The encoding is `Result<Result<(), DispatchError>, TransactionValidityError>`.
/// Returns `None` for a success, for a non-module error, and for any blob this
/// build does not recognise. Diagnostics must never fail loudly.
#[must_use]
pub fn decode_dispatch_error(bytes: &[u8]) -> Option<ModuleError> {
    // Outer Result: 0x00 is Ok. Anything else is a validity error, which
    // carries no pallet error to report.
    if bytes.first().copied()? != 0x00 {
        return None;
    }
    // Inner Result: 0x00 is a successful dispatch.
    if bytes.get(1).copied()? != 0x01 {
        return None;
    }
    match decode_inner(bytes.get(2..)?)? {
        DispatchError::Module(module) => Some(module),
        DispatchError::Other(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ApplyExtrinsicResult` is `Result<Result<(), DispatchError>, _>`. The outer
    /// Ok is 0x00, the inner Err is 0x01, then `DispatchError::Module` is
    /// variant 3 followed by the pallet index and a four-byte error index.
    /// A successful dry run (outer Ok, inner Ok) has no error, and a truncated
    /// blob yields none rather than a panic.
    #[test]
    fn a_module_error_is_decoded_from_the_documented_layout() {
        let module = Some(ModuleError {
            pallet: 10,
            error: [7, 0, 0, 0],
        });
        for (label, raw, expected) in [
            (
                "module error",
                &[0x00, 0x01, 0x03, 10, 7, 0, 0, 0][..],
                module,
            ),
            ("successful dry run", &[0x00, 0x00][..], None),
            ("truncated module error", &[0x00, 0x01, 0x03, 10][..], None),
            ("empty", &[][..], None),
        ] {
            assert_eq!(decode_dispatch_error(raw), expected, "{label}");
        }
    }
}
