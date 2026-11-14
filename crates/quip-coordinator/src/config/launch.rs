//! The per-miner launch plan: one supervised process per backend section.

use super::ConfigError;
use crate::types::miner::{MinerKind, MinerSpecScale};
use quip_proto::v1::Configure;
use std::collections::BTreeMap;

/// One supervised miner: identity, binary path, and handshake `Configure`.
pub struct LaunchEntry {
    /// Miner id used on the session wire (`cpu-0`, `cuda-1`, …).
    pub miner_id: String,
    /// Executable name or path for the miner subprocess.
    pub binary: String,
    /// Handshake configure payload (queue depths, heartbeat, backend TOML).
    pub configure: Configure,
    /// Backend section that produced this entry (`cpu`, `cuda`, `metal`, `dwave`).
    pub backend: String,
    /// CUDA device ordinal from the `[cuda.N]` section key, forwarded to the
    /// child as `--device N`. `None` for backends whose binary has no
    /// `--device` flag (`cpu`, `dwave`) and for `[metal]`, which the
    /// coordinator only ever emits as device 0. This cannot travel in
    /// `Configure.backend_toml`: the miner opens its device inside the `open`
    /// closure, before the session receives `Configure`.
    pub device: Option<usize>,
}

impl LaunchEntry {
    /// Pallet kind for this backend section.
    #[must_use]
    pub fn miner_kind(&self) -> MinerKind {
        MinerKind::from_backend(&self.backend)
    }

    /// Descriptor miner spec for this launch entry.
    #[must_use]
    pub fn miner_spec(&self) -> MinerSpecScale {
        MinerSpecScale {
            kind: self.miner_kind(),
            label: Some(self.miner_id.as_bytes().to_vec()),
            backend: Some(self.backend.as_bytes().to_vec()),
            // The ordinal as text, matching `label` and `backend`. It is the
            // only remote signal of which GPU a `cuda-N` miner actually holds:
            // the label alone said `cuda-1` even while every miner shared
            // GPU 0. `None` for backends that name no device.
            device_id: self.device.map(|n| n.to_string().into_bytes()),
        }
    }
}

/// Rank used to pick one kind for `participate` from a mixed fleet.
pub(super) fn kind_rank(kind: MinerKind) -> u8 {
    match kind {
        MinerKind::Cpu => 0,
        MinerKind::Gpu => 1,
        MinerKind::Metal => 2,
        MinerKind::Asic => 3,
        MinerKind::QpuPasqal => 4,
        MinerKind::QpuIonq => 5,
        MinerKind::QpuIbm => 6,
        MinerKind::QpuDwave => 7,
    }
}

/// Kind a mixed fleet declares on `participate`: the highest-capability miner.
///
/// The pallet takes one kind. A node that runs CPU and Metal must not look
/// like a CPU-only node. Order: QPU, then ASIC, then Metal, then GPU, then CPU.
#[must_use]
pub fn participate_kind(launch: &[LaunchEntry]) -> MinerKind {
    launch
        .iter()
        .map(LaunchEntry::miner_kind)
        .max_by_key(|k| kind_rank(*k))
        .unwrap_or(MinerKind::Cpu)
}

pub(super) fn make_configure(table: &toml::Table) -> Configure {
    let u32_of = |k: &str, d: u32| {
        table
            .get(k)
            .and_then(toml::Value::as_integer)
            .map_or(d, |i| {
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "config u32 knobs are small non-negative integers"
                )]
                {
                    i as u32
                }
            })
    };
    // Strip coordinator-owned keys; the rest passes through verbatim.
    let mut passthrough = table.clone();
    for k in [
        "binary",
        "queue_depth",
        "idle_timeout_s",
        "heartbeat_s",
        "reconnect_window_s",
    ] {
        let _ = passthrough.remove(k);
    }
    Configure {
        // 0 is "not set" on the wire: the miner picks its own depth. The SDKs
        // default to 3; the D-Wave miner uses its measured QPU pipeline depth.
        queue_depth: u32_of("queue_depth", 0),
        idle_timeout_s: u32_of("idle_timeout_s", 300),
        heartbeat_s: u32_of("heartbeat_s", 15),
        reconnect_window_s: u32_of("reconnect_window_s", 60),
        backend_toml: toml::to_string(&passthrough).unwrap_or_default(),
    }
}

