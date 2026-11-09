//! CO-1: family capability matching and the operator solver pin.

use quip_coordinator::producer::problem::milli_problem;
use quip_coordinator::router::{Ineligible, MinerCaps, Router};
use quip_proto::v1::{
    ising_problem, EdgeList, FamilyBinding, FamilyDescriptor, FamilySupport, GeneratorAlgorithm,
    IsingProblemGenerator, Job, JobKind, RejectReason,
};

const FAMILY: &str = "quip/ice-fields";
const DIGEST: [u8; 32] = [5; 32];

fn descriptor(revision: u32) -> FamilyDescriptor {
    FamilyDescriptor {
        family_id: FAMILY.to_owned(),
        revision,
        params: vec![1],
        params_digest: DIGEST.to_vec(),
    }
}

fn family_lease(revision: u32) -> Job {
    Job {
        job_id: b"lease:family".to_vec(),
        kind: JobKind::IsingGenerate as i32,
        generation: 1,
        generator: Some(IsingProblemGenerator {
            algorithm: GeneratorAlgorithm::FamilyChacha8V1 as i32,
            topology_hash: vec![9; 32],
            last_proof_block_hash: vec![7; 32],
            miner_account: vec![1; 32],
            base_salt: vec![0; 32],
            salt_start: 0,
            salt_count: 4,
            family: Some(descriptor(revision)),
            topology_order_digest: vec![3; 32],
        }),
        ..Job::default()
    }
}

fn legacy_lease() -> Job {
    let mut job = family_lease(1);
    if let Some(generator) = job.generator.as_mut() {
        generator.algorithm = GeneratorAlgorithm::Blake3Chacha8V1 as i32;
        generator.family = None;
        generator.topology_order_digest.clear();
    }
    job
}

fn explicit_job(h: &[i32], j: &[i32]) -> Job {
    let mut ising = milli_problem(
        Some(ising_problem::Graph::Edges(EdgeList {
            u: vec![0],
            v: vec![1],
        })),
        h,
        j,
    );
    ising.family = Some(FamilyBinding {
        family: Some(descriptor(1)),
        ..FamilyBinding::default()
    });
    Job {
        job_id: b"explicit".to_vec(),
        kind: JobKind::IsingSample as i32,
        ising: Some(ising),
        ..Job::default()
    }
}

fn support(revisions: &[u32], digests: &[[u8; 32]]) -> FamilySupport {
    FamilySupport {
        family_id: FAMILY.to_owned(),
        revisions: revisions.to_vec(),
        params_digests: digests.iter().map(|d| d.to_vec()).collect(),
    }
}

fn legacy_caps() -> MinerCaps {
    MinerCaps {
        supported_kinds: vec![JobKind::IsingSample as i32, JobKind::IsingGenerate as i32],
        generators: vec![GeneratorAlgorithm::Blake3Chacha8V1 as i32],
        stream_width: 4,
        ..MinerCaps::default()
    }
}

fn family_caps(entry: FamilySupport) -> MinerCaps {
    let mut caps = legacy_caps();
    caps.generators
        .push(GeneratorAlgorithm::FamilyChacha8V1 as i32);
    caps.generated_families = vec![entry];
    caps
}

fn reasons(router: &Router, job: &Job, miner: &str) -> Option<Ineligible> {
    router
        .ineligible_reasons(job)
        .into_iter()
        .find(|(id, _)| id == miner)
        .map(|(_, reason)| reason)
}

#[test]
fn family_lease_never_reaches_a_legacy_only_miner() {
    let mut router = Router::new();
    router.register_miner("legacy", legacy_caps());
    assert_eq!(router.route(family_lease(1)), None);
    assert_eq!(router.unroutable.len(), 1);
    assert_eq!(
        reasons(&router, &family_lease(1), "legacy"),
        Some(Ineligible::GeneratorUnsupported)
    );
}

#[test]
fn legacy_lease_still_routes_and_needs_the_legacy_generator() {
    let mut router = Router::new();
    let mut family_only = family_caps(support(&[1], &[]));
    family_only.generators = vec![GeneratorAlgorithm::FamilyChacha8V1 as i32];
    router.register_miner("a-family", family_only);
    router.register_miner("z-legacy", legacy_caps());
    assert_eq!(router.route(legacy_lease()).as_deref(), Some("z-legacy"));
}

