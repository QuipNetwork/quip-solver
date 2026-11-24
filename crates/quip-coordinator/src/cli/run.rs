//! The production run: read the config and start the chain environment.

use crate::config::{parse_config, CoordinatorConfig};
use crate::types::log::LogLevel;
use quip_protocol::session::ExitCode;
use std::path::PathBuf;
use std::process::ExitCode as StdExitCode;

pub(crate) fn load_coordinator_config(
    config: Option<PathBuf>,
) -> Result<(PathBuf, CoordinatorConfig), StdExitCode> {
    let Some(config_path) = config else {
        tracing::error!("--config <path> is required");
        return Err(StdExitCode::from(ExitCode::ConfigInvalid as u8));
    };

    let text = match std::fs::read_to_string(&config_path) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(path = %config_path.display(), error = %e, "cannot read config");
            return Err(StdExitCode::from(ExitCode::ConfigInvalid as u8));
        }
    };

    match parse_config(&text) {
        Ok(c) => Ok((config_path, c)),
        Err(e) => {
            tracing::error!(path = %config_path.display(), error = %e, "invalid config");
            Err(StdExitCode::from(ExitCode::ConfigInvalid as u8))
        }
    }
}

pub(crate) fn run_config_path(config: Option<PathBuf>, log_level: LogLevel) -> StdExitCode {
    // --help is handled by clap (exit 0). Missing/invalid config → exit 64.
    let (config_path, cfg) = match load_coordinator_config(config) {
        Ok(v) => v,
        Err(code) => return code,
    };

    let environment_api_key = std::env::var(quip_id::API_KEY_VARIABLE).ok();
    crate::app::chain_env::run::run(&config_path, cfg, log_level, environment_api_key.as_deref())
}
