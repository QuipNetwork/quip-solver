//! Salt leases: `ISING_GENERATE` jobs that let a miner draw its own
//! proof-of-work problems, and how many salts each lease names.

use crate::chain::snapshot::MiningSnapshot;
use crate::topology::Topology;
use ising_generators::descriptor::{admit, params_digest, AdvertisedLimits, Contract};
use ising_generators::family::{admit_topology, feasible, generate_instance, BaseTopology};
use ising_generators::random::derive_nonce;
use ising_generators::Code;
use quip_proto::v1::{
    FamilyDescriptor, GeneratorAlgorithm, IsingProblemGenerator, Job, JobKind, Provenance,
};
use quip_protocol::lease::{LeaseSpec, TopologyView, Verified, VerifyError};
use quip_protocol::scoring::energy_from_milli;
use quip_protocol::wire::decode_spins_packed;

/// Seconds of sampling one lease should hold. A lease completes only when its
/// slowest salt does, and a screening miner can hold one salt for tens of
/// seconds at full budget, so a lease must be long against that tail or the
/// miner idles while it waits for `LeaseDone` to refund the credit.
pub const LEASE_TARGET_SECS: f64 = 60.0;

/// Time constant, in seconds, of the window [`SaltRate`] averages over. It
/// spans several leases at [`LEASE_TARGET_SECS`], so one completion moves the
/// estimate by a fraction of a lease rather than a whole one.
pub const LEASE_RATE_WINDOW_SECS: f64 = 300.0;

/// Upper bound on salts in one lease. Salts are counter values, so this bounds
/// how far one lease can run past a rate estimate, not memory.
pub const LEASE_MAX_SALTS: u64 = 1 << 20;

/// Leases kept staged per miner. Credits bound what is dispatched, so two
/// staged leases keep a replacement ready when one completes.
pub const LEASE_STAGE_DEPTH: usize = 2;

/// Prefix of every lease `job_id`. A proof-of-work plain job id is a 32-byte
/// nonce, so the two id spaces cannot collide.
pub const LEASE_JOB_ID_PREFIX: &[u8] = b"lease:";

/// Assumed salts per second per pipeline slot before a miner's first lease
/// completes. A Metal screening miner declares 81 slots and runs about 6,000
/// salts per second, so its first lease already covers [`LEASE_TARGET_SECS`].
/// A small first lease would measure the cascade tail instead of the miner,
/// and the estimate would take several rounds to climb.
const INITIAL_SALTS_PER_SEC_PER_SLOT: f64 = 75.0;

/// Family revisions this build regenerates: every contract the generator crate
/// ships. A live challenge still takes the legacy draw until the chain binds a
/// family to it.
pub const FAMILY_CONTRACTS: &[Contract] = ising_generators::registry::CONTRACTS;

/// One problem family admitted for a challenge. The fields are private, so a
/// lease cannot change a descriptor that passed admission.
#[derive(Debug, Clone, PartialEq)]
pub struct ChallengeFamily {
    descriptor: FamilyDescriptor,
    topology_order_digest: [u8; 32],
}

impl ChallengeFamily {
    /// Commit to `params` and check the descriptor against `contracts` and the
    /// ordered `topology`.
    ///
    /// # Errors
    /// Returns the family code of the first failed admission check.
    pub fn admit(
        contracts: &[Contract],
        family_id: &str,
        revision: u32,
        params: Vec<u8>,
        topology: &Topology,
    ) -> Result<Self, Code> {
        let digest = params_digest(&params);
        let base = BaseTopology::new(topology.nodes.clone(), &topology.edge_pairs())
            .map_err(|_| Code::TopologyIncompatible)?;
        let admitted = admit(contracts, family_id.as_bytes(), revision, &params, &digest)?;
        let _ = admit_topology(
            &admitted,
            &base,
            base.order_digest(),
            &AdvertisedLimits::default(),
        )?;
        let topology_order_digest = *base.order_digest();
        Ok(Self {
            descriptor: FamilyDescriptor {
                family_id: family_id.to_owned(),
                revision,
                params,
                params_digest: digest.to_vec(),
            },
            topology_order_digest,
        })
    }