#[test]
fn unknown_family_wrong_revision_and_restricted_params_are_ineligible() {
    let mut router = Router::new();
    let mut other = support(&[1], &[]);
    other.family_id = "quip/other".to_owned();
    router.register_miner("unknown", family_caps(other));
    router.register_miner("revision", family_caps(support(&[2], &[])));
    router.register_miner("restricted", family_caps(support(&[1], &[[6; 32]])));
    let job = family_lease(1);
    assert_eq!(router.route(job.clone()), None);
    let family_reason = |miner| {
        let Some(Ineligible::Family(code)) = reasons(&router, &job, miner) else {
            panic!("{miner} should report a family code");
        };
        code.value()
    };
    assert_eq!(family_reason("unknown"), 5);
    assert_eq!(family_reason("revision"), 6);
    assert_eq!(family_reason("restricted"), 10);
}

#[test]
fn matching_support_routes_deterministically() {
    let mut router = Router::new();
    router.register_miner("b", family_caps(support(&[1], &[DIGEST])));
    router.register_miner("a", family_caps(support(&[1], &[])));
    assert_eq!(router.route(family_lease(1)).as_deref(), Some("a"));
}

#[test]
fn live_capability_change_moves_staged_family_leases() {
    let mut router = Router::new();
    router.register_miner("a", family_caps(support(&[1], &[])));
    router.register_miner("b", family_caps(support(&[1], &[])));
    assert_eq!(router.route(family_lease(1)).as_deref(), Some("a"));
    assert!(router.update_capabilities("a", family_caps(support(&[2], &[]))));
    assert_eq!(router.staged_len("a"), 0);
    assert_eq!(router.staged_len("b"), 1);
}

#[test]
fn family_lease_ignores_base_topology_size_limits() {
    let topology = quip_coordinator::topology::Topology::from_nodes_edges(
        vec![0, 1, 2],
        vec![(0, 1), (1, 2)],
        &[0],
        &[1000],
        &[-1000, 1000],
    );
    let mut router = Router::new();
    router.set_topology(Some(&topology));
    let mut caps = family_caps(support(&[1], &[]));
    caps.max_nodes = 1;
    caps.max_edges = 1;
    caps.max_degree = Some(1);
    router.register_miner("m", caps);
    let mut job = family_lease(1);
    if let Some(generator) = job.generator.as_mut() {
        generator.topology_hash.clone_from(&topology.hash);
    }
    assert!(router.stage_on("m", job.clone()));
    if let Some(generator) = job.generator.as_mut() {
        generator.algorithm = GeneratorAlgorithm::Blake3Chacha8V1 as i32;
        generator.family = None;
        generator.topology_order_digest.clear();
    }
    assert!(!router.stage_on("m", job), "legacy limits are unchanged");
}

#[test]
fn generic_sampler_gets_a_supported_explicit_model_without_claiming_the_generator() {
    let mut router = Router::new();
    let caps = MinerCaps {
        supported_kinds: vec![JobKind::IsingSample as i32],
        ..MinerCaps::default()
    };
    router.register_miner("generic", caps);
    assert_eq!(
        router.route(explicit_job(&[0, 0], &[1000])).as_deref(),
        Some("generic")
    );
}

#[test]
fn accepted_family_with_restricted_params_refuses_an_explicit_job() {
    let mut router = Router::new();
    let caps = MinerCaps {
        supported_kinds: vec![JobKind::IsingSample as i32],
        accepted_families: vec![support(&[1], &[[6; 32]])],
        ..MinerCaps::default()
    };
    router.register_miner("restricted", caps);
    let job = explicit_job(&[0, 0], &[1000]);
    assert_eq!(router.route(job.clone()), None);
    let Some(Ineligible::Family(code)) = reasons(&router, &job, "restricted") else {
        panic!("expected a family code");
    };
    assert_eq!(code.value(), 10);
}

