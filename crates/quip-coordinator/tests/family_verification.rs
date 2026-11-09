//! CO-3: every family result is regenerated and rescored before selection.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "test fixtures: a failed setup should stop the test"
)]

use ising_generators::descriptor::{admit, params_digest, AdvertisedLimits, Contract};
use ising_generators::family::{admit_topology, generate_instance, BaseTopology};
use ising_generators::instance::Instance;
use ising_generators::random::derive_nonce;
use quip_coordinator::drive::TopologySpec;
use quip_coordinator::lease::{build_lease_job, verify_and_select, ChallengeFamily, LeaseTopology};
use quip_coordinator::topology::Topology;
use quip_proto::v1::{IsingProblemGenerator, Result as JobResult, SetTarget, Solution};
use quip_protocol::lease::LeaseSpec;
use quip_protocol::scoring::energy_from_milli;
use quip_protocol::wire::encode_spins_packed;
use quip_solver_conformance::families::layered::{P_L2_F0, P_SPEC, RESTRICTED_V1, UNIFORM_V1};

static CONTRACTS: [Contract; 2] = [UNIFORM_V1, RESTRICTED_V1];
const FAMILY: &str = "vectors/uniform-field";
const OTHER_FAMILY: &str = "vectors/restricted-field";
const LAST_PROOF: [u8; 32] = [7; 32];
const MINER: [u8; 32] = [1; 32];

fn topology() -> Topology {
    Topology::from_nodes_edges(
        vec![0, 1, 2, 3],
        vec![(0, 1), (1, 2), (2, 3), (0, 3)],
        &[0],
        &[1000],
        &[-1000, 1000],
    )
}

fn spec_of(topo: &Topology) -> TopologySpec {
    TopologySpec {
        topology: topo.clone(),
        allowed_h_milli: vec![0],
        allowed_j_milli: vec![1000],
        allowed_spin_milli: vec![-1000, 1000],
        min_solutions: 1,
        max_energy_milli: 0,
        min_diversity_milli: 0,
    }
}

fn lease_for(
    family: &str,
    params: &[u8],
    topo: &Topology,
    root: [u8; 32],
) -> IsingProblemGenerator {
    let spec = spec_of(topo);
    let mut snap = spec.to_snapshot();
    snap.last_proof_block_hash = root;
    snap.family =
        Some(ChallengeFamily::admit(&CONTRACTS, family, 1, params.to_vec(), topo).unwrap());
    build_lease_job(&snap, MINER, 0, 32, 1).generator.unwrap()
}

fn lease() -> IsingProblemGenerator {
    lease_for(FAMILY, &P_SPEC, &topology(), LAST_PROOF)
}

fn verifier() -> LeaseTopology {
    LeaseTopology::from_topology(&topology(), &CONTRACTS).unwrap()
}

/// The instance a salt in `lease` regenerates, or `None` when the salt is skipped.
fn instance_of(
    lease: &IsingProblemGenerator,
    topo: &Topology,
    index: u64,
) -> Option<(Instance, [u8; 32], [u8; 32])> {
    let spec = LeaseSpec::from_proto(lease).unwrap();
    let salt = spec.salt(index).unwrap();
    let nonce = derive_nonce(LAST_PROOF, MINER, salt);
    let descriptor = lease.family.as_ref().unwrap();
    let admitted = admit(
        &CONTRACTS,
        descriptor.family_id.as_bytes(),
        descriptor.revision,
        &descriptor.params,
        &descriptor.params_digest,
    )
    .unwrap();
    let base = BaseTopology::new(topo.nodes.clone(), &topo.edge_pairs()).unwrap();
    let bounds = admit_topology(
        &admitted,
        &base,
        &lease.topology_order_digest,
        &AdvertisedLimits::default(),
    )
    .unwrap();
    let instance = generate_instance(&admitted, &base, &bounds, nonce).ok()?;
    Some((instance, salt, nonce))
}

fn energy(instance: &Instance, spins: &[i8]) -> i64 {
    let edges: Vec<(usize, usize)> = instance
        .edges()
        .iter()
        .map(|&(u, v)| (u as usize, v as usize))
        .collect();
    energy_from_milli(spins, instance.h(), instance.j(), &edges) + instance.offset_milli()
}

