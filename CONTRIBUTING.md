# Contributing to Ksana

Thank you for taking an interest. Ksana is a consensus system: a change that makes two honest nodes disagree is worse
than no change at all, so the rules below are strict about consensus code and about recording why things are the way
they are.

## Before you start

- [`docs/SPEC.md`](docs/SPEC.md) is the single source of requirements. [`docs/DECISIONS.md`](docs/DECISIONS.md)
  records the rationale, the trade-offs and the gaps the specification left; [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
  says what each crate and directory does.
- Report a vulnerability privately, as [`SECURITY.md`](SECURITY.md) says, never in an issue or a pull request.
- For anything larger than a fix, open an issue first, so that the change can be agreed before it is written.

## Toolchain

- Rust is pinned by [`rust-toolchain.toml`](rust-toolchain.toml) (Rust 1.98.1, edition 2021, with the
  `wasm32-unknown-unknown` target the example contracts are built for). Development and CI run on Ubuntu 22.04;
  RocksDB's bindings need `libclang`.
- Node.js 22 for the TypeScript SDK (which supports Node.js 20.19 or later), the playground and the fuzz workspace's
  differential test.
- Docker for the devnet and the regtest integration tests; a nightly toolchain and `cargo-fuzz` for fuzzing.

## The checks CI runs

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test                      # unit, property, two-process determinism tests and the shared vectors; no external services
tests/integration/run.sh        # Docker and Node.js: builds sdk/ts, compose up, cargo test -p ksana-integration-tests, down -v
devnet/smoke.sh                 # three nodes and the indexer on regtest: they converge, the CLI's transfer goes through,
                                # and the indexer's state roots match the nodes'

# The example contracts are a separate workspace
cargo fmt --manifest-path contracts/Cargo.toml --all --check
cargo clippy --manifest-path contracts/Cargo.toml --workspace --all-targets -- -D warnings
cargo test --manifest-path contracts/Cargo.toml --workspace      # native target with the contract SDK's mock host
(cd contracts && cargo clippy --workspace --target wasm32-unknown-unknown -- -D warnings)

# The TypeScript SDK, its wallet demo page, and the signet's faucet and Bitcoin proxy
(cd sdk/ts && npm ci && npm run lint && npm run build && npm test && npm run build:demo)
(cd sdk/ts && node --test '../../signet/services/test/*.test.mjs')

# The contract playground; its compile service's tests build in a slot when the wasm32 target is installed
(cd playground && npm ci && npm run typecheck && npm test && npm run build)

# Fuzzing is a separate cargo-fuzz workspace (nightly); the differential test needs the SDK built first
cargo run --manifest-path fuzz/Cargo.toml --example seeds
cargo run --release --manifest-path fuzz/Cargo.toml --example differential
(cd fuzz && cargo +nightly fuzz run host_params -- -max_total_time=60)
```

Longer runs that CI does not start on every push:

```sh
cargo test -p ksana-consensus --release --test reorg_race -- --ignored --nocapture       # payment reversal table (SPEC §10.4)
cargo test -p ksana-consensus --release --test selfish_mining -- --ignored --nocapture   # withholding table (SPEC §17.4)
cargo test -p ksana-indexer --release --test deep_reorg -- --ignored --nocapture        # Bitcoin reorgs of every depth around the anchor
KSANA_SOAK_SECONDS=3600 devnet/soak.sh   # random path B and C operations against a devnet, the indexer checked against the nodes
```

Regenerate the shared vectors only when an encoding changes on purpose, and record why in DECISIONS:

```sh
cargo run -p ksana-crypto --example gen_vectors                      # tests/vectors/{codec,signing,merkle}.json
cargo run -p ksana-consensus --example gen_pow_vectors               # tests/vectors/pow.json
cargo run -p ksana-contract-sdk --example gen_standard_vectors      # tests/vectors/standards.json
```

`crates/ksana-execution/tests/determinism.rs` runs the same blocks in two processes and compares every block's state
root with golden values; when the scenario or the execution rules change, update them as its header comment says. The
contracts it runs are fixed bytecode in `tests/data/*.wasm`, since a contract's wasm is not reproducible byte for byte
across checkout paths.

## Rules for consensus code

The crates that compute consensus results (`ksana-primitives`, `ksana-crypto`, `ksana-state`, `ksana-vm`,
`ksana-execution`, `ksana-consensus`, `ksana-btc`) follow these rules. `clippy.toml` and the `#![deny(...)]` list at the
top of every crate's `lib.rs`, the same in each, enforce most of them outside test code; a crate that really needs
`HashMap` or the system time, such as the node, allows it explicitly.

- No floating-point arithmetic and no system time: time is passed in by the caller. No reliance on the iteration order of
  `HashMap` or `HashSet`; use `BTreeMap` and `BTreeSet`.
- No `unwrap`, `expect`, `panic!`, out-of-bounds indexing or unchecked integer overflow. Errors are returned, and the
  block or the operation is judged invalid.
- The execution layer keeps three outcomes apart: a failed operation (its receipt says so and its changes are rolled
  back), an invalid block, and a local fault such as a storage error, which must never make a block invalid.
- Encodings are Borsh, and the order of fields and variants is the format: any change is a consensus change. External
  input is checked against its length limit before it is decoded, and it must be consumed entirely.
- Bitcoin transaction IDs and block hashes are stored in Bitcoin's internal byte order.
- The versions of wasmtime and jmt and the gas table are consensus parameters. VM execution depends only on its inputs
  (code, state and block environment), the gas limit is exact, and a failed operation's `gas_used` is its `max_gas`.
- A dependency outside the table in SPEC §12 needs its reason recorded in DECISIONS.

## Changing the specification

- If the specification contradicts itself, leaves a gap or cannot be implemented, record it in DECISIONS (the problem,
  the options, and the most conservative implementation, which is the one adopted until it is settled).
- The consensus was frozen on 2026-10-05 (SPEC v2 final). A consensus change after that goes through the upgrade
  mechanism of SPEC §6.9, or, before mainnet, a restart of devnet and signet from genesis, and gets its own entry in
  DECISIONS.
- A change to the CLI, the RPC, the SDK or a parameter also updates the documents that describe it: `docs/RPC.md`,
  `docs/ABI.md`, the SDK's README, and the matching page of [ksana-site](https://github.com/ksana-in-bitcoin/ksana-site).

## Commits and pull requests

- Everything in the repository is in English: code, comments, documents, commit messages, pull requests, test names and
  log messages. The one exception is the contract playground's Chinese interface (`playground/src/i18n/zh-CN/`); test
  inputs that need non-ASCII text use characters such as `ü`, `ñ`, `₿` or emoji.
- Follow the terms the code already uses: carrier, settlement, anchor, producer, block share, republish, path B and
  path C, reward slot, empty block, finality, pruning point, relay floor and so on.
- One commit per change that can be checked on its own. A commit that touches only documents ends its title with
  `[skip ci]`; that marker must not appear in any other commit message, since GitHub skips CI for a push whose last
  commit carries it anywhere in its message.
- A pull request says what it changes, why, and how it was checked; one that changes behavior also says which
  decisions or sections of the specification it follows or revises.

## License

By contributing you agree that your contributions are licensed under either of the Apache License, Version 2.0, or
the MIT license, at the user's option, as the [README](README.md#license) says.
