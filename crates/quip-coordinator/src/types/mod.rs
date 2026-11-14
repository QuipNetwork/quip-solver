//! Plain data shared across the crate. This module imports nothing from the
//! rest of the crate, so every other module may depend on it.

pub mod family;
pub mod hex;
pub mod host;
pub mod miner;
pub mod problem;
pub mod proof;
pub mod snapshot;
pub mod topology;
pub mod topology_spec;
