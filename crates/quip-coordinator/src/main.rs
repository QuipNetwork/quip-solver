//! quip-coordinator binary: parse the command line and dispatch it.

use clap::Parser as _;
use quip_coordinator::cli::{dispatch, Cli};

fn main() -> std::process::ExitCode {
    dispatch(Cli::parse())
}
