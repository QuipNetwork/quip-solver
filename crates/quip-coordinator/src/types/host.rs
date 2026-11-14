//! The host survey a node files in its descriptor, in the pallet's shape.

use parity_scale_codec::{Decode, Encode};

// The V2 survey bounds below mirror the validator runtime at the rev already
// pinned in Cargo.toml (`runtime/src/configs/mod.rs`). Nothing links them at
// compile time, so a runtime upgrade that moves a bound is a coordinator
// change too. Over-length is a SCALE decode failure rather than a pallet
// error, so it never reaches `DESCRIPTOR_REJECT`; see `crate::survey`.

/// Pallet `MaxOsStringBytes`. One bound for `os.system`, `os.release`, and
/// `os.machine` alike.
pub const MAX_OS_STRING_BYTES: usize = 64;
/// Pallet `MaxCpuBrandBytes`.
pub const MAX_CPU_BRAND_BYTES: usize = 96;
/// Pallet `MaxArchBytes`.
pub const MAX_ARCH_BYTES: usize = 16;
/// Pallet `MaxGpuVendorBytes`.
pub const MAX_GPU_VENDOR_BYTES: usize = 16;
/// Pallet `MaxGpuNameBytes`.
pub const MAX_GPU_NAME_BYTES: usize = 96;
/// Pallet `MaxGpus`.
pub const MAX_GPUS: usize = 16;
/// Pallet `MaxRuntimeVersionBytes`. One bound for `python` and `quip_version`
/// alike.
pub const MAX_RUNTIME_VERSION_BYTES: usize = 48;
/// Pallet `MaxDockerImageBytes`.
pub const MAX_DOCKER_IMAGE_BYTES: usize = 256;

/// Operating-system identity inside [`SystemInfoScale`].
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct OsInfoScale {
    /// OS family.
    pub system: Vec<u8>,
    /// Kernel or build string.
    pub release: Vec<u8>,
    /// Machine architecture.
    pub machine: Vec<u8>,
}

/// CPU identity inside [`SystemInfoScale`].
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct CpuInfoScale {
    /// Logical core count.
    pub logical_cores: Option<u32>,
    /// Physical core count.
    pub physical_cores: Option<u32>,
    /// Brand string.
    pub brand: Vec<u8>,
    /// Instruction-set architecture.
    pub arch: Vec<u8>,
}

/// One GPU inside [`SystemInfoScale`].
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct GpuInfoScale {
    /// Enumeration index.
    pub index: u8,
    /// Vendor string.
    pub vendor: Vec<u8>,
    /// Product name.
    pub name: Vec<u8>,
    /// Device memory in MiB.
    pub memory_mb: Option<u32>,
    /// Utilization 0..=100.
    pub utilization_pct: Option<u8>,
}

/// Optional hardware survey on a V2 descriptor.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct SystemInfoScale {
    /// OS identity.
    pub os: OsInfoScale,
    /// CPU identity.
    pub cpu: CpuInfoScale,
    /// Host memory in MiB.
    pub memory_mb: Option<u32>,
    /// Attached GPUs.
    pub gpus: Vec<GpuInfoScale>,
}

/// Optional node-software identity on a V2 descriptor.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct RuntimeInfoScale {
    /// Python interpreter version.
    pub python: Vec<u8>,
    /// Node software version.
    pub quip_version: Vec<u8>,
    /// Protocol version number.
    pub protocol_version: u32,
    /// Whether the process is inside a container.
    pub in_docker: bool,
    /// Container image, when running in a container.
    pub docker_image: Option<Vec<u8>>,
}

/// Both descriptor blocks the survey produces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSurvey {
    /// Hardware block.
    pub system: SystemInfoScale,
    /// Node-software block.
    pub runtime: RuntimeInfoScale,
}
