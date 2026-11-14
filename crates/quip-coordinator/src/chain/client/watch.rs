//! Decoding `author_submitAndWatchExtrinsic` notifications.
//!
//! The status stream reports transport and pool progress only. It says whether
//! the extrinsic reached a block. It does not say whether the dispatch inside
//! that block succeeded, because that lives in the block's events. The outcome
//! module answers that question from chain state instead.

/// One notification from the transaction status subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxStatus {
    /// Accepted into the pool and ready for inclusion.
    Ready,
    /// Sent to peers.
    Broadcast,
    /// Included in the block with this hash.
    InBlock(String),
    /// Included in a finalized block with this hash.
    Finalized(String),
    /// Removed from the pool without inclusion. Worth resubmitting.
    Dropped(String),
    /// Rejected by the pool. Resubmitting the same bytes cannot help.
    Invalid(String),
    /// Held for a future nonce.
    Future,
    /// A status this build does not model, reported verbatim.
    Other(String),
}

/// Decode one status notification.
///
/// Unknown shapes become [`TxStatus::Other`] rather than an error, so a node
/// that adds a status does not break the submit loop.
#[must_use]
pub fn parse_tx_status(v: &serde_json::Value) -> TxStatus {
    if let Some(s) = v.as_str() {
        return match s.to_ascii_lowercase().as_str() {
            "ready" => TxStatus::Ready,
            "broadcast" => TxStatus::Broadcast,
            "future" => TxStatus::Future,
            _ => TxStatus::Other(s.to_string()),
        };
    }
    let Some(obj) = v.as_object() else {
        return TxStatus::Other(v.to_string());
    };
    for (k, val) in obj {
        let hash = val.as_str().unwrap_or_default().to_string();
        match k.to_ascii_lowercase().as_str() {
            "inblock" => return TxStatus::InBlock(hash),
            "finalized" => return TxStatus::Finalized(hash),
            // Usurped and retracted both mean this extrinsic is no longer on
            // track for inclusion, and both are worth another attempt.
            "dropped" | "usurped" | "retracted" | "finalitytimeout" => {
                return TxStatus::Dropped(format!("{k}: {hash}"))
            }
            "invalid" => return TxStatus::Invalid(format!("{k}: {hash}")),
            "broadcast" => return TxStatus::Broadcast,
            _ => {}
        }
    }
    TxStatus::Other(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every node status shape maps to its own variant. An unknown status must
    /// not be read as success: it is reported verbatim so an operator can see
    /// what the node actually said.
    #[test]
    fn node_status_shapes_map_to_their_variants() {
        type Check = fn(&TxStatus) -> bool;
        let cases: [(&str, serde_json::Value, Check); 10] = [
            ("ready", json!("ready"), |s| *s == TxStatus::Ready),
            ("broadcast", json!("broadcast"), |s| {
                *s == TxStatus::Broadcast
            }),
            ("future", json!("future"), |s| *s == TxStatus::Future),
            // Substrate has used both camelCase and lowercase over time. Accept
            // both so a node upgrade does not silently stop reporting inclusion.
            ("inBlock camelCase", json!({"inBlock": "0xabc"}), |s| {
                *s == TxStatus::InBlock("0xabc".into())
            }),
            ("inblock lowercase", json!({"inblock": "0xabc"}), |s| {
                *s == TxStatus::InBlock("0xabc".into())
            }),
            (
                "finalized carries its block hash",
                json!({"finalized": "0xdef"}),
                |s| *s == TxStatus::Finalized("0xdef".into()),
            ),
            ("dropped", json!({"dropped": null}), |s| {
                matches!(s, TxStatus::Dropped(_))
            }),
            ("invalid", json!({"invalid": null}), |s| {
                matches!(s, TxStatus::Invalid(_))
            }),
            (
                "usurped counts as dropped",
                json!({"usurped": "0x01"}),
                |s| matches!(s, TxStatus::Dropped(_)),
            ),
            (
                "unknown status",
                json!({"somethingNew": "0x02"}),
                |s| matches!(s, TxStatus::Other(t) if t.contains("somethingNew")),
            ),
        ];
        for (label, input, check) in cases {
            let status = parse_tx_status(&input);
            assert!(check(&status), "{label}: got {status:?}");
        }
    }
}
