//! Offline calibration: read the reference winners from a chain, spawn one
//! miner, send it `Topology` and then `Calibrate`, and report how it answered.
//! No job is sent and nothing is submitted.

use super::{spawn_miner_session, MinerSession, SpawnSpec};
use crate::app::chain_env::readiness::{plan_calibration, CalibrationSkip};
use crate::chain::ChainClient;
use crate::config::launch::LaunchEntry;
use crate::config::{CalibrationConfig, MAX_CALIBRATION_QBLOCKS};
use crate::coordinator::calibration::CalibrationPlan;
pub use crate::coordinator::calibration::CALIBRATION_GRACE;
use crate::miner::session::handlers::{Flow, SessionLink};
use crate::miner::SessionEvents;
use crate::types::calibration::HardwareFilter;
use crate::types::log::LogLevel;
use crate::types::topology::Topology;
use quip_proto::v1::{Calibrate, CalibrateDone, CalibrateOutcome, Capabilities, Configure};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Mutex};

/// The id the calibration run gives its one miner.
const MINER_ID: &str = "calibrate-0";

/// How long the miner gets to exit after `Shutdown` before it is killed.
const EXIT_WAIT: Duration = Duration::from_secs(10);

/// What a calibration run asks for.
pub struct CalibrateRequest<'a> {
    /// Miner binary to spawn.
    pub miner_bin: &'a str,
    /// Unix-domain socket path the run binds and the miner dials.
    pub sock_path: &'a str,
    /// How many recent winning qblocks to send, `1..=1000`.
    pub qblocks: u32,
    /// Hardware classes the winners must come from. Empty admits every winner.
    pub kinds: &'a [String],
    /// Seconds the miner has to calibrate. Must be above zero.
    pub deadline_s: u64,
    /// How long past the deadline the run waits for an answer.
    pub grace: Duration,
    /// Verbosity forwarded to the miner.
    pub log_level: LogLevel,
}

/// How a calibration that ran to an answer ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrateResult {
    /// The new tuning is in effect.
    Done,
    /// The solver kept its tuning.
    Failed,
    /// The solver passed its deadline and kept its tuning.
    DeadlinePassed,
    /// No answer arrived by the deadline and grace.
    TimedOut,
}

impl CalibrateResult {
    /// The name printed in the `outcome=` field.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Done => "DONE",
            Self::Failed => "FAILED",
            Self::DeadlinePassed => "DEADLINE_PASSED",
            Self::TimedOut => "TIMED_OUT",
        }
    }
}

/// A finished calibration run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrateReport {
    /// How the run ended.
    pub result: CalibrateResult,
    /// The miner's free-text summary, or a note when it sent none.
    pub summary: String,
}

/// Why a calibration run did not reach an answer.
#[derive(Debug)]
pub enum CalibrateError {
    /// The request, the miner, or the chain state cannot calibrate: a bad
    /// value, a miner without the `calibrate` feature, no family, or no winners.
    Usage(String),
    /// The chain could not be read.
    Rpc(String),
    /// The miner never completed the handshake.
    Handshake(i32),
    /// The miner left, or the run was interrupted, before it answered.
    Incomplete(String),
}

impl std::fmt::Display for CalibrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(m) | Self::Rpc(m) | Self::Incomplete(m) => write!(f, "{m}"),
            Self::Handshake(code) => write!(f, "miner handshake failed (exit code {code})"),
        }
    }
}

impl std::error::Error for CalibrateError {}

/// What the session tells the run.
enum Event {
    /// The miner does not list the `calibrate` feature.
    Unsupported,
    /// The miner answered.
    Done(CalibrateDone),
    /// The miner gave up.
    Fatal,
}

/// The session side of a calibration run: sends the one `Calibrate`, forwards
/// the answer, and keeps the link so the run can shut the miner down.
struct CalibrateService {
    plan: Mutex<CalibrationPlan>,
    events: mpsc::UnboundedSender<Event>,
    link: StdMutex<Option<SessionLink>>,
    /// The miner completed the `Hello` handshake.
    joined: AtomicBool,
    /// The miner lacks the `calibrate` feature; the session ends at once.
    unsupported: AtomicBool,
}

