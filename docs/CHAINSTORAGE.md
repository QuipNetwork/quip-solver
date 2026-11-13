# Chain storage

This document shows how the chain writes the main mining structures to storage,
and how the coordinator reads them. The source is the `quip-validator` `main`
branch at runtime spec version 120 and `QuantumPow` storage version 6. A deployed
chain can run an older runtime.

Three pallets hold the state:

| Pallet | State |
|---|---|
| `QuantumPow` | Topologies, difficulty, miners, proofs, and qblocks |
| `MinerRegistry` | Node descriptors and participation records |
| `QuantumComputeMempool` | Job specifications, orders, solutions, and results |

## Key and value encoding

Every value is SCALE-encoded. The runtime uses these types:

| Type | Runtime type |
|---|---|
| `AccountId` | `AccountId32` |
| `BlockNumber` | `u32` |
| `Balance` | `u128` |
| `Hash` | `H256` |

The pallet test runtime (`mock.rs`) builds its block type from
`frame_system::mocking::MockBlock`, whose block number type differs from the
production `u32`. A client of the production chain must decode a block number
as four little-endian bytes.

Storage keys have this layout:

| Item kind | Key bytes |
|---|---|
| `StorageValue` | `twox128(pallet)`, then `twox128(item)` |
| `StorageMap` | The 32-byte item prefix, then the hashed key |
| `StorageDoubleMap` | The 32-byte item prefix, then both hashed keys |

A `Blake2_128Concat` key part is the 16-byte hash followed by the SCALE-encoded
key. For example, a `QBlocks` key has 52 bytes: 32 for the prefix, 16 for the
hash, and 4 for the block number. All maps in this document use
`Blake2_128Concat`, except `OrdersExpiringAt`, which uses `Twox64Concat`.

Substrate's `state_getKeysPaged` treats `startKey` as exclusive. A page that
starts at a `StorageValue` key does not return that key. Read a `StorageValue`
with `state_getStorage`.

These are the pallet prefixes:

| Pallet | `twox128` prefix |
|---|---|
| `QuantumPow` | `0x9b2c4dbe49d7a1aed7ce99e4b8c072e8` |
| `QuantumComputeMempool` | `0xcbfd51865888632eb84e8a6a17f30b4a` |

## Topologies and difficulty

A topology is the graph and value sets that every proof for that topology uses.
The topology hash is the key for these items.

| Call | Items written |
|---|---|
| `register_topology` | `RegisteredTopologies`. If `DefaultTopology` is unset, also `DefaultTopology` and `MineableTopologies`. |
| `set_default_topology` | `DefaultTopology` |
| `set_difficulty` | `Difficulties` |
| `set_topology_curve` | `TopologyCurveC` |
| `add_mineable_topology`, `remove_mineable_topology` | `MineableTopologies` |
| `set_block_reward` | `BlockRewardOverride` |
| `set_target_proof_blocks` | `TargetProofBlocksOverride` |

`MineableTopologies` is the whitelist of topologies that accept proofs.
`submit_proof` refuses a registered topology that has no entry in this map.
`remove_mineable_topology` refuses the current default topology.

`TopologyMeta` holds `nodes`, `edges`, `allowed_h_values`, `allowed_j_values`,
`allowed_spin_values`, and `registered_at`. The chain regenerates each problem
from these values, so a proof carries the topology hash and no graph.

`DifficultyConfig` holds `min_solutions`, `max_energy_milli`, and
`min_diversity_milli`. Each win rewrites the `Difficulties` entry for the winning
topology only.

## Miners

`register_miner` writes a `MinerInfo` entry to `Miners`, keyed by account, and
`deregister_miner` removes it. `MinerInfo` holds `registered_at`, `deposit`,
`proofs_submitted`, `proofs_won`, and `rewards_earned`. `Miners` holds counters
and no proof data.

## Proofs and qblocks

A proof moves through storage in four steps.

