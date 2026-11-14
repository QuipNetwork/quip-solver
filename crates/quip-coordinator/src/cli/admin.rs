//! The admin subcommands, `seed-chain` and `set-family`: their arguments and
//! their output. The chain calls are in `chain::governance::seed`.

use crate::chain::codec::scale_types::{DifficultyConfig, EnergyBandScale};
use crate::chain::{
    apply_family, dimer_1200, plan_family, seed_chain, twisted_dimer, twisted_dimer_params,
    ExistingDefault, FamilyOutcome, FamilyPlan, SeedParams, SeedTopology, DEFAULT_ENERGY_BAND,
    DEFAULT_SEED_DIFFICULTY,
};
use crate::types::hex::{hex_decode, hex_encode};
use crate::types::topology_spec::{parse_topology_spec, preset_spec};
use std::path::PathBuf;
use std::process::ExitCode as StdExitCode;

/// Environment variable `set-family` reads the Foundation signer from when
/// `--mnemonic-file` is absent. Any form `seed-chain --sudo-key` accepts.
pub(crate) const FOUNDATION_KEY_ENV: &str = "QUIP_FOUNDATION_KEY";

#[derive(clap::Args, Debug)]
#[command(group(
    clap::ArgGroup::new("family")
        .args(["weak_milli", "params_hex"])
        .required(true)
))]
#[command(group(
    clap::ArgGroup::new("graph")
        .args(["topology", "topology_preset"])
        .required(true)
))]
pub(crate) struct SetFamilyArgs {
    /// Validator RPC endpoint.
    #[arg(long, default_value = "ws://quip-validator:9944")]
    pub(crate) validator: String,
    /// Path to a file holding the Foundation member's BIP39 mnemonic. Without
    /// it the signer comes from the `QUIP_FOUNDATION_KEY` environment variable.
    #[arg(long)]
    pub(crate) mnemonic_file: Option<PathBuf>,
    /// Weak coupling magnitude of quip/twisted-ice-dimer r1, in milli (1-999).
    #[arg(long, requires = "pairs")]
    pub(crate) weak_milli: Option<u16>,
    /// Ferromagnetic pair count of quip/twisted-ice-dimer r1.
    #[arg(long, requires = "weak_milli")]
    pub(crate) pairs: Option<u32>,
    /// Canonical quip/twisted-ice-dimer r1 parameter bytes as hex, instead of
    /// --weak-milli and --pairs.
    #[arg(long)]
    pub(crate) params_hex: Option<String>,
    /// Topology spec JSON of the graph to register the family on.
    #[arg(long)]
    pub(crate) topology: Option<PathBuf>,
    /// Built-in topology by name, instead of --topology.
    #[arg(long)]
    pub(crate) topology_preset: Option<String>,
    /// Minimum valid solutions a proof must carry.
    #[arg(long, default_value_t = DEFAULT_SEED_DIFFICULTY.min_solutions)]
    pub(crate) min_solutions: u32,
    /// First energy ceiling in milli units; solutions must be strictly below
    /// it. Must sit inside the band.
    #[arg(long, allow_negative_numbers = true)]
    pub(crate) max_energy_milli: i64,
    /// Minimum solution-set diversity in milli units.
    #[arg(long, default_value_t = DEFAULT_SEED_DIFFICULTY.min_diversity_milli)]
    pub(crate) min_diversity_milli: u32,
    /// Hardest (most negative) end of the energy band, in milli.
    #[arg(long, allow_negative_numbers = true)]
    pub(crate) band_hard_milli: i64,
    /// Easiest (least negative) end of the energy band, in milli.
    #[arg(long, allow_negative_numbers = true)]
    pub(crate) band_easy_milli: i64,
    /// Print the encoded calls, the family hash and the motion, and stop
    /// before signing anything. Reads the chain; needs no signer.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

#[derive(clap::Args, Debug)]
pub(crate) struct SeedChainArgs {
    /// Validator RPC endpoint.
    #[arg(long, default_value = "ws://quip-validator:9944")]
    pub(crate) validator: String,
    /// Foundation member signer: a dev URI (//Alice), a BIP39 mnemonic, a
    /// 32-byte hex master seed, or a keystore path. Mutually exclusive with
    /// --mnemonic-file.
    #[arg(long, conflicts_with = "mnemonic_file")]
    pub(crate) sudo_key: Option<String>,
    /// Path to a file holding a BIP39 mnemonic phrase. Mount it read-only.
    #[arg(long)]
    pub(crate) mnemonic_file: Option<PathBuf>,
    /// Built-in topology by name. Mutually exclusive with --topology. The
    /// dimer1200 family is registered on it.
    #[arg(long, default_value = "zephyr-z12t4-live", conflicts_with = "topology")]
    pub(crate) topology_preset: String,
    /// Topology spec JSON path.
    #[arg(long)]
    pub(crate) topology: Option<PathBuf>,
    /// Minimum valid solutions a proof must carry.
    #[arg(long, default_value_t = DEFAULT_SEED_DIFFICULTY.min_solutions)]
    pub(crate) min_solutions: u32,
    /// Energy ceiling in milli units; solutions must be strictly below it.
    #[arg(
        long,
        default_value_t = DEFAULT_SEED_DIFFICULTY.max_energy_milli,
        allow_negative_numbers = true
    )]
    pub(crate) max_energy_milli: i64,
    /// Minimum solution-set diversity in milli units.
    #[arg(long, default_value_t = DEFAULT_SEED_DIFFICULTY.min_diversity_milli)]
    pub(crate) min_diversity_milli: u32,
    /// Hardest (most negative) end of the energy band, in milli.
    #[arg(
        long,
        default_value_t = DEFAULT_ENERGY_BAND.hard_milli,
        allow_negative_numbers = true
    )]
    pub(crate) band_hard_milli: i64,
    /// Easiest (least negative) end of the energy band, in milli.
    #[arg(
        long,
        default_value_t = DEFAULT_ENERGY_BAND.easy_milli,
        allow_negative_numbers = true
    )]
    pub(crate) band_easy_milli: i64,
}

