//! Command-line entry points: parse the arguments of each subcommand and hand
//! the work to the application layer. One module per subcommand.

pub mod admin;
pub mod calibrate;
pub mod drive;
pub mod keygen;
pub mod run;
#[cfg(test)]
mod tests;

use crate::types::log::LogLevel;
use clap::{Parser, Subcommand};
use quip_protocol::session::{ExitCode, PROTOCOL_VERSION};
use std::path::PathBuf;
use std::process::ExitCode as StdExitCode;
use std::sync::LazyLock;

/// `--version` text: the package version, then the miner wire protocol the
/// session handshake requires. CI reads the second field as the release version.
static VERSION: LazyLock<String> =
    LazyLock::new(|| format!("{} protocol {PROTOCOL_VERSION}", env!("CARGO_PKG_VERSION")));

/// The `quip-coordinator` command line.
#[derive(Parser, Debug)]
#[command(
    name = "quip-coordinator",
    version = VERSION.as_str(),
    about = "QuIP v0.3 coordinator: chain access, routing, miner supervision"

)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to coordinator config.toml (ignored when a subcommand is given)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Log verbosity. Defaults to `info`. Takes precedence over `RUST_LOG`;
    /// when omitted, `RUST_LOG` is honored if set. Logs go to stderr.
    #[arg(long, value_enum, global = true)]
    log_level: Option<LogLevel>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Run one miner through calibration on recent winners; no jobs, no submit.
    Calibrate(calibrate::CalibrateArgs),
    /// Drive a spawned miner with generated or replayed work; no submit.
    Drive(drive::DriveArgs),
    /// Create a signer keystore at --out. Refuses to overwrite.
    Keygen(keygen::KeygenArgs),
    /// Seed a fresh chain: register the default topology and set its difficulty.
    SeedChain(admin::SeedChainArgs),
    /// Make a twisted-ice dimer family topology the chain's default, on a
    /// fresh or an already seeded chain, through a Foundation motion.
    SetFamily(admin::SetFamilyArgs),
}

/// Run the parsed command line. Logging is set up first.
#[must_use]
#[expect(
    clippy::print_stderr,
    reason = "the log subscriber is what failed; stderr is the only channel left"
)]
pub fn dispatch(cli: Cli) -> StdExitCode {
    // Before anything else: without this, every `tracing` call in the process
    // is a silent no-op and `RUST_LOG` has no effect.
    if let Err(e) = crate::app::init_logging(cli.log_level) {
        eprintln!("error: {e}");
        return StdExitCode::from(ExitCode::ConfigInvalid as u8);
    }
    match cli.command {
        Some(Command::Drive(args)) => {
            drive::run_drive_cli(args, cli.log_level.unwrap_or(LogLevel::Info))
        }
        Some(Command::Calibrate(args)) => {
            calibrate::run_calibrate_cli(&args, cli.log_level.unwrap_or(LogLevel::Info))
        }
        Some(Command::Keygen(args)) => keygen::run_keygen_cli(&args),
        Some(Command::SeedChain(args)) => admin::run_seed_chain_cli(args),
        Some(Command::SetFamily(args)) => admin::run_set_family_cli(args),
        // When --log-level is omitted the coordinator default is Info (unless
        // RUST_LOG overrides the coordinator filter alone). Forward that same
        // default to miner children so their verbosity matches the CLI default.
        None => run::run_config_path(cli.config, cli.log_level.unwrap_or(LogLevel::Info)),
    }
}
