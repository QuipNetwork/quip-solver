//! SCALE mirrors of the `Foundation` collective (`pallet_collective`):
//! call indices, vote storage, and dispatch weight.

use parity_scale_codec::{Decode, Encode};

/// `Foundation` (`pallet_collective` instance 1) pallet index on runtime 120.
pub const FOUNDATION_PALLET_INDEX: u8 = 20;

/// `pallet_collective::Call::propose` call index. A threshold below 2
/// executes the proposal at once with the proposer's member origin.
pub const PROPOSE_CALL_INDEX: u8 = 2;

/// `pallet_collective::Call::vote` call index. A vote never executes the
/// proposal; `close` does.
pub const VOTE_CALL_INDEX: u8 = 3;

/// `pallet_collective::Call::close` call index. Executes the proposal once
/// its ayes reach the threshold.
pub const CLOSE_CALL_INDEX: u8 = 6;

/// `pallet_collective::Votes` as stored under `Foundation.Voting[hash]`.
/// Field order is the pallet's; accounts are `AccountId32`.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct FoundationVotesScale {
    /// The proposal index `vote` and `close` name.
    pub index: u32,
    /// Ayes the motion needs.
    pub threshold: u32,
    /// Members that voted aye.
    pub ayes: Vec<[u8; 32]>,
    /// Members that voted nay.
    pub nays: Vec<[u8; 32]>,
    /// Last block of the voting period.
    pub end: u32,
}

/// `sp_weights::Weight`: both fields are compact on the wire.
#[derive(Clone, Copy, Debug, Encode, Decode, PartialEq, Eq)]
pub struct WeightScale {
    /// Reference execution time.
    #[codec(compact)]
    pub ref_time: u64,
    /// Proof size in bytes.
    #[codec(compact)]
    pub proof_size: u64,
}
