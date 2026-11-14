//! Composition root: wires the coordinator, the miner sessions, the chain,
//! and the dashboard together for one environment.

pub mod chain_env;

use crate::coordinator::{ResultSink, WorkSource};

/// Everything the coordinator needs from the world it runs in: where work
/// comes from, and where validated results go.
pub trait Environment: WorkSource + ResultSink + 'static {}

impl<T: WorkSource + ResultSink + 'static> Environment for T {}
