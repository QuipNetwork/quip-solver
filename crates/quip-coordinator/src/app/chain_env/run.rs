//! The production run: keys, identity, the compatibility gate, the host
//! survey, the startup readiness walk, and the runtime.

use super::{ChainEnv, ChainEnvParams};
use crate::app::RuntimeParams;
use crate::chain::extrinsic::{load_hybrid_pair, miner_identity_bytes, signer_account_bytes};
use crate::chain::RealChainClient;
use crate::config::CoordinatorConfig;
use crate::coordinator::state::CoordinatorState;
use crate::miner::supervisor::BackoffPolicy;
use crate::types::hex::hex_encode;
use crate::types::log::LogLevel;
use quip_protocol::session::{ExitCode, PROTOCOL_VERSION};
use sp_core::crypto::Ss58Codec;
use std::path::{Path, PathBuf};
use std::process::ExitCode as StdExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Start the production coordinator from a parsed config and run it until
/// SIGINT or SIGTERM: chain client, compatibility gate, keys, host survey,
/// the readiness walk, then the runtime.
#[expect(
    clippy::too_many_lines,
    reason = "linear startup: config, chain client, readiness, then runtime params"
)]
pub fn run(config_path: &Path, cfg: CoordinatorConfig, log_level: LogLevel) -> StdExitCode {
    // Identify the process before doing anything that can warn or fail, so the
    // first line of any captured log says what this is and what it is talking to.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        protocol = PROTOCOL_VERSION,
        config = %config_path.display(),
        "quip-coordinator starting"
    );
    tracing::info!(validators = ?cfg.validators, "chain validators configured");

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "cannot start tokio runtime");
            return StdExitCode::from(ExitCode::InternalFatal as u8);
        }
    };

    let chain = Arc::new(RealChainClient::new(
        cfg.validators.clone(),
        cfg.signer_key.clone(),
        crate::config::launch::participate_kind(&cfg.launch),
    ));

    if let Some(code) = run_preflight_gate(&rt, chain.as_ref()) {
        return code;
    }

    let mut coordinator_state = CoordinatorState::new();
    coordinator_state
        .router
        .set_solver_pin(cfg.solver_pin.clone());
    let state = Arc::new(Mutex::new(coordinator_state));
    // Two distinct 32-byte values come out of one signer key, and mixing them
    // up funds an address that never signs anything. A live mining coordinator
    // needs its signer key; without a usable one, warn and fall back to zeros —
    // it still serves and feeds, but its proofs won't verify on-chain.
    let MinerKeys {
        identity: miner_identity,
        account: miner_account,
        ss58: miner_ss58,
    } = miner_keys_from_key(&cfg.signer_key);

    let identity = identity_from_keys(miner_ss58, miner_account, cfg.node_id.as_deref());

    let funding = crate::chain::funding::FundingParams {
        faucet_url: cfg.faucet_url.clone(),
        min_balance: cfg.min_balance_plancks,
        top_up: cfg.faucet_top_up_plancks,
        timeout: std::time::Duration::from_secs(cfg.funding_timeout_s),
    };
    let mut descriptor = crate::config::DescriptorParams::from_config(&cfg);
    // Survey the host once, here. `build_descriptor_payload` is a synchronous
    // fn reached from async with no `spawn_blocking`, so probing there would
    // stall a tokio worker. Once per process also matches the semantics of the
    // `descriptor_filed` latch.
    if let Some(surveyed) = crate::host::survey::collect(crate::host::survey::SURVEY_BUDGET) {
        descriptor.system_info = Some(surveyed.system);
        descriptor.runtime = Some(surveyed.runtime);
    }
    let descriptor_filed = Arc::new(AtomicBool::new(false));
    let miner_registered = Arc::new(AtomicBool::new(false));
    if let Some(code) = run_startup_prepare(
        &rt,
        chain.as_ref(),
        miner_account,
        &funding,
        &descriptor,
        &super::readiness::ProcessLatches {
            descriptor_filed: descriptor_filed.as_ref(),
            miner_registered: miner_registered.as_ref(),
        },
    ) {
        return code;
    }

    let params = RuntimeParams {
        sock_path: format!("/tmp/quip-coordinator-{}.sock", std::process::id()),
        max_submit_attempts: cfg.max_submit_attempts,
        grace_ms: 2000,
        backoff: BackoffPolicy::default(),
        miner_identity,
        miner_account,
        // Generous floor: keep every miner well-fed from the first poll, before
        // its drain-rate EMA ramps. The adaptive window grows above this for
        // fast, many-core backends; slow ones simply sit at the floor.
        buffer_depth: 256,
        poll_interval_ms: 1000,
        dashboard: cfg
            .dashboard
            .as_ref()
            .map(|d| (d.listen.clone(), PathBuf::from(&d.data_dir))),
        log_level,
        identity,
    };
    let env = Arc::new(ChainEnv::new(
        chain,
        ChainEnvParams {
            miner_account,
            funding,
            descriptor,
            descriptor_filed,
            miner_registered,
            solver_registered: Arc::new(AtomicBool::new(false)),
        },
    ));
    tracing::info!(
        miners = cfg.launch.len(),
        ids = ?cfg.launch.iter().map(|e| e.miner_id.as_str()).collect::<Vec<_>>(),
        socket = %params.sock_path,
        "serving miner session socket"
    );

    let result = rt.block_on(async move {
        crate::app::run(cfg.launch, env, state, params, shutdown_signal()).await
    });
    match result {
        Ok(()) => {
            tracing::info!("quip-coordinator stopped cleanly");
            StdExitCode::from(ExitCode::Clean as u8)
        }
        Err(e) => {
            tracing::error!(error = %e, "runtime failed");
            StdExitCode::from(ExitCode::InternalFatal as u8)
        }
    }
}