| Step | Time | Writes |
|---|---|---|
| 1. Start of a block | `on_initialize` | Resets `BlockProofCount` to 0. |
| 2. Accepted proof | `submit_proof` | `Miners`, `BlockBestProof`, and `BlockProofCount` |
| 3. End of a block | `on_finalize` | `Miners`, `Difficulties`, `LastProofBlock`, `QBlocks`, `QBlockCount`, and both qblock indexes |
| 4. Start of the next block | `on_initialize` | Stores the hash of the winning block in `LastProofBlockHash`. |

### Submission

The coordinator builds a `QuantumProof` in `proof_encode.rs` with these fields:

| Field | Content |
|---|---|
| `topology_hash` | The registered topology |
| `nonce` | `derive_nonce(last_proof_block_hash, identity, salt)`, where `identity` is `blake2_256(SCALE(account))`. The pallet computes the same value in `account_to_bytes`. The coordinator supplies it from `miner_identity_bytes` (`chain/extrinsic.rs`). |
| `salt` | 32 bytes chosen by the miner |
| `solutions` | Spin vectors, bit-packed under `allowed_spin_values`. On a family topology, each row is a compact witness in the codec of the family revision. |
| `device_access_time_us` | Self-reported compute time. Consensus does not read it. |

The proof carries no energies. The chain recalculates them from the solutions.

`submit_proof` validates the proof, then increments `proofs_submitted`. It keeps
one `ProofRecord` per block in `BlockBestProof`, and a new record replaces the
stored record only if the new energy is lower. The chain does not store the
solutions of a proof.

### Finalization

`on_finalize` takes the `BlockBestProof` value, which clears that item. A read of
`BlockBestProof` at a finished block returns nothing. If the item is empty, the
hook writes nothing.

If a record exists, the hook pays the reward and writes these items:

| Item | Change |
|---|---|
| `Miners[miner]` | Increments `proofs_won` and adds the reward to `rewards_earned`, if the entry still exists |
| `Difficulties[topology]` | Stores the adjusted difficulty |
| `LastProofBlock` | Stores the current block number |
| `QBlockCount` | Increments the count that supplies the qblock id |
| `QBlocks[block]` | Stores the permanent record of the win |
| `QBlockBlockById[id]` | Maps the qblock id to the block number |
| `QBlockIdByBlock[block]` | Maps the block number to the qblock id |

`QBlock` holds `miner`, `salt`, `energy_milli`, `reward`, `submitted_at`,
`difficulty`, `last_proof_block_hash`, `topology_hash`, and
`device_access_time_us`. The `difficulty` field is the threshold that the proof
had to clear, before the adjustment.

### The nonce seed

`LastProofBlockHash` is the seed for the nonce of the current round. A block
cannot read its own hash, so the pallet stores the hash one block later, from
`parent_hash`. At the winning block, `LastProofBlock` already names the head, but
`LastProofBlockHash` still holds the previous seed.

The pallet keeps this hash in its own item because `frame_system::block_hash`
returns zero for blocks older than `BlockHashCount`. The first block of a chain
stores the genesis hash.

### Win confirmation

The coordinator confirms a win in `real.rs`. It reads `QBlocks[block].miner` at
the block that included its extrinsic. `miner` is the first field of `QBlock`, so
the coordinator decodes only the first 32 bytes. Fields added to the end of the
struct do not affect this read. An empty value means that no proof won that block.

`Miners` is the only `QuantumPow` map keyed by account, and it does not name the
winner of a block. Use `QBlocks` for this purpose.

## Participation

`MinerRegistry::set_descriptor` writes `NodeDescriptors`, keyed by account.
`participate` requires a descriptor and the id of the candidate qblock. The call
writes these items:

| Item | Key | Change |
|---|---|---|
| `LatestParticipation` | Account | Stores the latest record |
| `ParticipantsByQBlock` | Qblock id, then account | Stores the record |
| `ParticipantCountByQBlock` | Qblock id | Increments the count |

An account can participate once per qblock id.

## Job orders

`register_job_spec` writes `JobSpecs`. An order then moves through storage in
four steps.

