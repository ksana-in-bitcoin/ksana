# Ksana

Ksana is a programmable chain on Bitcoin. A permissionless proof-of-work fast lane confirms operations in about five
seconds and runs them in a WebAssembly VM; every operation is then written to Bitcoin, Bitcoin's chain decides
finality, and anyone can recompute the whole state from Bitcoin data alone.

- **Pay in BTC or in KSA.** Path C carries an operation in the user's own Bitcoin transaction, and the fast lane must
  include it. Path B pays gas in KSA, the chain's token, while the block producer pays the Bitcoin fee; one Bitcoin
  transaction to an exchange contract tops up KSA.
- **Bitcoin wallets sign.** An account is a Taproot address and operations are signed with BIP340 or BIP322, so
  wallets such as Unisat and Xverse work as they are.
- **Bitcoin decides.** Blocks form a GHOSTDAG block DAG mined with Argon2id, so parallel blocks are merged instead of
  wasted. Headers and bodies land on Bitcoin, a block whose data is deep enough there is final, and a standalone indexer
  that reads only Bitcoin checks every state root.

Ksana is not on mainnet yet. A public test network runs on Bitcoin's signet.

The name comes from the Sanskrit kṣaṇa, the smallest unit of time in Buddhist scripture: the fast lane confirms within
an instant, and the result is written into Bitcoin for good.

## Try it

| What | Where |
| --- | --- |
| Web wallet and explorer on the signet | <https://signet.ksana.network/> |
| Contract playground: edit, compile and deploy example contracts from the browser | <https://playground.ksana.network/> |
| Documentation | <https://ksana.network/docs/> |
| Run a node or an indexer on the signet | [`docs/SIGNET.md`](docs/SIGNET.md) |
| Community | [Discord](https://discord.gg/t7ckfmNMU) |

Everything on the signet is test money, and a consensus change before mainnet restarts it from genesis.

## Repository

| Path | Contents |
| --- | --- |
| [`crates/`](crates) | The node, the indexer, the CLI and the libraries they share: primitives and encodings, cryptography, state, the VM, execution, consensus, Bitcoin, the entry pool, RPC, P2P, and the contract SDK |
| [`contracts/`](contracts) | Example contracts (a KS-20 token, an AMM and its factory, an NFT collection, lending, multisig, vesting, a BTC-for-KSA exchange and more) and test contracts, a separate Cargo workspace |
| [`sdk/ts/`](sdk/ts) | The TypeScript SDK (`@ksana/sdk`) for browsers and Node.js |
| [`playground/`](playground) | The contract playground's page and its compile service |
| [`devnet/`](devnet) | A local network on Bitcoin regtest with Docker Compose, with its smoke and soak tests |
| [`signet/`](signet) | The hosted public signet: its genesis config and its deployment |
| [`mainnet/`](mainnet) | The mainnet genesis config template |
| [`tests/`](tests) | Regtest end-to-end tests and the test vectors shared by Rust and TypeScript |
| [`fuzz/`](fuzz) | Fuzz targets and the differential test of the TypeScript SDK against the Rust code |
| [`docs/`](docs) | The specification and the references below |
| [`audits/`](audits) | Security review reports |

The web wallet and explorer live in [ksana-web](https://github.com/ksana-in-bitcoin/ksana-web), and the site and its
documentation in [ksana-site](https://github.com/ksana-in-bitcoin/ksana-site).

## Build and test

The toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml); development and CI run on Ubuntu 22.04.

```sh
cargo build --release   # ksana-node, ksana-indexer, ksana-cli
cargo test              # unit, property and determinism tests and the shared vectors; no external services needed

# a devnet: bitcoind on regtest, three fast-lane nodes (RPC 127.0.0.1:18545-18547) and the indexer (127.0.0.1:18548)
docker compose -f devnet/docker-compose.yml --profile nodes up -d --build

# or a devnet that comes and goes: the nodes must converge, a transfer must go through, and the indexer's state roots
# must match the nodes'
devnet/smoke.sh
```

[`CONTRIBUTING.md`](CONTRIBUTING.md) has every check CI runs, the TypeScript SDK's and the playground's, and the rules
for consensus code.

## Documentation

| Document | What it covers |
| --- | --- |
| [`docs/SPEC.md`](docs/SPEC.md) | The specification: the single source of requirements for the protocol |
| [`docs/ABI.md`](docs/ABI.md) | The contract interface: host functions, the gas table and the state layout |
| [`docs/RPC.md`](docs/RPC.md) | The JSON-RPC interface of the node and the indexer |
| [`docs/STANDARDS.md`](docs/STANDARDS.md) | The token standards KS-20 and KS-721 |
| [`docs/SIGNET.md`](docs/SIGNET.md) | Joining the public signet |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | What each crate and directory does |
| [`docs/DECISIONS.md`](docs/DECISIONS.md) | The design decision log: the rationale and trade-offs behind the specification |
| [`docs/HISTORY.md`](docs/HISTORY.md) | How the project got here before the repository was published |
| [`SECURITY.md`](SECURITY.md) | Reporting a vulnerability, and the audits |

## License

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or the [MIT license](LICENSE-MIT) at your
option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this
repository, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or
conditions.
