//! The one session loop every environment runs: handshake, dispatcher task,
//! and message dispatch.

use super::cancel_timing::{CancelLatency, CancelTiming};
use super::state::SessionHost;
use super::stream::{coord, shutdown_msg};
use crate::miner::SessionEvents;
use quip_proto::v1::{coord_msg, miner_msg, Configure, CoordMsg, MinerMsg, Welcome};
use quip_protocol::session::PROTOCOL_VERSION;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::StreamExt as _;
use tonic::Status;

/// Depth of the read loop's queue of credit grants to the dispatcher task.
/// Grants are single `u32`s, so this only absorbs a burst of `JobRequest`s
/// arriving while the dispatcher is mid-send.
pub(crate) const GRANT_CHANNEL_DEPTH: usize = 1024;

/// Grant `credits` and send every job the miner is now entitled to.
///
/// Jobs are collected first and sent after: the implementation takes its
/// locks while collecting, and holding one across a `tx.send().await` blocks
/// every *other* miner's session for as long as this one's outbound channel
/// stays full.
///
/// Returns `false` when the outbound channel is closed (session is over).
pub(crate) async fn dispatch_granted<E: SessionEvents + ?Sized>(
    events: &E,
    miner_id: &str,
    credits: u32,
    tx: &mpsc::Sender<Result<CoordMsg, Status>>,
) -> bool {
    for job in events.job_request(miner_id, credits).await {
        if tx.send(Ok(coord(coord_msg::Msg::Job(job)))).await.is_err() {
            return false;
        }
    }
    true
}

/// Whether the session loop keeps reading after an event, or ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Keep reading the miner's messages.
    Continue,
    /// End the session.
    End,
}

/// A live session as an event handler sees it: the miner and the channel to it.
#[derive(Clone, Debug)]
pub struct SessionLink {
    /// The miner on the other end.
    pub miner_id: String,
    tx: mpsc::Sender<Result<CoordMsg, Status>>,
}

impl SessionLink {
    /// Send the miner an in-band `Shutdown` with `grace_ms`.
    pub async fn shutdown(&self, grace_ms: u32) {
        let _ = self.tx.send(Ok(shutdown_msg(grace_ms))).await;
    }
}