    /// The admitted descriptor.
    #[must_use]
    pub fn descriptor(&self) -> &FamilyDescriptor {
        &self.descriptor
    }

    /// Digest of the node and edge order the descriptor was admitted on.
    #[must_use]
    pub const fn topology_order_digest(&self) -> &[u8; 32] {
        &self.topology_order_digest
    }

    /// Bytes that identify this family binding in a challenge key.
    #[must_use]
    pub fn commitment(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        let _ = h.update(self.descriptor.family_id.as_bytes());
        let _ = h.update(&[0]);
        let _ = h.update(&self.descriptor.revision.to_le_bytes());
        let _ = h.update(&self.descriptor.params_digest);
        let _ = h.update(&self.topology_order_digest);
        *h.finalize().as_bytes()
    }
}

/// What lease verification needs to know about the session topology.
#[derive(Debug)]
pub struct LeaseTopology {
    view: TopologyView,
    base: BaseTopology,
    contracts: &'static [Contract],
}

impl LeaseTopology {
    /// Prepare `topology` once, so a result does not rebuild it.
    ///
    /// # Errors
    /// Returns a message when the topology cannot back lease verification.
    pub fn from_topology(
        topology: &Topology,
        contracts: &'static [Contract],
    ) -> Result<Self, String> {
        let view = TopologyView::from_proto(&topology.to_proto()).map_err(|e| format!("{e:?}"))?;
        let base = BaseTopology::new(topology.nodes.clone(), &topology.edge_pairs())
            .map_err(|e| format!("{e:?}"))?;
        Ok(Self {
            view,
            base,
            contracts,
        })
    }
}

/// Unique `job_id` for the lease whose first counter is `salt_start`. The feeder's
/// counter never repeats within a process, so neither does the id.
#[must_use]
pub fn lease_job_id(salt_start: u64) -> Vec<u8> {
    let mut id = LEASE_JOB_ID_PREFIX.to_vec();
    id.extend_from_slice(&salt_start.to_le_bytes());
    id
}

/// Whether `job` is a salt lease.
#[must_use]
pub fn is_lease(job: &Job) -> bool {
    job.kind == JobKind::IsingGenerate as i32
}

/// An `ISING_GENERATE` job naming counters `salt_start..salt_start + salt_count`
/// over `snap`'s topology and round. `base_salt` is all zeros, so salt `i`
/// equals the counter salt of `salt_start + i`, the salt a plain proof-of-work
/// job would use. A snapshot with a family issues a family lease and has no
/// fallback to the legacy draw.
#[must_use]
pub fn build_lease_job(
    snap: &MiningSnapshot,
    miner_identity: [u8; 32],
    salt_start: u64,
    salt_count: u64,
    generation: u64,
) -> Job {
    let (algorithm, family, topology_order_digest) = match &snap.family {
        Some(family) => (
            GeneratorAlgorithm::FamilyChacha8V1,
            Some(family.descriptor().clone()),
            family.topology_order_digest().to_vec(),
        ),
        None => (GeneratorAlgorithm::Blake3Chacha8V1, None, vec![]),
    };
    Job {
        job_id: lease_job_id(salt_start),
        kind: JobKind::IsingGenerate as i32,
        generation,
        deadline_ms: 0,
        ising: None,
        provenance: Some(Provenance {
            is_pow: true,
            order_id: vec![],
        }),
        generator: Some(IsingProblemGenerator {
            algorithm: algorithm as i32,
            topology_hash: snap.topology_hash.clone(),
            last_proof_block_hash: snap.last_proof_block_hash.to_vec(),
            miner_account: miner_identity.to_vec(),
            base_salt: vec![0u8; 32],
            salt_start,
            salt_count,
            family,
            topology_order_digest,
        }),
    }
}

