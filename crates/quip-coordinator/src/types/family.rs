//! A problem family the chain bound to a challenge, after admission.

use crate::types::topology::Topology;
use ising_generators::descriptor::{admit, params_digest, AdvertisedLimits, Contract};
use ising_generators::family::{admit_topology, BaseTopology};
use ising_generators::Code;
use quip_proto::v1::FamilyDescriptor;

/// Family revisions this build regenerates: every contract the generator crate
/// ships. A live challenge still takes the legacy draw until the chain binds a
/// family to it.
pub const FAMILY_CONTRACTS: &[Contract] = ising_generators::registry::CONTRACTS;

/// One problem family admitted for a challenge. The fields are private, so a
/// lease cannot change a descriptor that passed admission.
#[derive(Debug, Clone, PartialEq)]
pub struct ChallengeFamily {
    descriptor: FamilyDescriptor,
    descriptor_digest: [u8; 32],
    topology_order_digest: [u8; 32],
}

impl ChallengeFamily {
    /// Commit to `params` and check the descriptor against `contracts` and the
    /// ordered `topology`.
    ///
    /// # Errors
    /// Returns the family code of the first failed admission check.
    pub fn admit(
        contracts: &[Contract],
        family_id: &str,
        revision: u32,
        params: Vec<u8>,
        topology: &Topology,
    ) -> Result<Self, Code> {
        let digest = params_digest(&params);
        let base = BaseTopology::new(topology.nodes.clone(), &topology.edge_pairs())
            .map_err(|_| Code::TopologyIncompatible)?;
        let admitted = admit(contracts, family_id.as_bytes(), revision, &params, &digest)?;
        let _ = admit_topology(
            &admitted,
            &base,
            base.order_digest(),
            &AdvertisedLimits::default(),
        )?;
        let topology_order_digest = *base.order_digest();
        let descriptor_digest = admitted.descriptor().digest();
        Ok(Self {
            descriptor_digest,
            descriptor: FamilyDescriptor {
                family_id: family_id.to_owned(),
                revision,
                params,
                params_digest: digest.to_vec(),
            },
            topology_order_digest,
        })
    }

    /// The admitted descriptor.
    #[must_use]
    pub fn descriptor(&self) -> &FamilyDescriptor {
        &self.descriptor
    }

    /// Digest of the admitted descriptor: family id, revision, and parameter
    /// digest. The chain mixes it into a family topology hash.
    #[must_use]
    pub const fn descriptor_digest(&self) -> &[u8; 32] {
        &self.descriptor_digest
    }

    /// Digest of the node and edge order the descriptor was admitted on.
    #[must_use]
    pub const fn topology_order_digest(&self) -> &[u8; 32] {
        &self.topology_order_digest
    }

    /// Bytes that identify this family binding in a challenge key.
    #[must_use]
    pub fn commitment(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        let _ = h.update(self.descriptor.family_id.as_bytes());
        let _ = h.update(&[0]);
        let _ = h.update(&self.descriptor.revision.to_le_bytes());
        let _ = h.update(&self.descriptor.params_digest);
        let _ = h.update(&self.topology_order_digest);
        *h.finalize().as_bytes()
    }
}
