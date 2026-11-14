//! The coordinator's own log verbosity, as the `--log-level` flag names it.

use std::fmt;

/// Verbosity for the default filter, mirroring the `--log-level` values the
/// v0.2.1 miner CLI accepted.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    /// Everything, including per-job and per-poll detail.
    Trace,
    /// Per-round and per-miner detail useful when diagnosing a stall.
    Debug,
    /// Operational narration: startup, rounds, staging, submits. The default.
    Info,
    /// Only degraded conditions.
    Warn,
    /// Only failures.
    Error,
}

impl LogLevel {
    /// The `tracing` directive text for this level.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
