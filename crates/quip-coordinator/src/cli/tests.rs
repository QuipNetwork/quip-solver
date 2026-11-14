//! Argument parsing and the `drive` subcommand.

use super::drive::*;
use super::{Cli, Command};
use crate::chain::DEFAULT_SEED_DIFFICULTY;
use crate::types::log::LogLevel;
use crate::types::topology::Topology;
use crate::types::topology_spec::{parse_topology_spec, preset_spec, TopologySpec};
use clap::Parser as _;
use quip_protocol::session::ExitCode;
use std::path::PathBuf;
use std::process::ExitCode as StdExitCode;

/// The drive counterpart of the `[cuda.N]` bug: without `--device`, a
/// drive run on a multi-GPU host silently lands on GPU 0. Backends whose
/// CLI has no `--device` must not be sent one, so the absent flag has to
/// stay absent rather than defaulting to 0.
#[test]
fn drive_device_flag_reaches_the_launch_entry() {
    let mut args = drive_args(DriveSourceKind::Random);
    assert_eq!(drive_entry(&args).device, None);
    args.device = Some(1);
    assert_eq!(drive_entry(&args).device, Some(1));
}

fn drive_args(source: DriveSourceKind) -> DriveArgs {
    DriveArgs {
        family_id: None,
        family_revision: 1,
        family_params: None,
        miner: PathBuf::from("miner"),
        source,
        topology: None,
        topology_preset: None,
        topology_from_chain: None,
        count: 10,
        seed: 0,
        lease_size: 64,
        list: None,
        target_energy: None,
        min_solutions: None,
        num_reads: None,
        num_sweeps: None,
        deadline_ms: 1000,
        device: None,
        report: None,
        utilization: None,
        yielding: false,
    }
}

#[test]
fn drive_topology_selectors_conflict_and_chain_selector_parses() {
    for selectors in [
        ["--topology", "missing.json", "--topology-preset", "aglais"],
        [
            "--topology",
            "missing.json",
            "--topology-from-chain",
            "ws://fixture",
        ],
        [
            "--topology-preset",
            "aglais",
            "--topology-from-chain",
            "ws://fixture",
        ],
    ] {
        let error = Cli::try_parse_from(
            [
                "quip-coordinator",
                "drive",
                "--miner",
                "miner",
                "--source",
                "random",
            ]
            .into_iter()
            .chain(selectors),
        )
        .expect_err("expected conflicting topology selectors");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    for source in ["random", "lease", "list"] {
        let cli = Cli::try_parse_from([
            "quip-coordinator",
            "drive",
            "--miner",
            "miner",
            "--source",
            source,
            "--topology-from-chain",
            "ws://fixture",
        ])
        .expect("chain topology selector parses");
        let Some(Command::Drive(args)) = cli.command else {
            panic!("expected drive command");
        };
        assert_eq!(args.topology_from_chain.as_deref(), Some("ws://fixture"));
        assert!(args.topology.is_none());
        assert!(args.topology_preset.is_none());
    }
}

#[tokio::test]
async fn drive_resolves_default_presets_named_presets_and_files() {
    for source in [DriveSourceKind::Random, DriveSourceKind::Lease] {
        let args = drive_args(source);
        let spec = resolve_topology(&args)
            .await
            .expect("resolve default topology")
            .expect("missing default preset");
        assert_eq!(spec.topology.nodes.len(), 4_575);
        assert_eq!(spec.topology.edge_pairs().len(), 41_474);
        assert_eq!(spec.allowed_h_milli, vec![0]);
    }

    let args = drive_args(DriveSourceKind::List);
    assert!(resolve_topology(&args)
        .await
        .expect("resolve list topology")
        .is_none());

    let mut args = drive_args(DriveSourceKind::Random);
    args.topology_preset = Some("aglais".into());
    let spec = resolve_topology(&args)
        .await
        .expect("resolve Aglais")
        .expect("missing Aglais");
    assert_eq!(spec.topology.edge_pairs().len(), 41_514);
    let file = tempfile::NamedTempFile::new().expect("temporary topology file");
    std::fs::write(file.path(), preset_spec("smoke").expect("smoke preset"))
        .expect("write topology file");
    args.topology_preset = None;
    args.topology = Some(file.path().to_path_buf());
    let spec = resolve_topology(&args)
        .await
        .expect("resolve topology file")
        .expect("missing file topology");
    let smoke = parse_topology_spec(preset_spec("smoke").expect("smoke preset"))
        .expect("valid smoke topology");
    assert_eq!(spec.topology.hash, smoke.topology.hash);
    args.topology = None;
    args.topology_preset = Some("unknown".into());
    assert!(resolve_topology(&args).await.is_err());
}

fn resolved_chain_fixture() -> Result<TopologySpec, String> {
    use crate::chain::codec::scale_types::TopologyMetaScale;
    use quantum_validation::AllowedValueSpec;
    let spec = parse_topology_spec(
        r#"{"nodes":[30,10],"edges":[[30,10]],
            "allowed_h_milli":[0],"allowed_j_milli":[1000,-1000]}"#,
    )
    .map_err(|error| error.to_string())?;
    let hash = spec
        .topology
        .hash
        .as_slice()
        .try_into()
        .map_err(|_| "topology hash is not 32 bytes".to_string())?;
    let edges = spec.topology.edge_pairs();
    TopologyMetaScale {
        nodes: spec.topology.nodes,
        edges,
        allowed_h_values: AllowedValueSpec::Set(spec.allowed_h_milli),
        allowed_j_values: AllowedValueSpec::Set(spec.allowed_j_milli),
        allowed_spin_values: AllowedValueSpec::Set(spec.allowed_spin_milli),
        family: None,
        registered_at: 99,
    }
    .into_topology_spec(hash)
}