/// Salts for one lease from a miner's measured rate.
///
/// Covers [`LEASE_TARGET_SECS`] at `salts_per_sec`, but never fewer than one
/// pipeline fill (`stream_width`, at least 1), so the miner's stream stays
/// full, and never more than [`LEASE_MAX_SALTS`]. With no usable rate yet, it
/// assumes [`INITIAL_SALTS_PER_SEC_PER_SLOT`] per slot.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "rate * seconds is finite, non-negative, and clamped to LEASE_MAX_SALTS"
)]
pub fn lease_salt_count(salts_per_sec: f64, stream_width: u32) -> u64 {
    let fill = u64::from(stream_width.max(1));
    let salts_per_sec = if salts_per_sec.is_finite() && salts_per_sec > 0.0 {
        salts_per_sec
    } else {
        fill as f64 * INITIAL_SALTS_PER_SEC_PER_SLOT
    };
    let want = (salts_per_sec * LEASE_TARGET_SECS).ceil();
    let want = if want >= LEASE_MAX_SALTS as f64 {
        LEASE_MAX_SALTS
    } else {
        want as u64
    };
    want.clamp(fill.min(LEASE_MAX_SALTS), LEASE_MAX_SALTS)
}

/// Accepts every authentic result: each energy clears it and any solution
/// count passes, so only the redraw and rescore in `verify_lease_result` fail.
const AUTHENTICITY_ONLY: quip_protocol::target::Target = quip_protocol::target::Target {
    max_energy_milli: i64::MAX,
    min_solutions: 0,
    min_diversity_milli: 0,
    max_proof_solutions: u32::MAX,
};

/// Verify one lease `Result` in two steps, then select against `target`.
///
/// A solver-core 0.0.2 miner sends every read for a reported salt, unfiltered
/// and uncapped. The first step redraws the problem and rescores every read
/// with a permissive target, so it rejects only a forged or corrupt result. An
/// authentic result counts as participation even if it misses the target. The
/// second step applies the live gates through the same selection as a plain
/// job, so a miss reaches the decay stash instead of being dropped.
///
/// # Errors
/// Returns the verification error when the result is not authentic.
pub fn verify_and_select(
    generator: &IsingProblemGenerator,
    topology: &LeaseTopology,
    target: &quip_proto::v1::SetTarget,
    result: &quip_proto::v1::Result,
) -> Result<(Verified, crate::validate::Validated), String> {
    let (verified, num_nodes) = if generator.algorithm == GeneratorAlgorithm::FamilyChacha8V1 as i32
    {
        verify_family_result(generator, topology, result)?
    } else {
        let verified = quip_protocol::lease::verify_lease_result(
            generator,
            &topology.view,
            &AUTHENTICITY_ONLY,
            result,
        )
        .map_err(|e| e.to_string())?;
        (verified, topology.view.num_nodes)
    };
    // Verification decoded and rescored every read, so neither step fails here.
    let (rows, energies): (Vec<Vec<i8>>, Vec<i64>) = result
        .solutions
        .iter()
        .filter_map(|s| {
            decode_spins_packed(&s.spins, num_nodes)
                .ok()
                .map(|spins| (spins, s.energy_milli))
        })
        .unzip();
    let gates = crate::validate::gates_from_target(Some(target));
    let validated = crate::validate::validate_scored(&rows, &energies, &gates);
    Ok((verified, validated))
}