#[test]
fn explicit_family_jobs_respect_field_encoding_and_resolution_limits() {
    let base = MinerCaps {
        supported_kinds: vec![JobKind::IsingSample as i32],
        ..MinerCaps::default()
    };
    let job = explicit_job(&[500, 0], &[1000]);
    let check = |caps: MinerCaps| {
        let mut router = Router::new();
        router.register_miner("m", caps);
        reasons(&router, &job, "m")
    };

    assert_eq!(
        check(MinerCaps {
            max_local_field_milli: Some(1499),
            ..base.clone()
        }),
        Some(Ineligible::LocalField)
    );
    assert_eq!(
        check(MinerCaps {
            max_local_field_milli: Some(1500),
            ..base.clone()
        }),
        None
    );
    assert_eq!(
        check(MinerCaps {
            encodings: vec![quip_proto::v1::CoefficientEncoding::F64 as i32],
            ..base.clone()
        }),
        Some(Ineligible::Encoding)
    );
    let exact = MinerCaps {
        exact_resolution_milli: Some(1000),
        features: vec!["exact-coefficients".to_owned()],
        ..base.clone()
    };
    assert_eq!(check(exact), Some(Ineligible::ExactResolution));
    assert_eq!(
        check(MinerCaps {
            exact_resolution_milli: Some(1000),
            ..base
        }),
        None,
        "a solver without exact-coefficients may quantize"
    );
}

#[test]
fn pin_narrows_candidates_by_miner_id_or_solver_id() {
    let mut router = Router::new();
    router.register_miner("a", family_caps(support(&[1], &[])));
    let mut pinned = family_caps(support(&[1], &[]));
    pinned.solver_id = "quip-cpu-sa/1".to_owned();
    router.register_miner("b", pinned);

    router.set_solver_pin(Some("quip-cpu-sa/1".to_owned()));
    assert_eq!(router.route(family_lease(1)).as_deref(), Some("b"));
    router.set_solver_pin(Some("a".to_owned()));
    assert_eq!(router.route(family_lease(1)).as_deref(), Some("a"));
}

#[test]
fn pin_never_bypasses_admission_and_reports_why() {
    let mut router = Router::new();
    router.register_miner("legacy", legacy_caps());
    router.register_miner("ok", family_caps(support(&[1], &[])));
    router.set_solver_pin(Some("legacy".to_owned()));
    let job = family_lease(1);
    assert_eq!(router.route(job.clone()), None);
    assert_eq!(
        reasons(&router, &job, "legacy"),
        Some(Ineligible::GeneratorUnsupported)
    );
    assert_eq!(reasons(&router, &job, "ok"), Some(Ineligible::NotPinned));
    assert!(!router.stage_on("ok", job));
}

#[test]
fn pin_leaves_legacy_work_alone() {
    let mut router = Router::new();
    router.register_miner("a", legacy_caps());
    router.set_solver_pin(Some("missing".to_owned()));
    assert_eq!(router.route(legacy_lease()).as_deref(), Some("a"));
}

#[test]
fn family_reject_is_scoped_to_that_family_case() {
    let mut router = Router::new();
    router.register_miner("m", family_caps(support(&[1, 2], &[])));
    router.grant_credits("m", 1);
    assert_eq!(router.route(family_lease(1)).as_deref(), Some("m"));
    let dispatched = router.next_job("m").expect("dispatched");
    router.on_reject("m", dispatched, RejectReason::FamilyUnsupported as i32);

    assert_eq!(
        reasons(&router, &family_lease(1), "m"),
        Some(Ineligible::FamilyRejected)
    );
    assert!(router.stage_on("m", family_lease(2)), "other revision");
    assert!(router.stage_on("m", legacy_lease()), "other Ising work");

    assert!(router.update_capabilities("m", family_caps(support(&[1, 2], &[]))));
    assert!(router.stage_on("m", family_lease(1)), "fresh snapshot");
}

#[test]
fn unsupported_kind_reject_of_family_work_keeps_the_kind_open() {
    let mut router = Router::new();
    router.register_miner("m", family_caps(support(&[1], &[])));
    router.on_reject("m", family_lease(1), RejectReason::UnsupportedKind as i32);
    assert!(router.stage_on("m", legacy_lease()));
}