fn assert_chain_topology(topology: &Topology, expected_hash: &[u8]) {
    assert_eq!(topology.nodes, vec![30, 10]);
    assert_eq!(topology.edge_pairs(), vec![(30, 10)]);
    assert_eq!(topology.allowed_h, vec![0]);
    assert_eq!(topology.allowed_j, vec![1000, -1000]);
    assert_eq!(topology.hash, expected_hash);
}

#[test]
fn drive_resolved_chain_topology_drives_random_jobs_and_overrides() {
    use crate::types::problem::problem_milli;
    let spec = resolved_chain_fixture().expect("valid chain fixture");
    let expected_hash = spec.topology.hash.clone();
    let mut args = drive_args(DriveSourceKind::Random);
    args.count = 2;
    args.target_energy = Some(-4.0);
    args.min_solutions = Some(2);
    args.num_reads = Some(3);
    args.num_sweeps = Some(10);
    let (jobs, topology, target) =
        build_jobs(&args, Some(spec), 0).expect("build random jobs from chain topology");
    assert_chain_topology(&topology.expect("missing topology"), &expected_hash);
    let target = target.expect("missing target");
    assert_eq!(target.max_energy_milli, -4000);
    assert_eq!(target.min_solutions, 2);
    assert_eq!(target.num_reads, 3);
    assert_eq!(target.num_sweeps, 10);
    assert_eq!(jobs.len(), 2);
    for job in jobs {
        let problem = job.ising.as_ref().expect("missing problem");
        let (h, j) = problem_milli(problem).expect("valid random coefficients");
        assert_eq!(h, vec![0, 0]);
        assert_eq!(j.len(), 1);
        assert_eq!(
            problem.graph,
            Some(quip_proto::v1::ising_problem::Graph::TopologyHash(
                expected_hash.clone()
            ))
        );
    }
}

#[test]
fn drive_resolved_chain_topology_drives_leases_and_nonce_replay() {
    use crate::types::problem::problem_milli;
    let spec = resolved_chain_fixture().expect("valid chain fixture");
    let hash = spec.topology.hash.clone();
    let mut lease_args = drive_args(DriveSourceKind::Lease);
    lease_args.count = 3;
    lease_args.lease_size = 2;
    let (jobs, topology, target) =
        build_jobs(&lease_args, Some(spec.clone()), 0).expect("build leases from chain topology");
    assert_chain_topology(&topology.expect("missing lease topology"), &hash);
    let target = target.expect("missing lease target");
    assert_eq!(target.min_solutions, 0);
    assert_eq!(target.max_energy_milli, i64::MAX);
    assert_eq!(target.min_diversity_milli, 0);
    assert_eq!(jobs.len(), 2);
    for (job, count) in jobs.iter().zip([2, 1]) {
        let generator = job.generator.as_ref().expect("missing generator");
        assert_eq!(generator.topology_hash, hash);
        assert_eq!(generator.salt_count, count);
    }
    let file = tempfile::NamedTempFile::new().expect("temporary nonce list");
    std::fs::write(
        file.path(),
        concat!(
            r#"{"nonce":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
            "\n"
        ),
    )
    .expect("write nonce list");
    let mut list_args = drive_args(DriveSourceKind::List);
    list_args.list = Some(file.path().to_path_buf());
    let (jobs, topology, _) =
        build_jobs(&list_args, Some(spec), 0).expect("build nonce replay from chain topology");
    assert_chain_topology(&topology.expect("missing replay topology"), &hash);
    assert_eq!(jobs.len(), 1);
    let problem = jobs
        .first()
        .and_then(|job| job.ising.as_ref())
        .expect("missing replay problem");
    assert_eq!(
        problem_milli(problem).expect("valid replay coefficients").0,
        vec![0, 0]
    );
    assert!(build_jobs(&list_args, None, 0).is_err());
}

