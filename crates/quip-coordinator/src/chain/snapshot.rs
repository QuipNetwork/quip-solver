//! The round root: which block hash the next `PoW` nonce derives from.

/// The block hash the next `PoW` nonce derives from.
///
/// The pallet writes `LastProofBlockHash` in `on_initialize` of the block
/// after a win, because a block cannot store its own hash. At the winning
/// block itself `LastProofBlock` already equals the head number, and the head
/// hash is the value the pallet stores next block. Returning it here starts
/// the next round one block early. A proof derived from it is valid from the
/// next block on, which is the first block that can include it.
#[must_use]
pub fn round_root(
    head_hash: [u8; 32],
    block_number: u64,
    last_proof_block: u64,
    stored_hash: [u8; 32],
) -> [u8; 32] {
    if last_proof_block == block_number {
        head_hash
    } else {
        stored_hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantum_pow_storage_keys_have_expected_shape() {
        use crate::chain::extrinsic::{
            difficulties_storage_key, last_proof_block_hash_storage_key,
            last_proof_block_storage_key, topology_curve_c_storage_key,
        };
        let hash = [9u8; 32];
        let diff = difficulties_storage_key(&hash);
        // twox128(pallet)=16 + twox128(item)=16 + blake2_128(hash)=16 + hash=32
        assert_eq!(diff.len(), 80);
        assert!(diff.ends_with(&hash));
        let curve = topology_curve_c_storage_key(&hash);
        assert_eq!(curve.len(), 80);
        // Same pallet prefix, different storage item.
        #[expect(
            clippy::indexing_slicing,
            reason = "storage keys are fixed 80/32 bytes; prefixes checked by len asserts"
        )]
        {
            assert_eq!(diff[..16], curve[..16]);
            assert_ne!(diff[16..32], curve[16..32]);
            // Plain StorageValue: pallet + item, no key suffix.
            assert_eq!(last_proof_block_storage_key().len(), 32);
            assert_eq!(last_proof_block_storage_key()[..16], diff[..16]);
            assert_eq!(last_proof_block_hash_storage_key().len(), 32);
            assert_eq!(last_proof_block_hash_storage_key()[..16], diff[..16]);
            assert_ne!(
                last_proof_block_hash_storage_key()[16..],
                last_proof_block_storage_key()[16..]
            );
        }
    }

    /// The pallet writes `LastProofBlockHash` one block after the win. At the
    /// winning block, `LastProofBlock` already names the head, and the head
    /// hash is the value the pallet will store next block.
    #[test]
    fn root_is_the_head_hash_only_at_the_winning_block() {
        let head = [1u8; 32];
        let stored = [2u8; 32];
        for (label, head_number, last_proof_block, expected) in [
            ("at the winning block", 100, 100, head),
            ("after the winning block", 101, 100, stored),
            ("before any win", 5, 0, stored),
        ] {
            assert_eq!(
                round_root(head, head_number, last_proof_block, stored),
                expected,
                "{label}"
            );
        }
    }
}
