//! Log subscriber setup for the `quip-coordinator` binary.
//!
//! Every operational message in this crate goes through `tracing`. The
//! `tracing` macros are a facade: without an installed subscriber they compile
//! to no-ops, so a binary that never calls [`init`] emits nothing at any
//! `RUST_LOG` setting. [`init`] is that call.
//!
//! Output goes to **stderr**, never stdout: `drive` mode prints its timing
//! table to stdout, and log lines interleaved into it would corrupt a report a
//! caller is parsing.

use std::fmt;
use std::io::IsTerminal as _;
use tracing_subscriber::EnvFilter;

/// Floor-divide a milli-energy to whole units for an operator log.
///
/// Rust `/` truncates toward zero. Negative energies would then look better
/// than they are. Euclid division floors.
#[must_use]
pub(crate) fn energy_units(milli: i64) -> i64 {
    milli.div_euclid(1000)
}

/// Render an optional value as the bare value, or `none` when empty.
///
/// Operator logs must not print `Some(...)` or `None`.
#[must_use]
pub(crate) fn display_option<T: fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "none".to_owned(), |v| v.to_string())
}

/// Render an optional milli-energy as whole units, or `none`.
#[must_use]
pub(crate) fn display_energy(milli: Option<i64>) -> String {
    display_option(milli.map(energy_units))
}

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
    const fn as_str(self) -> &'static str {
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

/// Targets whose events are the coordinator's own operational narration. Third
/// party crates (jsonrpsee, tonic, hyper) stay at `warn` under the default
/// filter so `--log-level debug` stays readable; `RUST_LOG` is the escape hatch
/// when their internals are what you actually need. Miner child processes
/// inherit the coordinator stdio and emit under their own subscriber, so they
/// are not listed here.
const OWN_TARGETS: [&str; 3] = ["quip_coordinator", "quip_protocol", "quip_solver_core"];

/// Build the default filter for `level`: third-party crates at `warn`, this
/// coordinator's own targets at `level`.
fn default_filter(level: LogLevel) -> String {
    let mut s = String::from("warn");
    for target in OWN_TARGETS {
        s.push(',');
        s.push_str(target);
        s.push('=');
        s.push_str(level.as_str());
    }
    s
}

/// Resolve the filter directives to install.
///
/// Precedence, highest first:
/// 1. An explicit `--log-level` (`level` is `Some`) — the operator asked for a
///    specific verbosity on this run.
/// 2. `RUST_LOG`, when set and non-empty.
/// 3. [`LogLevel::Info`], matching the v0.2.1 CLI default.
fn resolve_directives(level: Option<LogLevel>, rust_log: Option<&str>) -> String {
    if let Some(level) = level {
        return default_filter(level);
    }
    match rust_log {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => default_filter(LogLevel::Info),
    }
}

/// Install the global log subscriber, writing to stderr.
///
/// Call once, as early in `main` as possible: messages emitted before this
/// returns are discarded.
///
/// # Errors
/// Returns an error when the resolved directives do not parse or a global
/// subscriber is already installed.
pub fn init(level: Option<LogLevel>) -> Result<(), String> {
    let rust_log = std::env::var("RUST_LOG").ok();
    let directives = resolve_directives(level, rust_log.as_deref());
    let filter = EnvFilter::try_new(&directives)
        .map_err(|e| format!("invalid log filter {directives:?}: {e}"))?;
    // Color only a real terminal. A coordinator normally runs supervised, with
    // stderr captured to a file or journal by whatever started it (the node
    // manager does exactly this), and escape sequences baked into that capture
    // corrupt every downstream grep.
    let ansi = std::io::stderr().is_terminal();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_ansi(ansi)
        .try_init()
        .map_err(|e| format!("cannot install log subscriber: {e}"))
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex, Once};
    use tracing_subscriber::fmt::MakeWriter;

    pub(crate) fn init_capture() {
        static INIT: Once = Once::new();
        // Keep a silent subscriber alive between captures, so callsites used
        // concurrently by uncaptured tests never cache a disabled interest.
        INIT.call_once(|| {
            tracing::subscriber::set_global_default(tracing_subscriber::registry())
                .expect("test tracing registry");
        });
    }

    #[derive(Clone)]
    pub(crate) struct Capture(pub(crate) Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("capture lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    pub(crate) fn drain(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        let mut g = buf.lock().expect("capture lock");
        let text = String::from_utf8_lossy(&g).into_owned();
        g.clear();
        text
    }

    pub(crate) fn capture_logs(run: impl FnOnce()) -> String {
        init_capture();
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .without_time()
            .with_ansi(false)
            .with_writer(Capture(Arc::clone(&buf)))
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        drain(&buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr as _;

    #[test]
    fn resolve_directives_prefers_explicit_level_then_rust_log_then_info() {
        for (label, level, rust_log, expected) in [
            (
                "explicit level beats RUST_LOG",
                Some(LogLevel::Debug),
                Some("trace"),
                default_filter(LogLevel::Debug),
            ),
            (
                "RUST_LOG used when no explicit level",
                None,
                Some("quip_coordinator=trace"),
                "quip_coordinator=trace".to_string(),
            ),
            (
                "blank RUST_LOG falls back to info",
                None,
                Some("   "),
                default_filter(LogLevel::Info),
            ),
            (
                "no RUST_LOG falls back to info",
                None,
                None,
                default_filter(LogLevel::Info),
            ),
        ] {
            assert_eq!(resolve_directives(level, rust_log), expected, "{label}");
        }
    }

    #[test]
    fn default_filter_keeps_third_party_at_warn_and_parses_at_every_level() {
        let d = default_filter(LogLevel::Debug);
        assert!(d.starts_with("warn,"), "{d}");
        assert!(d.contains("quip_coordinator=debug"), "{d}");
        assert!(d.contains("quip_protocol=debug"), "{d}");
        assert!(d.contains("quip_solver_core=debug"), "{d}");
        // Miner children write straight to the terminal; no re-emit target.
        assert!(!d.contains("miner="), "{d}");

        for level in [
            LogLevel::Trace,
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warn,
            LogLevel::Error,
        ] {
            let d = default_filter(level);
            assert!(EnvFilter::try_new(&d).is_ok(), "{d}");
        }
    }

    #[test]
    fn level_display_round_trips_to_directive_text() {
        assert_eq!(LogLevel::Warn.to_string(), "warn");
        assert!(tracing::Level::from_str(LogLevel::Trace.as_str()).is_ok());
    }

    #[test]
    fn energy_units_floors_and_display_prints_none_not_debug() {
        assert_eq!(energy_units(-14_369_000), -14_369);
        assert_eq!(energy_units(-14_535_322), -14_536);
        assert_eq!(energy_units(-14_536_604), -14_537);
        assert_eq!(energy_units(-14_513_000), -14_513);
        assert_eq!(energy_units(0), 0);
        assert_eq!(energy_units(1_500), 1);

        assert_eq!(display_option(Some(10_132_u64)), "10132");
        assert_eq!(display_option(None::<u64>), "none");
        assert_eq!(display_energy(Some(-14_536_604)), "-14537");
        assert_eq!(display_energy(None), "none");
    }
}