#[expect(
    clippy::print_stderr,
    reason = "CLI binary reports seed-chain errors to stderr"
)]
pub(crate) fn run_seed_chain_cli(args: SeedChainArgs) -> StdExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("seed-chain: tokio runtime: {e}");
            return StdExitCode::FAILURE;
        }
    };
    match rt.block_on(seed_chain_inner(args)) {
        Ok(outcome) => {
            print_family_outcome(&outcome, "seed-chain");
            StdExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("seed-chain: {e}");
            StdExitCode::FAILURE
        }
    }
}

/// Report a family activation. Only `Applied` and `AlreadyDefault` mean the
/// chain's default is the family topology.
#[expect(
    clippy::print_stdout,
    reason = "CLI binary reports the activation to stdout"
)]
pub(crate) fn print_family_outcome(outcome: &FamilyOutcome, command: &str) {
    match outcome {
        FamilyOutcome::Applied {
            topology_hash,
            nodes,
            edges,
            block,
        } => {
            println!(
                "DefaultTopology is now {} ({nodes} nodes, {edges} edges)",
                hex_encode(topology_hash)
            );
            println!("  Foundation motion executed in {block}");
        }
        FamilyOutcome::AlreadyDefault { topology_hash } => {
            println!(
                "DefaultTopology is already {}; nothing to do",
                hex_encode(topology_hash)
            );
        }
        FamilyOutcome::Pending {
            topology_hash,
            proposal_hash,
            index,
            ayes,
            threshold,
        } => {
            println!("motion open: {ayes} of {threshold} ayes; DefaultTopology is unchanged");
            println!("  proposal hash  {}", hex_encode(proposal_hash));
            println!("  proposal index {index}");
            println!("  family topology {}", hex_encode(topology_hash));
            println!(
                "  each other Foundation member runs this same {command} command with their \
                 own signer: it votes aye on this proposal hash, and the member whose vote \
                 reaches {threshold} also closes the motion"
            );
        }
    }
}

/// A BIP39 phrase from a file, trimmed. An empty file is an error.
pub(crate) fn read_mnemonic_file(path: &std::path::Path) -> Result<String, String> {
    let phrase = std::fs::read_to_string(path)
        .map_err(|e| format!("read mnemonic file {}: {e}", path.display()))?;
    let phrase = phrase.trim().to_string();
    if phrase.is_empty() {
        return Err(format!("mnemonic file is empty: {}", path.display()));
    }
    Ok(phrase)
}

/// The graph from a spec JSON path, else the named preset.
pub(crate) fn load_seed_topology(
    path: Option<&std::path::Path>,
    preset: &str,
) -> Result<SeedTopology, String> {
    let text = match path {
        Some(p) => {
            std::fs::read_to_string(p).map_err(|e| format!("read topology {}: {e}", p.display()))?
        }
        None => preset_spec(preset)?.to_string(),
    };
    let spec = parse_topology_spec(&text).map_err(|e| format!("{e:?}"))?;
    Ok(SeedTopology::from_spec(&spec))
}

pub(crate) async fn seed_chain_inner(args: SeedChainArgs) -> Result<FamilyOutcome, String> {
    let sudo_key = match (&args.sudo_key, &args.mnemonic_file) {
        (Some(k), None) => k.clone(),
        (None, Some(p)) => read_mnemonic_file(p)?,
        _ => return Err("give exactly one of --sudo-key or --mnemonic-file".into()),
    };
    seed_chain(SeedParams {
        validator: args.validator,
        sudo_key,
        topology: load_seed_topology(args.topology.as_deref(), &args.topology_preset)?,
        difficulty: DifficultyConfig {
            min_solutions: args.min_solutions,
            max_energy_milli: args.max_energy_milli,
            min_diversity_milli: args.min_diversity_milli,
        },
        descriptor: dimer_1200(),
        band: EnergyBandScale {
            hard_milli: args.band_hard_milli,
            easy_milli: args.band_easy_milli,
        },
    })
    .await
    .map_err(|e| e.to_string())
}

