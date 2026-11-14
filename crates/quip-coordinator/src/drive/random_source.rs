//! Deterministic golden-draw problems over a topology spec.

use crate::drive::JobSource;
use crate::producer::derive_pow_job;
use crate::types::snapshot::MiningSnapshot;
use crate::types::topology_spec::TopologySpec;
use quip_proto::v1::Job;

/// Draws `count` problems over a topology via the golden `draw_ising_milli`
/// path (the same code the network uses), keyed by `seed`: the same
/// `(seed, topology)` pair always yields the same sequence of jobs.
pub struct RandomSource {
    snapshot: MiningSnapshot,
    miner_account: [u8; 32],
    seed: u64,
    deadline_ms: u64,
    count: u32,
    next_index: u32,
}

impl RandomSource {
    /// Build a source that yields `count` deterministic jobs for `(spec, seed)`.
    #[must_use]
    pub fn new(
        spec: &TopologySpec,
        miner_account: [u8; 32],
        seed: u64,
        count: u32,
        deadline_ms: u64,
    ) -> Self {
        Self {
            snapshot: spec.to_snapshot(),
            miner_account,
            seed,
            deadline_ms,
            count,
            next_index: 0,
        }
    }

    /// Per-index salt: `blake3(seed_le || index_le)`, so a given
    /// `(seed, index)` always derives the same nonce/problem.
    fn salt_for(&self, index: u32) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        let _ = h.update(&self.seed.to_le_bytes());
        let _ = h.update(&index.to_le_bytes());
        *h.finalize().as_bytes()
    }
}

impl JobSource for RandomSource {
    fn next_job(&mut self) -> Option<Job> {
        if self.next_index >= self.count {
            return None;
        }
        let salt = self.salt_for(self.next_index);
        let generation = u64::from(self.next_index) + 1;
        let job = match derive_pow_job(
            &self.snapshot,
            self.miner_account,
            salt,
            generation,
            self.deadline_ms,
        ) {
            Ok(job) => job,
            Err(e) => {
                tracing::error!(error = %e, "random source: cannot draw job from snapshot");
                return None;
            }
        };
        self.next_index += 1;
        Some(job)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::drain_all;
    use crate::types::topology_spec::{parse_topology_spec, preset_spec};

    const SPEC: &str = r#"{
        "nodes": [0, 1, 2, 3],
        "edges": [[0, 1], [1, 2], [2, 3], [0, 3]],
        "allowed_h_milli": [-1000, 0, 1000],
        "allowed_j_milli": [-1000, 1000]
    }"#;

    #[test]
    fn same_seed_and_topology_yields_identical_jobs_and_another_seed_differs() {
        let spec = parse_topology_spec(SPEC).unwrap();
        let draw = |seed| drain_all(&mut RandomSource::new(&spec, [1u8; 32], seed, 3, 9_999_999));
        let jobs_a = draw(7);
        let jobs_b = draw(7);
        assert_eq!(jobs_a.len(), jobs_b.len());
        for (ja, jb) in jobs_a.iter().zip(&jobs_b) {
            assert_eq!(ja.job_id, jb.job_id);
            assert_eq!(
                crate::types::problem::problem_milli(ja.ising.as_ref().unwrap()).unwrap(),
                crate::types::problem::problem_milli(jb.ising.as_ref().unwrap()).unwrap()
            );
        }
        let jobs_c = draw(8);
        assert_ne!(
            jobs_a.first().map(|j| &j.job_id),
            jobs_c.first().map(|j| &j.job_id)
        );
    }

    #[test]
    fn draws_exactly_count_jobs_with_values_in_allowed_sets() {
        let spec = parse_topology_spec(SPEC).unwrap();
        let mut src = RandomSource::new(&spec, [1u8; 32], 99, 5, 9_999_999);
        let jobs = drain_all(&mut src);
        assert_eq!(jobs.len(), 5);
        for job in jobs {
            let ising = job.ising.unwrap();
            let (h, j) = crate::types::problem::problem_milli(&ising).unwrap();
            assert!(h.iter().all(|v| [-1000, 0, 1000].contains(v)));
            assert!(j.iter().all(|v| [-1000, 1000].contains(v)));
        }
    }

    #[test]
    fn aglais_random_jobs_have_zero_fields_and_the_chain_edge_count() {
        use crate::types::problem::problem_milli;

        let spec = parse_topology_spec(preset_spec("aglais").expect("Aglais preset resolves"))
            .expect("Aglais preset parses");
        let mut source = RandomSource::new(&spec, [0; 32], 7, 2, 0);
        let jobs = drain_all(&mut source);
        assert_eq!(jobs.len(), 2);
        for job in jobs {
            let problem = job.ising.as_ref().expect("job has an Ising problem");
            let (h, j) = problem_milli(problem).expect("problem uses milli coefficients");
            assert_eq!(h, vec![0; 4_577]);
            assert_eq!(j.len(), 41_514);
            assert!(j.iter().all(|value| [-1000, 1000].contains(value)));
            assert_eq!(
                problem.graph,
                Some(quip_proto::v1::ising_problem::Graph::TopologyHash(
                    spec.topology.hash.clone()
                ))
            );
        }
    }
}
