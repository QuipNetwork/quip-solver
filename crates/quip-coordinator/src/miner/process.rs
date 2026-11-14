//! Launch one miner binary: its argv and its environment.

use crate::config::launch::LaunchEntry;
use crate::types::log::LogLevel;
use tokio::process::{Child, Command};

/// Argv for a supervised miner child, program name excluded. Split out of the
/// spawn so the forwarding is testable: `[cuda.N]` must reach the binary as
/// `--device N`, and a backend whose CLI has no `--device` must never see the
/// flag (clap would exit 2, which `restart_policy` maps to `Never` — a miner
/// that never starts at all).
pub(crate) fn child_args(entry: &LaunchEntry, sock_uri: &str, log_level: LogLevel) -> Vec<String> {
    let mut args = vec![
        "--quip-coordinator".to_string(),
        sock_uri.to_string(),
        "--miner-id".to_string(),
        entry.miner_id.clone(),
        "--log-level".to_string(),
        log_level.to_string(),
    ];
    // Gate on the `Option`, never on a `backend == "cuda"` string test: the
    // type already carries "this backend takes a device ordinal".
    if let Some(n) = entry.device {
        args.push("--device".to_string());
        args.push(n.to_string());
    }
    args
}

/// Spawn a miner child with `QUIP_SESSION_TOKEN` (never argv). Stdout and stderr
/// inherit the coordinator's streams so miner log lines appear without a
/// re-emit pump. Pass `--log-level` so the child's own subscriber matches the
/// coordinator verbosity (the coordinator filter no longer sees child bytes).
pub(crate) fn spawn_supervised_child(
    entry: &LaunchEntry,
    token: &str,
    sock_uri: &str,
    log_level: LogLevel,
) -> std::io::Result<Child> {
    Command::new(&entry.binary)
        .args(child_args(entry, sock_uri, log_level))
        .env("QUIP_SESSION_TOKEN", token)
        .kill_on_drop(true)
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cuda_entry(miner_id: &str, device: Option<usize>) -> LaunchEntry {
        LaunchEntry {
            miner_id: miner_id.into(),
            binary: "quip-cuda-sa".into(),
            configure: quip_proto::v1::Configure::default(),
            backend: "cuda".into(),
            device,
        }
    }

    /// Position of the value that follows `flag`, or `None` when the flag is
    /// absent. Adjacency is the property under test: a matching value parked
    /// somewhere else in the argv would not reach the flag.
    fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let i = args.iter().position(|a| a == flag)?;
        args.get(i + 1).map(String::as_str)
    }

    #[test]
    fn cpu_child_argv_has_no_device_flag() {
        // A binary whose clap has no `--device` exits 2 on the flag, which the
        // restart policy turns into a backoff loop.
        let mut entry = cuda_entry("cpu-0", None);
        entry.backend = "cpu".into();
        entry.binary = "quip-cpu-sa".into();
        let args = child_args(&entry, "unix:///s.sock", LogLevel::Info);
        assert!(
            !args.iter().any(|a| a == "--device"),
            "cpu argv must not carry --device: {args:?}"
        );
    }

    /// Every child carries the coordinator address, its miner id and the log
    /// level, and the device flag pairs with the id. The first entry is built
    /// through the real config path, because the invariant the bug broke is
    /// that `[cuda.N]` produces both `cuda-N` and `--device N`.
    #[test]
    fn child_argv_carries_coordinator_miner_id_log_level_and_device() {
        let cfg = crate::config::parse_config(
            "\n[miner]\npublic_host = \"h\"\npublic_port = 1\n\n[cuda.1]\n",
        )
        .expect("config parses");
        let from_config = cfg.launch.first().expect("one launch entry");
        for (label, entry, level, miner_id, device, log_level) in [
            (
                "from config [cuda.1]",
                from_config,
                LogLevel::Info,
                "cuda-1",
                "1",
                "info",
            ),
            (
                "constructed cuda-0",
                &cuda_entry("cuda-0", Some(0)),
                LogLevel::Debug,
                "cuda-0",
                "0",
                "debug",
            ),
        ] {
            let args = child_args(entry, "unix:///s.sock", level);
            assert_eq!(
                value_after(&args, "--quip-coordinator"),
                Some("unix:///s.sock"),
                "{label}"
            );
            assert_eq!(value_after(&args, "--miner-id"), Some(miner_id), "{label}");
            assert_eq!(value_after(&args, "--device"), Some(device), "{label}");
            assert_eq!(
                value_after(&args, "--log-level"),
                Some(log_level),
                "{label}"
            );
        }
    }
}
