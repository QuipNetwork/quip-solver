//! Lease staging: keep each lease-capable miner two leases deep, each sized
//! from the miner's measured salt rate.

use super::Draw;
use crate::coordinator::scoring::lease::{
    build_lease_job, lease_generator, lease_salt_count, LEASE_STAGE_DEPTH,
};
use crate::coordinator::state::CoordinatorState;

/// Stage leases for `id` until it holds [`LEASE_STAGE_DEPTH`], each sized to
/// the miner's `rate` in salts per second and its `stream_width`. Returns the
/// window.
pub(super) fn stage_leases(
    st: &mut CoordinatorState,
    id: &str,
    stream_width: u32,
    rate: f64,
    draw: &Draw<'_>,
    salt_ctr: &mut u64,
) -> usize {
    let count = lease_salt_count(rate, stream_width);
    while st.router.staged_len(id) < LEASE_STAGE_DEPTH {
        let start = salt_ctr.saturating_add(1);
        let job = build_lease_job(
            lease_generator(draw.snap.family.as_ref()),
            &draw.snap.topology_hash,
            draw.snap.last_proof_block_hash,
            draw.miner_identity,
            start,
            count,
            draw.generation,
        );
        if !st.router.stage_on(id, job) {
            break;
        }
        tracing::info!(
            miner = %id,
            generation = draw.generation,
            salt_start = start,
            salt_count = count,
            salts_per_sec = rate,
            "lease issued"
        );
        *salt_ctr = start.saturating_add(count - 1);
    }
    LEASE_STAGE_DEPTH
}
