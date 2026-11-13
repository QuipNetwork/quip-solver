# AGENTS.md — QuIP Protocol Project Instructions

Cross-tool instructions for AI coding assistants (Claude Code, Codex, Cursor, Gemini CLI).

QuIP v0.3 is a Rust workspace. The coordinator owns chain access and work routing.
It spawns and supervises the miner subprocesses that do the sampling.
The miner binaries ship from their own repos. The solver contract lives in
`quip.network/quip-solver-core` and is consumed from crates.io at v0.0.14.
It supplies the wire protocol, consensus primitives, and solver session loop.

For deeper detail, see the companion guides: `docs/COORDINATOR.md` covers how the coordinator works, and `docs/NEWMINER.md` covers adding a new miner. The solver contract itself is specified in the quip-solver-core repo (`SPEC.md`).

## Repository layout

- `crates/` — the Cargo workspace (root manifest: `Cargo.toml`):
  - `quip-coordinator` — the coordinator binary: chain access, routing, miner supervision.
  - `quip-solver-exec` — a miner that shells out to a generic external solver.
  - `quip-mock-miner` — the miner test double the coordinator tests spawn.
- `docker/` — image builds (`Dockerfile.quip-solver`, `Dockerfile.quip-solver-cuda`), `entrypoint.sh`, and `config.toml`.
- `docs/` — the guides: `ARCHITECTURE.md`, `COORDINATOR.md`, `NEWMINER.md`, `CHAINSTORAGE.md`, and `VERSIONING.md` (the release-tag standard).

The miner binaries live in their own repos: `quip.network/quip-solver-{cpu,cuda,metal,dwave}`. The `quip-proto` (wire types), `quip-protocol` (consensus math), `quip-solver-core` (`Sampler` trait + session loop), and `ising-generators` crates come from crates.io at v0.0.14. Their source repo is `quip.network/quip-solver-core`, which also publishes the `quip-solver-core` Python wheel the D-Wave miner uses.

## CUDA card support requirement

The CUDA miner supports NVIDIA GPUs with compute capability 7.0 through 12.1
(the kernels need sm_70 for `__nanosleep`, and NVRTC 12.9 supplies the 12.1
ceiling), with a host driver from a CUDA 12.9 branch (r535+). The cudarc pin in
`quip-solver-cuda` (`cuda-12090`) and the runtime image's `cuda-nvrtc-12-9`
package must move together. The CUDA major of the pin decides which
`libnvrtc.so.N` the miner dlopens, and `docker/Dockerfile.quip-solver-cuda`
asserts the load at build time. Widening or narrowing card support is a
reviewed change to `SUPPORTED_ARCHS` in `quip-solver-cuda`
(`tests/arch_coverage.rs` enforces it), not a side effect of a toolkit bump.

## Build and test

The workspace builds from the repo root. The toolchain is pinned to 1.97.1 (`rust-toolchain.toml`).

```bash
cargo build --workspace

# Test (mirrors CI; the CI test job excludes quip-coordinator, which has its own job).
cargo test --workspace --exclude quip-coordinator
cargo test -p quip-coordinator

# Lint and format.
cargo clippy --workspace --exclude quip-coordinator --all-targets -- -D warnings
cargo clippy -p quip-coordinator --all-targets -- -D warnings
cargo fmt --all --check
```

## Running the coordinator

`quip-coordinator` runs from a config file, or as a subcommand.

```bash
# Production: read the config, connect to the validators, spawn and supervise the
# configured miners, feed them work, submit proofs. Stops on SIGINT/SIGTERM.
quip-coordinator --config ./docker/config.toml

# Drive: spawn one miner and feed it synthetic problems without submissions.
# Use a preset, a JSON file, or a read-only chain topology lookup.
quip-coordinator drive --miner ./miners/quip-cpu-sa --source random \
  --topology-preset zephyr-z12t4-live --count 100 \
  --target-energy -10000 --report drive.jsonl

# Draw the Aglais graph with fields fixed at zero.
quip-coordinator drive --miner ./miners/quip-cpu-sa --source random \
  --topology-preset aglais --count 100 --seed 7

# Read the local validator's default topology, then draw synthetic jobs.
quip-coordinator drive --miner ./miners/quip-cpu-sa --source random \
  --topology-from-chain ws://127.0.0.1:9944 --count 100 --seed 7

# Drive a miner with generated salt leases.
quip-coordinator drive --miner ./miners/quip-cpu-sa --source lease \
  --topology-preset smoke --count 200 --lease-size 32 --seed 7

# Seed a fresh runtime-120 chain with the dimer1200 family topology.
# Give exactly one of --sudo-key or --mnemonic-file.
quip-coordinator seed-chain --sudo-key //Alice
```

