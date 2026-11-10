//! Helpers shared by the integration tests that spawn `quip-mock-miner`.

use std::sync::OnceLock;

/// Build and resolve the sibling `quip-mock-miner` binary.
///
/// The mock miner is another package, so `CARGO_BIN_EXE_*` does not name it.
/// The build runs once per test binary, however many tests call this.
pub(crate) fn mock_miner() -> String {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "-p", "quip-mock-miner"])
            .status()
            .expect("build quip-mock-miner");
        assert!(status.success(), "failed to build quip-mock-miner");
        let name = if cfg!(windows) {
            "quip-mock-miner.exe"
        } else {
            "quip-mock-miner"
        };
        let mut p = std::env::current_exe().expect("test exe path");
        let _ = p.pop();
        let _ = p.pop();
        p.push(name);
        p.to_string_lossy().into_owned()
    })
    .clone()
}