/// An honest report for the first salt of `lease` that yields an instance.
fn honest(lease: &IsingProblemGenerator) -> (JobResult, Instance) {
    let topo = topology();
    let (instance, salt, nonce) = (0..32)
        .find_map(|index| instance_of(lease, &topo, index))
        .expect("a salt yields an instance");
    let n = instance.node_ids().len();
    let mut spins = vec![1i8; n];
    let mut solutions = Vec::new();
    for flip in 0..2 {
        spins[flip] = -1;
        solutions.push(Solution {
            spins: encode_spins_packed(&spins),
            energy_milli: energy(&instance, &spins),
        });
    }
    let result = JobResult {
        job_id: b"lease".to_vec(),
        solutions,
        salt: salt.to_vec(),
        nonce: nonce.to_vec(),
        ..JobResult::default()
    };
    (result, instance)
}

fn target(max_energy_milli: i64) -> SetTarget {
    SetTarget {
        max_energy_milli,
        min_solutions: 1,
        min_diversity_milli: 0,
        ..SetTarget::default()
    }
}

fn open_target() -> SetTarget {
    target(i64::MAX / 2)
}

fn forged(lease: &IsingProblemGenerator, result: &JobResult) -> String {
    verify_and_select(lease, &verifier(), &open_target(), result)
        .expect_err("a forged result must fail before selection")
}

#[test]
fn an_honest_result_verifies_and_selects() {
    let lease = lease();
    let (result, _) = honest(&lease);
    let (verified, validated) =
        verify_and_select(&lease, &verifier(), &open_target(), &result).unwrap();
    assert_eq!(verified.salt.to_vec(), result.salt);
    assert_eq!(verified.nonce.to_vec(), result.nonce);
    assert!(validated.accepted);
}

#[test]
fn an_authentic_target_miss_stays_eligible_for_decay() {
    let lease = lease();
    let (result, instance) = honest(&lease);
    let mut spins = vec![1i8; instance.node_ids().len()];
    spins[0] = -1;
    let best = energy(&instance, &spins).min(result.solutions[1].energy_milli);
    let (_, validated) =
        verify_and_select(&lease, &verifier(), &target(best - 1), &result).unwrap();
    assert!(!validated.accepted, "misses the live gate");
    assert_eq!(validated.raw_best_energy_milli, best, "kept for the stash");
}

#[test]
fn wrong_salt_is_forged() {
    let lease = lease();
    let (mut result, _) = honest(&lease);
    result.salt[0] = result.salt[0].wrapping_add(200);
    assert!(forged(&lease, &result).contains("salt"));
    result.salt = vec![0; 31];
    assert!(forged(&lease, &result).contains("salt"));
}

#[test]
fn wrong_nonce_is_forged() {
    let lease = lease();
    let (mut result, _) = honest(&lease);
    result.nonce[3] ^= 1;
    assert!(forged(&lease, &result).contains("nonce"));
}

#[test]
fn a_result_from_a_stale_challenge_is_forged() {
    let lease = lease();
    let (result, _) = honest(&lease);
    let newer = lease_for(FAMILY, &P_SPEC, &topology(), [8; 32]);
    assert!(forged(&newer, &result).contains("nonce"));
}

#[test]
fn wrong_family_is_forged() {
    let lease = lease();
    let (result, _) = honest(&lease);
    let other = lease_for(OTHER_FAMILY, &P_SPEC, &topology(), LAST_PROOF);
    assert!(!forged(&other, &result).is_empty());
}

#[test]
fn wrong_revision_is_forged() {
    let mut lease = lease();
    let (result, _) = honest(&lease);
    lease.family.as_mut().unwrap().revision = 2;
    assert!(forged(&lease, &result).contains("RevisionUnsupported"));
}

#[test]
fn wrong_parameters_are_forged() {
    let lease = lease();
    let (result, _) = honest(&lease);
    let other = lease_for(FAMILY, &P_L2_F0, &topology(), LAST_PROOF);
    assert!(!forged(&other, &result).is_empty());
}

