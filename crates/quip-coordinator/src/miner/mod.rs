//! Miner interfaces: the session protocol each miner process speaks, and the
//! supervision of those processes. The traits here are what this module
//! calls; the coordinator and the environments implement them.

pub mod liveness;
pub mod process;
pub mod session;
pub mod supervisor;

use async_trait::async_trait;
use quip_proto::v1::{coord_msg, Capabilities};
use session::handlers::{Flow, SessionLink};

/// What a miner session reports, and what the work behind it answers. One
/// session loop, [`run_session`](session::handlers::run_session), drives every environment through this
/// trait: the loop owns the handshake, the outbound channel, and the
/// dispatcher task, and the implementation owns the work.
#[async_trait]
pub trait SessionEvents: Send + Sync + 'static {
    /// The miner passed the handshake with `caps`. Called before `Welcome`.
    async fn joined(&self, miner_id: &str, caps: &Capabilities);
    /// The miner sent `Ready`.
    async fn ready(&self, miner_id: &str);
    /// The miner granted `credits`. Returns the jobs to send it now.
    async fn job_request(&self, miner_id: &str, credits: u32) -> Vec<quip_proto::v1::Job>;
    /// The miner returned a `Result`.
    async fn result(&self, link: &SessionLink, result: quip_proto::v1::Result) -> Flow;
    /// The miner refused a job.
    async fn reject(&self, link: &SessionLink, reject: quip_proto::v1::Reject) -> Flow;
    /// The miner finished a lease.
    async fn lease_done(&self, link: &SessionLink, done: quip_proto::v1::LeaseDone) -> Flow;
    /// The miner sent a `Status`.
    async fn status(&self, miner_id: &str, status: &quip_proto::v1::Status);
    /// The miner sent a new capability snapshot.
    async fn capabilities(&self, miner_id: &str, caps: &Capabilities);
    /// The miner gave up.
    async fn fatal(&self, link: &SessionLink, fatal: &quip_proto::v1::Fatal) -> Flow;
    /// Called after the handshake and after every message: whether the
    /// session has work left.
    async fn settled(&self, link: &SessionLink) -> Flow;
}

/// The session server as the supervisor sees it: who may connect, how to stop
/// a live session, and where an exited miner's work goes.
#[async_trait]
pub trait SessionGate: Send + Sync {
    /// Issue a fresh session token for `miner_id`. The next handshake from
    /// that miner must present it.
    async fn expect(&self, miner_id: &str) -> String;
    /// Send an in-band `Shutdown` with `grace_ms` to the live session of
    /// `miner_id`, if it has one.
    async fn request_shutdown(&self, miner_id: &str, grace_ms: u32);
    /// Return the in-flight and staged jobs of an exited miner for routing to
    /// the others.
    async fn reclaim(&self, miner_id: &str);
    /// The supervisor gave up on `miner_id`: remove it from the router and
    /// route its jobs to the miners that remain. A job nobody can take is
    /// dropped, and a mempool order among them becomes stageable again.
    async fn retire(&self, miner_id: &str);
}

/// Sends one coordinator message to every live miner session.
#[async_trait]
pub trait MinerBroadcast: Send + Sync {
    /// Send `msg` to every live miner. Returns the number of miners sent to.
    async fn broadcast(&self, msg: coord_msg::Msg) -> usize;
}
