//! The `drive` subcommand: resolve a topology, build synthetic jobs, run one
//! miner against them, and report. Nothing is submitted.

use crate::app::local_env::{
    aggregate, drain_all, print_table, run_drive, write_jsonl, DriveManyParams, LeaseSource,
    ListSource, RandomSource,
};
use crate::chain::RealChainClient;
use crate::config::launch::LaunchEntry;
use crate::types::log::LogLevel;
use crate::types::miner::MinerKind;
use crate::types::topology::Topology;
use crate::types::topology_spec::{parse_topology_spec, preset_spec, TopologySpec};
use clap::ValueEnum;
use quip_proto::v1::{Configure, Job};
use quip_protocol::session::ExitCode;
use std::path::PathBuf;
use std::process::ExitCode as StdExitCode;

#[derive(ValueEnum, Clone, Debug)]
pub(crate) enum DriveSourceKind {
    Random,
    Lease,
    List,
}

#[derive(clap::Args, Debug)]
#[command(group(
    clap::ArgGroup::new("drive_topology")
        .args(["topology", "topology_preset", "topology_from_chain"])
        .multiple(false)
))]
pub(crate) struct DriveArgs {
    /// Miner binary to spawn.
    #[arg(long)]
    pub(crate) miner: PathBuf,
    /// Job source: random problems, salt leases, or a JSONL replay list.
    #[arg(long, value_enum)]
    pub(crate) source: DriveSourceKind,
    /// Topology spec JSON. Random and lease default to Advantage2 when no
    /// topology selector is given. Nonce-ref list entries need a topology.
    #[arg(long)]
    pub(crate) topology: Option<PathBuf>,
    /// Built-in topology: `zephyr-z12t4-live`, `aglais`, or `smoke`.
    #[arg(long)]
    pub(crate) topology_preset: Option<String>,
    /// Read the validator's default topology once from this RPC URL.
    /// Mutually exclusive with `--topology` and `--topology-preset`.
    #[arg(long, value_name = "URL")]
    pub(crate) topology_from_chain: Option<String>,
    /// Number of problems (`random`) or salts (`lease`) to draw.
    #[arg(long, default_value_t = 10)]
    pub(crate) count: u32,
    /// Draw seed: same seed + topology draws the same jobs (`random` or `lease`).
    #[arg(long, default_value_t = 0)]
    pub(crate) seed: u64,
    /// Salts per lease (`--source lease`). `--count` is the total salt count.
    #[arg(long, default_value_t = 64)]
    pub(crate) lease_size: u64,
    /// Problem family for `--source lease`. Requires `--family-params`.
    #[arg(long, requires = "family_params")]
    pub(crate) family_id: Option<String>,
    /// Revision of `--family-id`.
    #[arg(long, default_value_t = 1)]
    pub(crate) family_revision: u32,
    /// File holding the canonical parameter bytes of `--family-id`.
    #[arg(long, requires = "family_id")]
    pub(crate) family_params: Option<PathBuf>,
    /// JSONL model list (`--source list`).
    #[arg(long)]
    pub(crate) list: Option<PathBuf>,
    /// Difficulty target energy (the adapt target, in energy units). Overrides
    /// the spec's gate; the miner adapts its reads/sweeps from this.
    #[arg(long, allow_negative_numbers = true)]
    pub(crate) target_energy: Option<f64>,
    /// Minimum unique solutions gate. Overrides the spec.
    #[arg(long)]
    pub(crate) min_solutions: Option<u32>,
    /// Pin `num_reads` via the `SetTarget` control-plane override (bypasses adapt).
    /// Useful for the dwave/QPU path, which does not adapt yet.
    #[arg(long)]
    pub(crate) num_reads: Option<u32>,
    /// Pin `num_sweeps` via the `SetTarget` override (bypasses adapt). Pairs with
    /// `--num-reads` for controlled, matched-condition throughput/parity runs.
    #[arg(long)]
    pub(crate) num_sweeps: Option<u32>,
    /// Per-job deadline, milliseconds from now. 0 = no deadline (the default);
    /// a deadline is only meaningful for real mempool/chain jobs.
    #[arg(long, default_value_t = 0)]
    pub(crate) deadline_ms: u64,
    /// Optional path to write a per-job + aggregate JSONL report.
    #[arg(long)]
    pub(crate) report: Option<PathBuf>,
    /// GPU utilization ceiling 1–100 forwarded to the spawned miner's
    /// `--utilization` (cuda/metal only; other backends reject it).
    #[arg(long)]
    pub(crate) utilization: Option<u32>,
    /// Forward `--yielding` to the spawned miner (cuda/metal only).
    #[arg(long, default_value_t = false)]
    pub(crate) yielding: bool,
    /// CUDA device ordinal for the spawned miner. A config run takes this from
    /// the `[cuda.N]` section key, which drive has no equivalent of, so a
    /// multi-GPU host needs it stated here or the miner lands on GPU 0.
    /// Omit for backends whose CLI has no `--device`.
    #[arg(long)]
    pub(crate) device: Option<usize>,
}

