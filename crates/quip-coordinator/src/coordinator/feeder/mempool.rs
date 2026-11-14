//! Mempool staging: each open order once, to one capable miner.

use crate::coordinator::producer::job_order_to_job;
use crate::coordinator::scoring::validate::QualityGates;
use crate::coordinator::state::CoordinatorState;
use crate::coordinator::WorkSource;
use crate::types::order::JobOrder;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Validation gates for results on `order`.
///
/// The pallet keeps a row when `energy <= min_energy_milli`, but
/// [`crate::coordinator::scoring::validate::validate_result`] admits only `energy < min_energy_milli`,
/// so the ceiling moves up one milli. An order without `min_solutions` keeps
/// every submitted row; asking for two keeps a diversity gate reachable, since
/// a single row scores zero diversity.
fn order_gates(order: &JobOrder) -> QualityGates {
    let min_diversity_milli = order.min_diversity_milli.unwrap_or(0);
    QualityGates {
        min_energy_milli: order
            .min_energy_milli
            .map_or(i64::MAX, |e| e.saturating_add(1)),
        min_diversity_milli,
        min_solutions: order
            .min_solutions
            .unwrap_or(if min_diversity_milli > 0 { 2 } else { 1 }),
    }
}

/// Stage each open mempool order this process has not staged before.
///
/// Every order goes to one capable miner as a generation-0 job, which a round
/// turnover never cancels. Its gates are remembered by order id for the session
/// that validates the result.
///
/// Returns whether the orders were read. With no miner connected, nothing is
/// read: routing would drop every order for good.
pub(super) async fn stage_mempool_orders<W: WorkSource + ?Sized>(
    work: &W,
    state: &Arc<Mutex<CoordinatorState>>,
) -> bool {
    if state.lock().await.router.miner_ids().is_empty() {
        return false;
    }
    let orders = match work.mempool_orders().await {
        Ok(orders) => orders,
        Err(e) => {
            tracing::debug!(error = %e, "feeder: mempool order read failed");
            return false;
        }
    };
    let mut st = state.lock().await;
    for order in orders {
        if st.mempool_orders.contains_key(&order.order_id) {
            continue;
        }
        let _ = st
            .mempool_orders
            .insert(order.order_id.clone(), order_gates(&order));
        let id = crate::types::hex::hex_encode(&order.order_id);
        let Some(job) = job_order_to_job(&order) else {
            tracing::warn!(
                order = %id,
                nodes = order.nodes.len(),
                edges = order.edges.len(),
                "feeder: mempool order's node list cannot index its own edges; skipping it"
            );
            continue;
        };
        if let Some(miner) = st.router.route(job) {
            tracing::info!(order = %id, miner = %miner, nodes = order.nodes.len(), "feeder: staged mempool order");
            st.sessions.wake_dispatcher(&miner);
        } else {
            tracing::warn!(
                order = %id,
                nodes = order.nodes.len(),
                edges = order.edges.len(),
                "feeder: no connected miner can take this mempool order; skipping it"
            );
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::order_gates;
    use crate::types::order::JobOrder;

    fn order(min_energy: Option<i64>, min_diversity: Option<u32>) -> JobOrder {
        JobOrder {
            order_id: 5u64.to_le_bytes().to_vec(),
            mode: crate::types::order::JobMode::Open,
            nodes: vec![0, 1],
            edges: vec![(0, 1)],
            h_milli: vec![0, 0],
            j_milli: vec![1000],
            min_energy_milli: min_energy,
            min_diversity_milli: min_diversity,
            min_solutions: None,
            deadline_ms: 0,
        }
    }

    /// The pallet keeps `energy == min_energy`; the local gate is strict.
    #[test]
    fn order_gates_admit_the_pallets_inclusive_energy_bound() {
        let gates = order_gates(&order(Some(-1000), None));
        assert_eq!(gates.min_energy_milli, -999);
        assert_eq!(gates.min_solutions, 1);
        assert_eq!(order_gates(&order(None, None)).min_energy_milli, i64::MAX);
    }

    /// One row scores zero diversity, so an order with a diversity gate and
    /// no `min_solutions` needs at least two rows to pass.
    #[test]
    fn order_gates_ask_for_two_rows_when_diversity_is_gated() {
        let gates = order_gates(&order(None, Some(300)));
        assert_eq!(gates.min_diversity_milli, 300);
        assert_eq!(gates.min_solutions, 2);
    }
}
