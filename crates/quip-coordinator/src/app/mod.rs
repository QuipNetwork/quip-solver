//! Composition root: wires the coordinator, the miner sessions, the chain,
//! and the dashboard together for one environment.

pub mod chain_env;
pub mod compliance;
pub mod local_env;

use crate::config::launch::LaunchEntry;
use crate::coordinator::events::CoordinatorService;
use crate::coordinator::feeder::{feeder_loop, FeederParams};
use crate::coordinator::state::CoordinatorState;
use crate::coordinator::{ResultSink, WorkSource};
use crate::miner::session::service::SessionService;
use crate::miner::supervisor::{supervise_miner, BackoffPolicy};
use crate::types::log::LogLevel;
use quip_proto::v1::miner_service_server::MinerServiceServer;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::sync::{watch, Mutex};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

/// Everything the coordinator needs from the world it runs in: where work
/// comes from, and where validated results go.
pub trait Environment: WorkSource + ResultSink + 'static {}

impl<T: WorkSource + ResultSink + 'static> Environment for T {}

/// Install the process log subscriber, writing to stderr. Call once, before
/// anything else logs: messages emitted before it returns are discarded.
///
/// # Errors
/// Returns an error when the resolved filter does not parse or a global
/// subscriber is already installed.
pub fn init_logging(level: Option<LogLevel>) -> Result<(), String> {
    crate::observe::logging::init(level)
}

/// Runtime knobs shared by every supervised miner.
pub struct RuntimeParams {
    /// Unix-domain socket the server binds and miners connect to.
    pub sock_path: String,
    /// Consecutive failed submissions of one proof, inside one quantum block,
    /// before the coordinator stops retrying it. From `[miner]` config.
    pub max_submit_attempts: u32,
    /// Grace period (ms) between an in-band `Shutdown` and a hard kill.
    pub grace_ms: u32,
    /// Restart backoff + failure budget applied to each miner.
    pub backoff: BackoffPolicy,
    /// `PoW` nonce input: `blake2_256(SCALE(account))`, matching the pallet's
    /// `account_to_bytes`. Not an address — see [`Self::miner_account`].
    pub miner_identity: [u8; 32],
    /// The signing `AccountId32`. Pays fees, holds the balance the funding path
    /// tops up, and keys every per-signer chain map.
    pub miner_account: [u8; 32],
    /// Floor for the adaptive staging window: minimum staged jobs per miner
    /// (0 disables feeding). The window grows above this with each miner's
    /// observed drain rate; there is no ceiling. See [`feeder_loop`].
    pub buffer_depth: usize,
    /// How often the feeder polls the chain head and tops up buffers.
    pub poll_interval_ms: u64,
    /// Optional attempt-dashboard: `(listen_addr, data_dir)`. `None` disables
    /// recording + the REST endpoint.
    pub dashboard: Option<(String, std::path::PathBuf)>,
    /// Log level forwarded to each miner child as `--log-level`. Children inherit
    /// coordinator stdio, so their own subscriber needs this flag to match
    /// coordinator verbosity.
    pub log_level: LogLevel,
    /// Chain identity advertised on `/api/v1/status`. Empty when the process
    /// holds no usable signer key.
    pub identity: crate::observe::metrics::Identity,
}

/// Periodic liveness ping interval for [`crate::miner::liveness::liveness_loop`].
const LIVENESS_PING_INTERVAL: Duration = Duration::from_secs(15);

/// Optional mining-attempt dashboard: a single writer thread records every
/// solved model to `<data_dir>/<qblock_id>/attempts.jsonl`, and an HTTP task
/// serves those files plus the three `/api/v1` endpoints.
///
/// Logs which way it went either way. A silent skip here used to surface only
/// as a refused connection from whatever proxies the port, which points the
/// operator at the proxy instead of at the config that disabled the server.
async fn spawn_dashboard(
    params: &RuntimeParams,
    state: &Arc<Mutex<CoordinatorState>>,
    metrics: &Arc<crate::observe::metrics::CoordinatorMetrics>,
) -> Option<tokio::task::JoinHandle<()>> {
    let Some((listen, data_dir)) = params.dashboard.clone() else {
        // Reached both when the config carries no [dashboard] section and when
        // it carries an unusable one; `parse_dashboard` warns about the latter
        // and names the missing key.
        tracing::info!("dashboard disabled: no usable [dashboard] section in the config");
        return None;
    };
    tracing::info!(%listen, data_dir = %data_dir.display(), "dashboard enabled");
    let tx = crate::observe::attempt::spawn_writer(data_dir.clone());
    state.lock().await.attempt_tx = Some(tx);
    Some(tokio::spawn(crate::dashboard::serve(
        listen,
        crate::dashboard::DashboardState {
            data_dir,
            metrics: Arc::clone(metrics),
        },
    )))
}