/// Regenerate the reported salt's instance from the lease descriptor, rescore
/// every read, and check each read against the family's hard constraints. A
/// soft family has none. The instance node count is returned because a family
/// instance need not have the base topology's width.
fn verify_family_result(
    generator: &IsingProblemGenerator,
    topology: &LeaseTopology,
    result: &quip_proto::v1::Result,
) -> Result<(Verified, usize), String> {
    let spec = LeaseSpec::from_proto(generator).map_err(|e| e.to_string())?;
    let salt: [u8; 32] = result
        .salt
        .as_slice()
        .try_into()
        .map_err(|_| VerifyError::SaltOutsideLease.to_string())?;
    if spec.index_of(&salt).is_none() {
        return Err(VerifyError::SaltOutsideLease.to_string());
    }
    let nonce: [u8; 32] = result
        .nonce
        .as_slice()
        .try_into()
        .map_err(|_| VerifyError::NonceMismatch.to_string())?;
    if derive_nonce(spec.last_proof_block_hash, spec.miner_account, salt) != nonce {
        return Err(VerifyError::NonceMismatch.to_string());
    }
    let descriptor = generator
        .family
        .as_ref()
        .ok_or_else(|| Code::DescriptorMissing.to_string())?;
    let admitted = admit(
        topology.contracts,
        descriptor.family_id.as_bytes(),
        descriptor.revision,
        &descriptor.params,
        &descriptor.params_digest,
    )
    .map_err(|code| code.to_string())?;
    let bounds = admit_topology(
        &admitted,
        &topology.base,
        &generator.topology_order_digest,
        &AdvertisedLimits::default(),
    )
    .map_err(|code| code.to_string())?;
    let instance = generate_instance(&admitted, &topology.base, &bounds, nonce)
        .map_err(|fault| format!("salt gives no instance: {fault:?}"))?;

    let num_nodes = instance.node_ids().len();
    let mut edges = Vec::with_capacity(instance.edges().len());
    for &(u, v) in instance.edges() {
        edges.push((u as usize, v as usize));
    }
    let mut rows = Vec::with_capacity(result.solutions.len());
    for (index, solution) in result.solutions.iter().enumerate() {
        let spins = decode_spins_packed(&solution.spins, num_nodes)
            .map_err(|_| VerifyError::MalformedSpins { index }.to_string())?;
        // A lease salt reports the offset in its energy (FAM-52).
        let energy = energy_from_milli(&spins, instance.h(), instance.j(), &edges)
            .saturating_add(instance.offset_milli());
        if energy != solution.energy_milli {
            return Err(VerifyError::EnergyMismatch { index }.to_string());
        }
        if !feasible(&admitted, &instance, &spins) {
            return Err(format!(
                "solution {index} breaks a hard constraint of the family"
            ));
        }
        rows.push((spins, energy));
    }
    let proof: Vec<(&[i8], i64)> = rows.iter().map(|(s, e)| (s.as_slice(), *e)).collect();
    let stats = quip_protocol::target::validate_proof_set(&proof, &AUTHENTICITY_ONLY)
        .map_err(|e| format!("{e:?}"))?;
    Ok((Verified { salt, nonce, stats }, num_nodes))
}

/// A miner's lease throughput in salts per second.
///
/// Salts are counted only at `LeaseDone`, so a single poll sees either no
/// salts or a whole lease. The estimate divides salts by elapsed time, both
/// decayed over [`LEASE_RATE_WINDOW_SECS`], which turns those steps into a
/// rate instead of a spike followed by decay toward zero.
#[derive(Debug, Default, Clone, Copy)]
pub struct SaltRate {
    salts: f64,
    secs: f64,
}