impl CalibrateService {
    fn emit(&self, event: Event) {
        // The receiver lives until the run ends; a send after that is moot.
        let _ = self.events.send(event);
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[tonic::async_trait]
impl SessionEvents for CalibrateService {
    async fn joined(&self, _miner_id: &str, _caps: &Capabilities) {
        self.joined.store(true, Ordering::SeqCst);
    }

    async fn ready(&self, _miner_id: &str) {}

    async fn job_request(&self, _miner_id: &str, _credits: u32) -> Vec<quip_proto::v1::Job> {
        Vec::new()
    }

    async fn result(&self, _link: &SessionLink, _result: quip_proto::v1::Result) -> Flow {
        Flow::Continue
    }

    async fn reject(&self, _link: &SessionLink, _rej: quip_proto::v1::Reject) -> Flow {
        Flow::Continue
    }

    async fn lease_done(&self, _link: &SessionLink, _done: quip_proto::v1::LeaseDone) -> Flow {
        Flow::Continue
    }

    async fn status(&self, _miner_id: &str, _status: &quip_proto::v1::Status) {}

    async fn capabilities(&self, _miner_id: &str, _caps: &Capabilities) {}

    async fn calibration(
        &self,
        miner_id: &str,
        caps: &Capabilities,
        topology_hash: &[u8],
    ) -> Option<Calibrate> {
        let due =
            self.plan
                .lock()
                .await
                .due_for(miner_id, &caps.features, topology_hash, now_unix_ms());
        if due.is_none() {
            self.unsupported.store(true, Ordering::SeqCst);
            self.emit(Event::Unsupported);
        }
        due
    }

    async fn calibrate_done(&self, _miner_id: &str, done: &CalibrateDone) {
        self.emit(Event::Done(done.clone()));
    }

    async fn fatal(&self, _link: &SessionLink, _fatal: &quip_proto::v1::Fatal) -> Flow {
        self.emit(Event::Fatal);
        Flow::End
    }

    async fn settled(&self, link: &SessionLink) -> Flow {
        if let Ok(mut slot) = self.link.lock() {
            *slot = Some(link.clone());
        }
        if self.unsupported.load(Ordering::SeqCst) {
            link.shutdown(500).await;
            return Flow::End;
        }
        Flow::Continue
    }
}

fn result_of(done: &CalibrateDone) -> CalibrateReport {
    let result = match CalibrateOutcome::try_from(done.outcome) {
        Ok(CalibrateOutcome::Done) => CalibrateResult::Done,
        Ok(CalibrateOutcome::DeadlinePassed) => CalibrateResult::DeadlinePassed,
        // An outcome this build cannot name keeps the solver's tuning too.
        Ok(CalibrateOutcome::Failed | CalibrateOutcome::Unspecified) | Err(_) => {
            CalibrateResult::Failed
        }
    };
    CalibrateReport {
        result,
        summary: done
            .summary
            .clone()
            .unwrap_or_else(|| "the miner sent no summary".to_owned()),
    }
}

fn topology_of(snapshot: &crate::types::snapshot::MiningSnapshot) -> Topology {
    let mut topology = Topology::from_nodes_edges(
        snapshot.nodes.clone(),
        snapshot.edges.clone(),
        &snapshot.allowed_h_milli,
        &snapshot.allowed_j_milli,
        &snapshot.allowed_spin_milli,
    );
    // A family topology hash also covers the descriptor, so the value sets
    // alone cannot rebuild the chain's hash.
    topology.hash.clone_from(&snapshot.topology_hash);
    topology
}

/// Check the request and read the chain: the plan to send and the topology to
/// send it on.
async fn prepare(
    chain: &(impl ChainClient + ?Sized),
    req: &CalibrateRequest<'_>,
) -> Result<(CalibrationPlan, Topology), CalibrateError> {
    if !(1..=MAX_CALIBRATION_QBLOCKS).contains(&req.qblocks) {
        return Err(CalibrateError::Usage(format!(
            "--qblocks must be from 1 to {MAX_CALIBRATION_QBLOCKS}"
        )));
    }
    if req.deadline_s == 0 {
        return Err(CalibrateError::Usage(
            "--deadline-s must be above zero".to_owned(),
        ));
    }
    // Reject an unknown kind before any RPC.
    let _ = HardwareFilter::parse(req.kinds).map_err(CalibrateError::Usage)?;

    let snapshot = chain
        .fetch_mining_snapshot(None, [0; 32], None)
        .await
        .map_err(|e| CalibrateError::Rpc(format!("read mining snapshot: {e}")))?
        .ok_or_else(|| CalibrateError::Usage("the chain has no mining snapshot".to_owned()))?;
    let cfg = CalibrationConfig {
        enabled: true,
        qblocks: req.qblocks,
        kinds: req.kinds.to_vec(),
        deadline_s: req.deadline_s,
    };
    let plan = plan_calibration(chain, &snapshot, &cfg)
        .await
        .map_err(|skip| match skip {
            CalibrationSkip::Read(e) => CalibrateError::Rpc(format!("read reference winners: {e}")),
            other => CalibrateError::Usage(format!("cannot calibrate: {other}")),
        })?;
    Ok((plan, topology_of(&snapshot)))
}

/// Calibrate one miner on the newest winners of `chain`'s mining topology.
///
/// Reads the mining snapshot and the winners, spawns the miner, sends
/// `Topology` and then `Calibrate`, waits for `CalibrateDone` or for the
/// deadline plus grace, and sends `Shutdown`. The miner process always ends
/// before this returns.
///
/// # Errors
/// [`CalibrateError`] names why no answer arrived.
pub async fn calibrate_miner(
    chain: &(impl ChainClient + ?Sized),
    req: &CalibrateRequest<'_>,
) -> Result<CalibrateReport, CalibrateError> {
    let (plan, topology) = prepare(chain, req).await?;
    let wait = Duration::from_secs(req.deadline_s).saturating_add(req.grace);
    let entry = LaunchEntry {
        miner_id: MINER_ID.into(),
        binary: req.miner_bin.to_owned(),
        backend: "cpu".into(),
        device: None,
        configure: Configure {
            queue_depth: 0,
            // The miner gets no job while it calibrates, so it must not
            // time out as idle before the deadline and grace pass.
            idle_timeout_s: u32::try_from(wait.as_secs().saturating_add(30)).unwrap_or(u32::MAX),
            heartbeat_s: 15,
            reconnect_window_s: 60,
            backend_toml: String::new(),
        },
    };
    let mut st = crate::coordinator::state::CoordinatorState::new();
    st.set_topology(Some(topology));

    let (events, mut inbox) = mpsc::unbounded_channel();
    let service = Arc::new(CalibrateService {
        plan: Mutex::new(plan),
        events,
        link: StdMutex::new(None),
        joined: AtomicBool::new(false),
        unsupported: AtomicBool::new(false),
    });
    let MinerSession {
        state,
        server,
        mut child,
    } = spawn_miner_session(
        &SpawnSpec {
            miner_bin: req.miner_bin,
            sock_path: req.sock_path,
            miner_id: MINER_ID,
            entry: &entry,
            log_level: req.log_level,
            utilization: None,
            yielding: false,
        },
        st,
        |_| Arc::clone(&service),
    );

    let waited = tokio::select! {
        Some(event) = inbox.recv() => Waited::Event(event),
        status = child.wait() => {
            let code = status.map_or(-1, |s| s.code().unwrap_or(-1));
            // An answer sent just before the miner exited is already queued.
            inbox.try_recv().map_or(Waited::Exited(code), Waited::Event)
        }
        () = tokio::time::sleep(wait) => Waited::TimedOut,
        _ = tokio::signal::ctrl_c() => Waited::Interrupted,
    };
    let link = service.link.lock().ok().and_then(|slot| slot.clone());
    if let Some(link) = link {
        link.shutdown(500).await;
    }
    // Reap the miner on every path: give it time to leave after `Shutdown`,
    // then kill it.
    if tokio::time::timeout(EXIT_WAIT, child.wait()).await.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    server.abort();
    let _ = std::fs::remove_file(req.sock_path);
    let joined = service.joined.load(Ordering::SeqCst);
    drop(state);

    outcome_of(waited, joined, req.grace)
}

/// Map what ended the wait to the report or the error.
fn outcome_of(
    waited: Waited,
    joined: bool,
    grace: Duration,
) -> Result<CalibrateReport, CalibrateError> {
    match waited {
        Waited::Event(Event::Done(done)) => Ok(result_of(&done)),
        Waited::Event(Event::Unsupported) => Err(CalibrateError::Usage(
            "the miner does not list the calibrate feature".to_owned(),
        )),
        Waited::Event(Event::Fatal) => Err(CalibrateError::Incomplete(
            "the miner sent Fatal before it answered".to_owned(),
        )),
        Waited::Exited(code) if joined => Err(CalibrateError::Incomplete(format!(
            "the miner exited (code {code}) before it answered"
        ))),
        Waited::Exited(code) => Err(CalibrateError::Handshake(code)),
        Waited::TimedOut if joined => Ok(CalibrateReport {
            result: CalibrateResult::TimedOut,
            summary: format!(
                "no CalibrateDone within {} s of the deadline",
                grace.as_secs()
            ),
        }),
        Waited::TimedOut => Err(CalibrateError::Handshake(-1)),
        Waited::Interrupted => Err(CalibrateError::Incomplete(
            "interrupted before the miner answered".to_owned(),
        )),
    }
}

/// What ended the wait.
enum Waited {
    Event(Event),
    Exited(i32),
    TimedOut,
    Interrupted,
}