| Step | Call | Writes |
|---|---|---|
| 1. Propose | `propose_job` or `propose_job_with_payee_requirement` | `NextOrderId`, `JobOrders`, `OpenOrders`, `OrdersExpiringAt`, `ProposerOrders`, and `JobSpecs`. The payee variant also writes `OrderPayeeRequirements`. |
| 2. Solve | `submit_solution` or `submit_solution_with_payee` | `OrderSolutions`, `Solvers`, `JobOrders`, and one ranking item. `OrderPayees` changes only when the payee changes. The first solution can move the `OrdersExpiringAt` entry to an earlier bucket. |
| 3. Expire | `on_initialize` | `JobOrders`, `OpenOrders`, `OrdersExpiringAt`, `NextExpiryBucket`, and `OrderResults` |
| 4. Claim | `claim_reward` | `JobOrders`, `OpenOrders`, `JobSpecs`, `Solvers`, and `ProposerOrders` |

A proposer can call `reclaim_order` on an order that has no accepted solution.
It writes `JobOrders` and removes the order from `OpenOrders`.

Unlike a proof, a job solution stays in storage. `OrderSolutions` holds one
`JobSolution` per order and solver, with the spin vectors as `BoundedVec<i8>`.

The ranking item depends on the reward resolution of the order. A `SingleBest`
order uses `OrderFrontRunner`, and other resolutions use `OrderTopSolvers`.

Expiry removes the order from `OpenOrders`. The chain writes `OrderResults` only
for an expired order that has a solution and `CallbackWithPoll` delivery.
`purge_result` removes that entry after `settled_at + ResultTtlBlocks`.
`OrdersExpiringAt` and `NextExpiryBucket` schedule order expiry.

## Storage items

### QuantumPow

| Item | Kind | Key | Value |
|---|---|---|---|
| `BlockRewardOverride` | Value | None | `Balance` |
| `TargetProofBlocksOverride` | Value | None | `u32` |
| `RegisteredTopologies` | Map | Topology hash | `TopologyMeta` |
| `DefaultTopology` | Value | None | `H256` |
| `Difficulties` | Map | Topology hash | `DifficultyConfig` |
| `TopologyCurveC` | Map | Topology hash | `CurveC` |
| `MineableTopologies` | Map | Topology hash | `()` |
| `Miners` | Map | Account | `MinerInfo` |
| `BlockBestProof` | Value | None | `ProofRecord` |
| `LastProofBlock` | Value | None | `BlockNumber` |
| `LastProofBlockHash` | Value | None | `H256` |
| `BlockProofCount` | Value | None | `u32` |
| `QBlocks` | Map | Block number | `QBlock` |
| `QBlockCount` | Value | None | `u64` |
| `QBlockBlockById` | Map | Qblock id | `BlockNumber` |
| `QBlockIdByBlock` | Map | Block number | `u64` |

### MinerRegistry

| Item | Kind | Key | Value |
|---|---|---|---|
| `NodeDescriptors` | Map | Account | `NodeDescriptor` |
| `LatestParticipation` | Map | Account | `ParticipationRecord` |
| `ParticipantsByQBlock` | Double map | Qblock id, then account | `ParticipationRecord` |
| `ParticipantCountByQBlock` | Map | Qblock id | `u32` |

### QuantumComputeMempool

| Item | Kind | Key | Value |
|---|---|---|---|
| `JobSpecs` | Map | `Hash` | `JobSpec` |
| `JobOrders` | Map | Order id | `JobOrder` |
| `NextOrderId` | Value | None | `u64` |
| `OpenOrders` | Map | Order id | `()` |
| `OrdersExpiringAt` | Double map | Block number, then order id | `()` |
| `NextExpiryBucket` | Value | None | `BlockNumber` |
| `OrderSolutions` | Double map | Order id, then account | `JobSolution` |
| `OrderPayeeRequirements` | Map | Order id | `u64` |
| `OrderPayees` | Double map | Order id, then account | `WinnerPayee` |
| `OrderFrontRunner` | Map | Order id | `FrontRunner` |
| `OrderTopSolvers` | Map | Order id | Bounded list of ranked solvers |
| `Solvers` | Map | Account | `SolverInfo` |
| `ProposerOrders` | Map | Account | Bounded list of order ids |
| `OrderResults` | Map | Order id | `StoredResult` |
