//! The tonic `MinerService` server: session tokens, and one task per session
//! running the shared message loop.

use crate::miner::session::cancel_timing::CancelTiming;
use crate::miner::session::handlers::run_session;
use crate::miner::session::state::SessionHost;
use crate::miner::session::stream::{outbound_stream, OutboundStream};
use crate::miner::SessionEvents;
use quip_proto::v1::miner_service_server::MinerService;
use quip_proto::v1::{CoordMsg, MinerMsg};
use std::fmt::Write as _;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tonic::{Request, Response, Status, Streaming};

/// Unguessable per-spawn session token (32 random bytes, hex-encoded).
///
/// # Panics
///
/// Panics if the OS random number generator fails.
#[must_use]
pub fn gen_session_token() -> String {
    let mut buf = [0u8; 32];
    #[expect(
        clippy::expect_used,
        reason = "OS RNG failure is unrecoverable at process start"
    )]
    {
        getrandom::getrandom(&mut buf).expect("os rng");
    }
    let mut s = String::with_capacity(buf.len() * 2);
    for b in &buf {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Run one miner session on its own task and return the stream the transport
/// reads the session's outbound messages from.
pub fn start_session<S: SessionHost, E: SessionEvents>(
    mut inbound: Streaming<MinerMsg>,
    state: Arc<Mutex<S>>,

    events: Arc<E>,
) -> OutboundStream {
    let (tx, rx) = mpsc::channel::<Result<CoordMsg, Status>>(64);
    let cancel_timing = Arc::new(CancelTiming::default());
    let outbound = outbound_stream(rx, Arc::clone(&cancel_timing));
    drop(tokio::spawn(async move {
        run_session(&mut inbound, &tx, state, events, cancel_timing).await;
    }));
    outbound
}

/// The tonic `MinerService` for one environment: every session it accepts
/// runs the shared loop over `state`, and reports to `events`.
pub struct SessionService<S, E> {
    state: Arc<Mutex<S>>,
    events: Arc<E>,
}

impl<S, E> SessionService<S, E> {
    /// Serve sessions over `state`, with `events` doing the work.
    #[must_use]
    pub const fn new(state: Arc<Mutex<S>>, events: Arc<E>) -> Self {
        Self { state, events }
    }
}

#[tonic::async_trait]
impl<S: SessionHost, E: SessionEvents> MinerService for SessionService<S, E> {
    type SessionStream = OutboundStream;

    async fn session(
        &self,
        request: Request<Streaming<MinerMsg>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        Ok(Response::new(start_session(
            request.into_inner(),
            Arc::clone(&self.state),
            Arc::clone(&self.events),
        )))
    }
}