/// The one session loop: handshake, dispatcher task, and message dispatch.
///
/// The token, `Configure`, topology, and target come from `state`; every
/// message after the handshake goes to `events`.
#[expect(
    clippy::too_many_lines,
    reason = "session loop is one cohesive handshake + message dispatch"
)]
pub async fn run_session<S: SessionHost, E: SessionEvents>(
    inbound: &mut (impl tokio_stream::Stream<Item = Result<MinerMsg, Status>> + Unpin),
    tx: &mpsc::Sender<Result<CoordMsg, Status>>,
    state: Arc<Mutex<S>>,
    events: Arc<E>,
    cancel_timing: Arc<CancelTiming>,
) {
    // 1. Hello
    let Some(Ok(MinerMsg {
        msg: Some(miner_msg::Msg::Hello(hello)),
    })) = inbound.next().await
    else {
        tracing::warn!("miner handshake rejected; expected Hello");
        return;
    };

    let miner_id = hello.miner_id.clone();
    let caps = hello.capabilities.clone().unwrap_or_default();
    let (token_ok, protocol_ok, configure) = {
        let st = state.lock().await;
        let sessions = st.sessions();
        let expected = sessions
            .expected_tokens
            .get(&miner_id)
            .cloned()
            .unwrap_or_default();
        let token_ok = !expected.is_empty() && hello.session_token == expected;
        let protocol_ok = hello.capabilities.is_some() && caps.protocol_version == PROTOCOL_VERSION;
        let configure = sessions
            .configure
            .get(&miner_id)
            .cloned()
            .unwrap_or(Configure {
                queue_depth: 0,
                idle_timeout_s: 300,
                heartbeat_s: 15,
                reconnect_window_s: 60,
                backend_toml: String::new(),
            });
        (token_ok, protocol_ok, configure)
    };

    if !token_ok || !protocol_ok {
        // Drop the stream; well-behaved miners exit 77 on handshake failure.
        // Say why: a rejected miner is otherwise invisible from both sides —
        // the coordinator drops the stream and the miner only sees EOF.
        tracing::warn!(
            miner = %miner_id,
            token_ok,
            protocol_ok,
            capabilities_present = hello.capabilities.is_some(),
            offered_protocol = ?hello.capabilities.as_ref().map(|c| c.protocol_version),
            "miner handshake rejected; dropping session"
        );
        return;
    }

    events.joined(&miner_id, &caps).await;

    // 2. Welcome + Configure (+ Topology if cached)
    if tx
        .send(Ok(coord(coord_msg::Msg::Welcome(Welcome {
            protocol_version: PROTOCOL_VERSION,
        }))))
        .await
        .is_err()
    {
        return;
    }
    if tx
        .send(Ok(coord(coord_msg::Msg::Configure(configure))))
        .await
        .is_err()
    {
        return;
    }
    {
        let st = state.lock().await;
        for msg in st.requirements() {
            let _ = tx.send(Ok(coord(msg))).await;
        }
    }

    // Register this session's outbound channel so the supervisor can push an
    // in-band Shutdown/cancel while it runs; dropped when the loop exits.
    {
        let mut st = state.lock().await;
        let sessions = st.sessions_mut();
        sessions.register_outbound(&miner_id, tx.clone());
        let _ = sessions
            .cancel_timings
            .insert(miner_id.clone(), Arc::clone(&cancel_timing));
    }

    let link = SessionLink {
        miner_id: miner_id.clone(),
        tx: tx.clone(),
    };
    if events.settled(&link).await == Flow::End {
        state
            .lock()
            .await
            .sessions_mut()
            .deregister_outbound(&miner_id);
        return;
    }

    // Declared before the dispatcher so the wakeup sender can be registered
    // alongside it; see `wake_dispatcher`.
    // Dispatcher task: the only writer of `Job`s for this session.
    //
    // Job sends must not happen on the read path. `tx.send().await` blocks once
    // the outbound channel fills, and this loop is the only reader of the
    // miner's `Result`s — so blocking here stops us consuming the results that
    // would free the miner's credits, while the miner (symmetrically blocked
    // writing those results) stops reading jobs. Both peers park permanently.
    let (grants, mut grant_rx) = mpsc::channel::<u32>(GRANT_CHANNEL_DEPTH);
    state
        .lock()
        .await
        .sessions_mut()
        .register_wakeup(&miner_id, grants.clone());
    let _dispatcher = {
        let events = Arc::clone(&events);
        let tx = tx.clone();
        let miner_id = miner_id.clone();
        tokio::spawn(async move {
            while let Some(credits) = grant_rx.recv().await {
                if !dispatch_granted(events.as_ref(), &miner_id, credits, &tx).await {
                    return;
                }
            }
        })
    };

    // 3. Message loop
    loop {
        let Some(Ok(msg)) = inbound.next().await else {
            break;
        };
        let flow = match msg.msg {
            Some(miner_msg::Msg::Ready(_)) => {
                events.ready(&miner_id).await;
                Flow::Continue
            }
            // The only source of dispatch credit. Routed through the dispatcher
            // so a job send never blocks the read path.
            Some(miner_msg::Msg::JobRequest(req)) => {
                tracing::debug!(miner = %miner_id, credits = req.credits, "miner requested work");
                if grants.send(req.credits).await.is_err() {
                    Flow::End
                } else {
                    Flow::Continue
                }
            }
            Some(miner_msg::Msg::Result(result)) => events.result(&link, result).await,
            Some(miner_msg::Msg::Reject(rej)) => events.reject(&link, rej).await,
            Some(miner_msg::Msg::Status(s)) => {
                if let Some(latency) = cancel_timing.acknowledge(s.abandoned_generation) {
                    log_cancel_acknowledged(&miner_id, s.abandoned_generation, &latency);
                }
                events.status(&miner_id, &s).await;
                Flow::Continue
            }
            Some(miner_msg::Msg::Fatal(f)) => events.fatal(&link, &f).await,
            // The handshake already consumed one Hello. A second one means the
            // miner restarted its session without reconnecting.
            Some(miner_msg::Msg::Hello(_)) => {
                tracing::warn!(miner = %miner_id, "miner sent Hello mid-session; ignoring");
                Flow::Continue
            }
            Some(miner_msg::Msg::Capabilities(caps)) => {
                events.capabilities(&miner_id, &caps).await;
                Flow::Continue
            }
            Some(miner_msg::Msg::LeaseDone(done)) => events.lease_done(&link, done).await,
            Some(miner_msg::Msg::CalibrateDone(done)) => {
                events.calibrate_done(&miner_id, &done).await;
                Flow::Continue
            }
            // An empty `msg` is a message this build cannot name: either a
            // field number the miner uses and this coordinator does not, or the
            // reverse. Version skew between the two arrives here and nowhere
            // else, so it must not be silent.
            None => {
                tracing::warn!(
                    miner = %miner_id,
                    "unrecognized message from miner; check the miner and coordinator protocol versions"
                );
                Flow::Continue
            }
        };
        if flow == Flow::End || events.settled(&link).await == Flow::End {
            break;
        }
    }

    state
        .lock()
        .await
        .sessions_mut()
        .deregister_outbound(&miner_id);
}

/// Report how long one miner took to acknowledge a `Cancel`, split into the
/// intervals [`CancelLatency`] describes. An interval the transport never
/// observed prints as `-`.
fn log_cancel_acknowledged(miner_id: &str, abandoned_generation: u64, latency: &CancelLatency) {
    let ms = |d: Option<Duration>| {
        d.map_or_else(
            || "-".to_owned(),
            |d| format!("{:.1}", d.as_secs_f64() * 1000.0),
        )
    };
    tracing::info!(
        miner = %miner_id,
        max_generation = latency.max_generation,
        abandoned_generation,
        send_wait_ms = %ms(latency.send_wait),
        queue_delay_ms = %ms(latency.queue_delay),
        ack_wait_ms = %ms(latency.ack_wait),
        total_ms = %ms(Some(latency.total)),
        "miner acknowledged cancel"
    );
}
