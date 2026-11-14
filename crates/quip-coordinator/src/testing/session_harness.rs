//! A one-shot session server for integration tests: spawn a miner binary and
//! report whether its handshake completed.

use crate::coordinator::events::CoordinatorService;
use crate::coordinator::router::MinerCaps;
use crate::coordinator::state::CoordinatorState;
use crate::miner::session::service::SessionService;
use quip_proto::v1::miner_service_server::MinerServiceServer;
use quip_proto::v1::Configure;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

/// Result of a one-shot handshake harness run.
#[derive(Debug)]
pub struct SessionHarnessReport {
    /// Miner caps registered on a successful handshake.
    pub caps: Option<MinerCaps>,
    /// Miner process exit code (`-1` if unknown).
    pub miner_exit_code: i32,
    /// Whether the handshake completed and caps were registered.
    pub handshake_ok: bool,
}

/// Spawn `miner_bin` against a coordinator on `sock_path`. The coordinator
/// expects `coord_token` while the miner is given `miner_token`, so a mismatch
/// test passes two different values.
///
/// Used by integration tests. `sock_path` is a filesystem path (no `unix://`).
///
/// # Panics
///
/// Panics if the Unix socket cannot be bound or the miner process cannot be
/// spawned.
pub async fn serve_one_session_expecting(
    miner_bin: &str,
    sock_path: &str,
    miner_id: &str,
    miner_token: &str,
    coord_token: &str,
) -> SessionHarnessReport {
    let _ = std::fs::remove_file(sock_path);
    if let Some(parent) = Path::new(sock_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    #[expect(
        clippy::expect_used,
        reason = "test harness: bind failure is a setup bug"
    )]
    let uds = UnixListener::bind(sock_path).expect("bind unix socket");
    let incoming = UnixListenerStream::new(uds);

    let mut st = CoordinatorState::new();
    let _ = st
        .sessions
        .expected_tokens
        .insert(miner_id.into(), coord_token.into());
    let _ = st.sessions.configure.insert(
        miner_id.into(),
        Configure {
            queue_depth: 3,
            idle_timeout_s: 5,
            heartbeat_s: 15,
            reconnect_window_s: 60,
            backend_toml: String::new(),
        },
    );
    let state = Arc::new(Mutex::new(st));
    // Chain is unused for handshake-only; use a no-op fake with empty snapshot.
    let chain = Arc::new(crate::testing::fake_chain::FakeChain::new(
        crate::types::snapshot::MiningSnapshot {
            head_hash: [0u8; 32],
            last_proof_block_hash: [0u8; 32],
            topology_hash: vec![0u8; 32],
            nodes: vec![],
            edges: vec![],
            allowed_h_milli: vec![0],
            allowed_j_milli: vec![0],
            allowed_spin_milli: vec![-1000, 1000],
            min_solutions: 0,
            max_energy_milli: 0,
            min_diversity_milli: 0,
            block_number: 0,
            spec_version: 117,
            target_proof_blocks: crate::types::snapshot::TARGET_PROOF_BLOCKS,
            family: None,
        },
        None,
    ));

    let svc = CoordinatorService {
        state: Arc::clone(&state),
        sink: chain,
        submit_notify: Arc::new(Mutex::new(None)),
    };
    let svc = SessionService::new(Arc::clone(&state), Arc::new(svc));
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(MinerServiceServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });

    let socket_uri = format!("unix://{sock_path}");
    #[expect(
        clippy::expect_used,
        reason = "test harness: spawn failure is a setup bug"
    )]
    let mut child = Command::new(miner_bin)
        .arg("--quip-coordinator")
        .arg(&socket_uri)
        .arg("--miner-id")
        .arg(miner_id)
        .env("QUIP_SESSION_TOKEN", miner_token)
        .spawn()
        .expect("spawn miner");

    // Give the miner a moment to handshake, then shut it down cleanly so the
    // idle path doesn't dominate: send nothing further; for a successful
    // handshake the miner waits on idle_timeout (5s). For a failed handshake
    // the stream is already dropped and the miner exits promptly.
    let exit_code = match tokio::time::timeout(Duration::from_secs(15), child.wait()).await {
        Ok(Ok(status)) => status.code().unwrap_or(-1),
        Ok(Err(_)) => -1,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            -1
        }
    };

    // After Ready, send Shutdown so a successful session exits 0 quickly
    // rather than waiting the full idle timeout — only applies if still alive.
    // (If already exited, we're done.)
    let _ = exit_code;

    server.abort();
    let _ = std::fs::remove_file(sock_path);

    let st = state.lock().await;
    let caps = st.router.caps(miner_id).cloned();
    let handshake_ok = caps.is_some();
    SessionHarnessReport {
        caps,
        miner_exit_code: exit_code,
        handshake_ok,
    }
}
