# quip-solver

> **Experimental software.** Use at your own risk. No production warranties.

The v0.3 mining stack for the
[quip-validator](https://gitlab.com/quip.network/quip-validator) Substrate
chain. A coordinator follows the chain head and stages Ising problems for the
miner subprocesses. When a solution clears the difficulty gate, the coordinator
submits a `QuantumPow.submit_proof` extrinsic. The miners run simulated
annealing and Gibbs sampling on CPU, CUDA, and Metal, and quantum annealing on
D-Wave hardware.

This is a Rust workspace. The coordinator and the chain integration are Rust.
The solver contract (the wire protocol, the consensus primitives, and the
shared solver harness) lives in
[quip-solver-core](https://gitlab.com/quip.network/quip-solver-core) and is
consumed from crates.io. The miner binaries ship from their own repositories.

## Repository layout

- `crates/` — the Cargo workspace: `quip-coordinator` (the binary),
  `quip-solver-exec` (a miner that shells out to an external solver), and
  `quip-mock-miner` (the miner test double).
- `docker/` — image builds and the example coordinator config.

The miner binaries live in `quip.network/quip-solver-{cpu,cuda,metal,dwave}`.
The `quip-proto`, `quip-protocol`, `quip-solver-core`, and `ising-generators`
dependencies come from crates.io at v0.0.11. Their source of truth is
`quip.network/quip-solver-core`.

## CUDA card support

The CUDA miner supports NVIDIA GPUs with compute capability 7.0 through 12.1
(Volta through consumer Blackwell). The kernels set the floor, because they use
`__nanosleep`, an sm_70+ instruction. NVRTC 12.9, which the
`quip-solver-cuda` image ships, sets the ceiling. The host needs an NVIDIA
driver from a CUDA 12.9-supported branch (r535 or newer) and the NVIDIA
container runtime (`--gpus all`). The miner compiles its kernels per card for
the detected capability. A capability outside the range is clamped to the
nearest supported target. A capability the toolkit lacks (for example 8.8)
selects the next lower architecture, and the driver's forward-compatible PTX
JIT loads it.

## Documentation

- `AGENTS.md` — build, test, and run commands, and repository conventions.
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — how the coordinator, solver
  contract, miners, and chain fit together.
- [`docs/COORDINATOR.md`](docs/COORDINATOR.md) — how the coordinator works
  (chain access, feeder, routing, supervision, session protocol).
- [`docs/NEWMINER.md`](docs/NEWMINER.md) — how to add a new miner and wire it
  into the coordinator.
- [`docs/CHAINSTORAGE.md`](docs/CHAINSTORAGE.md) — how the main structures are
  written to chain.
- `docs/VERSIONING.md` — the release-tag standard.

The solver contract (the wire protocol, the `Sampler` trait, handshake,
credits, cancel, and exit codes) is specified in the
[quip-solver-core](https://gitlab.com/quip.network/quip-solver-core)
repository (`SPEC.md`).

## Build and run

The workspace builds from the repository root (toolchain pinned in
`rust-toolchain.toml`):

```bash
cargo build --workspace
```

Run the coordinator against a config that lists the miners and the validators:

```bash
quip-coordinator --config ./docker/config.toml
```

See `AGENTS.md` for the full command set and
[`docs/COORDINATOR.md`](docs/COORDINATOR.md) for the run model.

## Testing

The workspace tests run offline by default:

```bash
cargo test --workspace
```

### Live devnet integration tests

`crates/quip-coordinator/tests/devnet_submit.rs` drives the real chain client
against a running node. The tests carry `#[ignore]` and read the
`QUIP_DEVNET` environment variable, so the default `cargo test` run stays
offline. Three milestones:

- **M1, snapshot read:** decode a live `MiningSnapshot` (runtime API +
  SCALE).
- **M2, proof submit:** register `//Alice`, solve the PoW Ising locally, and
  submit a `QuantumPow.submit_proof` extrinsic. Assert on-chain acceptance.
- **M3, mempool decode:** propose a `QuantumComputeMempool` job order, then
  confirm `fetch_mempool_orders` discovers its `order_id` from the head-block
  `JobProposed` event, storage-reads `JobOrders(order_id)`, and builds a Job.

Start a local devnet from the chain repo
([quip-validator](https://gitlab.com/quip.network/quip-validator)) with
`make local-3-node` or `docker compose up`. Both publish node RPC on
`ws://127.0.0.1:9944`. Then run:

```bash
QUIP_DEVNET=ws://127.0.0.1:9944 \
  cargo test -p quip-coordinator --test devnet_submit -- --ignored --nocapture
```

The URL accepts `ws://`, `wss://`, `http://`, and `https://`. The chain client
uses `jsonrpsee` for every scheme.

M3 self-seeds. It proposes its own order against the canonical `plain-ising-v1`
spec the runtime seeds at genesis, so it runs on a fresh devnet with an empty
mempool.

## What changed from v0.2

In v0.2 this repository was a single Python mining stack that attached to the
chain. v0.3 splits it apart:

- The coordinator and chain integration are Rust (`crates/`), talking to the
  node over Substrate JSON-RPC.
- Each miner is a standalone binary in its own repository, supervised by the
  coordinator over a local socket. The Python miner stack is gone.
- The solver contract (proto, consensus primitives, harness, and the Python
  wheel) moved to `quip.network/quip-solver-core`. This repository holds no
  Python.

## License

AGPL-3.0-or-later. See LICENSE.