impl SaltRate {
    /// Record `salts` finished over the last `elapsed_secs` and return the
    /// updated rate.
    #[expect(
        clippy::cast_precision_loss,
        reason = "salt counts stay far below 2^52"
    )]
    pub fn observe(&mut self, salts: u64, elapsed_secs: f64) -> f64 {
        let keep = (-elapsed_secs / LEASE_RATE_WINDOW_SECS).exp();
        self.salts = self.salts * keep + salts as f64;
        self.secs = self.secs * keep + elapsed_secs;
        if self.secs > 0.0 {
            self.salts / self.secs
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::snapshot::MiningSnapshot;
    use quip_proto::v1::{GeneratorAlgorithm, JobKind};
    use quip_protocol::lease::LeaseSpec;

    fn snap() -> MiningSnapshot {
        MiningSnapshot {
            head_hash: [0u8; 32],
            last_proof_block_hash: [7u8; 32],
            topology_hash: vec![9u8; 32],
            nodes: vec![0, 1, 2, 3],
            edges: vec![(0, 1), (1, 2), (2, 3), (0, 3)],
            allowed_h_milli: vec![-1000, 0, 1000],
            allowed_j_milli: vec![-1000, 1000],
            allowed_spin_milli: vec![-1000, 1000],
            min_solutions: 5,
            max_energy_milli: -14_000_000,
            min_diversity_milli: 200,
            block_number: 42,
            spec_version: 117,
            target_proof_blocks: crate::decay::TARGET_PROOF_BLOCKS,
            family: None,
        }
    }

    #[test]
    fn lease_job_names_a_valid_contiguous_range() {
        let job = build_lease_job(&snap(), [1u8; 32], 11, 5, 3);
        assert_eq!(job.kind, JobKind::IsingGenerate as i32);
        assert_eq!(job.generation, 3);
        assert!(job.ising.is_none());
        assert!(job.provenance.as_ref().unwrap().is_pow);
        let g = job.generator.as_ref().unwrap();
        assert_eq!(g.algorithm, GeneratorAlgorithm::Blake3Chacha8V1 as i32);
        assert_eq!(g.topology_hash, vec![9u8; 32]);
        let spec = LeaseSpec::from_proto(g).unwrap();
        assert_eq!((spec.salt_start, spec.salt_count), (11, 5));
        assert_eq!(spec.miner_account, [1u8; 32]);
        assert_eq!(spec.last_proof_block_hash, [7u8; 32]);

        // Lease salts equal the counter salts: index 2 of a range from 11 is 13.
        let mut expected = [0u8; 32];
        expected[..8].copy_from_slice(&13u64.to_le_bytes());
        assert_eq!(spec.salt(2), Some(expected));
    }

    #[test]
    fn lease_job_ids_are_distinct_from_32_byte_nonces() {
        let id = lease_job_id(11);
        assert!(id.starts_with(LEASE_JOB_ID_PREFIX));
        assert_ne!(id.len(), 32);
        assert_ne!(lease_job_id(11), lease_job_id(12));
        assert!(is_lease(&build_lease_job(&snap(), [0; 32], 11, 1, 1)));
    }

    #[test]
    fn salt_rate_averages_lease_completions_over_quiet_polls() {
        // A miner finishing one 60,000-salt lease every 10 one-second polls
        // runs at 6,000 salts per second. Per-poll smoothing swings between
        // the whole lease and near zero. The window holds the true rate.
        let mut rate = SaltRate::default();
        let mut seen = Vec::new();
        for poll in 1..=600 {
            let salts = if poll % 10 == 0 { 60_000 } else { 0 };
            seen.push(rate.observe(salts, 1.0));
        }
        for r in seen.iter().skip(300) {
            assert!(
                (5_000.0..=7_000.0).contains(r),
                "rate {r} strays from 6,000"
            );
        }
    }

    #[test]
    fn salt_count_covers_the_target_duration_within_bounds() {
        let cases = [
            ("no rate: sized from stream width", 0.0, 81, 364_500),
            ("no rate, zero stream width", 0.0, 0, 4_500),
            ("rate covers the target duration", 10.0, 1, 600),
            ("never below one pipeline fill", 0.1, 8, 8),
            ("capped at the maximum", 1e12, 1, LEASE_MAX_SALTS),
            (
                "a bad rate falls back to the initial size",
                f64::NAN,
                2,
                9_000,
            ),
        ];
        for (label, rate, stream_width, expected) in cases {
            assert_eq!(lease_salt_count(rate, stream_width), expected, "{label}");
        }
    }
}
