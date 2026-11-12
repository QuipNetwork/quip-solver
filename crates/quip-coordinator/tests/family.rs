//! Family work: capability routing and the solver pin (CO-1), lease issuance
//! (CO-2), and result verification (CO-3).

mod dispatch {
    //! CO-1: family capability matching and the operator solver pin.

    use quip_coordinator::producer::problem::milli_problem;
    use quip_coordinator::router::{Ineligible, MinerCaps, Router};
    use quip_proto::v1::{
        ising_problem, EdgeList, FamilyBinding, FamilyDescriptor, FamilySupport,
        GeneratorAlgorithm, IsingProblemGenerator, Job, JobKind, RejectReason,
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
}

mod drive {
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

    /// Leases tile the salt range exactly, each carries the one admitted
    /// descriptor, and identical inputs issue identical leases.
    #[test]
    fn leases_cover_exact_ranges_and_carry_one_descriptor() {
        let spec = spec();
        let family = admit(&P_SPEC, &spec.topology).unwrap();
        let jobs = drain_all(&mut family_source(&family, 3));
        assert_eq!(jobs, drain_all(&mut family_source(&family, 3)));
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

    /// The production table is every family the generator crate ships, and no
    /// test vector family.
    #[test]
    fn production_table_is_the_generator_registry() {
        let ids = |table: &[Contract]| -> Vec<(&str, u32)> {
            table.iter().map(|c| (c.family_id, c.revision)).collect()
        };
        assert_eq!(
            ids(quip_coordinator::lease::FAMILY_CONTRACTS),
            ids(ising_generators::registry::CONTRACTS)
        );
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
}

mod verification {
    //! CO-3: every family result is regenerated and rescored before selection.

    #![expect(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        reason = "test fixtures: a failed setup should stop the test"
    )]