pub(super) fn default_binary(backend: &str) -> String {
    match backend {
        "dwave" => "quip-dwave-qa".into(),
        other => format!("quip-{other}-sa"),
    }
}

pub(super) fn entry(
    miner_id: &str,
    backend: &str,
    device: Option<usize>,
    table: &toml::Table,
) -> LaunchEntry {
    let binary = table
        .get("binary")
        .and_then(|v| v.as_str())
        .map_or_else(|| default_binary(backend), String::from);
    LaunchEntry {
        miner_id: miner_id.into(),
        binary,
        configure: make_configure(table),
        backend: backend.into(),
        device,
    }
}

/// Reject `[backend.N]` under a backend that launches exactly one miner.
///
/// Only `[cuda]` is indexed. Written anywhere else, the sub-table is not a
/// section: `entry()` reads the *outer* table, so the `binary` key nested
/// under `N` is ignored and the sub-table is serialized into the miner's
/// `backend_toml` as a stray `[N]` block. The entry still launches, on device
/// 0, under the unindexed id — the same silent-wrong-GPU shape as the CUDA
/// bug, which is why this is an error rather than a warning.
pub(super) fn reject_indexed(
    backend: &'static str,
    table: &toml::Table,
) -> Result<(), ConfigError> {
    for (key, value) in table {
        if value.is_table() {
            return Err(ConfigError::IndexedSingleDeviceSection {
                backend,
                key: key.clone(),
            });
        }
    }
    Ok(())
}

/// Build the launch plan from the backend sections.
///
/// Two invariants hold for CUDA. The miner id is built from the *parsed*
/// ordinal, so the id and the forwarded `--device` can never disagree
/// (`[cuda.01]` becomes `cuda-1` on device 1). And entries come out in numeric
/// order, so device 2 precedes device 10 — the old lexicographic key sort put
/// them the other way round.
pub(super) fn parse_launch(root: &toml::Table) -> Result<Vec<LaunchEntry>, ConfigError> {
    let mut launch = Vec::new();
    if let Some(t) = root.get("cpu").and_then(|v| v.as_table()) {
        reject_indexed("cpu", t)?;
        launch.push(entry("cpu-0", "cpu", None, t));
    }
    if let Some(cuda) = root.get("cuda").and_then(|v| v.as_table()) {
        // The section key is an executable device ordinal now, not a label, so
        // a key that is not a number must not fall through to device 0.
        let mut by_device: BTreeMap<usize, &toml::Table> = BTreeMap::new();
        for (k, v) in cuda {
            let Some(t) = v.as_table() else { continue }; // non-table keys ignored, as today
            let n = k
                .parse::<usize>()
                .map_err(|_| ConfigError::CudaSectionNotIndexed { key: k.clone() })?;
            if by_device.insert(n, t).is_some() {
                return Err(ConfigError::DuplicateCudaDevice { device: n });
            }
        }
        for (n, t) in by_device {
            launch.push(entry(&format!("cuda-{n}"), "cuda", Some(n), t));
        }
    }
    if let Some(t) = root.get("metal").and_then(|v| v.as_table()) {
        reject_indexed("metal", t)?;
        launch.push(entry("metal-0", "metal", None, t));
    }
    // Prefer [dwave], fall back to [qpu].
    if let Some(t) = root
        .get("dwave")
        .or_else(|| root.get("qpu"))
        .and_then(|v| v.as_table())
    {
        launch.push(entry("qpu-0", "dwave", None, t));
    }
    Ok(launch)
}
