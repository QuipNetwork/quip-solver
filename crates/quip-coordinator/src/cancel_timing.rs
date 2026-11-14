//! Monotonic timing for one miner's `Cancel`, from the moment the coordinator
//! starts to send it to the `Status` that acknowledges it.
//!
//! The pause log line is written after the coordinator queues the `Cancel`,
//! not after the miner has it, so that line cannot show how long the miner
//! kept mining. These timestamps split the gap into three intervals:
//!
//! - **send wait**: how long the send blocked on a full outbound channel.
//! - **queue delay**: from the `Cancel` entering the outbound channel to the
//!   transport taking it off. Every message queued ahead of it, such as
//!   dispatched jobs, adds to this interval.
//! - **acknowledgement**: from the transport taking the `Cancel` to the
//!   session reading a `Status` whose `abandoned_generation` covers it. This
//!   interval includes the wire, the miner, and any messages queued ahead of
//!   the `Status` in the session's read loop.
//!
//! The clock is tokio's, which is monotonic and which tests can pause.

use quip_proto::v1::{coord_msg, CoordMsg};
use std::sync::{Mutex, PoisonError};
use tokio::time::{Duration, Instant};

/// Timestamps for the newest unacknowledged `Cancel` of one session.
#[derive(Debug, Default)]
pub struct CancelTiming {
    trace: Mutex<Option<Trace>>,
}

#[derive(Debug, Clone, Copy)]
struct Trace {
    max_generation: u64,
    send_started: Instant,
    enqueued: Option<Instant>,
    dequeued: Option<Instant>,
}

/// The measured intervals of one acknowledged `Cancel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancelLatency {
    /// The generation watermark the `Cancel` carried.
    pub max_generation: u64,
    /// Time the send blocked on a full outbound channel.
    pub send_wait: Option<Duration>,
    /// Time the `Cancel` sat in the outbound channel.
    pub queue_delay: Option<Duration>,
    /// Time from the transport taking the `Cancel` to the acknowledging
    /// `Status` being read.
    pub ack_wait: Option<Duration>,
    /// Time from the start of the send to the acknowledging `Status`.
    pub total: Duration,
}

impl CancelTiming {
    fn with_trace<T>(&self, f: impl FnOnce(&mut Option<Trace>) -> T) -> T {
        // A panic while the lock is held leaves only timestamps behind, which
        // stay valid, so a poisoned lock is used as it is.
        let mut trace = self.trace.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut trace)
    }

    /// Start timing a `Cancel` for `max_generation`. An earlier `Cancel` that
    /// is still unacknowledged is replaced: the `Status` that acknowledges the
    /// newer one covers both.
    pub fn send_started(&self, max_generation: u64) {
        let now = Instant::now();
        self.with_trace(|trace| {
            *trace = Some(Trace {
                max_generation,
                send_started: now,
                enqueued: None,
                dequeued: None,
            });
        });
    }

    /// Record that the `Cancel` for `max_generation` entered the outbound
    /// channel.
    pub fn enqueued(&self, max_generation: u64) {
        let now = Instant::now();
        self.with_trace(|trace| {
            if let Some(t) = trace
                .as_mut()
                .filter(|t| t.max_generation == max_generation)
            {
                // The transport can take the message before the sender records
                // this. That observation already set `enqueued`, so keep it.
                let _ = t.enqueued.get_or_insert(now);
            }
        });
    }

    /// Observe one message as the transport takes it off the outbound
    /// channel. Only the timed `Cancel` is recorded.
    pub fn observe_outbound(&self, msg: &CoordMsg) {
        let Some(coord_msg::Msg::Cancel(cancel)) = &msg.msg else {
            return;
        };
        let now = Instant::now();
        self.with_trace(|trace| {
            if let Some(t) = trace
                .as_mut()
                .filter(|t| t.max_generation == cancel.max_generation)
            {
                let _ = t.enqueued.get_or_insert(now);
                let _ = t.dequeued.get_or_insert(now);
            }
        });
    }

    /// Finish timing when a `Status` reports `abandoned_generation`. Returns
    /// the intervals if that covers the timed `Cancel`, and `None` otherwise.
    pub fn acknowledge(&self, abandoned_generation: u64) -> Option<CancelLatency> {
        let now = Instant::now();
        let t = self.with_trace(|trace| {
            if trace
                .as_ref()
                .is_some_and(|t| abandoned_generation >= t.max_generation)
            {
                trace.take()
            } else {
                None
            }
        })?;
        Some(CancelLatency {
            max_generation: t.max_generation,
            send_wait: t
                .enqueued
                .map(|e| e.saturating_duration_since(t.send_started)),
            queue_delay: t
                .enqueued
                .zip(t.dequeued)
                .map(|(e, d)| d.saturating_duration_since(e)),
            ack_wait: t.dequeued.map(|d| now.saturating_duration_since(d)),
            total: now.saturating_duration_since(t.send_started),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quip_proto::v1::Cancel;

    fn cancel(max_generation: u64) -> CoordMsg {
        CoordMsg {
            msg: Some(coord_msg::Msg::Cancel(Cancel { max_generation })),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn intervals_cover_send_queue_and_acknowledgement() {
        let timing = CancelTiming::default();
        timing.send_started(4);
        tokio::time::advance(Duration::from_millis(3)).await;
        timing.enqueued(4);
        tokio::time::advance(Duration::from_millis(20)).await;
        timing.observe_outbound(&CoordMsg::default());
        timing.observe_outbound(&cancel(4));
        tokio::time::advance(Duration::from_millis(7)).await;

        assert_eq!(
            timing.acknowledge(3),
            None,
            "an older watermark does not cover it"
        );
        let latency = timing.acknowledge(5).expect("a newer watermark covers it");
        assert_eq!(
            latency,
            CancelLatency {
                max_generation: 4,
                send_wait: Some(Duration::from_millis(3)),
                queue_delay: Some(Duration::from_millis(20)),
                ack_wait: Some(Duration::from_millis(7)),
                total: Duration::from_millis(30),
            }
        );
        assert_eq!(
            timing.acknowledge(5),
            None,
            "one Cancel is acknowledged once"
        );
    }

    /// The transport can take the `Cancel` before the sender records that it
    /// entered the channel. The queue delay is then zero, not negative or lost.
    #[tokio::test(start_paused = true)]
    async fn a_cancel_taken_before_the_enqueue_record_has_no_queue_delay() {
        let timing = CancelTiming::default();
        timing.send_started(2);
        tokio::time::advance(Duration::from_millis(1)).await;
        timing.observe_outbound(&cancel(2));
        tokio::time::advance(Duration::from_millis(1)).await;
        timing.enqueued(2);
        let latency = timing.acknowledge(2).expect("acknowledged");
        assert_eq!(latency.send_wait, Some(Duration::from_millis(1)));
        assert_eq!(latency.queue_delay, Some(Duration::ZERO));
        assert_eq!(latency.ack_wait, Some(Duration::from_millis(1)));
    }

    /// A newer `Cancel` replaces an unacknowledged older one, and the older
    /// one leaving the queue does not mark the newer one as taken.
    #[tokio::test(start_paused = true)]
    async fn a_newer_cancel_replaces_an_unacknowledged_one() {
        let timing = CancelTiming::default();
        timing.send_started(1);
        timing.enqueued(1);
        timing.send_started(2);
        timing.enqueued(2);
        timing.observe_outbound(&cancel(1));
        let latency = timing.acknowledge(2).expect("acknowledged");
        assert_eq!(latency.max_generation, 2);
        assert_eq!(latency.queue_delay, None);
    }
}
