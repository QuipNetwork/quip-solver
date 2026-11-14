//! Proof-of-work staging: fresh-salt jobs up to each miner's adaptive window.

use super::Draw;
use crate::coordinator::producer::derive_pow_job;
use crate::coordinator::state::CoordinatorState;
use crate::types::snapshot::MiningSnapshot;

/// Staging headroom over smoothed consumption: keep ~2 poll-intervals of drain
/// staged so a fast miner never idles waiting for the next top-up.
const WINDOW_HEADROOM: f64 = 2.0;

/// Adaptive staging depth for one miner from its smoothed drain rate. The
/// `buffer_depth` floor keeps a small reserve for idle/slow miners. `ema`
/// tracks dispatches per poll (`Router::take_consumed`): a job leaves the
/// staged queue at dispatch, so that is the drain signal the feeder sizes
/// against. The feeder additionally clamps the depth to [`stage_ceiling`] so a
/// fast miner on a large topology cannot stage an unbounded-memory reserve.
fn adaptive_depth(ema: f64, floor: usize) -> usize {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "EMA*headroom is non-negative and small for operational drain rates"
    )]
    {
        floor.max((ema * WINDOW_HEADROOM).ceil() as usize)
    }
}

/// Per-miner budget on staged-job memory. The adaptive window sizes to drain
/// rate with no ceiling of its own, so a fast miner on a large topology could
/// stage an unbounded reserve — each staged `PoW` job holds a full materialized
/// Ising model (`h`/`j`), ~`4·(nodes+edges)` bytes. This bounds the peak
/// staging memory per miner regardless of topology size or drain rate.
const MAX_STAGE_BYTES_PER_MINER: usize = 64 * 1024 * 1024; // 64 MiB

/// Estimated materialized-model bytes for one staged job on this topology: one
/// i32 field per node (`h`) and one per edge (`j`). The 32-byte `topology_hash`
/// and `job_id` are negligible next to these. Floored at 1 to avoid a zero
/// divisor on an empty topology.
fn est_job_bytes(num_nodes: usize, num_edges: usize) -> usize {
    4usize
        .saturating_mul(num_nodes.saturating_add(num_edges))
        .max(1)
}

/// Memory-aware ceiling on the staging window: the most jobs that fit the
/// per-miner byte budget, but never below `floor` — a miner must not idle for
/// lack of a staged job even on a topology where the floor alone exceeds the
/// budget.
fn stage_ceiling(num_nodes: usize, num_edges: usize, floor: usize) -> usize {
    floor.max(MAX_STAGE_BYTES_PER_MINER / est_job_bytes(num_nodes, num_edges))
}

/// A unique 32-byte salt from a monotonic counter: distinct salts derive
/// distinct nonces (job ids), so each attempt is a fresh `PoW` draw.
fn salt_from_counter(ctr: u64) -> [u8; 32] {
    let mut salt = [0u8; 32];
    salt[..8].copy_from_slice(&ctr.to_le_bytes());
    salt
}

/// The staging window for a miner with smoothed drain `ema`: the adaptive
/// depth over the `floor`, clamped to the memory ceiling of `snap`'s topology.
pub(super) fn window(ema: f64, floor: usize, snap: &MiningSnapshot) -> usize {
    adaptive_depth(ema, floor).min(stage_ceiling(snap.nodes.len(), snap.edges.len(), floor))
}

/// Stage fresh-salt jobs for `id` until it holds `depth`, or a job cannot be
/// drawn or staged.
pub(super) fn stage_pow(
    st: &mut CoordinatorState,
    id: &str,
    depth: usize,
    draw: &Draw<'_>,
    salt_ctr: &mut u64,
) {
    while st.router.staged_len(id) < depth {
        *salt_ctr = salt_ctr.saturating_add(1);
        let salt = salt_from_counter(*salt_ctr);
        let job = match derive_pow_job(draw.snap, draw.miner_identity, salt, draw.generation, 0) {
            Ok(job) => job,
            Err(e) => {
                // The snapshot's allowed-value sets are validated at
                // fetch (`require_set_values`), so an empty set here is
                // an invariant violation, not routine. Stop topping up
                // this miner rather than crash the feeder.
                tracing::error!(error = %e, miner = %id, "feeder: cannot draw PoW job");
                break;
            }
        };
        let job_id = job.job_id.clone();
        if st.router.stage_on(id, job) {
            st.record_salt(&job_id, salt);
        } else {
            break; // not capable for this shape — stop topping up
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{adaptive_depth, stage_ceiling, MAX_STAGE_BYTES_PER_MINER, WINDOW_HEADROOM};

    /// depth = ceil(ema * headroom), floor when that is smaller.
    #[test]
    fn adaptive_depth_is_the_ceiled_headroom_product_above_the_floor() {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "test fixture: exact integer headroom product"
        )]
        let headroom_10 = (10.0 * WINDOW_HEADROOM) as usize;
        let cases = [
            ("idle holds the floor", 0.0, 8, 8),
            ("idle with a zero floor", 0.0, 0, 0),
            ("grows with consumption", 10.0, 8, headroom_10),
            ("a many-core miner has no upper cap", 250.0, 8, 500),
            ("2.5 * 2.0 = 5.0 exactly", 2.5, 2, 5),
            ("0.8 ceils to 1, but floor 4 wins", 0.4, 4, 4),
        ];
        for (label, ema, floor, expected) in cases {
            assert_eq!(adaptive_depth(ema, floor), expected, "{label}");
        }
    }

    #[test]
    fn stage_ceiling_binds_only_on_large_topologies_and_never_below_the_floor() {
        // 2 nodes / 1 edge -> ~12 bytes/job -> the byte budget allows millions
        // of staged jobs, so the ceiling never clamps a real drain-driven depth.
        let cap = stage_ceiling(2, 1, 8);
        assert!(cap > 1_000_000, "dev topology must not be capped: {cap}");
        assert!(cap >= MAX_STAGE_BYTES_PER_MINER / 12);

        // Zephyr-scale: ~4577 nodes / 41515 edges -> ~184 KB/job. A fast miner
        // (ema 250 -> uncapped depth 500) is clamped to the byte budget.
        let cap = stage_ceiling(4577, 41515, 8);
        assert!(cap >= 8, "never below floor: {cap}");
        assert!(cap < 500, "large topology is memory-capped: {cap}");

        // Pathological topology where even one job exceeds the budget: the floor
        // still wins so the miner never idles for lack of a staged job.
        assert_eq!(stage_ceiling(usize::MAX / 8, 0, 8), 8);
    }
}
