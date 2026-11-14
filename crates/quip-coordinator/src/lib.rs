//! quip-coordinator: chain access, job production, routing, validation,
//! miner session server, and process supervision for the v0.3 protocol.

pub mod app;
pub mod chain;
pub mod cli;
pub mod config;
pub mod coordinator;
pub mod dashboard;
pub mod host;
pub mod miner;
pub mod observe;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
pub mod types;
