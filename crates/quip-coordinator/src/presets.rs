//! Built-in topology specs, embedded in the binary.

/// The live D-Wave Advantage2 graph, Zephyr z(12, 4) with the removed qubits
/// and couplers of the 2026-09-21 hardware snapshot: 4575 nodes and 41474
/// edges. It is solver-core's `zephyr-z12t4-live` topology, the graph its
/// live twisted-ice-dimer recipes are frozen on, and the file is
/// byte-identical to the snapshot solver-core pins by sha256.
const ZEPHYR_Z12T4_LIVE: &str = include_str!("../fixtures/drive/zephyr-z12t4-live.spec.json");

/// The graph the spec-119 Aglais chain registered: an older Advantage2
/// snapshot (4577 nodes) without coupler (938, 2812), and `h` fixed at 0.
const AGLAIS: &str = include_str!("../fixtures/drive/aglais.spec.json");

/// A small graph for fast smoke runs.
const SMOKE: &str = include_str!("../fixtures/drive/smoke.spec.json");

/// Every preset name this binary understands.
pub const PRESET_NAMES: [&str; 3] = ["zephyr-z12t4-live", "aglais", "smoke"];

/// Return the embedded topology-spec JSON for `name`.
///
/// # Errors
/// Returns an error for an unknown name.
pub fn preset_spec(name: &str) -> Result<&'static str, String> {
    match name {
        "zephyr-z12t4-live" => Ok(ZEPHYR_Z12T4_LIVE),
        "aglais" => Ok(AGLAIS),
        "smoke" => Ok(SMOKE),
        other => Err(format!(
            "unknown topology preset {other:?}; valid presets: {}",
            PRESET_NAMES.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::parse_topology_spec;

    #[test]
    fn every_listed_preset_resolves_and_parses_and_unknown_names_the_choices() {
        for name in PRESET_NAMES {
            let text = preset_spec(name)
                .unwrap_or_else(|e| panic!("listed preset {name} must resolve: {e}"));
            assert!(
                parse_topology_spec(text).is_ok(),
                "listed preset {name} must parse"
            );
        }
        assert!(PRESET_NAMES.contains(&"aglais"));

        for name in ["nope", "unknown"] {
            let err = preset_spec(name).expect_err("unknown preset is an error");
            assert!(err.contains(name), "error names the input: {err}");
            assert!(
                err.contains("zephyr-z12t4-live"),
                "error lists choices: {err}"
            );
            assert!(err.contains("aglais"), "error lists choices: {err}");
        }
    }

    /// The live preset is solver-core's `zephyr-z12t4-live` graph, in the same
    /// node and edge order.
    #[test]
    fn the_live_preset_is_the_solver_core_live_graph() {
        let spec = parse_topology_spec(preset_spec("zephyr-z12t4-live").expect("live preset"))
            .expect("live preset parses");
        let live = quip_solver_conformance::topology("../topologies/zephyr-z12t4-live.bin.gz")
            .expect("solver-core live topology");
        assert_eq!(spec.topology.nodes.len(), 4_575);
        assert_eq!(spec.topology.edge_pairs().len(), 41_474);
        assert_eq!(spec.topology.nodes, live.nodes);
        assert_eq!(spec.topology.edge_pairs(), live.edges);
        assert_eq!(spec.allowed_h_milli, vec![0]);
        assert_eq!(spec.allowed_j_milli, vec![-1000, 1000]);
        assert_eq!(spec.allowed_spin_milli, vec![-1000, 1000]);
    }

    /// The Aglais preset keeps the topology hash the spec-119 chain registered.
    #[test]
    fn aglais_matches_chain_identity() {
        let spec = parse_topology_spec(preset_spec("aglais").expect("Aglais preset resolves"))
            .expect("Aglais preset parses");
        assert_eq!(spec.topology.nodes.len(), 4_577);
        let edges = spec.topology.edge_pairs();
        assert_eq!(edges.len(), 41_514);
        assert!(!edges.contains(&(938, 2812)));
        assert!(!edges.contains(&(2812, 938)));
        assert_eq!(spec.allowed_h_milli, vec![0]);
        assert_eq!(spec.allowed_j_milli, vec![-1000, 1000]);
        assert_eq!(spec.allowed_spin_milli, vec![-1000, 1000]);
        assert_eq!(
            crate::chain::extrinsic::hex_encode(&spec.topology.hash),
            "0xcbec1eb4e9dcc7ae343b386902d4dbba19b694a66cebf95bc06bd9260c3270e7"
        );
    }

    #[test]
    fn aglais_random_jobs_have_zero_fields_and_the_chain_edge_count() {
        use crate::drive::{drain_all, RandomSource};
        use crate::producer::problem::problem_milli;

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
