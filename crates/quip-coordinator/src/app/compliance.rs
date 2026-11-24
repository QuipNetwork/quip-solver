//! Opt-in id.quip.network compliance gate for mempool orders.
//!
//! With `[miner].quip_id_url` and `QUIP_ID_API_KEY` set, only orders whose
//! proposer id.quip.network allows are staged. With neither, every order is.

use std::sync::Arc;

use futures::StreamExt as _;

use crate::types::order::JobOrder;

const CONCURRENT_DECISIONS: usize = 16;

fn present(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// The gate's client, or `None` when the gate is off.
///
/// # Errors
/// When only one of the URL and the key is set, the key is refused, or the
/// provider is not ready. An unreachable id.quip.network still opens the gate;
/// orders are skipped until it answers.
pub async fn open_gate(
    url: Option<&str>,
    api_key: Option<&str>,
) -> Result<Option<Arc<quip_id::Client>>, String> {
    let (url, api_key) = match (present(url), present(api_key)) {
        (None, None) => return Ok(None),
        (Some(url), Some(api_key)) => (url, api_key),
        _ => {
            return Err(format!(
                "[miner].quip_id_url and {} must be set together",
                quip_id::API_KEY_VARIABLE
            ))
        }
    };
    let client = quip_id::Client::new(url, api_key).map_err(|e| e.to_string())?;
    match client.whoami_at_startup().await {
        quip_id::Startup::Ok(who) => {
            tracing::info!(%url, provider = %who.provider_name, "compliance gate enabled");
        }
        quip_id::Startup::Unreachable(detail) => {
            tracing::warn!(%url, %detail, "compliance gate enabled; id.quip.network unreachable");
        }
        refused => {
            return Err(format!(
                "id.quip.network refused the compliance gate: {refused:?}"
            ))
        }
    }
    Ok(Some(Arc::new(client)))
}

/// The chain's order number: the order id's little-endian `u64`, or 0.
#[must_use]
pub fn order_number(order: &JobOrder) -> u64 {
    order
        .order_id
        .get(..8)
        .and_then(|bytes| bytes.try_into().ok())
        .map_or(0, u64::from_le_bytes)
}

/// The orders id.quip.network allows, in their original order. Anything else,
/// including an unanswered request, keeps the order out.
pub async fn admit(oracle: &quip_id::Client, orders: Vec<JobOrder>) -> Vec<JobOrder> {
    futures::stream::iter(orders)
        .map(|order| async move {
            let allowed = oracle.allowed(order.proposer, order_number(&order)).await;
            if !allowed {
                tracing::debug!(
                    order = order_number(&order),
                    "mempool order not allowed; skipping"
                );
            }
            allowed.then_some(order)
        })
        .buffered(CONCURRENT_DECISIONS)
        .filter_map(|kept| async move { kept })
        .collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order_with_id(order_id: Vec<u8>) -> JobOrder {
        JobOrder {
            order_id,
            mode: crate::types::order::JobMode::Open,
            nodes: vec![],
            edges: vec![],
            h_milli: vec![],
            j_milli: vec![],
            min_energy_milli: None,
            min_diversity_milli: None,
            min_solutions: None,
            deadline_ms: 0,
            proposer: [0; 32],
        }
    }

    #[test]
    fn order_number_reads_the_chains_little_endian_id() {
        assert_eq!(
            order_number(&order_with_id(4242u64.to_le_bytes().to_vec())),
            4242
        );
    }

    #[test]
    fn order_number_of_a_short_id_is_zero() {
        assert_eq!(order_number(&order_with_id(vec![1, 2, 3])), 0);
    }
}