#[expect(
    clippy::print_stderr,
    reason = "CLI binary reports set-family errors to stderr"
)]
pub(crate) fn run_set_family_cli(args: SetFamilyArgs) -> StdExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("set-family: tokio runtime: {e}");
            return StdExitCode::FAILURE;
        }
    };
    match rt.block_on(set_family_inner(args)) {
        Ok(()) => StdExitCode::SUCCESS,
        Err(e) => {
            eprintln!("set-family: {e}");
            StdExitCode::FAILURE
        }
    }
}

/// The activation `set-family` asks for. The signer is left empty; it is read
/// only when the run will sign.
pub(crate) fn set_family_params(args: &SetFamilyArgs) -> Result<SeedParams, String> {
    let params = match (&args.params_hex, args.weak_milli, args.pairs) {
        (Some(hex), None, None) => {
            let hex = hex.trim();
            let hex = if hex.starts_with("0x") {
                hex.to_string()
            } else {
                format!("0x{hex}")
            };
            hex_decode(&hex).map_err(|e| format!("--params-hex: {e}"))?
        }
        (None, Some(weak), Some(pairs)) => twisted_dimer_params(weak, pairs),
        _ => return Err("give --weak-milli with --pairs, or --params-hex".into()),
    };
    let preset = args.topology_preset.as_deref().unwrap_or_default();
    Ok(SeedParams {
        validator: args.validator.clone(),
        sudo_key: String::new(),
        topology: load_seed_topology(args.topology.as_deref(), preset)?,
        descriptor: twisted_dimer(params),
        difficulty: DifficultyConfig {
            min_solutions: args.min_solutions,
            max_energy_milli: args.max_energy_milli,
            min_diversity_milli: args.min_diversity_milli,
        },
        band: EnergyBandScale {
            hard_milli: args.band_hard_milli,
            easy_milli: args.band_easy_milli,
        },
    })
}

/// The Foundation signer: `--mnemonic-file`, else `QUIP_FOUNDATION_KEY`.
/// Neither is ever logged.
pub(crate) fn foundation_signer(mnemonic_file: Option<&std::path::Path>) -> Result<String, String> {
    if let Some(path) = mnemonic_file {
        return read_mnemonic_file(path);
    }
    match std::env::var(FOUNDATION_KEY_ENV) {
        Ok(key) if !key.trim().is_empty() => Ok(key.trim().to_string()),
        _ => Err(format!(
            "give --mnemonic-file or set {FOUNDATION_KEY_ENV} to a Foundation member's signer"
        )),
    }
}

#[expect(
    clippy::print_stdout,
    reason = "CLI binary prints the dry run to stdout"
)]
pub(crate) async fn set_family_inner(args: SetFamilyArgs) -> Result<(), String> {
    let mut params = set_family_params(&args)?;
    if args.dry_run {
        let plan = plan_family(&params, ExistingDefault::Replace)
            .await
            .map_err(|e| e.to_string())?;
        print_family_plan(&plan, &params);
        return Ok(());
    }
    params.sudo_key = foundation_signer(args.mnemonic_file.as_deref())?;
    let outcome = apply_family(&params, ExistingDefault::Replace)
        .await
        .map_err(|e| e.to_string())?;
    print_family_outcome(&outcome, "set-family");
    if matches!(outcome, FamilyOutcome::Pending { .. }) {
        println!("  the default moves only once the motion closes");
    }
    Ok(())
}

#[expect(
    clippy::print_stdout,
    reason = "CLI binary prints the dry run to stdout"
)]
pub(crate) fn print_family_plan(plan: &FamilyPlan, params: &SeedParams) {
    println!(
        "family           {} r{}",
        String::from_utf8_lossy(&params.descriptor.family_id),
        params.descriptor.revision
    );
    println!("params           {}", hex_encode(&params.descriptor.params));
    println!(
        "graph            {} nodes, {} edges, legacy hash {}",
        params.topology.nodes.len(),
        params.topology.edges.len(),
        hex_encode(&plan.legacy_hash)
    );
    println!("family hash      {}", hex_encode(&plan.topology_hash));
    println!(
        "current default  {}",
        plan.chain
            .default
            .map_or_else(|| "unset".to_string(), |h| hex_encode(&h))
    );
    for hash in &plan.chain.mineable {
        println!("mineable         {}", hex_encode(hash));
    }
    println!(
        "foundation       {} members, threshold {}{}",
        plan.members.len(),
        plan.threshold,
        if plan.threshold < 2 {
            " (executes at once)"
        } else {
            " (needs votes)"
        }
    );
    if plan.calls.is_empty() {
        println!("the family topology is already the default; nothing to send");
        return;
    }
    for (i, call) in plan.calls.iter().enumerate() {
        println!("call {}           {}", i + 1, call.label);
        println!("                 {}", hex_encode(&call.bytes));
    }
    println!("proposal hash    {}", hex_encode(&plan.proposal_hash));
    println!("motion           {}", hex_encode(&plan.motion));
    println!("dry run: nothing was signed or sent");
}
