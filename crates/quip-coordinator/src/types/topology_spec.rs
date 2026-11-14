//! Topology specs: the JSON format, its parser, and the presets embedded in
//! the binary. A spec yields a validated `TopologySpec` and a synthetic
//! `MiningSnapshot`.

use crate::types::snapshot::{MiningSnapshot, TARGET_PROOF_BLOCKS};
use crate::types::topology::{Topology, DEFAULT_SPIN_SET};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct GatesJson {
    #[serde(default)]
    min_solutions: u32,
    #[serde(default = "default_max_energy_milli")]
    max_energy_milli: i64,
    #[serde(default)]
    min_diversity_milli: u32,
}

fn default_max_energy_milli() -> i64 {
    i64::MAX
}

impl Default for GatesJson {
    fn default() -> Self {
        Self {
            min_solutions: 0,
            max_energy_milli: default_max_energy_milli(),
            min_diversity_milli: 0,
        }
    }
}

#[derive(Debug, Deserialize)]
struct TopologySpecJson {
    nodes: Vec<u32>,
    #[serde(default)]
    edges: Vec<(u32, u32)>,
    allowed_h_milli: Vec<i32>,
    allowed_j_milli: Vec<i32>,
    /// Allowed solution-spin values; defaults to the binary set `±1000` when
    /// omitted, matching a chain topology with no explicit spin spec.
    #[serde(default)]
    allowed_spin_milli: Vec<i32>,
    #[serde(default)]
    gates: Option<GatesJson>,
}

/// Errors parsing/validating a drive-mode topology-spec file.
#[derive(Debug, PartialEq)]
pub enum TopologySpecError {
    /// JSON decode failure (message is the serde error).
    Json(String),
    /// `nodes` array was empty.
    EmptyNodes,
    /// `allowed_h_milli` array was empty.
    EmptyAllowedH,
    /// `allowed_j_milli` array was empty.
    EmptyAllowedJ,
    /// An `edges` endpoint is a node id not present in `nodes`. Edges reference
    /// native node ids (possibly sparse), which the miner and consensus scorer
    /// map to dense positions — so an id just has to exist in `nodes`.
    EdgeUnknownNode {
        /// Zero-based index of the bad edge in the `edges` array.
        edge_index: usize,
        /// Node id that was not present in `nodes`.
        endpoint: u32,
    },
}

impl std::fmt::Display for TopologySpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(e) => write!(f, "invalid topology spec JSON: {e}"),
            Self::EmptyNodes => write!(f, "topology spec: nodes must be non-empty"),
            Self::EmptyAllowedH => {
                write!(f, "topology spec: allowed_h_milli must be non-empty")
            }
            Self::EmptyAllowedJ => {
                write!(f, "topology spec: allowed_j_milli must be non-empty")
            }
            Self::EdgeUnknownNode {
                edge_index,
                endpoint,
            } => write!(
                f,
                "topology spec: edge {edge_index} references node id {endpoint}, \
                 which is not in the topology"
            ),
        }
    }
}

impl std::error::Error for TopologySpecError {}

/// Parsed drive-mode topology spec: a `Topology` (for the wire message) plus
/// the golden-draw + gate parameters needed to build a synthetic
/// `MiningSnapshot`.
#[derive(Debug, Clone)]
pub struct TopologySpec {
    /// Topology wire message (nodes, edges, hash) sent to the miner.
    pub topology: Topology,
    /// Allowed h-field values (milli) for golden draw.
    pub allowed_h_milli: Vec<i32>,
    /// Allowed J-coupling values (milli) for golden draw.
    pub allowed_j_milli: Vec<i32>,
    /// Allowed solution-spin values (milli); defaults to `±1000` when omitted.
    pub allowed_spin_milli: Vec<i32>,
    /// Minimum number of solutions required to pass quality gates.
    pub min_solutions: u32,
    /// Maximum (worst) energy (milli) accepted by quality gates.
    pub max_energy_milli: i64,
    /// Minimum diversity (milli) required to pass quality gates.
    pub min_diversity_milli: u32,
}

