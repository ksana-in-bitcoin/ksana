# Security model

The threat model that security reviews and code scans of Ksana start from. It covers this repository and the three
that pin it as the `core` submodule: ksana-web, ksana-points and ksana-site. How to report a vulnerability, and what
counts as one, is in [SECURITY.md](SECURITY.md). The priorities below are the project owner's.

## System

Ksana is a programmable chain on Bitcoin and has not launched on mainnet. Only devnet and the public signet run today,
and nothing on them holds value.

- **Fast lane**: nodes mine Argon2id PoW blocks into a GHOSTDAG block DAG and execute operations in a wasmtime VM.
- **Settlement**: each block's headers and body are written to Bitcoin as settlement and republish payloads.
- **Indexer**: runs on its own, reads only bitcoind, rebuilds the state and checks every state root.
- **Wallet signatures**: Taproot keys sign with BIP340 and BIP322; path B pays fees in KSA, path C carries the
  operation in the user's own BTC transaction.

## Components

| Component | Code | Trust boundary |
|---|---|---|
| Chain core | `crates/*` (primitives, crypto, state, execution, consensus, mempool, p2p, rpc, btc, node, indexer, cli) | Untrusted P2P peers, RPC callers, miners and BTC data |
| VM | `crates/ksana-vm`, `crates/ksana-contract-sdk` | Untrusted contract bytecode and calls |
| Contracts | `contracts/` (KS-20, AMM and factory, NFT, lending, multisig, vesting, ksa-exchange) | Untrusted callers of the example contracts |
| TS SDK | `sdk/ts` (`@ksana/sdk`) | Encodes and signs operations for browser wallets; parses untrusted node responses |
| Web clients | ksana-web (wallet and explorer), `playground/src` (playground page), ksana-site (static site) | The browser; reachable by malicious node responses or links |
| Compile service | `playground/compiler` | Runs `cargo rustc` on untrusted Rust source of up to 128 KiB |
| Points | ksana-points (service `server/`, page `src/`) | Public `/points` API: BIP322 sign-in, X links; holds the top-up seller's key |
| Signet infra | `signet/` (Compose, Caddyfile, faucet, btc-proxy, `deploy.sh`), `devnet/`, each `wrangler.jsonc` | The internet, through Caddy |

## Entry points

- **P2P and BTC data**: the P2P port (`node1:9000`) is public; anyone can publish payloads on Bitcoin, and both the
  nodes and the indexer read them.
- **RPC**: the node's JSON-RPC and WebSocket at `/rpc`, with `--rpc-cors '*'`; the indexer's RPC at `/indexer`.
- **Contracts**: deployed and called through path B or path C.
- **Public services** (`signet/Caddyfile`):
  - `/btc`: the btc-proxy, which passes four bitcoind methods and two Esplora paths, rate-limited per address;
  - `/faucet`: a cooldown per address;
  - `/compile`;
  - `/points`: CORS for the page's origin only, a bearer token after BIP322 sign-in.
- **Static pages**: Cloudflare Workers (the points page, the playground, the site).

## Assets, in priority order

1. **Mainnet-bound chain code (most critical)**, since its bugs ship to mainnet:
   - consensus between nodes, and between a node and the indexer;
   - the KSA supply cap, rewards, fees and unlocks;
   - funds held by contracts;
   - the VM's isolation and determinism;
   - signature verification.
2. **Everything else**, at normal priority, since nothing is out of scope:
   - the hosted signet's keys (faucet, seller, keeper, operator) and the bitcoind RPC credentials;
   - the points ledger, which feeds the airdrop snapshot;
   - users' signing safety in the web wallet and the playground.

## Attackers that matter

- **Malicious contract authors**: crafted wasm that escapes metering, breaks determinism, crashes nodes or takes
  funds; crafted source sent to the compile service to escape its sandbox or exhaust its resources.
- **Malicious peers, miners and publishers of BTC data**: crafted blocks, headers, P2P messages or settlement payloads
  that make nodes and the indexer disagree, stop block production, settlement or finality, exhaust CPU, memory or
  disk, or crash a process.

Anonymous users of the public endpoints, airdrop farmers and malicious sites targeting wallet users are not the focus,
but they are not excluded either: findings about them keep the severity the scanner gives them.

## Existing controls

- **Decoding**: Borsh decoding checks lengths first; a payload carries at most `MAX_SETTLEMENT_HEADERS` headers
  (D-100).
- **VM**: static checks at deployment (no floats, an import allowlist, stack frame estimates) and runtime stack
  metering (D-073); the gas table is in `docs/ABI.md`.
- **Storage**: bounded RocksDB memory (D-103).
- **Caddy**: request body limits; sets `X-Forwarded-For` itself rather than trusting the incoming value; pages carry
  `frame-ancestors 'none'`.
- **Compile service container** (`signet/docker-compose.yml`): only on the internal `compile` network, with no way
  out; `cap_drop: ALL`, `no-new-privileges`; `pids_limit` 512, `mem_limit` 2g, one CPU, tmpfs `/tmp`.
- **btc-proxy**: refuses `scantxoutset`; rate limits per address.
- **Points**: stores only the SHA-256 of session tokens; checks BIP322 with the SDK; optional Turnstile.
- **Audits and tests**: two internal audits ([SECURITY.md](SECURITY.md#audits)); fuzz targets and differential tests
  of the TS SDK against Rust (`fuzz/`).

## Out of scope and accepted risks

None. Every finding is reported; signet assets have no value, but findings that only affect the hosted signet are not
ignored for that reason.

## Open questions

- Will mainnet run the same hosted services (faucet, btc-proxy, compile, points), or only nodes and the indexer?

## Finding tags

- **Component**: the component's name from the table above.
- **Network**: `mainnet-bound` for code that ships to mainnet (crates, VM, contracts, SDK, wallet); `signet-only` for
  the hosted signet, points, the faucet and devnet.
- **Consensus-critical**: `yes` or `no`.
- **CVE**: the CVE ID of a vulnerable dependency.