/// Serve the UDS session server, supervise every miner in `launch`, and return
/// once `shutdown` resolves — fanning an in-band `Shutdown` to each live miner
/// and killing after grace. Generic over [`Environment`] so tests drive it
/// with a `FakeChain`; `main` passes the chain environment. `state` is shared
/// with the caller so a live coordinator (and tests) can inspect
/// routing/inflight.
///
/// # Errors
/// Returns an I/O error if the UDS socket cannot be bound.
pub async fn run<E, S>(
    launch: Vec<LaunchEntry>,
    env: Arc<E>,
    state: Arc<Mutex<CoordinatorState>>,
    params: RuntimeParams,
    shutdown: S,
) -> std::io::Result<()>
where
    E: Environment,
    S: Future<Output = ()>,
{
    let _ = std::fs::remove_file(&params.sock_path);
    if let Some(parent) = Path::new(&params.sock_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let uds = UnixListener::bind(&params.sock_path)?;
    let incoming = UnixListenerStream::new(uds);

    // Seed each miner's Configure so the server can answer its handshake; the
    // per-spawn session token is set by the supervisor before each spawn.
    {
        let mut st = state.lock().await;
        for e in &launch {
            let _ = st
                .sessions
                .configure
                .insert(e.miner_id.clone(), e.configure.clone());
            let _ = st.miner_types.insert(
                e.miner_id.clone(),
                crate::observe::metrics::miner_type_label(&e.backend),
            );
        }
    }

    // Live counters and identity, shared by the session path, the feeder, and
    // the dashboard router. Built from the launch plan so `modes` carries one
    // entry per backend group.
    let metrics = Arc::new(crate::observe::metrics::CoordinatorMetrics::new(
        &launch
            .iter()
            .map(|e| (e.miner_id.clone(), e.backend.clone()))
            .collect::<Vec<_>>(),
    ));
    metrics.set_identity(params.identity.clone());
    state.lock().await.metrics = Arc::clone(&metrics);

    let dashboard_server = spawn_dashboard(&params, &state, &metrics).await;

    let svc = CoordinatorService {
        state: Arc::clone(&state),
        sink: Arc::clone(&env),
        submit_notify: Arc::new(Mutex::new(None)),
    };
    let server = tokio::spawn(
        Server::builder()
            .add_service(MinerServiceServer::new(SessionService::new(
                Arc::clone(&state),
                Arc::new(svc),
            )))
            .serve_with_incoming(incoming),
    );

    let (stop_tx, stop_rx) = watch::channel(false);
    let sock_uri = format!("unix://{}", params.sock_path);
    let mut supervisors = Vec::with_capacity(launch.len());
    for entry in launch {
        supervisors.push(tokio::spawn(supervise_miner(
            entry,
            sock_uri.clone(),
            Arc::clone(&state),
            params.backoff,
            params.grace_ms,
            params.log_level,
            stop_rx.clone(),
        )));
    }

    // The replenished feeder follows the chain head and keeps buffers full.
    let feeder = tokio::spawn(feeder_loop(
        Arc::clone(&env),
        Arc::clone(&state),
        FeederParams {
            miner_identity: params.miner_identity,
            miner_account: params.miner_account,
            buffer_depth: params.buffer_depth,
            poll_interval: Duration::from_millis(params.poll_interval_ms),
            max_submit_attempts: params.max_submit_attempts,
            metrics: Arc::clone(&metrics),
        },
        stop_rx.clone(),
    ));

    let liveness = tokio::spawn(crate::miner::liveness::liveness_loop(
        Arc::clone(&state),
        LIVENESS_PING_INTERVAL,
        stop_rx.clone(),
    ));
    drop(stop_rx);

    // Run until asked to stop.
    shutdown.await;

    // Fan shutdown out; drain the feeder + supervisors before dropping the
    // server so in-flight Shutdown/kill completes.
    let _ = stop_tx.send(true);
    let _ = feeder.await;
    let _ = liveness.await;
    for h in supervisors {
        let _ = h.await;
    }
    server.abort();
    if let Some(h) = dashboard_server {
        h.abort();
    }
    let _ = std::fs::remove_file(&params.sock_path);
    Ok(())
}
