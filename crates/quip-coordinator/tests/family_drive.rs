//! CO-2: family leases bind one descriptor, topology order, range, and challenge.

#![expect(
    clippy::unwrap_used,
    reason = "test fixtures: a failed setup should stop the test"
)]

use ising_generators::descriptor::{params_digest, Contract};
use ising_generators::Code;
use quip_coordinator::chain::snapshot::head_state_key;
use quip_coordinator::drive::lease_source::LeaseSource;
use quip_coordinator::drive::{drain_all, parse_topology_spec, TopologySpec};
use quip_coordinator::lease::{build_lease_job, ChallengeFamily};
use quip_coordinator::topology::Topology;
use quip_proto::v1::{GeneratorAlgorithm, IsingProblemGenerator, Job};
use quip_solver_conformance::families::layered::{P_F5000, P_L2_F0, P_SPEC, UNIFORM_V1};

static CONTRACTS: [Contract; 1] = [UNIFORM_V1];
const FAMILY: &str = "vectors/uniform-field";

fn spec() -> TopologySpec {
    parse_topology_spec(quip_coordinator::presets::preset_spec("smoke").unwrap()).unwrap()
}

fn admit(params: &[u8], topology: &Topology) -> Result<ChallengeFamily, Code> {
    ChallengeFamily::admit(&CONTRACTS, FAMILY, 1, params.to_vec(), topology)
}

fn generator(job: &Job) -> &IsingProblemGenerator {
    job.generator.as_ref().unwrap()
}

fn family_source(family: &ChallengeFamily, seed: u64) -> LeaseSource {
    LeaseSource::new(&spec(), [1; 32], seed, 10, 4).with_family(family.clone())
}

#[test]
fn leases_cover_exact_ranges_and_carry_one_descriptor() {
    let spec = spec();
    let family = admit(&P_SPEC, &spec.topology).unwrap();
    let jobs = drain_all(&mut family_source(&family, 3));
    let ranges: Vec<_> = jobs
        .iter()
        .map(|j| (generator(j).salt_start, generator(j).salt_count))
        .collect();
    assert_eq!(ranges, vec![(0, 4), (4, 4), (8, 2)]);
    for job in &jobs {
        let g = generator(job);
        assert_eq!(g.algorithm, GeneratorAlgorithm::FamilyChacha8V1 as i32);
        let d = g.family.as_ref().unwrap();
        assert_eq!((d.family_id.as_str(), d.revision), (FAMILY, 1));
        assert_eq!(d.params, P_SPEC);
        assert_eq!(d.params_digest, params_digest(&P_SPEC));
        assert_eq!(g.topology_order_digest.len(), 32);
    }
}

#[test]
fn identical_inputs_issue_identical_leases() {
    let spec = spec();
    let family = admit(&P_SPEC, &spec.topology).unwrap();
    assert_eq!(
        drain_all(&mut family_source(&family, 3)),
        drain_all(&mut family_source(&family, 3))
    );
}

#[test]
fn snapshot_without_a_family_stays_on_the_legacy_generator() {
    let spec = spec();
    let job = build_lease_job(&spec.to_snapshot(), [1; 32], 0, 4, 1);
    let g = generator(&job);
    assert_eq!(g.algorithm, GeneratorAlgorithm::Blake3Chacha8V1 as i32);
    assert_eq!(g.family, None);
    assert!(g.topology_order_digest.is_empty());
}

#[test]
fn descriptor_faults_stop_before_a_lease_exists() {
    let spec = spec();
    assert_eq!(
        admit(&P_F5000, &spec.topology).err(),
        Some(Code::ParamsOutOfBounds)
    );
    assert_eq!(
        ChallengeFamily::admit(
            &CONTRACTS,
            "vectors/other",
            1,
            P_SPEC.to_vec(),
            &spec.topology
        )
        .err(),
        Some(Code::FamilyUnsupported)
    );
    assert_eq!(
        ChallengeFamily::admit(&CONTRACTS, FAMILY, 2, P_SPEC.to_vec(), &spec.topology).err(),
        Some(Code::RevisionUnsupported)
    );
}

#[test]
fn production_table_admits_nothing_until_families_activate() {
    let spec = spec();
    assert_eq!(
        ChallengeFamily::admit(
            quip_coordinator::lease::FAMILY_CONTRACTS,
            FAMILY,
            1,
            P_SPEC.to_vec(),
            &spec.topology
        )
        .err(),
        Some(Code::FamilyUnsupported)
    );
}

#[test]
fn topology_order_is_part_of_the_commitment() {
    let spec = spec();
    let family = admit(&P_SPEC, &spec.topology).unwrap();
    let mut pairs = spec.topology.edge_pairs();
    pairs.reverse();
    let reordered = Topology::from_nodes_edges(
        spec.topology.nodes.clone(),
        pairs,
        &spec.topology.allowed_h,
        &spec.topology.allowed_j,
        &[-1000, 1000],
    );
    let other = admit(&P_SPEC, &reordered).unwrap();
    assert_ne!(
        family.topology_order_digest(),
        other.topology_order_digest()
    );
}

#[test]
fn a_family_change_is_a_new_challenge() {
    let spec = spec();
    let legacy = spec.to_snapshot();
    let mut first = legacy.clone();
    first.family = Some(admit(&P_SPEC, &spec.topology).unwrap());
    let mut second = legacy.clone();
    second.family = Some(admit(&P_L2_F0, &spec.topology).unwrap());
    let keys = [
        head_state_key(&legacy),
        head_state_key(&first),
        head_state_key(&second),
    ];
    assert_ne!(keys[0], keys[1]);
    assert_ne!(keys[1], keys[2]);
    assert_eq!(keys[1], head_state_key(&first));
}