pub(crate) fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    #[expect(
        clippy::cast_possible_truncation,
        reason = "unix millis fit u64 for practical coordinator lifetimes"
    )]
    {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

pub(crate) type BuiltJobs = (
    Vec<Job>,
    Option<Topology>,
    Option<quip_proto::v1::SetTarget>,
);

/// Convert a CLI energy value to milli-units.
#[expect(
    clippy::cast_possible_truncation,
    reason = "CLI target energy is a bounded value; milli fits i64"
)]
pub(crate) fn energy_to_milli(e: f64) -> i64 {
    (e * 1000.0) as i64
}

/// Build the `SetTarget` a drive run advertises, from the spec's gates, with
/// optional CLI overrides (`--target-energy`, `--min-solutions`).
pub(crate) fn set_target_from_spec(
    spec: &TopologySpec,
    args: &DriveArgs,
) -> quip_proto::v1::SetTarget {
    let max_energy_milli = args
        .target_energy
        .map_or(spec.max_energy_milli, energy_to_milli);
    quip_proto::v1::SetTarget {
        max_proof_solutions: crate::types::proof::MAX_PROOF_SOLUTIONS_WIRE,
        max_energy_milli,
        min_solutions: args.min_solutions.unwrap_or(spec.min_solutions),
        min_diversity_milli: spec.min_diversity_milli,
        num_reads: args.num_reads.unwrap_or(0),
        num_sweeps: args.num_sweeps.unwrap_or(0),
        anneal_time_us: 0,
    }
}

/// Build jobs from one resolved topology spec, without network I/O.
pub(crate) fn build_jobs(
    args: &DriveArgs,
    spec: Option<TopologySpec>,
    deadline_ms: u64,
) -> Result<BuiltJobs, String> {
    if args.family_id.is_some() && !matches!(args.source, DriveSourceKind::Lease) {
        return Err("--family-id requires --source lease".to_owned());
    }
    match args.source {
        DriveSourceKind::Random => {
            let spec = spec.ok_or("--source random requires a topology")?;
            let mut source = RandomSource::new(&spec, [0; 32], args.seed, args.count, deadline_ms);
            let jobs = drain_all(&mut source);
            let target = set_target_from_spec(&spec, args);
            Ok((jobs, Some(spec.topology), Some(target)))
        }
        DriveSourceKind::Lease => {
            let spec = spec.ok_or("--source lease requires a topology")?;
            let mut source = LeaseSource::new(
                &spec,
                [0; 32],
                args.seed,
                u64::from(args.count),
                args.lease_size,
            );
            if let (Some(id), Some(path)) = (&args.family_id, &args.family_params) {
                let params = std::fs::read(path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                let family = crate::types::family::ChallengeFamily::admit(
                    crate::types::family::FAMILY_CONTRACTS,
                    id,
                    args.family_revision,
                    params,
                    &spec.topology,
                )
                .map_err(|code| format!("family descriptor refused: {code}"))?;
                source = source.with_family(family);
            }
            let jobs = drain_all(&mut source);
            let target = set_target_from_spec(&spec, args);
            Ok((jobs, Some(spec.topology), Some(target)))
        }
        DriveSourceKind::List => {
            let list_path = args
                .list
                .as_ref()
                .ok_or("--source list requires --list <models.jsonl>")?;
            let (topology, snapshot, target) = match spec {
                Some(spec) => {
                    let target = set_target_from_spec(&spec, args);
                    let snapshot = spec.to_snapshot();
                    (Some(spec.topology), Some(snapshot), Some(target))
                }
                None => (None, None, None),
            };
            let mut source = ListSource::load(list_path, snapshot.as_ref(), deadline_ms)
                .map_err(|error| error.to_string())?;
            Ok((drain_all(&mut source), topology, target))
        }
    }
}

/// Default for generated work when no topology selector is supplied.
pub(crate) const DEFAULT_PRESET: &str = "zephyr-z12t4-live";

/// Resolve a file, preset, or one read-only chain lookup before job creation.
pub(crate) async fn resolve_topology(args: &DriveArgs) -> Result<Option<TopologySpec>, String> {
    if let Some(url) = &args.topology_from_chain {
        let client = RealChainClient::new(vec![url.clone()], String::new(), MinerKind::Cpu);
        let (hash, meta) = client
            .fetch_default_topology()
            .await
            .map_err(|error| format!("fetch topology from {url}: {error}"))?
            .ok_or_else(|| format!("fetch topology from {url}: chain has no default topology"))?;
        return meta
            .into_topology_spec(hash)
            .map(Some)
            .map_err(|error| format!("topology from {url}: {error}"));
    }
    let text = if let Some(path) = &args.topology {
        std::fs::read_to_string(path)
            .map_err(|error| format!("read topology {}: {error}", path.display()))?
    } else if let Some(name) = &args.topology_preset {
        preset_spec(name)?.to_string()
    } else {
        match args.source {
            DriveSourceKind::Random | DriveSourceKind::Lease => {
                preset_spec(DEFAULT_PRESET)?.to_string()
            }
            DriveSourceKind::List => return Ok(None),
        }
    };
    parse_topology_spec(&text)
        .map(Some)
        .map_err(|error| error.to_string())
}

#[expect(
    clippy::print_stderr,
    reason = "CLI binary reports drive runtime errors to stderr"
)]
pub(crate) fn run_drive_cli(args: DriveArgs, log_level: LogLevel) -> StdExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: runtime: {e}");
            return StdExitCode::from(ExitCode::InternalFatal as u8);
        }
    };
    rt.block_on(drive_main(args, log_level))
}