/// Compatibility gate run before anything else touches the chain.
///
/// The coordinator drives the chain through mirrored SCALE types and a pinned
/// runtime API; against a validator that predates them every read fails one
/// poll at a time, deep in the feeder. Decide it here instead.
///
/// A *skewed* validator is fatal (exit 64 — operator error, do not respawn).
/// An *unreachable* one is not: the node manager starts the coordinator and its
/// validator together, so exiting because the node is still booting would just
/// crash-loop. The feeder retries, and reachability transitions are logged.
fn run_preflight_gate(
    rt: &tokio::runtime::Runtime,
    chain: &RealChainClient,
) -> Option<StdExitCode> {
    match rt.block_on(chain.preflight()) {
        Ok(_) => None,
        Err(crate::chain::preflight::PreflightError::Unreachable(e)) => {
            tracing::warn!(
                error = %e,
                "cannot reach a validator to verify compatibility; starting anyway and \
                 will retry (set --log-level debug to watch the retries)"
            );
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "incompatible validator; refusing to start");
            Some(StdExitCode::from(ExitCode::ConfigInvalid as u8))
        }
    }
}

/// The two unrelated 32-byte values one signer key produces.
struct MinerKeys {
    /// `PoW` nonce input. Holds no balance and keys no storage map.
    identity: [u8; 32],
    /// Signing `AccountId32`: pays fees, holds the balance, keys the maps.
    account: [u8; 32],
    /// SS58 form of `account`, or `None` when no usable signer key loaded.
    ss58: Option<String>,
}

fn miner_keys_from_key(signer_key: &str) -> MinerKeys {
    match load_hybrid_pair(signer_key) {
        Ok(pair) => {
            let account = signer_account_bytes(&pair);
            MinerKeys {
                identity: miner_identity_bytes(&pair),
                account,
                ss58: Some(sp_core::crypto::AccountId32::from(account).to_ss58check()),
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "no usable signer key; PoW proofs will not verify on-chain"
            );
            MinerKeys {
                identity: [0u8; 32],
                account: [0u8; 32],
                ss58: None,
            }
        }
    }
}

/// Build the dashboard identity from the loaded signer keys.
///
/// An unkeyed coordinator advertises no identity at all. The dashboard reads
/// an empty `ss58_address` as an absent identity and skips its whole identity
/// block, which is the right answer for a process that cannot sign.
fn identity_from_keys(
    ss58: Option<String>,
    account: [u8; 32],
    node_id: Option<&str>,
) -> crate::observe::metrics::Identity {
    ss58.map_or_else(crate::observe::metrics::Identity::default, |ss58_address| {
        crate::observe::metrics::Identity {
            ss58_address,
            account_id_hex: hex_encode(&account),
            node_id: node_id.map_or_else(
                || super::readiness::node_id_from_account(&account),
                str::to_string,
            ),
        }
    })
}

/// Same readiness walk the feeder re-runs on every later round.
///
/// Funding failure is fatal at startup (exit 64). A missing snapshot is not:
/// the feeder retries once miners are connected.
fn run_startup_prepare(
    rt: &tokio::runtime::Runtime,
    chain: &RealChainClient,
    miner_account: [u8; 32],
    funding: &crate::chain::funding::FundingParams,
    descriptor: &crate::config::DescriptorParams,
    latches: &super::readiness::ProcessLatches<'_>,
) -> Option<StdExitCode> {
    let faucet = super::readiness::build_faucet(funding.faucet_url.as_deref());
    match rt.block_on(super::readiness::prepare_round(
        chain,
        faucet.as_ref(),
        miner_account,
        funding,
        tokio::time::sleep,
        descriptor,
        latches,
    )) {
        Ok(_) => None,
        Err(super::readiness::ReadinessError::Funding(e)) => {
            tracing::error!(error = %e, "miner account is not funded; refusing to start");
            Some(StdExitCode::from(ExitCode::ConfigInvalid as u8))
        }
        Err(super::readiness::ReadinessError::Snapshot(e)) => {
            tracing::warn!(
                error = %e,
                "no mining snapshot at startup; feeder will retry"
            );
            None
        }
    }
}

/// Resolve when the process receives SIGINT or SIGTERM (SIGINT-only off-unix).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