impl TopologySpec {
    /// Build a spec from graph and value sets, with the gates of a JSON spec
    /// that has none.
    ///
    /// # Errors
    /// Returns [`TopologySpecError`] when a required set is empty or an edge
    /// references an unknown node id.
    pub fn from_sets(
        nodes: Vec<u32>,
        edges: Vec<(u32, u32)>,
        allowed_h_milli: Vec<i32>,
        allowed_j_milli: Vec<i32>,
        allowed_spin_milli: Vec<i32>,
    ) -> Result<Self, TopologySpecError> {
        validate_spec(TopologySpecJson {
            nodes,
            edges,
            allowed_h_milli,
            allowed_j_milli,
            allowed_spin_milli,
            gates: None,
        })
    }

    /// Build a synthetic `MiningSnapshot` for generated or replayed drive work.
    /// Round hashes, block number, and runtime version use synthetic defaults.
    #[must_use]
    pub fn to_snapshot(&self) -> MiningSnapshot {
        MiningSnapshot {
            head_hash: [0u8; 32],
            last_proof_block_hash: [0u8; 32],
            topology_hash: self.topology.hash.clone(),
            nodes: self.topology.nodes.clone(),
            edges: self.topology.edge_pairs(),
            allowed_h_milli: self.allowed_h_milli.clone(),
            allowed_j_milli: self.allowed_j_milli.clone(),
            allowed_spin_milli: self.allowed_spin_milli.clone(),
            min_solutions: self.min_solutions,
            max_energy_milli: self.max_energy_milli,
            min_diversity_milli: self.min_diversity_milli,
            block_number: 0,
            spec_version: 0,
            target_proof_blocks: TARGET_PROOF_BLOCKS,
            family: None,
        }
    }
}

fn validate_edges(nodes: &[u32], edges: &[(u32, u32)]) -> Result<(), TopologySpecError> {
    let node_set: std::collections::HashSet<u32> = nodes.iter().copied().collect();
    for (edge_index, &(u, v)) in edges.iter().enumerate() {
        for endpoint in [u, v] {
            if !node_set.contains(&endpoint) {
                return Err(TopologySpecError::EdgeUnknownNode {
                    edge_index,
                    endpoint,
                });
            }
        }
    }
    Ok(())
}

/// Parse and validate a drive-mode topology-spec JSON document.
///
/// # Errors
///
/// Returns [`TopologySpecError`] when the JSON is invalid, required fields are
/// empty, or an edge references an unknown node id.
pub fn parse_topology_spec(text: &str) -> Result<TopologySpec, TopologySpecError> {
    let raw: TopologySpecJson =
        serde_json::from_str(text).map_err(|error| TopologySpecError::Json(error.to_string()))?;
    validate_spec(raw)
}

fn validate_spec(raw: TopologySpecJson) -> Result<TopologySpec, TopologySpecError> {
    if raw.nodes.is_empty() {
        return Err(TopologySpecError::EmptyNodes);
    }
    if raw.allowed_h_milli.is_empty() {
        return Err(TopologySpecError::EmptyAllowedH);
    }
    if raw.allowed_j_milli.is_empty() {
        return Err(TopologySpecError::EmptyAllowedJ);
    }
    validate_edges(&raw.nodes, &raw.edges)?;

    let allowed_spin_milli = if raw.allowed_spin_milli.is_empty() {
        DEFAULT_SPIN_SET.to_vec()
    } else {
        raw.allowed_spin_milli
    };
    let topology = Topology::from_nodes_edges(
        raw.nodes,
        raw.edges,
        &raw.allowed_h_milli,
        &raw.allowed_j_milli,
        &allowed_spin_milli,
    );
    let gates = raw.gates.unwrap_or_default();
    Ok(TopologySpec {
        topology,
        allowed_h_milli: raw.allowed_h_milli,
        allowed_j_milli: raw.allowed_j_milli,
        allowed_spin_milli,
        min_solutions: gates.min_solutions,
        max_energy_milli: gates.max_energy_milli,
        min_diversity_milli: gates.min_diversity_milli,
    })
}

/// The live D-Wave Advantage2 graph, Zephyr z(12, 4) with the removed qubits
/// and couplers of the 2026-09-21 hardware snapshot: 4575 nodes and 41474
/// edges. It is solver-core's `zephyr-z12t4-live` topology, the graph its
/// live twisted-ice-dimer recipes are frozen on, and the file is
/// byte-identical to the snapshot solver-core pins by sha256.
const ZEPHYR_Z12T4_LIVE: &str = include_str!("../../fixtures/drive/zephyr-z12t4-live.spec.json");

