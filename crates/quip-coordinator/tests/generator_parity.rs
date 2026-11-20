//! The coordinator scores lease results against instances from the
//! `ising-generators` release it depends on, while the chain verifier
//! (`quantum-validation` from quip-validator) regenerates them with the
//! generator it pins. The two are different builds in one binary. If they
//! drew different instances for the live family, every lease result would be
//! scored against the wrong instance and no proof would land.
//!
//! These tests pin the two together: for the twisted-ice dimer family on the
//! live Zephyr graph, both generators must give the same nodes, edges, h, J
//! and metadata for every nonce.

#![expect(clippy::unwrap_used, reason = "test helpers outside #[test]")]
#![expect(
    clippy::panic,
    reason = "a mismatch between the generators fails the test"
)]

use ising_generators::descriptor::{admit, AdvertisedLimits as CoordinatorLimits};
use ising_generators::family::{admit_topology, generate_instance, BaseTopology};
use ising_generators::instance::Instance as CoordinatorInstance;
use ising_generators::random::derive_nonce;
use quantum_validation::families::{
    AdvertisedLimits as ChainLimits, FamilyChallenge, Instance as ChainInstance, CONTRACTS,
};
use quip_coordinator::chain::governance::seed::{
    SeedTopology, RUNTIME_MAX_EDGES, RUNTIME_MAX_NODES,
};
use quip_coordinator::chain::governance::set_family::{
    twisted_dimer_params, TWISTED_DIMER_FAMILY_ID, TWISTED_DIMER_REVISION,
};
use quip_coordinator::types::family::{ChallengeFamily, FAMILY_CONTRACTS};
use quip_coordinator::types::topology_spec::{parse_topology_spec, preset_spec};

/// Nonces drawn per parameter set.
const SALTS: u8 = 4;

/// The instance the coordinator scores a lease result against: the calls
/// `verify_family_result` in `coordinator::scoring::lease` makes, on the base
/// topology `LeaseTopology::from_topology` builds.
fn coordinator_instance(
    family: &ChallengeFamily,
    base: &BaseTopology,
    nonce: [u8; 32],
) -> Result<CoordinatorInstance, String> {
    let descriptor = family.descriptor();
    let admitted = admit(
        FAMILY_CONTRACTS,
        descriptor.family_id.as_bytes(),
        descriptor.revision,
        &descriptor.params,
        &descriptor.params_digest,
    )
    .map_err(|code| code.to_string())?;
    let bounds = admit_topology(
        &admitted,
        base,
        family.topology_order_digest(),
        &CoordinatorLimits::default(),
    )
    .map_err(|code| code.to_string())?;
    generate_instance(&admitted, base, &bounds, nonce).map_err(|fault| format!("{fault:?}"))
}

/// Assert the two generators give the same instance for every drawn nonce,
/// and that at least three nonces give an instance.
fn assert_generators_agree(weak_milli: u16, pairs: u32) {
    let spec = parse_topology_spec(preset_spec("zephyr-z12t4-live").unwrap()).unwrap();
    let params = twisted_dimer_params(weak_milli, pairs);
    let family = ChallengeFamily::admit(
        FAMILY_CONTRACTS,
        TWISTED_DIMER_FAMILY_ID,
        TWISTED_DIMER_REVISION,
        params.clone(),
        &spec.topology,
    )
    .unwrap();
    let base = BaseTopology::new(spec.topology.nodes.clone(), &spec.topology.edge_pairs()).unwrap();
    // The chain side: the graph as `seed-chain` registers it, admitted as the
    // pallet admits it.
    let seed = SeedTopology::from_spec(&spec);
    let chain = FamilyChallenge::admit(
        CONTRACTS,
        TWISTED_DIMER_FAMILY_ID.as_bytes(),
        TWISTED_DIMER_REVISION,
        &params,
        &seed.nodes,
        &seed.edges,
        &ChainLimits {
            max_nodes: RUNTIME_MAX_NODES,
            max_edges: RUNTIME_MAX_EDGES,
            max_degree: 0,
        },
    )
    .unwrap();
    assert_eq!(
        chain.topology_order_digest(),
        *family.topology_order_digest(),
        "both sides see the same node and edge order"
    );

    let mut generated = 0;
    for salt_byte in 1..=SALTS {
        let head = [0x5a; 32];
        let miner = [0xc3; 32];
        let nonce = derive_nonce(head, miner, [salt_byte; 32]);
        let ours = coordinator_instance(&family, &base, nonce);
        let theirs: Result<ChainInstance, _> = chain.regenerate(nonce);
        match (ours, theirs) {
            (Ok(ours), Ok(theirs)) => {
                let label = format!("weak {weak_milli} pairs {pairs} salt {salt_byte}");
                assert_eq!(ours.node_ids(), theirs.node_ids(), "nodes, {label}");
                assert_eq!(ours.edges(), theirs.edges(), "edges, {label}");
                assert_eq!(ours.h(), theirs.h(), "h, {label}");
                assert_eq!(ours.j(), theirs.j(), "J, {label}");
                assert_eq!(ours.metadata(), theirs.metadata(), "metadata, {label}");
                assert_eq!(ours.digest(), theirs.digest(), "digest, {label}");
                assert!(!ours.j().is_empty(), "a real instance, {label}");
                generated += 1;
            }
            (ours, theirs) => panic!(
                "weak {weak_milli} pairs {pairs} salt {salt_byte}: coordinator {:?}, chain {:?}",
                ours.map(|i| i.digest()),
                theirs.map(|i| i.digest())
            ),
        }
    }
    assert!(generated >= 3, "only {generated} nonces gave an instance");
}

/// The live recipe `x-w62-p1200` that `seed-chain` registers.
#[test]
fn lease_instances_match_the_chain_verifier_for_dimer_1200() {
    assert_generators_agree(62, 1200);
}

/// The `set-family --weak-milli 45 --pairs 1350` parameters.
#[test]
fn lease_instances_match_the_chain_verifier_for_weak_45_pairs_1350() {
    assert_generators_agree(45, 1350);
}
