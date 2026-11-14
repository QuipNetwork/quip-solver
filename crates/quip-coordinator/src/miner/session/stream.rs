//! A session's outbound stream and the messages the coordinator pushes on it.

use super::cancel_timing::CancelTiming;
use quip_proto::v1::{coord_msg, CoordMsg, Shutdown};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt as _;
use tonic::Status;

/// A session's outbound messages as the transport reads them.
pub type OutboundStream =
    std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<CoordMsg, Status>> + Send>>;

/// Wrap a session's outbound channel so `timing` sees each message at the
/// moment the transport takes it off the channel.
#[expect(
    clippy::result_large_err,
    reason = "tonic fixes the stream item as Result<CoordMsg, Status>"
)]
pub(crate) fn outbound_stream(
    rx: mpsc::Receiver<Result<CoordMsg, Status>>,
    timing: Arc<CancelTiming>,
) -> OutboundStream {
    Box::pin(ReceiverStream::new(rx).map(move |item| {
        if let Ok(msg) = &item {
            timing.observe_outbound(msg);
        }
        item
    }))
}

/// Push a cancel for generations `<= max_generation` to a live session
/// channel, timing it on `timing` when the session has one.
///
/// # Errors
///
/// Returns a send error if the session channel is closed.
pub(crate) async fn send_cancel(
    tx: &mpsc::Sender<Result<CoordMsg, Status>>,
    timing: Option<&CancelTiming>,
    max_generation: u64,
) -> Result<(), mpsc::error::SendError<Result<CoordMsg, Status>>> {
    if let Some(t) = timing {
        t.send_started(max_generation);
    }
    tx.send(Ok(coord(coord_msg::Msg::Cancel(quip_proto::v1::Cancel {
        max_generation,
    }))))
    .await?;
    if let Some(t) = timing {
        t.enqueued(max_generation);
    }
    Ok(())
}

pub(crate) fn coord(msg: coord_msg::Msg) -> CoordMsg {
    CoordMsg { msg: Some(msg) }
}

/// Issue a `Shutdown` on an outbound channel (for supervisor).
#[must_use]
pub fn shutdown_msg(grace_ms: u32) -> CoordMsg {
    coord(coord_msg::Msg::Shutdown(Shutdown { grace_ms }))
}