/// The graph the spec-119 Aglais chain registered: an older Advantage2
/// snapshot (4577 nodes) without coupler (938, 2812), and `h` fixed at 0.
const AGLAIS: &str = include_str!("../../fixtures/drive/aglais.spec.json");

/// A small graph for fast smoke runs.
const SMOKE: &str = include_str!("../../fixtures/drive/smoke.spec.json");

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

    const VALID: &str = r#"{
        "nodes": [0, 1, 2, 3],
        "edges": [[0, 1], [1, 2], [2, 3], [0, 3]],
        "allowed_h_milli": [-1000, 0, 1000],
        "allowed_j_milli": [-1000, 1000]
    }"#;

    #[test]
    fn parses_valid_specs() {
        // Default gates and snapshot.
        let spec = parse_topology_spec(VALID).unwrap();
        assert_eq!(spec.topology.nodes.len(), 4);
        assert_eq!(spec.topology.edge_pairs().len(), 4);
        assert_eq!(spec.min_solutions, 0);
        assert_eq!(spec.max_energy_milli, i64::MAX);
        assert_eq!(spec.min_diversity_milli, 0);
        let snap = spec.to_snapshot();
        assert_eq!(snap.topology_hash, spec.topology.hash);
        assert_eq!(snap.nodes, spec.topology.nodes);

        // Explicit gates.
        let text = r#"{
            "nodes": [0, 1],
            "edges": [[0, 1]],
            "allowed_h_milli": [1000],
            "allowed_j_milli": [1000],
            "gates": { "min_solutions": 5, "max_energy_milli": -100, "min_diversity_milli": 200 }
        }"#;
        let spec = parse_topology_spec(text).unwrap();
        assert_eq!(spec.min_solutions, 5);
        assert_eq!(spec.max_energy_milli, -100);
        assert_eq!(spec.min_diversity_milli, 200);

        // Native D-Wave-style sparse ids (gaps): edges reference ids, not
        // positions. The miner and consensus scorer map ids to positions.
        let text = r#"{
            "nodes": [0, 12, 2400],
            "edges": [[0, 12], [12, 2400]],
            "allowed_h_milli": [-1000, 0, 1000],
            "allowed_j_milli": [-1000, 1000]
        }"#;
        let spec = parse_topology_spec(text).unwrap();
        assert_eq!(spec.topology.nodes, vec![0, 12, 2400]);
        assert_eq!(spec.topology.edge_pairs(), vec![(0, 12), (12, 2400)]);
    }

    #[test]
    fn rejects_invalid_specs() {
        assert!(matches!(
            parse_topology_spec("not json"),
            Err(TopologySpecError::Json(_))
        ));
        let cases = [
            (
                "empty nodes",
                r#"{"nodes":[],"edges":[],"allowed_h_milli":[1],"allowed_j_milli":[1]}"#,
                TopologySpecError::EmptyNodes,
            ),
            (
                "empty allowed h",
                r#"{"nodes":[0],"edges":[],"allowed_h_milli":[],"allowed_j_milli":[1]}"#,
                TopologySpecError::EmptyAllowedH,
            ),
            (
                "empty allowed j",
                r#"{"nodes":[0],"edges":[],"allowed_h_milli":[1],"allowed_j_milli":[]}"#,
                TopologySpecError::EmptyAllowedJ,
            ),
            (
                "edge with unknown node id",
                r#"{"nodes":[0,1],"edges":[[0,5]],"allowed_h_milli":[1000],"allowed_j_milli":[1000]}"#,
                TopologySpecError::EdgeUnknownNode {
                    edge_index: 0,
                    endpoint: 5,
                },
            ),
        ];
        for (label, text, expected) in cases {
            assert_eq!(parse_topology_spec(text).unwrap_err(), expected, "{label}");
        }
    }

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
            crate::types::hex::hex_encode(&spec.topology.hash),
            "0xcbec1eb4e9dcc7ae343b386902d4dbba19b694a66cebf95bc06bd9260c3270e7"
        );
    }
}
