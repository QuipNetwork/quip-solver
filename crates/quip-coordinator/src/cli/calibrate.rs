//! The `calibrate` subcommand: read the recent winners from a validator, run
//! one miner through calibration, and report the outcome. Read-only: it needs
//! no signer, sends no job, and submits nothing.

use crate::app::local_env::{
    calibrate_miner, CalibrateError, CalibrateReport, CalibrateRequest, CalibrateResult,
    CALIBRATION_GRACE,
};
use crate::chain::{ChainClient, RealChainClient};
use crate::types::log::LogLevel;
use crate::types::miner::MinerKind;
use quip_protocol::session::ExitCode;
use std::path::PathBuf;
use std::process::ExitCode as StdExitCode;

#[derive(clap::Args, Debug)]
pub(crate) struct CalibrateArgs {
    /// Miner binary to spawn.
    #[arg(long)]
    pub(crate) miner: PathBuf,
    /// Validator RPC URL to read the mining snapshot and winners from.
    #[arg(long, value_name = "URL")]
    pub(crate) validator: String,
    /// How many recent winning qblocks to send, 1 to 1000.
    #[arg(long, default_value_t = 40)]
    pub(crate) qblocks: u32,
    /// Hardware classes the winners must come from: `cpu`, `gpu`, `qpu`,
    /// comma separated. Empty admits every winner.
    #[arg(long, value_delimiter = ',')]
    pub(crate) kinds: Vec<String>,
    /// Seconds the miner has to calibrate. The run waits 30 s longer for the
    /// answer.
    #[arg(long, default_value_t = 1800)]
    pub(crate) deadline_s: u64,
}

/// Exit code for a finished run: 0 for `DONE`, 1 for any other outcome or an
/// unfinished run, 64 for a request, miner, or chain state that cannot
/// calibrate, 69 for an RPC error, 70 for a miner that never joined.
#[must_use]
pub fn exit_code(result: &Result<CalibrateReport, CalibrateError>) -> StdExitCode {
    match result {
        Ok(report) if report.result == CalibrateResult::Done => {
            StdExitCode::from(ExitCode::Clean as u8)
        }
        Ok(_) | Err(CalibrateError::Incomplete(_)) => StdExitCode::FAILURE,
        Err(CalibrateError::Usage(_)) => StdExitCode::from(ExitCode::ConfigInvalid as u8),
        Err(CalibrateError::Rpc(_)) => StdExitCode::from(ExitCode::EnvIncompatible as u8),
        Err(CalibrateError::Handshake(_)) => StdExitCode::from(ExitCode::InternalFatal as u8),
    }
}

#[expect(
    clippy::print_stderr,
    reason = "CLI binary reports runtime setup errors to stderr"
)]
pub(crate) fn run_calibrate_cli(args: &CalibrateArgs, log_level: LogLevel) -> StdExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: runtime: {e}");
            return StdExitCode::from(ExitCode::InternalFatal as u8);
        }
    };
    let chain = RealChainClient::new(vec![args.validator.clone()], String::new(), MinerKind::Cpu);
    rt.block_on(calibrate_main(&chain, args, log_level))
}

/// Run the calibration against `chain`, print the outcome line, and map the
/// result to an exit code.
/// Write the outcome line to `out` (or the error to `err`) and return the
/// exit code of `result`.
#[must_use]
pub fn render_outcome(
    result: &Result<CalibrateReport, CalibrateError>,
    out: &mut impl std::io::Write,
    err: &mut impl std::io::Write,
) -> StdExitCode {
    // A closed stdout or stderr leaves nothing to report to; the exit code stands.
    let _ = match result {
        Ok(report) => writeln!(
            out,
            "outcome={} summary={}",
            report.result.label(),
            report.summary
        ),
        Err(error) => writeln!(err, "error: {error}"),
    };
    exit_code(result)
}

/// Run the calibration against `chain`, print the outcome line, and map the
/// result to an exit code.
pub(crate) async fn calibrate_main(
    chain: &(impl ChainClient + ?Sized),
    args: &CalibrateArgs,
    log_level: LogLevel,
) -> StdExitCode {
    let sock = format!(
        "/tmp/quip-coordinator-calibrate-{}.sock",
        std::process::id()
    );
    let miner = args.miner.to_string_lossy();
    let result = calibrate_miner(
        chain,
        &CalibrateRequest {
            miner_bin: &miner,
            sock_path: &sock,
            qblocks: args.qblocks,
            kinds: &args.kinds,
            deadline_s: args.deadline_s,
            grace: CALIBRATION_GRACE,
            log_level,
        },
    )
    .await;
    render_outcome(&result, &mut std::io::stdout(), &mut std::io::stderr())
}