#[tokio::test]
async fn drive_chain_failure_does_not_fall_back_or_start_a_miner() {
    let mut args = drive_args(DriveSourceKind::Random);
    args.topology_from_chain = Some("://invalid-url".into());
    args.miner = PathBuf::from("/nonexistent/quip-miner-QUI-1539");
    let error = resolve_topology(&args)
        .await
        .expect_err("expected invalid RPC URL failure");
    assert!(error.contains("fetch topology from ://invalid-url"));
    assert_eq!(
        drive_main(args, LogLevel::Info).await,
        StdExitCode::from(ExitCode::ConfigInvalid as u8)
    );
}

#[test]
fn qui_1540_drive_accepts_signed_energy_values() {
    let spec = parse_topology_spec(preset_spec("smoke").expect("smoke preset exists"))
        .expect("smoke preset is valid");
    for (value, expected_milli) in [
        ("-10000", -10_000_000),
        ("-10000.125", -10_000_125),
        ("-0.125", -125),
        ("0", 0),
        ("10.5", 10_500),
    ] {
        let equals = format!("--target-energy={value}");
        for energy_args in [vec!["--target-energy", value], vec![equals.as_str()]] {
            let mut argv = vec![
                "quip-coordinator",
                "drive",
                "--miner",
                "miner",
                "--source",
                "list",
                "--list",
                "nonces.jsonl",
                "--topology",
                "spec.json",
            ];
            argv.extend(energy_args);
            argv.extend(["--num-reads", "64"]);
            let cli = Cli::try_parse_from(argv).expect("signed drive energy parses");
            let Some(Command::Drive(args)) = cli.command else {
                panic!("expected drive subcommand");
            };
            assert_eq!(args.num_reads, Some(64));
            assert_eq!(
                set_target_from_spec(&spec, &args).max_energy_milli,
                expected_milli
            );
        }
    }
}

#[test]
fn qui_1540_seed_chain_accepts_signed_milli_values_and_keeps_the_default() {
    for (value, expected_milli) in [
        ("-2500000", -2_500_000),
        ("-1", -1),
        ("-9223372036854775808", i64::MIN),
        ("0", 0),
        ("2500000", 2_500_000),
    ] {
        let equals = format!("--max-energy-milli={value}");
        for energy_args in [vec!["--max-energy-milli", value], vec![equals.as_str()]] {
            let mut argv = vec!["quip-coordinator", "seed-chain", "--sudo-key", "//Alice"];
            argv.extend(energy_args);
            let cli = Cli::try_parse_from(argv).expect("signed seed-chain energy parses");
            let Some(Command::SeedChain(args)) = cli.command else {
                panic!("expected seed-chain subcommand");
            };
            assert_eq!(args.max_energy_milli, expected_milli);
        }
    }

    let cli = Cli::try_parse_from(["quip-coordinator", "seed-chain", "--sudo-key", "//Alice"])
        .expect("default seed-chain arguments parse");
    let Some(Command::SeedChain(args)) = cli.command else {
        panic!("expected seed-chain subcommand");
    };
    assert_eq!(
        args.max_energy_milli,
        DEFAULT_SEED_DIFFICULTY.max_energy_milli
    );
}

#[test]
fn qui_1540_energy_options_keep_numeric_and_unsigned_constraints() {
    for value in ["--bogus", "-oops"] {
        assert!(Cli::try_parse_from([
            "quip-coordinator",
            "drive",
            "--miner",
            "miner",
            "--source",
            "list",
            "--target-energy",
            value,
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "quip-coordinator",
            "seed-chain",
            "--sudo-key",
            "//Alice",
            "--max-energy-milli",
            value,
        ])
        .is_err());
    }
    assert!(Cli::try_parse_from([
        "quip-coordinator",
        "drive",
        "--miner",
        "miner",
        "--source",
        "list",
        "--target-energy",
        "--num-reads",
        "64",
    ])
    .is_err());
    assert!(Cli::try_parse_from([
        "quip-coordinator",
        "drive",
        "--miner",
        "miner",
        "--source",
        "random",
        "--count",
        "-1",
    ])
    .is_err());
    assert!(Cli::try_parse_from([
        "quip-coordinator",
        "seed-chain",
        "--sudo-key",
        "//Alice",
        "--max-energy-milli=-0.5",
    ])
    .is_err());
}