    use ising_generators::descriptor::{admit, params_digest, AdvertisedLimits, Contract};
    use ising_generators::family::{admit_topology, feasible, generate_instance, BaseTopology};
    use ising_generators::instance::Instance;
    use ising_generators::polariton::POLARITON_V1;
    use ising_generators::random::derive_nonce;
    use ising_generators::registry::CONTRACTS as REGISTRY;
    use quip_coordinator::drive::TopologySpec;
    use quip_coordinator::lease::{
        build_lease_job, verify_and_select, ChallengeFamily, LeaseTopology,
    };
    use quip_coordinator::topology::Topology;
    use quip_proto::v1::{IsingProblemGenerator, Result as JobResult, SetTarget, Solution};
    use quip_protocol::lease::LeaseSpec;
    use quip_protocol::scoring::energy_from_milli;
    use quip_protocol::wire::encode_spins_packed;
    use quip_solver_conformance::families::layered::{P_L2_F0, P_SPEC, RESTRICTED_V1, UNIFORM_V1};
    use quip_solver_conformance::families::polariton::P_POLARITON_Z24_L2_P0;
    use quip_solver_conformance::families::ROUTING_TOPOLOGY;

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
        contracts: &[Contract],
        lease: &IsingProblemGenerator,
        topo: &Topology,
        index: u64,
    ) -> Option<(Instance, [u8; 32], [u8; 32])> {
        let spec = LeaseSpec::from_proto(lease).unwrap();
        let salt = spec.salt(index).unwrap();
        let nonce = derive_nonce(LAST_PROOF, MINER, salt);
        let descriptor = lease.family.as_ref().unwrap();
        let admitted = admit(
            contracts,
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
            .find_map(|index| instance_of(&CONTRACTS, lease, &topo, index))
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

    /// Every field that binds a report to its challenge is checked: the salt,
    /// the nonce, the root, the family, the revision, the parameters and their
    /// digest, and the topology order. One tampered field fails verification
    /// with an error that names it.
    #[test]
    fn a_report_that_does_not_match_its_challenge_is_forged() {
        type Tamper = fn(&mut IsingProblemGenerator, &mut JobResult);
        let cases: [(&str, Tamper, &str); 9] = [
            (
                "salt byte",
                |_, r| r.salt[0] = r.salt[0].wrapping_add(200),
                "salt",
            ),
            ("salt width", |_, r| r.salt = vec![0; 31], "salt"),
            ("nonce", |_, r| r.nonce[3] ^= 1, "nonce"),
            (
                "stale root",
                |l, _| *l = lease_for(FAMILY, &P_SPEC, &topology(), [8; 32]),
                "nonce",
            ),
            (
                "family",
                |l, _| *l = lease_for(OTHER_FAMILY, &P_SPEC, &topology(), LAST_PROOF),
                "",
            ),
            (
                "revision",
                |l, _| l.family.as_mut().unwrap().revision = 2,
                "RevisionUnsupported",
            ),
            (
                "parameters",
                |l, _| *l = lease_for(FAMILY, &P_L2_F0, &topology(), LAST_PROOF),
                "",
            ),
            (
                "parameter digest",
                |l, _| {
                    assert_ne!(params_digest(&P_L2_F0), params_digest(&P_SPEC));
                    l.family.as_mut().unwrap().params = P_L2_F0.to_vec();
                },
                "ParamsDigestMismatch",
            ),
            (
                "topology order",
                |l, _| l.topology_order_digest = vec![0; 32],
                "TopologyOrderMismatch",
            ),
        ];
        for (label, tamper, needle) in cases {
            let mut lease = lease();
            let (mut result, _) = honest(&lease);
            tamper(&mut lease, &mut result);
            let err = forged(&lease, &result);
            assert!(err.contains(needle), "{label}: {err}");
        }
    }

    /// The rescore refuses a report whose energies or spins do not regenerate:
    /// an energy without the instance offset, a changed energy, changed spins,
    /// spins of the wrong width, and spins packed for the base topology width.
    /// The last case packs into the same byte count as an honest report, so only
    /// the rescore can refuse it.
    #[test]
    fn a_report_that_does_not_rescore_is_forged() {
        type Tamper = fn(&mut JobResult, &Instance);
        let cases: [(&str, Tamper, &str); 5] = [
            (
                "energy without the offset",
                |r, i| {
                    assert_ne!(i.offset_milli(), 0);
                    r.solutions[0].energy_milli -= i.offset_milli();
                },
                "energy",
            ),
            (
                "changed energy",
                |r, _| r.solutions[1].energy_milli -= 1,
                "energy",
            ),
            (
                "changed spins",
                |r, _| r.solutions[0].spins[0] ^= 0b100,
                "energy",
            ),
            (
                "too wide",
                |r, i| {
                    assert_ne!(i.node_ids().len(), topology().nodes.len());
                    let too_wide = vec![1i8; i.node_ids().len() + 8];
                    r.solutions[0].spins = encode_spins_packed(&too_wide);
                },
                "spin",
            ),
            (
                "base topology width",
                |r, _| r.solutions[0].spins = encode_spins_packed(&[1, 1, 1, 1]),
                "energy",
            ),
        ];
        let lease = lease();
        for (label, tamper, needle) in cases {
            let (mut result, instance) = honest(&lease);
            tamper(&mut result, &instance);
            let err = forged(&lease, &result);
            assert!(err.contains(needle), "{label}: {err}");
        }
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

    /// A `quip/polariton-static@1` lease on the conformance topology, and a
    /// verifier for it. Polariton is a `hard` family: a witness must also meet
    /// every labeled vertex charge.
    fn polariton() -> (Topology, IsingProblemGenerator, LeaseTopology) {
        let blob = quip_solver_conformance::topology(ROUTING_TOPOLOGY).unwrap();
        let topo =
            Topology::from_nodes_edges(blob.nodes, blob.edges, &[0], &[1000], &[-1000, 1000]);
        let mut snap = spec_of(&topo).to_snapshot();
        snap.last_proof_block_hash = LAST_PROOF;
        snap.family = Some(
            ChallengeFamily::admit(
                REGISTRY,
                POLARITON_V1.family_id,
                POLARITON_V1.revision,
                P_POLARITON_Z24_L2_P0.to_vec(),
                &topo,
            )
            .unwrap(),
        );
        let lease = build_lease_job(&snap, MINER, 0, 32, 1).generator.unwrap();
        let verifier = LeaseTopology::from_topology(&topo, REGISTRY).unwrap();
        (topo, lease, verifier)
    }

    /// A polariton lease, its verifier, and an honestly scored report whose one
    /// witness is feasible or not as asked.
    fn polariton_report(want_feasible: bool) -> (IsingProblemGenerator, LeaseTopology, JobResult) {
        let (topo, lease, verifier) = polariton();
        let (instance, salt, nonce) = (0..32)
            .find_map(|index| instance_of(REGISTRY, &lease, &topo, index))
            .expect("a salt yields an instance");
        let descriptor = lease.family.as_ref().unwrap();
        let admitted = admit(
            REGISTRY,
            descriptor.family_id.as_bytes(),
            descriptor.revision,
            &descriptor.params,
            &descriptor.params_digest,
        )
        .unwrap();
        let n = instance.node_ids().len();
        assert!(n <= 20, "the witness search is exhaustive: {n} nodes");
        let spins = (0u32..1 << n)
            .map(|bits| {
                (0..n)
                    .map(|k| if bits >> k & 1 == 1 { -1 } else { 1 })
                    .collect::<Vec<i8>>()
            })
            .find(|spins| feasible(&admitted, &instance, spins) == want_feasible)
            .expect("the instance has a witness of each kind");
        let result = JobResult {
            job_id: b"lease".to_vec(),
            solutions: vec![Solution {
                spins: encode_spins_packed(&spins),
                energy_milli: energy(&instance, &spins),
            }],
            salt: salt.to_vec(),
            nonce: nonce.to_vec(),
            ..JobResult::default()
        };
        (lease, verifier, result)
    }

    #[test]
    fn a_feasible_hard_family_witness_verifies() {
        let (lease, verifier, result) = polariton_report(true);
        let (_, validated) = verify_and_select(&lease, &verifier, &open_target(), &result).unwrap();
        assert!(validated.accepted);
    }

    /// CO-4: a family witness this coordinator accepts reaches the chain as a
    /// witness the validator accepts. The proof is packed the way
    /// `submit_proof` packs it, and the pallet's own steps check it: the nonce
    /// as a `U256`, the instance regenerated from its big-endian bytes, each
    /// row unpacked at the instance size, then `FamilyChallenge::verify_witness`.
    #[test]
    fn an_accepted_family_witness_passes_the_validator_check() {
        use quantum_validation::families::{AdvertisedLimits, FamilyChallenge, CONTRACTS};
        use quantum_validation::packed::unpack_solution;
        use quantum_validation::AllowedValueSpec;
        use quip_coordinator::chain::proof_encode::{build_quantum_proof, ProofBuildContext};
        use quip_coordinator::chain::submit::Proof;

        let (lease, verifier, result) = polariton_report(true);
        let (_, validated) = verify_and_select(&lease, &verifier, &open_target(), &result).unwrap();
        let salt: [u8; 32] = result.salt.as_slice().try_into().unwrap();
        let rows = validated.selected_solutions;
        let spin_set = vec![-1000, 1000];
        let quantum = build_quantum_proof(
            &Proof {
                job_id: result.job_id.clone(),
                best_energy_milli: validated.best_energy_milli,
                diversity_milli: validated.diversity_milli,
                n_valid: validated.n_valid,
                solutions: rows.clone(),
                is_pow: true,
                order_id: Vec::new(),
                generation: 1,
                salt: salt.to_vec(),
                device_access_time_us: 0,
            },
            &ProofBuildContext {
                topology_hash: [0; 32],
                last_proof_block_hash: LAST_PROOF,
                miner_identity: MINER,
                salt,
                num_nodes: rows[0].spins.len(),
                allowed_spin: AllowedValueSpec::Set(spin_set.clone()),
            },
        )
        .unwrap();
        assert_eq!(
            quantum.nonce.to_big_endian().to_vec(),
            result.nonce,
            "the lease nonce is the chain nonce in big-endian bytes"
        );

        let blob = quip_solver_conformance::topology(ROUTING_TOPOLOGY).unwrap();
        let descriptor = lease.family.as_ref().unwrap();
        let challenge = FamilyChallenge::admit(
            CONTRACTS,
            descriptor.family_id.as_bytes(),
            descriptor.revision,
            &descriptor.params,
            &blob.nodes,
            &blob.edges,
            &AdvertisedLimits {
                max_nodes: 5_000,
                max_edges: 50_000,
                max_degree: 0,
            },
        )
        .unwrap();
        let instance = challenge.regenerate(quantum.nonce.to_big_endian()).unwrap();
        let spec = AllowedValueSpec::Set(spin_set.as_slice());
        for (packed, row) in quantum.solutions.iter().zip(&rows) {
            let milli = unpack_solution(packed, instance.node_ids().len(), &spec).unwrap();
            let spins: Vec<i8> = milli.iter().map(|&v| if v > 0 { 1 } else { -1 }).collect();
            assert_eq!(spins, row.spins);
            assert_eq!(
                challenge.verify_witness(&instance, &spins),
                Ok(row.energy_milli)
            );
        }
    }

    /// The rescore alone accepts this report: its energy is honest. Only the
    /// family's hard constraint refuses it.
    #[test]
    fn an_infeasible_hard_family_witness_is_forged() {
        let (lease, verifier, result) = polariton_report(false);
        let err = verify_and_select(&lease, &verifier, &open_target(), &result).unwrap_err();
        assert!(err.contains("hard constraint"), "{err}");
    }
}