#[test]
fn a_parameter_digest_that_breaks_the_commitment_is_forged() {
    let mut lease = lease();
    let (result, _) = honest(&lease);
    lease.family.as_mut().unwrap().params = P_L2_F0.to_vec();
    assert_ne!(params_digest(&P_L2_F0), params_digest(&P_SPEC));
    assert!(forged(&lease, &result).contains("ParamsDigestMismatch"));
}

#[test]
fn wrong_topology_order_is_forged() {
    let mut lease = lease();
    let (result, _) = honest(&lease);
    lease.topology_order_digest = vec![0; 32];
    assert!(forged(&lease, &result).contains("TopologyOrderMismatch"));
}

#[test]
fn a_verifier_on_another_edge_order_rejects_the_lease() {
    let lease = lease();
    let (result, _) = honest(&lease);
    let mut reversed = topology();
    reversed.edges.0.reverse();
    reversed.edges.1.reverse();
    let other = LeaseTopology::from_topology(&reversed, &CONTRACTS).unwrap();
    let err = verify_and_select(&lease, &other, &open_target(), &result).unwrap_err();
    assert!(err.contains("TopologyOrderMismatch"));
}

#[test]
fn an_energy_without_the_offset_is_forged() {
    let lease = lease();
    let (mut result, instance) = honest(&lease);
    assert_ne!(instance.offset_milli(), 0);
    result.solutions[0].energy_milli -= instance.offset_milli();
    assert!(forged(&lease, &result).contains("energy"));
}

#[test]
fn a_changed_reported_energy_is_forged() {
    let lease = lease();
    let (mut result, _) = honest(&lease);
    result.solutions[1].energy_milli -= 1;
    assert!(forged(&lease, &result).contains("energy"));
}

#[test]
fn changed_spins_are_forged() {
    let lease = lease();
    let (mut result, _) = honest(&lease);
    result.solutions[0].spins[0] ^= 0b100;
    assert!(forged(&lease, &result).contains("energy"));
}

#[test]
fn spins_of_the_wrong_width_are_forged() {
    let lease = lease();
    let (mut result, instance) = honest(&lease);
    assert_ne!(instance.node_ids().len(), topology().nodes.len());
    let too_wide = vec![1i8; instance.node_ids().len() + 8];
    result.solutions[0].spins = encode_spins_packed(&too_wide);
    assert!(forged(&lease, &result).contains("spin"));
}

/// The first eight spins of a four-node report pack into the same byte count,
/// so only the rescore can refuse it.
#[test]
fn a_report_packed_for_the_base_topology_width_fails_the_rescore() {
    let lease = lease();
    let (mut result, _) = honest(&lease);
    result.solutions[0].spins = encode_spins_packed(&[1, 1, 1, 1]);
    assert!(forged(&lease, &result).contains("energy"));
}

#[test]
fn an_empty_contract_table_verifies_nothing() {
    let lease = lease();
    let (result, _) = honest(&lease);
    let empty = LeaseTopology::from_topology(&topology(), &[]).unwrap();
    let err = verify_and_select(&lease, &empty, &open_target(), &result).unwrap_err();
    assert!(err.contains("FamilyUnsupported"));
}

#[test]
fn a_legacy_lease_still_verifies_on_the_legacy_path() {
    let topo = topology();
    let spec = spec_of(&topo);
    let mut snap = spec.to_snapshot();
    snap.last_proof_block_hash = LAST_PROOF;
    let job = build_lease_job(&snap, MINER, 0, 4, 1);
    let lease = job.generator.unwrap();
    let parsed = LeaseSpec::from_proto(&lease).unwrap();
    let salt = parsed.salt(0).unwrap();
    let nonce = derive_nonce(LAST_PROOF, MINER, salt);
    let view = quip_protocol::lease::TopologyView::from_proto(&topo.to_proto()).unwrap();
    let (h, j) = view.draw(nonce).unwrap();
    let spins = vec![1i8; 4];
    let result = JobResult {
        job_id: b"lease".to_vec(),
        solutions: vec![Solution {
            spins: encode_spins_packed(&spins),
            energy_milli: energy_from_milli(&spins, &h, &j, &view.edges),
        }],
        salt: salt.to_vec(),
        nonce: nonce.to_vec(),
        ..JobResult::default()
    };
    assert!(verify_and_select(&lease, &verifier(), &open_target(), &result).is_ok());
}
