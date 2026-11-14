//! SCALE mirrors of `MinerRegistry`: the node descriptor, its pallet bounds,
//! and the `set_descriptor` and `participate` call encoders.

use parity_scale_codec::{Decode, Encode};

use crate::types::host::{RuntimeInfoScale, SystemInfoScale};
use crate::types::miner::{MinerKind, MinerSpecScale, NodeLogLevel};

/// Pallet index of `MinerRegistry` in the runtime construct.
pub const MINER_REGISTRY_PALLET_INDEX: u8 = 13;

/// Call index of `set_descriptor` within `MinerRegistry`.
pub const SET_DESCRIPTOR_CALL_INDEX: u8 = 0;

/// Call index of `participate` within `MinerRegistry`.
pub const PARTICIPATE_CALL_INDEX: u8 = 2;

/// Pallet `MaxNodeIdBytes`.
pub const MAX_NODE_ID_BYTES: usize = 64;

/// Pallet `MaxNodeNameBytes`.
pub const MAX_NODE_NAME_BYTES: usize = 64;

/// Pallet `MaxPublicHostBytes`.
pub const MAX_PUBLIC_HOST_BYTES: usize = 253;

/// Pallet `MaxRpcEndpointBytes`.
pub const MAX_RPC_ENDPOINT_BYTES: usize = 256;

/// Pallet `MaxRpcEndpoints`.
pub const MAX_RPC_ENDPOINTS: usize = 8;

/// Pallet `MaxMinerSpecs`.
pub const MAX_MINER_SPECS: usize = 16;

/// Pallet `MaxMinerLabelBytes`.
pub const MAX_MINER_LABEL_BYTES: usize = 64;

/// Pallet `MaxMinerBackendBytes`.
pub const MAX_MINER_BACKEND_BYTES: usize = 32;

/// Pallet `MaxMinerDeviceIdBytes`.
pub const MAX_MINER_DEVICE_ID_BYTES: usize = 128;

/// Caller-supplied V2 descriptor. This is the payload the coordinator files.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct NodeDescriptorV2Input {
    /// Node identity bytes.
    pub node_id: Vec<u8>,
    /// Display name.
    pub node_name: Vec<u8>,
    /// Optional public host.
    pub public_host: Option<Vec<u8>>,
    /// Optional public port.
    pub public_port: Option<u16>,
    /// Validator RPC endpoints.
    pub rpc_endpoints: Vec<Vec<u8>>,
    /// Whether the node mines without a manual start.
    pub auto_mine: bool,
    /// Advertised log verbosity.
    pub log_level: NodeLogLevel,
    /// Miners on this node.
    pub miners: Vec<MinerSpecScale>,
    /// Optional hardware survey, from [`crate::host::survey::collect`]. `None` when
    /// the probe missed its budget or left a pallet bound violated.
    pub system_info: Option<SystemInfoScale>,
    /// Optional node-software block, from [`crate::host::survey::collect`]. `None`
    /// when the probe missed its budget or left a pallet bound violated.
    pub runtime: Option<RuntimeInfoScale>,
}

/// SCALE-encode `MinerRegistry.participate(qblock_id, kind, budget_seconds)`.
#[must_use]
pub fn encode_participate_call(
    qblock_id: u64,
    kind: MinerKind,
    budget_seconds: Option<u32>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(MINER_REGISTRY_PALLET_INDEX);
    out.push(PARTICIPATE_CALL_INDEX);
    out.extend(qblock_id.encode());
    out.extend(kind.encode());
    out.extend(budget_seconds.encode());
    out
}

/// SCALE-encode `MinerRegistry.set_descriptor(V2(descriptor))`.
///
/// The version tag is `1` so a V1 signed extrinsic (tag `0`) still decodes.
#[must_use]
pub fn encode_set_descriptor_call(descriptor: &NodeDescriptorV2Input) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(MINER_REGISTRY_PALLET_INDEX);
    out.push(SET_DESCRIPTOR_CALL_INDEX);
    out.push(1);
    out.extend(descriptor.encode());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_descriptor_call_encodes_pallet_call_and_v2_args() {
        let v2 = NodeDescriptorV2Input {
            node_id: b"n".to_vec(),
            node_name: b"N".to_vec(),
            public_host: None,
            public_port: None,
            rpc_endpoints: Vec::new(),
            auto_mine: true,
            log_level: NodeLogLevel::Info,
            miners: vec![MinerSpecScale {
                kind: MinerKind::Cpu,
                label: None,
                backend: None,
                device_id: None,
            }],
            system_info: None,
            runtime: None,
        };
        let call = encode_set_descriptor_call(&v2);
        let mut expected = vec![MINER_REGISTRY_PALLET_INDEX, SET_DESCRIPTOR_CALL_INDEX, 1];
        expected.extend(v2.encode());
        assert_eq!(call, expected);
        // V2 variant index 1, then compact-len 1 + b'n', compact-len 1 + b'N',
        // three Nones / empty, auto_mine=true, log_level=Info, one Cpu miner
        // with three Nones, system_info=None, runtime=None.
        assert_eq!(
            call.get(2..),
            Some([1, 4, b'n', 4, b'N', 0, 0, 0, 1, 1, 4, 0, 0, 0, 0, 0, 0].as_slice()),
            "V2 variant index must be 1"
        );
    }

    #[test]
    fn participate_call_encodes_pallet_call_and_args() {
        let call = encode_participate_call(5, MinerKind::Cpu, None);
        assert_eq!(
            call,
            [
                MINER_REGISTRY_PALLET_INDEX,
                PARTICIPATE_CALL_INDEX,
                5,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ]
        );
        let with_budget = encode_participate_call(1, MinerKind::Metal, Some(30));
        assert_eq!(
            with_budget,
            [
                MINER_REGISTRY_PALLET_INDEX,
                PARTICIPATE_CALL_INDEX,
                1,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                7,
                1,
                30,
                0,
                0,
                0,
            ]
        );
    }
}