/// The launch entry `drive` spawns its miner from.
///
/// Split out of `drive_main` so the one field an operator can get wrong is
/// testable without standing up a session: `--device` is the only way a drive
/// run reaches a GPU other than 0, because drive has no `[cuda.N]` section to
/// take an ordinal from.
pub(crate) fn drive_entry(args: &DriveArgs) -> LaunchEntry {
    LaunchEntry {
        miner_id: "drive-0".into(),
        binary: args.miner.to_string_lossy().into_owned(),
        backend: "cpu".into(),
        device: args.device,
        configure: Configure {
            // Unset, as in a launch entry without `queue_depth`: the miner
            // picks its own depth.
            queue_depth: 0,
            idle_timeout_s: 30,
            heartbeat_s: 15,
            reconnect_window_s: 60,
            backend_toml: String::new(),
        },
    }
}

#[expect(
    clippy::print_stderr,
    reason = "CLI binary reports drive run failures to stderr"
)]
pub(crate) async fn drive_main(args: DriveArgs, log_level: LogLevel) -> StdExitCode {
    let spec = match resolve_topology(&args).await {
        Ok(spec) => spec,
        Err(message) => {
            eprintln!("error: {message}");
            return StdExitCode::from(ExitCode::ConfigInvalid as u8);
        }
    };
    // 0 means no deadline. Compute a live deadline after the topology lookup.
    let deadline_ms = if args.deadline_ms == 0 {
        0
    } else {
        now_unix_ms() + args.deadline_ms
    };
    let (jobs, topology, target) = match build_jobs(&args, spec, deadline_ms) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("error: {msg}");
            return StdExitCode::from(ExitCode::ConfigInvalid as u8);
        }
    };

    let entry = drive_entry(&args);
    let sock = format!("/tmp/quip-coordinator-drive-{}.sock", std::process::id());
    let report = run_drive(DriveManyParams {
        miner_bin: &entry.binary,
        sock_path: &sock,
        miner_id: &entry.miner_id,
        entry: &entry,
        topology,
        target,
        jobs,
        utilization: args.utilization,
        yielding: args.yielding,
        log_level,
    })
    .await;

    if !report.handshake_ok {
        eprintln!(
            "error: miner handshake failed (exit code {})",
            report.miner_exit_code
        );
        return StdExitCode::from(ExitCode::InternalFatal as u8);
    }

    let agg = aggregate(&report.rows, report.run_wall_ms);
    print_table(&report.rows, &agg);
    if let Some(path) = &args.report {
        if let Err(e) = write_jsonl(path, &report.rows, &agg) {
            eprintln!("error: cannot write report {}: {e}", path.display());
            return StdExitCode::from(ExitCode::InternalFatal as u8);
        }
    }
    // A truncated run (miner crash / dropped Result / timeout) leaves fewer rows
    // than jobs. That is a run failure, not per-job data, so it must not exit 0.
    if report.rows.len() < report.total {
        eprintln!(
            "error: incomplete run — {} of {} jobs produced no result",
            report.total - report.rows.len(),
            report.total
        );
        return StdExitCode::from(ExitCode::InternalFatal as u8);
    }
    StdExitCode::from(ExitCode::Clean as u8)
}