Drive accepts one of `--topology <spec.json>`, `--topology-preset <name>`,
or `--topology-from-chain <url>`. The chain option reads `DefaultTopology`
and its metadata once at the same best-block hash. It requires no signer.
An unset default, invalid metadata, or failed RPC stops the run before
the miner starts. Random and lease sources default to `zephyr-z12t4-live`
when you supply no selector. List replay needs a topology for nonce entries.

The chain option reads graph and coefficient sets, without difficulty gates.
Its gates default to `min_solutions = 0`, `max_energy_milli = i64::MAX`,
and `min_diversity_milli = 0`. Use `--target-energy` and `--min-solutions`
to set benchmark gates. Work remains synthetic, and drive submits no proofs.
Use `--topology-preset aglais` for the fixed Aglais topology without an RPC.

`seed-chain` defaults to `--validator ws://quip-validator:9944` and
`--topology-preset zephyr-z12t4-live`. It always registers the dimer1200
family on that graph: `quip/twisted-ice-dimer` revision 1 with solver-core's
live recipe `x-w62-p1200`. Difficulty defaults are `--min-solutions 1`,
`--max-energy-milli -4850000`, and `--min-diversity-milli 0`. Band defaults
are `--band-hard-milli -5100000` and `--band-easy-milli -4800000`.
`--sudo-key` names a Foundation member and accepts a `//DevUri`, a BIP39
mnemonic, a 32-byte hex master seed, or a keystore path. `--mnemonic-file`
reads a BIP39 phrase from a file. Pass `--topology` to use a specification JSON
file instead of a preset.

Negative energy arguments accept a separate value. The drive option
`--target-energy -10000` uses energy units. The seed-chain option
`--max-energy-milli -4850000` uses milli units. Both options also accept the
equals form.

You can seed a chain only once. `seed-chain` refuses to run when a chain
already has a default topology. Wipe the chain data and restart the validator
instead.

`seed-chain` needs runtime 120 or later, which has family topologies and no
`Sudo` pallet. It sends one Foundation motion with threshold 1 that runs
register, difficulty, band, whitelist, and default through
`ValidatorAdmission::dispatch_as_root(Utility::batch_all(...))`. Threshold 1
executes at once only when the signer is the sole Foundation member, as
`//Alice` is on the `dev` preset. It refuses older runtimes.

The coordinator binary provides `zephyr-z12t4-live`, `aglais`, and `smoke`
topology presets. `drive --topology-preset` and `seed-chain` read them from
the binary. `zephyr-z12t4-live` is the live Advantage2 graph of the
2026-09-21 hardware snapshot, 4,575 nodes and 41,474 edges, byte-identical to
solver-core's `zephyr-z12t4-live` topology. Aglais is the graph the spec-119
chain registered: an older snapshot with 4,577 native node IDs, without native
edge `(938, 2812)`, and h fixed at zero. Aglais has 41,514 edges and topology
hash prefix `cbec1eb4`.

For ordinary drive results, compare miner energies with `raw_best_energy_milli`
in the `--report` JSONL file. The `best_energy_milli` field describes the diversity
selection. See `docs/COORDINATOR.md` for units, missing-energy values, and lease
summaries.

The config registers miners and their launch plan. Each backend section (`[cpu]`, `[cuda.N]`, `[metal]`, `[dwave]`) becomes one supervised subprocess. `binary` selects the executable: `quip-cpu-sa`, the chromatic `quip-cpu-gibbs`, `quip-cpu-sb`, or one of quip-solver-cpu's experimental kernels when that release published it. See `crates/quip-coordinator/config.toml.example`. `crates/quip-coordinator/tools/fetch-miners.sh` fetches the miner binaries per host from the standalone repos. The coordinator resolves them on `PATH` or by absolute path.

`signer_key` accepts a keystore path, a 32-byte hex master seed, or a `//DevUri`
such as `//Alice`. It also accepts any substrate secret URI, including a BIP39
mnemonic phrase. `.env` holds credentials such as `DWAVE_API_KEY` — **never read or display its contents.**

## The Python wheel

This repo holds no Python. The `quip-solver-core` PyPI wheel (consensus primitives + generated gRPC stubs) ships from `quip.network/quip-solver-core`, and the D-Wave miner repo depends on it. Report wheel or stub issues there.

## Conventions

- Rust standards and lints follow the `lang-rust` guidance. `cargo clippy -- -D warnings` is the gate.
- Follow the existing crate layout. Shared code goes in the crate that owns it, not a new top-level module.
- No `Co-Authored-By` or other assistant-attribution trailers in commits.
- Never read `.env`.
